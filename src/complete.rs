//! Pure autocomplete context logic: prefix extraction, qualifier detection,
//! alias-map building, candidate ranking/filtering.
//!
//! GUI-free and driver-free by design: the GPUI `CompletionProvider` (in
//! `app.rs`) calls into these helpers with buffer snapshots and cached
//! metadata. All Oracle folding rules (bare folds uppercase, quoted keeps
//! case) are honored here so ranking is deterministic and unit-testable.

use std::collections::HashMap;

/// A table/view reference: optional owner + name, case as stored
/// (dictionary returns uppercase for bare identifiers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub owner: Option<String>,
    pub name: String,
}

/// Completion context at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteContext {
    /// `SELECT em|` — rank everything, columns-in-scope first.
    BareWord,
    /// After `FROM`/`JOIN`/`INTO`/`UPDATE` — tables first.
    AfterFrom,
    /// After `alias.` or `table.` — columns of that object only.
    /// `qualifier` is the raw text before the dot (`e`, `emp`, `scott.emp`).
    ColumnOf(String),
    /// Qualifier is a known sequence — offer `NEXTVAL`/`CURRVAL`.
    SequenceMember(String),
    /// In `JOIN <right> [alias] ON |` with a fresh (operator-free) condition —
    /// offer FK-derived `left.col = right.col` conditions.
    JoinOn {
        right_alias: String,
        right: TableRef,
    },
}

/// Candidate kinds for iconing/ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    JoinCondition,
    ColumnInScope,
    Table,
    Column,
    Sequence,
    Keyword,
}

/// One completion row (provider maps this to `lsp_types::CompletionItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub detail: String,
    pub kind: CandidateKind,
    /// Owning schema when known (tables/sequences/qualified columns).
    /// Drives the own-schema ranking tier.
    pub owner: Option<String>,
    /// Usage count for recency/frequency boost (0 = never used).
    pub usage: u64,
}

impl Candidate {
    fn own_schema_first(&self, own: &str) -> u8 {
        if own.is_empty() {
            return 1;
        }
        match &self.owner {
            Some(o) if o.eq_ignore_ascii_case(own) => 0,
            _ => 1,
        }
    }
}

/// True for identifier characters (matches `sql.rs` var rules minus `&`/`:`).
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '#'
}

/// Extract the word prefix ending at byte `offset`: returns (prefix, start).
/// Handles quoted identifiers (`"MixedCase|` → prefix `MixedCase`).
pub fn word_prefix(text: &str, offset: usize) -> (String, usize) {
    let offset = offset.min(text.len());
    let head = &text[..offset];
    // Quoted: cursor inside `"...` — take back to the opening quote.
    if let Some(q) = head.rfind('"') {
        let inner = &head[q + 1..];
        if inner.chars().all(|c| c != '"') && !inner.is_empty() {
            return (inner.to_string(), q + 1);
        }
        // Empty quotes or closed: no prefix.
        if inner.is_empty() {
            return (String::new(), offset);
        }
    }
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    (head[start..].to_string(), start)
}

/// Qualifier before `word_start`: `e.|` → `Some("e")`, `scott.emp.|` →
/// `Some("scott.emp")`, else `None`. Skips whitespace between dot and word.
pub fn qualifier_before(text: &str, word_start: usize) -> Option<String> {
    let mut end = word_start;
    while end > 0 && text[..end].ends_with([' ', '\t', '\n', '\r']) {
        end -= 1;
    }
    if end == 0 || !text[..end].ends_with('.') {
        return None;
    }
    let mut dot = end - 1;
    while dot > 0 && text[..dot].ends_with([' ', '\t', '\n', '\r']) {
        dot -= 1;
    }
    // Read the dotted name back: [owner.]name, possibly quoted parts.
    let head = &text[..dot];
    let name_end = head.len();
    // Last identifier segment.
    let seg_end = name_end;
    // Handle trailing quote: `"Name"`.
    let (seg_start, has_dot_before) = if head[..seg_end].ends_with('"') {
        let inner = &head[..seg_end - 1];
        let q = inner.rfind('"')?;
        (q, head[..q].trim_end().ends_with('.'))
    } else {
        let s = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_word_char(*c))
            .last()
            .map(|(i, _)| i)?;
        (s, false)
    };
    let first = head[seg_start..seg_end].trim_matches('"').to_string();
    if first.is_empty() {
        return None;
    }
    if has_dot_before {
        // `owner.` precedes: read the owner segment too.
        let before = head[..seg_start].trim_end();
        let before = before.strip_suffix('.').unwrap_or(before).trim_end();
        let owner = if before.ends_with('"') {
            let inner = before.strip_suffix('"').unwrap_or(before);
            inner.rfind('"').map(|q| before[q + 1..before.len() - 1].to_string())
        } else {
            before
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_word_char(*c))
                .last()
                .map(|(i, _)| before[i..].trim_matches('"').to_string())
        };
        match owner {
            Some(o) if !o.is_empty() => Some(format!("{o}.{first}")),
            _ => Some(first),
        }
    } else {
        // Maybe `owner.name` without quotes: check for an earlier dot.
        let before = head[..seg_start].trim_end();
        if before.ends_with('.') {
            let b2 = before.strip_suffix('.').unwrap_or(before).trim_end();
            let owner = b2
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_word_char(*c))
                .last()
                .map(|(i, _)| b2[i..].to_string())
                .unwrap_or_default();
            if owner.is_empty() {
                Some(first)
            } else {
                Some(format!("{owner}.{first}"))
            }
        } else {
            Some(first)
        }
    }
}

/// Last significant word before `pos` (uppercased). Trailing non-word
/// characters (spaces, dots, parens) are skipped first.
fn last_keyword(text: &str, pos: usize) -> String {
    let head = &text[..pos.min(text.len())];
    let t = head.trim_end_matches(|c: char| !is_word_char(c));
    let start = t
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(t.len());
    t[start..].to_ascii_uppercase()
}

/// Classify the completion context at `offset` (cursor).
/// `sequence_names` (uppercase) enables `seq.|` → `SequenceMember`.
pub fn classify_context(
    text: &str,
    offset: usize,
    sequence_names: &dyn Fn(&str) -> bool,
) -> CompleteContext {
    let (_, word_start) = word_prefix(text, offset);
    if let Some(q) = qualifier_before(text, word_start) {
        // `seq.NEXTVAL`: qualifier is a known sequence → member list.
        let last_seg = q.rsplit('.').next().unwrap_or(&q);
        if sequence_names(&last_seg.to_ascii_uppercase()) {
            return CompleteContext::SequenceMember(q);
        }
        return CompleteContext::ColumnOf(q);
    }
    match last_keyword(text, word_start).as_str() {
        "FROM" | "JOIN" | "INTO" | "UPDATE" => CompleteContext::AfterFrom,
        _ => CompleteContext::BareWord,
    }
}

/// True when `offset` sits inside a string literal, quoted identifier, or
/// comment — positions where suggestions must never trigger. Scans the
/// current line for `--` and the whole head for unclosed `'`/`"`/`/*`.
pub fn is_trivia_position(text: &str, offset: usize) -> bool {
    let offset = offset.min(text.len());
    let head = &text[..offset];
    // Line comment: `--` after the last newline with no newline after it.
    let line_start = head.rfind('\n').map(|i| i + 1).unwrap_or(0);
    if head[line_start..].contains("--") {
        return true;
    }
    // Block comment: unclosed `/*`.
    let opens = head.matches("/*").count();
    let closes = head.matches("*/").count();
    if opens > closes {
        return true;
    }
    // String / quoted identifier: odd unescaped quote count.
    let mut singles = 0u32;
    let mut doubles = 0u32;
    let mut chars = head.chars();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.clone().next() == Some('\'') {
                chars.next();
            } else {
                singles += 1;
            }
        } else if c == '"' {
            doubles += 1;
        }
    }
    singles % 2 == 1 || doubles % 2 == 1
}
/// Convert a byte offset into an LSP `(line, character)` pair (`character`
/// in UTF-16 code units, matching `position_to_offset`). Floors mid-char
/// offsets back to the boundary.
pub fn byte_to_lsp_pos(text: &str, byte: usize) -> (u32, u32) {
    let mut b = byte.min(text.len());
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    let head = &text[..b];
    let line = head.bytes().filter(|&c| c == b'\n').count() as u32;
    let line_start = head.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = head[line_start..].encode_utf16().count() as u32;
    (line, character)
}

/// Split `owner.name` or `name` (quotes stripped, case preserved).
pub fn split_dotted(name: &str) -> (Option<String>, String) {
    let name = name.trim();
    // Respect quotes: split on unquoted dot.
    let mut in_q = false;
    let mut at = None;
    for (i, c) in name.char_indices() {
        if c == '"' {
            in_q = !in_q;
        } else if c == '.' && !in_q {
            at = Some(i);
            break;
        }
    }
    match at {
        Some(i) => {
            let o = name[..i].trim().trim_matches('"').to_string();
            let n = name[i + 1..].trim().trim_matches('"').to_string();
            (Some(o), n)
        }
        None => (None, name.trim_matches('"').to_string()),
    }
}

/// Build `alias → TableRef` from `FROM`/`JOIN` clauses in one statement.
/// Handles `FROM t`, `FROM sch.t alias`, `FROM t AS a`, comma lists,
/// and `JOIN … ON` tails (stops at `ON`/`WHERE`/etc.). Keys are lowercase.
pub fn build_alias_map(statement: &str) -> HashMap<String, TableRef> {
    let mut map = HashMap::new();
    let toks = tokenize(statement);
    let mut i = 0;
    while i < toks.len() {
        let up = toks[i].to_ascii_uppercase();
        if up == "FROM" || up == "JOIN" || up == "INTO" || up == "UPDATE" {
            i += 1;
            // Comma-separated table list.
            loop {
                // Skip stray commas.
                while i < toks.len() && toks[i] == "," {
                    i += 1;
                }
                if i >= toks.len() {
                    break;
                }
                let up2 = toks[i].to_ascii_uppercase();
                if is_clause_keyword(&up2) {
                    break;
                }
                // Table ref: [owner.]name (tokens may carry quotes).
                let first = toks[i].trim_matches('"').to_string();
                i += 1;
                let mut owner: Option<String> = None;
                let mut name = first;
                if i + 1 < toks.len() && toks[i] == "." {
                    owner = Some(name);
                    name = toks[i + 1].trim_matches('"').to_string();
                    i += 2;
                }
                // Optional AS + alias, or bare alias (if not a keyword).
                let mut alias: Option<String> = None;
                if i < toks.len() && toks[i].eq_ignore_ascii_case("AS") {
                    i += 1;
                    if i < toks.len() {
                        alias = Some(toks[i].trim_matches('"').to_string());
                        i += 1;
                    }
                } else if i < toks.len()
                    && toks[i] != ","
                    && !is_clause_keyword(&toks[i].to_ascii_uppercase())
                    && toks[i] != "."
                {
                    alias = Some(toks[i].trim_matches('"').to_string());
                    i += 1;
                }
                let tref = TableRef { owner, name: name.clone() };
                // Bare name always addressable (lowercased).
                map.entry(name.to_lowercase()).or_insert_with(|| tref.clone());
                if let Some(a) = alias {
                    if !a.is_empty() {
                        map.insert(a.to_lowercase(), tref);
                    }
                }
                // Continue comma list or stop.
                if i < toks.len() && toks[i] == "," {
                    continue;
                }
                break;
            }
        } else {
            i += 1;
        }
    }
    map
}

/// Tokenize for alias parsing: words (with quotes), dots, commas, parens.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            cur.push(c);
            for c2 in chars.by_ref() {
                cur.push(c2);
                if c2 == '"' {
                    break;
                }
            }
            out.push(std::mem::take(&mut cur));
        } else if is_word_char(c) {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if c == '.' || c == ',' || c == '(' || c == ')' {
                out.push(c.to_string());
            }
            // Whitespace and the rest are separators.
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Keywords that end a table list / start a new clause.
fn is_clause_keyword(up: &str) -> bool {
    matches!(
        up,
        "WHERE" | "GROUP" | "ORDER" | "HAVING" | "ON" | "USING" | "SET" | "VALUES"
            | "SELECT" | "FROM" | "JOIN" | "INNER" | "LEFT" | "RIGHT" | "FULL" | "CROSS"
            | "UNION" | "MINUS" | "INTERSECT" | "LIMIT" | "FETCH" | "FOR" | "WHEN" | "THEN"
            | "ELSE" | "END" | "AND" | "OR" | "INTO" | "UPDATE" | "BY" | "(" | ")" | "AS"
    )
}

/// Resolve a qualifier (`e`, `emp`, `scott.emp`) through the alias map.
/// Returns the concrete table (owner may be None).
pub fn resolve_qualifier(
    qualifier: &str,
    aliases: &HashMap<String, TableRef>,
) -> Option<TableRef> {
    let (owner, name) = split_dotted(qualifier);
    if let Some(o) = owner {
        return Some(TableRef { owner: Some(o), name });
    }
    aliases.get(&name.to_lowercase()).cloned().or(Some(TableRef {
        owner: None,
        name,
    }))
}

/// A referential constraint between two tables. Column vectors are
/// position-aligned (`from_cols[i]` references `to_cols[i]`); composite
/// keys render as `a.x = b.x AND a.y = b.y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub from_owner: Option<String>,
    pub from_table: String,
    pub from_cols: Vec<String>,
    pub to_owner: Option<String>,
    pub to_table: String,
    pub to_cols: Vec<String>,
}

/// Blank out single-quoted string literals (length-preserving, so byte
/// offsets stay valid). Keeps keyword scans from tripping over prose like
/// `'join on equal terms'`. Handles `''` escapes.
fn strip_string_literals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            out.push(' ');
            // `''` escape stays inside the literal.
            loop {
                match chars.next() {
                    None => break,
                    Some('\'') => {
                        out.push(' ');
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            out.push(' ');
                        } else {
                            break;
                        }
                    }
                    Some(ch) => {
                        out.push(' ');
                        let _ = ch;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Byte end-offset just past the last whole-word occurrence of `keyword`
/// (case-insensitive) at or after `from`. Returns `None` when absent.
fn find_last_keyword(head: &str, keyword: &str, from: usize) -> Option<usize> {
    let hb = head.as_bytes();
    let kb = keyword.as_bytes();
    if kb.is_empty() || from >= hb.len() {
        return None;
    }
    let mut last = None;
    let mut i = from;
    while i + kb.len() <= hb.len() {
        if hb[i..i + kb.len()].eq_ignore_ascii_case(kb)
            && (i == 0 || !is_word_char(hb[i - 1] as char))
            && (i + kb.len() == hb.len() || !is_word_char(hb[i + kb.len()] as char))
        {
            last = Some(i + kb.len());
        }
        i += 1;
    }
    last
}

/// Whole-word check for AND/OR in a tail segment (multi-condition ON tails
/// are out of scope for suggestions).
fn has_and_or(tail: &str) -> bool {
    let clean = strip_string_literals(tail);
    find_last_keyword(&clean, "AND", 0).is_some()
        || find_last_keyword(&clean, "OR", 0).is_some()
}

/// Detect `JOIN <table> [[AS] alias] ON |` with the cursor in a fresh
/// (operator-free, single-condition) ON tail. Returns the right side as
/// written plus its resolved table. `head` is buffer text before the cursor;
/// `aliases` resolves `AS` aliases to concrete tables.
///
/// v1 limits: only the first (operator-free) condition suggests; `AND`/`OR`
/// tails and quoted JOIN prose fall back to normal completion.
pub fn detect_join_on(
    head: &str,
    aliases: &HashMap<String, TableRef>,
) -> Option<(String, TableRef)> {
    let clean = strip_string_literals(head);
    // Last JOIN at/after buffer start (`find_last_keyword` returns the end
    // offset; back up over the keyword for slicing).
    let join_end = find_last_keyword(&clean, "JOIN", 0)?;
    let join_pos = join_end.saturating_sub("JOIN".len());
    // Parse `[owner.]table [[AS] alias]` right after JOIN.
    let after = &clean[join_pos..];
    let toks = tokenize(after);
    if toks.first().map(|t| t.eq_ignore_ascii_case("JOIN")) != Some(true) {
        return None;
    }
    let mut i = 1;
    if i >= toks.len() {
        return None;
    }
    let first = toks[i].trim_matches('"').to_string();
    i += 1;
    let mut owner: Option<String> = None;
    let mut name = first;
    if toks.get(i) == Some(&".".to_string()) {
        owner = Some(name);
        name = toks.get(i + 1).map(|s| s.trim_matches('"').to_string())?;
        i += 2;
    }
    // Optional alias: [AS] word that is not ON/a clause keyword.
    let mut alias = name.clone();
    if toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case("AS")) {
        i += 1;
        if let Some(a) = toks.get(i) {
            alias = a.trim_matches('"').to_string();
            i += 1;
        }
    } else if let Some(a) = toks.get(i) {
        if !a.eq_ignore_ascii_case("ON") && !is_clause_keyword(&a.to_ascii_uppercase()) {
            alias = a.trim_matches('"').to_string();
            i += 1;
        }
    }
    // ON must follow the reference.
    if !toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case("ON")) {
        return None;
    }
    // Byte offset of that ON in the original head: search after join_pos.
    // (`clean` preserves length, so offsets transfer.)
    let on_end = find_last_keyword(&clean, "ON", join_pos)?;
    let tail = &head[on_end.min(head.len())..];
    if tail.contains('=') || has_and_or(tail) {
        return None;
    }
    let tref = match aliases.get(&alias.to_lowercase()) {
        Some(t) => t.clone(),
        None => TableRef { owner, name },
    };
    Some((alias, tref))
}

/// FK join-condition candidates between the just-joined table and every
/// other in-scope table: `{left} = {right}` with aliases as written,
/// composite keys joined by ` AND `. Deterministic (sorted by left alias).
/// Empty when no FK links the pair — the caller then shows no popup.
pub fn join_condition_candidates(
    right_alias: &str,
    right: &TableRef,
    aliases: &HashMap<String, TableRef>,
    fks: &[ForeignKey],
) -> Vec<Candidate> {
    let lefts: Vec<(&String, &TableRef)> = aliases
        .iter()
        .filter(|(a, t)| {
            !a.eq_ignore_ascii_case(right_alias)
                && !(t.name.eq_ignore_ascii_case(&right.name)
                    && owners_match(&t.owner, &right.owner))
        })
        .collect();
    // De-dupe: the alias map holds both bare names and aliases for one
    // table — prefer the alias as written (`e.…`, not `emp.…`).
    let mut by_table: HashMap<(String, String), Vec<(&String, &TableRef)>> = HashMap::new();
    for (a, t) in lefts {
        by_table
            .entry((
                t.owner.clone().unwrap_or_default().to_ascii_uppercase(),
                t.name.to_ascii_uppercase(),
            ))
            .or_default()
            .push((a, t));
    }
    let mut lefts: Vec<(&String, &TableRef)> = by_table
        .values()
        .map(|group| {
            group
                .iter()
                .find(|(a, t)| !a.eq_ignore_ascii_case(&t.name))
                .or_else(|| group.first())
                .expect("non-empty group")
        })
        .map(|(a, t)| (*a, *t))
        .collect();
    lefts.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = Vec::new();
    for (left_alias, left) in lefts {
        for fk in fks {
            // Either direction; rendering is always left-first.
            let pairs = fk_pairs(fk, left, right).or_else(|| {
                fk_pairs(fk, right, left).map(|(rcols, lcols)| (lcols, rcols))
            });
            let Some((lcols, rcols)) = pairs else {
                continue;
            };
            let cond = lcols
                .iter()
                .zip(rcols.iter())
                .map(|(l, r)| format!("{left_alias}.{l} = {right_alias}.{r}"))
                .collect::<Vec<_>>()
                .join(" AND ");
            out.push(Candidate {
                label: cond,
                detail: format!("JOIN · {}", fk.name),
                kind: CandidateKind::JoinCondition,
                owner: None,
                usage: 0,
            });
        }
    }
    out
}

/// Column pairs with both ends on (`a`, `b`) respectively. Owners match when
/// both known; unknown sides wildcard.
fn fk_pairs(fk: &ForeignKey, a: &TableRef, b: &TableRef) -> Option<(Vec<String>, Vec<String>)> {
    let fwd = fk.from_table.eq_ignore_ascii_case(&a.name)
        && fk.to_table.eq_ignore_ascii_case(&b.name)
        && owners_match(&fk.from_owner, &a.owner)
        && owners_match(&fk.to_owner, &b.owner);
    if fwd && fk.from_cols.len() == fk.to_cols.len() && !fk.from_cols.is_empty() {
        return Some((fk.from_cols.clone(), fk.to_cols.clone()));
    }
    None
}

fn owners_match(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => true,
    }
}

/// Oracle keywords worth completing (v1 set — statements, clauses, common
/// functions, sequence pseudo-columns). Uppercase; matching is case-insensitive.
pub const ORACLE_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "OUTER", "ON",
    "GROUP", "BY", "ORDER", "HAVING", "UNION", "UNION ALL", "MINUS", "INTERSECT",
    "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "MERGE", "USING",
    "WHEN", "THEN", "ELSE", "END", "CASE", "AND", "OR", "NOT", "IN", "EXISTS",
    "BETWEEN", "LIKE", "IS", "NULL", "DISTINCT", "ALL", "AS", "ASC", "DESC",
    "WITH", "CONNECT", "START", "PRIOR", "SIBLINGS", "ROWNUM", "ROWID",
    "NEXTVAL", "CURRVAL", "SYSDATE", "SYSTIMESTAMP", "DUAL",
    "COUNT", "SUM", "AVG", "MIN", "MAX", "NVL", "NVL2", "COALESCE",
    "TO_DATE", "TO_CHAR", "TO_NUMBER", "TRUNC", "DECODE", "SUBSTR", "INSTR",
    "UPPER", "LOWER", "TRIM", "LENGTH", "SYSDATE", "COMMIT", "ROLLBACK",
    "CREATE", "TABLE", "VIEW", "INDEX", "SEQUENCE", "DESCRIBE", "EXPLAIN",
    "GRANT", "ORDER", "SIBLINGS",
];

/// System schemas hidden by default (toggleable). Covers the Oracle
/// catalogs a DBA account sees but never queries: core, spatial/text,
/// vault/audit/label-security, GSM, APEX/FLOWS, and maintenance accounts.
/// Shared with `metadata.rs`, which excludes them at the SQL level.
pub const SYSTEM_SCHEMAS: &[&str] = &[
    "SYS", "SYSTEM", "XDB", "MDSYS", "MDDATA", "CTXSYS", "ORDSYS", "ORDDATA", "ORDPLUGINS",
    "OLAPSYS", "SI_INFORMTN_SCHEMA", "WMSYS", "DBSNMP", "OUTLN", "EXFSYS", "DIP", "TSMSYS",
    "DMSYS", "ODM", "ODM_MTR", "ANONYMOUS", "APPQOSSYS", "AUDSYS", "DVSYS", "DVF", "DV_MONITOR",
    "LBACSYS", "GSMADMIN_INTERNAL", "GSMCATUSER", "GSMROOTUSER", "GSMUSER",
    "REMOTE_SCHEDULER_AGENT", "SYSBACKUP", "SYSDG", "SYSKM", "SYSRAC", "ORACLE_OCM",
    "SPATIAL_WFS_ADMIN_USR", "SPATIAL_CSW_ADMIN_USR", "XS$NULL", "OJVMSYS", "SYSMAN",
];

/// Versioned/prefixed system families (`APEX_240200`, `FLOWS_300100`, …).
pub fn is_system_schema_prefix(owner_upper: &str) -> bool {
    owner_upper.starts_with("APEX_") || owner_upper.starts_with("FLOWS_")
}

pub fn is_system_schema(owner: &str) -> bool {
    let up = owner.to_ascii_uppercase();
    is_system_schema_prefix(&up) || SYSTEM_SCHEMAS.contains(&up.as_str())
}

/// Rank + filter candidates for `prefix` (case-insensitive). Order:
/// kind rank, exact match, own-schema tier, prefix quality, usage, length,
/// label. `own_schema` is the connected user ("" = no preference).
/// Empty prefix returns kind order capped at `limit`.
pub fn rank_candidates(
    prefix: &str,
    mut cands: Vec<Candidate>,
    own_schema: &str,
    limit: usize,
) -> Vec<Candidate> {
    let pre = prefix.to_lowercase();
    cands.retain(|c| {
        if pre.is_empty() {
            return true;
        }
        let l = c.label.to_lowercase();
        l.starts_with(&pre) || l.contains(&pre)
    });
    cands.sort_by(|a, b| {
        let ka = kind_rank(a.kind);
        let kb = kind_rank(b.kind);
        // Exact match (case-insensitive) beats everything within a kind:
        // typing `emp` should offer `EMP` before a well-worn `EMPLOYEE_AUDIT`.
        let ea = u8::from(!a.label.eq_ignore_ascii_case(&pre));
        let eb = u8::from(!b.label.eq_ignore_ascii_case(&pre));
        ka.cmp(&kb)
            .then_with(|| ea.cmp(&eb))
            .then_with(|| {
                a.own_schema_first(own_schema)
                    .cmp(&b.own_schema_first(own_schema))
            })
            .then_with(|| prefix_score(&a.label, &pre).cmp(&prefix_score(&b.label, &pre)))
            .then_with(|| b.usage.cmp(&a.usage))
            .then_with(|| a.label.len().cmp(&b.label.len()))
            .then_with(|| a.label.cmp(&b.label))
    });
    cands.truncate(limit);
    cands
}

fn kind_rank(k: CandidateKind) -> u8 {
    match k {
        CandidateKind::JoinCondition => 0,
        CandidateKind::ColumnInScope => 1,
        CandidateKind::Table => 2,
        CandidateKind::Column => 3,
        CandidateKind::Sequence => 4,
        CandidateKind::Keyword => 5,
    }
}

/// 0 = prefix match, 1 = contains, 2 = no match (empty prefix = 0).
/// For qualified labels (`SCOTT.EMP`) the object part decides: typing `emp`
/// must prefer `SYSTEM.EMPLOYEES` over `SYS.MVIEW$_ADV_TEMP`.
fn prefix_score(label: &str, pre: &str) -> u8 {
    if pre.is_empty() {
        return 0;
    }
    // Don't split quoted labels (`"My.Table"` is one object).
    let short = if label.starts_with('"') && label.ends_with('"') && label.len() > 1 {
        label
    } else {
        label.rsplit('.').next().unwrap_or(label)
    };
    if short.to_lowercase().starts_with(pre) {
        0
    } else if label.to_lowercase().contains(pre) {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_extracts_word_and_offset() {
        assert_eq!(word_prefix("SELECT em", 9), ("em".to_string(), 7));
        assert_eq!(word_prefix("SELECT ", 7), ("".to_string(), 7));
        assert_eq!(word_prefix("e.emp_", 6), ("emp_".to_string(), 2));
        assert_eq!(word_prefix("SELECT \"Mixed", 14), ("Mixed".to_string(), 8));
    }

    #[test]
    fn qualifier_detects_dotted() {
        assert_eq!(qualifier_before("SELECT e.", 9), Some("e".to_string()));
        let sql = "SELECT scott.emp.";
        assert_eq!(
            qualifier_before(sql, sql.len()),
            Some("scott.emp".to_string())
        );
        assert_eq!(qualifier_before("SELECT em", 9), None);
        assert_eq!(qualifier_before("SELECT e. ", 10), Some("e".to_string()));
    }

    #[test]
    fn context_after_from_and_dot() {
        let no_seq = |_: &str| false;
        assert_eq!(
            classify_context("SELECT * FROM ", 14, &no_seq),
            CompleteContext::AfterFrom
        );
        assert_eq!(
            classify_context("SELECT e.", 9, &no_seq),
            CompleteContext::ColumnOf("e".to_string())
        );
        assert_eq!(
            classify_context("SELECT em", 9, &no_seq),
            CompleteContext::BareWord
        );
        let is_seq = |n: &str| n == "MYSEQ";
        assert_eq!(
            classify_context("SELECT myseq.", 13, &is_seq),
            CompleteContext::SequenceMember("myseq".to_string())
        );
    }

    #[test]
    fn alias_map_from_join_and_commas() {
        let m = build_alias_map("SELECT * FROM scott.emp e JOIN dept d ON e.deptno = d.deptno");
        assert_eq!(m["e"].name, "emp");
        assert_eq!(m["e"].owner.as_deref(), Some("scott"));
        assert_eq!(m["d"].name, "dept");
        let m2 = build_alias_map("SELECT * FROM emp, dept WHERE emp.deptno = dept.deptno");
        assert_eq!(m2["emp"].name, "emp");
        assert_eq!(m2["dept"].name, "dept");
        let m3 = build_alias_map("SELECT * FROM employees AS e");
        assert_eq!(m3["e"].name, "employees");
    }

    #[test]
    fn resolve_qualifier_prefers_alias_then_bare() {
        let m = build_alias_map("SELECT * FROM scott.emp e");
        let r = resolve_qualifier("e", &m).unwrap();
        assert_eq!((r.owner.as_deref(), r.name.as_str()), (Some("scott"), "emp"));
        let r = resolve_qualifier("scott.emp", &m).unwrap();
        assert_eq!((r.owner.as_deref(), r.name.as_str()), (Some("scott"), "emp"));
        let r = resolve_qualifier("dept", &m).unwrap();
        assert_eq!(r.name, "dept");
    }

    #[test]
    fn ranking_prefers_scope_then_prefix_then_usage() {
        let cand = |label: &str, kind: CandidateKind, usage: u64| Candidate {
            label: label.into(),
            detail: "".into(),
            kind,
            owner: None,
            usage,
        };
        let cands = vec![
            cand("EMPLOYEE_AUDIT", CandidateKind::Table, 99),
            cand("EMP", CandidateKind::Table, 0),
            cand("EMP_ID", CandidateKind::ColumnInScope, 0),
            cand("SELECT", CandidateKind::Keyword, 0),
        ];
        let out = rank_candidates("emp", cands, "", 10);
        assert_eq!(out[0].label, "EMP_ID");
        assert_eq!(out[1].label, "EMP");
        // Usage breaks ties between equal-quality prefix matches.
        let tied = vec![
            cand("DEPT", CandidateKind::Table, 1),
            cand("DEPTNO", CandidateKind::Table, 50),
        ];
        let out = rank_candidates("dep", tied, "", 10);
        assert_eq!(out[0].label, "DEPTNO");
    }

    #[test]
    fn ranking_prefers_own_schema() {
        let cand = |label: &str, owner: &str| Candidate {
            label: label.into(),
            detail: "".into(),
            kind: CandidateKind::Table,
            owner: Some(owner.into()),
            usage: 0,
        };
        let cands = vec![
            cand("DVSYS.DBA_X", "DVSYS"),
            cand("SCOTT.DEPT", "SCOTT"),
        ];
        let out = rank_candidates("d", cands, "SCOTT", 10);
        assert_eq!(out[0].label, "SCOTT.DEPT");
    }

    #[test]
    fn dotted_labels_score_on_object_part() {
        let cand = |label: &str| Candidate {
            label: label.into(),
            detail: "".into(),
            kind: CandidateKind::Table,
            owner: None,
            usage: 0,
        };
        // `emp` must prefer the table named EMP… over *TEMP* substring noise.
        let cands = vec![
            cand("SYS.MVIEW$_ADV_TEMP"),
            cand("SYSTEM.EMPLOYEES"),
        ];
        let out = rank_candidates("emp", cands, "", 10);
        assert_eq!(out[0].label, "SYSTEM.EMPLOYEES");
    }

    #[test]
    fn system_schemas_flagged() {
        assert!(is_system_schema("SYS"));
        assert!(is_system_schema("sys"));
        assert!(is_system_schema("DVSYS"));
        assert!(is_system_schema("AUDSYS"));
        assert!(is_system_schema("APEX_240200"));
        assert!(is_system_schema("FLOWS_300100"));
        assert!(is_system_schema("GSMADMIN_INTERNAL"));
        assert!(!is_system_schema("SCOTT"));
        assert!(!is_system_schema("HR"));
    }

    #[test]
    fn trivia_positions_detected() {
        assert!(is_trivia_position("SELECT '--x", 11));
        assert!(is_trivia_position("SELECT 1 -- foo", 15));
        assert!(is_trivia_position("SELECT /* open", 14));
        assert!(!is_trivia_position("SELECT /* shut */ 1", 18));
        assert!(!is_trivia_position("SELECT emp", 10));
        assert!(is_trivia_position("SELECT \"AB", 10));
    }

    #[test]
    fn detect_join_on_finds_fresh_condition() {
        let aliases = build_alias_map("SELECT * FROM emp e JOIN dept d ON ");
        let (alias, tref) = detect_join_on("SELECT * FROM emp e JOIN dept d ON ", &aliases).unwrap();
        assert_eq!(alias, "d");
        assert_eq!(tref.name, "dept");
        // AS alias + owner-qualified.
        let aliases =
            build_alias_map("SELECT * FROM scott.emp JOIN scott.dept AS dd ON ");
        let (alias, tref) =
            detect_join_on("SELECT * FROM scott.emp JOIN scott.dept AS dd ON ", &aliases).unwrap();
        assert_eq!(alias, "dd");
        assert_eq!(tref.owner.as_deref(), Some("scott"));
        // No ON yet → None.
        assert!(detect_join_on("SELECT * FROM emp e JOIN dept d", &aliases).is_none());
        // Condition already started → None (v1: first condition only).
        assert!(detect_join_on("SELECT * FROM emp e JOIN dept d ON e.x = 1", &aliases).is_none());
        assert!(detect_join_on("SELECT * FROM emp e JOIN dept d ON e.x = 1 AND ", &aliases).is_none());
        // Quoted JOIN prose doesn't fool it.
        assert!(detect_join_on("SELECT 'join dept on ' FROM emp e", &aliases).is_none());
    }

    #[test]
    fn join_conditions_render_alias_pairs_both_directions() {
        let aliases = build_alias_map("SELECT * FROM emp e JOIN dept d ON ");
        let fk = ForeignKey {
            name: "EMP_DEPT_FK".into(),
            from_owner: Some("SCOTT".into()),
            from_table: "EMP".into(),
            from_cols: vec!["DEPTNO".into()],
            to_owner: Some("SCOTT".into()),
            to_table: "DEPT".into(),
            to_cols: vec!["DEPTNO".into()],
        };
        let right = TableRef { owner: None, name: "dept".into() };
        let out = join_condition_candidates("d", &right, &aliases, &[fk]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "e.DEPTNO = d.DEPTNO");
        assert_eq!(out[0].kind, CandidateKind::JoinCondition);
        // Reversed FK direction renders left-first too.
        let fk_rev = ForeignKey {
            name: "DEPT_MGR_FK".into(),
            from_owner: None,
            from_table: "dept".into(),
            from_cols: vec!["MGR".into()],
            to_owner: None,
            to_table: "emp".into(),
            to_cols: vec!["EMPNO".into()],
        };
        let out = join_condition_candidates("d", &right, &aliases, &[fk_rev]);
        assert_eq!(out[0].label, "e.EMPNO = d.MGR");
        // Composite keys join with AND.
        let fk_multi = ForeignKey {
            name: "COMP_FK".into(),
            from_owner: None,
            from_table: "emp".into(),
            from_cols: vec!["A".into(), "B".into()],
            to_owner: None,
            to_table: "dept".into(),
            to_cols: vec!["A".into(), "B".into()],
        };
        let out = join_condition_candidates("d", &right, &aliases, &[fk_multi]);
        assert_eq!(out[0].label, "e.A = d.A AND e.B = d.B");
        // Unrelated tables → no candidates (caller shows no popup).
        let aliases2 = build_alias_map("SELECT * FROM emp e JOIN bonus b ON ");
        let right2 = TableRef { owner: None, name: "bonus".into() };
        let fk = ForeignKey {
            name: "EMP_DEPT_FK".into(),
            from_owner: None,
            from_table: "EMP".into(),
            from_cols: vec!["DEPTNO".into()],
            to_owner: None,
            to_table: "DEPT".into(),
            to_cols: vec!["DEPTNO".into()],
        };
        assert!(join_condition_candidates("b", &right2, &aliases2, &[fk]).is_empty());
    }

    #[test]
    fn byte_to_lsp_pos_counts_utf16() {
        let text = "SELECT *\nFROM émp;";
        // Line 1 (0-based), after "FROM ".
        let off = text.find("émp").unwrap();
        assert_eq!(byte_to_lsp_pos(text, off), (1, 5));
        // `é` is 1 UTF-16 unit; mid-char offsets floor back.
        assert_eq!(byte_to_lsp_pos(text, off + 1), (1, 5));
        assert_eq!(byte_to_lsp_pos(text, off + 2), (1, 6));
        assert_eq!(byte_to_lsp_pos(text, 0), (0, 0));
        assert_eq!(byte_to_lsp_pos(text, 999), byte_to_lsp_pos(text, text.len()));
    }

    #[test]
    fn split_dotted_handles_quotes() {
        assert_eq!(
            split_dotted("scott.emp"),
            (Some("scott".to_string()), "emp".to_string())
        );
        assert_eq!(
            split_dotted("\"MixedCase\""),
            (None, "MixedCase".to_string())
        );
        assert_eq!(split_dotted("emp"), (None, "emp".to_string()));
    }
}
