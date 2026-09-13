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

/// Completion context at the cursor. Strict by design: each position
/// offers only what SQL allows there — ranking never mixes kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteContext {
    /// Empty statement head — statement starters only.
    StatementStart,
    /// After `SELECT`/`DISTINCT`/`,`/`(`/operators in select scope —
    /// in-scope columns, functions, expression keywords. Never tables.
    SelectList,
    /// After `FROM`/`JOIN`/`INTO`/`UPDATE`/`TABLE`/`USING` with no table
    /// yet — tables only.
    AfterFrom,
    /// After `WHERE`/`GROUP`/`ORDER`/`HAVING`/`BY`/`AND`/`OR`/`SET`/`WHEN`/
    /// `ON` (started condition) — in-scope columns, functions. Never tables.
    Predicate,
    /// `owner.` after `FROM`/`JOIN` — tables of that owner (bare names).
    OwnerTables(String),
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
    /// Ambiguous (after a complete identifier/literal, post-paren, misc
    /// keywords) — keywords + functions only. Never tables/columns.
    BareWord,
}

/// Candidate kinds for iconing/ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    JoinCondition,
    ColumnInScope,
    Table,
    Column,
    Sequence,
    Function,
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
/// Completion semantics: only text *before* the cursor matters.
/// Handles quoted identifiers (`"MixedCase|` → prefix `MixedCase`).
pub fn word_prefix(text: &str, offset: usize) -> (String, usize) {
    let offset = offset.min(text.len());
    let head = &text[..offset];
    // Quoted: cursor inside `"...` — take back to the opening quote.
    // The opener comes from the scope-aware scan (not a bare rfind):
    // a closed pair — on this line or an earlier one, e.g. an @-script
    // `"path"` — must never claim the prefix. It used to swallow the
    // whole buffer tail and kill every popup below line 1.
    if let Some(q) = scan_head(head).1 {
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

/// Lexer scopes for scanning a buffer head with correct nesting:
///
/// - quotes inside comments never count (apostrophes in `-- don't`),
/// - comment markers inside strings never count (`'--'`, `'/*'`),
/// - `''` / `""` are single escaped quotes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HeadScope {
    Code,
    LineComment,
    BlockComment,
    SingleString,
    DoubleString,
}

/// Scan `head` (text before the cursor): whether the cursor sits inside
/// trivia (comment/string — no suggestions/cards there), plus the byte
/// offset of a `"` opening an unterminated quoted identifier, if any.
///
/// A single pass serves both questions so they can never disagree about
/// scope. Byte-stepping is safe: only ASCII markers are compared, and
/// ASCII bytes never appear inside multi-byte UTF-8 sequences.
fn scan_head(head: &str) -> (bool, Option<usize>) {
    let b = head.as_bytes();
    let mut i = 0;
    let mut scope = HeadScope::Code;
    let mut open_double: Option<usize> = None;
    while i < b.len() {
        match scope {
            HeadScope::Code => match b[i] {
                b'\'' => scope = HeadScope::SingleString,
                b'"' => {
                    open_double = Some(i);
                    scope = HeadScope::DoubleString;
                }
                b'-' if b.get(i + 1) == Some(&b'-') => scope = HeadScope::LineComment,
                b'/' if b.get(i + 1) == Some(&b'*') => scope = HeadScope::BlockComment,
                _ => {}
            },
            HeadScope::LineComment => {
                if b[i] == b'\n' {
                    scope = HeadScope::Code;
                }
            }
            HeadScope::BlockComment => {
                if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    scope = HeadScope::Code;
                    i += 1;
                }
            }
            HeadScope::SingleString => {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 1;
                    } else {
                        scope = HeadScope::Code;
                    }
                }
            }
            HeadScope::DoubleString => {
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        i += 1;
                    } else {
                        scope = HeadScope::Code;
                        open_double = None;
                    }
                }
            }
        }
        i += 1;
    }
    (scope != HeadScope::Code, open_double)
}

/// Full word under byte `offset`: returns (word, start). Hover semantics —
/// unlike [`word_prefix`], extends *forward* past the cursor, so a pointer
/// mid-`EMPLOYEES` yields the whole table name, not the `EMPL` prefix.
/// Quoted identifiers return the inner name without quotes.
pub fn word_at(text: &str, offset: usize) -> (String, usize) {
    let offset = offset.min(text.len());
    let (prefix, start) = word_prefix(text, offset);
    // Forward run from the cursor (stops at `.`, whitespace, `"`, …).
    let tail = &text[offset..];
    let end = tail
        .char_indices()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if prefix.is_empty() {
        // At a word start (or on a non-word char): forward only.
        if end == 0 {
            return (String::new(), offset);
        }
        return (tail[..end].to_string(), offset);
    }
    let mut word = prefix;
    word.push_str(&tail[..end]);
    (word, start)
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
            inner
                .rfind('"')
                .map(|q| before[q + 1..before.len() - 1].to_string())
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

/// Classify the completion context at `offset` (cursor), scoped to the
/// current statement. `sequence_names` (uppercase) enables `seq.|` →
/// `SequenceMember`.
pub fn classify_context(
    text: &str,
    offset: usize,
    sequence_names: &dyn Fn(&str) -> bool,
) -> CompleteContext {
    use crate::sql::split_statements;
    let offset = offset.min(text.len());
    let (_, word_start) = word_prefix(text, offset);
    // Scope to the containing statement (multi-statement buffers); past the
    // end or in a gap, the head is empty → StatementStart.
    let stmts = split_statements(text);
    let base = stmts
        .iter()
        .find(|s| s.start <= offset && offset < s.end)
        .or_else(|| {
            stmts
                .iter()
                .rev()
                .find(|s| s.start <= offset && offset <= s.end)
        })
        .map(|s| s.start)
        .unwrap_or(offset);
    let ws = word_start.max(base).min(text.len());
    let toks = tokenize(&text[base..ws]);
    // Qualifier first: `x.|` completes members of x — tables when the
    // qualifier follows FROM/JOIN, sequence members for sequences,
    // columns otherwise.
    if let Some(q) = qualifier_before(text, word_start) {
        if scan_clause(&toks) == Clause::From {
            return CompleteContext::OwnerTables(q);
        }
        let last_seg = q.rsplit('.').next().unwrap_or(&q);
        if sequence_names(&last_seg.to_ascii_uppercase()) {
            return CompleteContext::SequenceMember(q);
        }
        return CompleteContext::ColumnOf(q);
    }
    match scan_clause(&toks) {
        Clause::Start => CompleteContext::StatementStart,
        Clause::Select => CompleteContext::SelectList,
        Clause::From => CompleteContext::AfterFrom,
        Clause::Predicate => CompleteContext::Predicate,
        Clause::Bare => CompleteContext::BareWord,
    }
}

/// Clause scope from a token scan (see `scan_clause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    Start,
    Select,
    From,
    Predicate,
    Bare,
}

/// Map an uppercased word to its clause. `None` = identifier/literal.
fn clause_keyword(word_upper: &str) -> Option<Clause> {
    match word_upper {
        "SELECT" | "DISTINCT" => Some(Clause::Select),
        "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "USING" => Some(Clause::From),
        "WHERE" | "GROUP" | "ORDER" | "HAVING" | "BY" | "AND" | "OR" | "SET" | "WHEN" | "ON" => {
            Some(Clause::Predicate)
        }
        _ => None,
    }
}

/// Walk tokens back from the cursor word: commas start a fresh list item
/// (skip the completed word before them), DISTINCT/ALL/AS are transparent,
/// `(` opens expression scope, `)` and misc keywords bail to Bare, and a
/// `.` skips its qualifier. A bare identifier between the cursor and a
/// SELECT/FROM keyword means alias/complete-identifier position → Bare
/// (never tables/columns); other clauses ignore it.
fn scan_clause(toks: &[String]) -> Clause {
    if toks.is_empty() {
        return Clause::Start;
    }
    let mut saw_ident = false;
    let mut skip_word = false;
    let mut depth = 0u32;
    for t in toks.iter().rev() {
        match t.as_str() {
            "," => {
                saw_ident = false;
                skip_word = true;
                continue;
            }
            "(" => {
                if depth > 0 {
                    depth -= 1;
                    // Function name precedes `(` — not a table/alias.
                    skip_word = true;
                    continue;
                }
                return Clause::Select;
            }
            ")" => {
                depth += 1;
                continue;
            }
            "." => {
                skip_word = true;
                continue;
            }
            _ => {}
        }
        let up = t.to_ascii_uppercase();
        if up == "DISTINCT" || up == "ALL" || up == "AS" {
            saw_ident = false;
            skip_word = false;
            continue;
        }
        if skip_word && clause_keyword(up.as_str()).is_none() {
            skip_word = false;
            continue;
        }
        skip_word = false;
        if depth > 0 {
            continue;
        }
        match clause_keyword(up.as_str()) {
            Some(Clause::Select) => {
                return if saw_ident {
                    Clause::Bare
                } else {
                    Clause::Select
                };
            }
            Some(Clause::From) => {
                return if saw_ident {
                    Clause::Bare
                } else {
                    Clause::From
                };
            }
            Some(other) => return other,
            None => saw_ident = true,
        }
    }
    Clause::Bare
}

/// True when an empty prefix may still pop up: cursor right after an
/// operand-expecting clause keyword (`FROM |`, `WHERE |`, `SELECT |`).
/// Guards the space-trigger so post-identifier spaces stay quiet.
pub fn allows_empty_prefix(text: &str, offset: usize) -> bool {
    use crate::sql::split_statements;
    let offset = offset.min(text.len());
    let stmts = split_statements(text);
    let base = stmts
        .iter()
        .find(|s| s.start <= offset && offset < s.end)
        .or_else(|| {
            stmts
                .iter()
                .rev()
                .find(|s| s.start <= offset && offset <= s.end)
        })
        .map(|s| s.start)
        .unwrap_or(offset);
    let toks = tokenize(&text[base..offset]);
    matches!(
        scan_clause(&toks),
        Clause::Select | Clause::From | Clause::Predicate
    )
}
/// True when `offset` sits inside a string literal, quoted identifier, or
/// comment — positions where suggestions must never trigger. A single
/// scope-aware scan: quotes inside comments never count (apostrophes in
/// `-- don't`), comment markers inside strings never count (`'--'`,
/// `'/*'`), and `''` / `""` are escaped quotes.
pub fn is_trivia_position(text: &str, offset: usize) -> bool {
    let offset = offset.min(text.len());
    scan_head(&text[..offset]).0
}
/// Column names (uppercase) appearing in more than one scope table.
/// Inserting those bare is ambiguous SQL — callers qualify them.
/// `scope` is `(owner, table, column_names...)` per in-scope table.
pub fn ambiguous_columns(scope: &[ScopeTable]) -> std::collections::HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for t in scope {
        for c in &t.cols {
            *counts.entry(c.name.to_ascii_uppercase()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(c, _)| c)
        .collect()
}

use crate::metadata::{ColumnMeta, MetadataCache};

/// Scope table entry for ambiguity analysis: owner, table, and its columns.
pub struct ScopeTable {
    pub owner: Option<String>,
    pub table: String,
    pub cols: Vec<ColumnMeta>,
}

/// A scope table's display label for qualified inserts: the alias as
/// written when one exists (`e`), else the table name itself. `table` must
/// be in scope (callers fall back to it when this returns `None`).
pub fn scope_label(
    owner: &Option<String>,
    table: &str,
    aliases: &HashMap<String, TableRef>,
) -> Option<String> {
    let mut names: Vec<&String> = aliases.keys().collect();
    names.sort();
    names.into_iter().find_map(|a| {
        let t = &aliases[a.as_str()];
        (t.name.eq_ignore_ascii_case(table) && owners_match(&t.owner, owner)).then(|| a.clone())
    })
}

/// Display name for an object: bare `name` when `owner` is the connected
/// user's own schema (Oracle resolves unqualified names to it first, so the
/// prefix is noise), else `OWNER.name`. Used for suggestion labels/inserts,
/// hover titles, and DESCRIBE statements alike.
pub fn display_name(owner: Option<&str>, name: &str, own_schema: &str) -> String {
    match owner {
        Some(o) if !o.is_empty() && !o.eq_ignore_ascii_case(own_schema) => {
            format!("{o}.{name}")
        }
        _ => name.to_string(),
    }
}

/// Hover card markdown for the word under the cursor. `qualifier` is the
/// `x` in `x.word` (None for bare words); `aliases` maps the current
/// statement's aliases. Returns None when nothing reliable can be said
/// (unknown object, ambiguous bare column, system object while hidden).
/// Pure and snapshot-driven — the provider calls it with cached data.
pub fn hover_markdown(
    word: &str,
    qualifier: Option<&str>,
    aliases: &HashMap<String, TableRef>,
    cache: &MetadataCache,
    show_system: bool,
    own_schema: &str,
) -> Option<String> {
    if word.is_empty() {
        return None;
    }
    let hidden = |owner: &str| {
        !show_system && !owner.eq_ignore_ascii_case(own_schema) && is_system_schema(owner)
    };
    // Qualified: `alias.column`, `owner.table` (cursor on the table), or
    // `owner.table.column`. Alias columns first; a bare owner never lives
    // in the alias map, so `SYSTEM.EMPLOYEES` resolves directly.
    if let Some(q) = qualifier {
        if let Some(tref) = resolve_qualifier(q, aliases) {
            let cols = cache.columns_for(tref.owner.as_deref(), &tref.name);
            if !cols.is_empty() {
                if let Some(col) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(word)) {
                    return Some(column_card(
                        tref.owner.as_deref().unwrap_or(""),
                        &tref.name,
                        col,
                        own_schema,
                    ));
                }
                // Qualifier resolves to a real table but the word isn't its
                // column (cursor on the table part): table card.
                return Some(table_card(
                    tref.owner.as_deref(),
                    &tref.name,
                    &cols,
                    own_schema,
                ));
            }
        }
        let (q_owner, q_table) = split_dotted(q);
        match (q_owner, q_table) {
            // Single-segment qualifier + word: `SYSTEM.|EMPLOYEES`.
            (None, _) => {
                let cols = cache.columns_for(Some(q), word);
                if !cols.is_empty() {
                    return Some(table_card(Some(q), word, &cols, own_schema));
                }
            }
            // Dotted qualifier + word: `scott.emp.|ename` → column card.
            (Some(o), t) => {
                let cols = cache.columns_for(Some(&o), &t);
                if let Some(col) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(word)) {
                    return Some(column_card(&o, &t, col, own_schema));
                }
            }
        }
        return None;
    }
    // Bare word: prefer the qualifier's own table, then own-schema, then a
    // unique visible match. Never guess across several schemas.
    let mut matches: Vec<(&String, &String)> = cache
        .tables
        .iter()
        .filter(|t| t.name.eq_ignore_ascii_case(word) && !hidden(&t.owner))
        .map(|t| (&t.owner, &t.name))
        .collect();
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        let (owner, name) = matches[0];
        let cols = cache.columns_for(Some(owner), name);
        return Some(table_card(Some(owner), name, &cols, own_schema));
    }
    // Bare column: unique across in-scope tables only.
    let mut holders: Vec<(String, String, ColumnMeta)> = Vec::new();
    for tref in aliases.values() {
        for col in cache.columns_for(tref.owner.as_deref(), &tref.name) {
            if col.name.eq_ignore_ascii_case(word) {
                holders.push((
                    tref.owner.clone().unwrap_or_default(),
                    tref.name.clone(),
                    col,
                ));
            }
        }
    }
    holders.sort_by(|a, b| (&a.0, &a.1, &a.2.name).cmp(&(&b.0, &b.1, &b.2.name)));
    holders.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2.name == b.2.name);
    if holders.len() == 1 {
        let (owner, table, col) = &holders[0];
        return Some(column_card(owner, table, col, own_schema));
    }
    None
}

/// Resolve a hovered word to a describable table: returns
/// (owner, table). Table-only (v1): column words resolve to `None` — a
/// Cmd-click on a column is not a table jump. Mirrors the table-card
/// conditions of [`hover_markdown`] so the Cmd-hover underline and the
/// hover card agree on what is jumpable.
pub fn describe_target(
    word: &str,
    qualifier: Option<&str>,
    aliases: &HashMap<String, TableRef>,
    cache: &MetadataCache,
    show_system: bool,
    own_schema: &str,
) -> Option<(Option<String>, String)> {
    if word.is_empty() {
        return None;
    }
    let hidden = |owner: &str| {
        !show_system && !owner.eq_ignore_ascii_case(own_schema) && is_system_schema(owner)
    };
    if let Some(q) = qualifier {
        if let Some(tref) = resolve_qualifier(q, aliases) {
            let cols = cache.columns_for(tref.owner.as_deref(), &tref.name);
            if !cols.is_empty() {
                // Cursor on the table part (word is not one of its columns).
                if !cols.iter().any(|c| c.name.eq_ignore_ascii_case(word)) {
                    return Some((tref.owner.clone(), tref.name.clone()));
                }
                return None;
            }
            // Empty: not an alias hit (`resolve_qualifier` always returns
            // `Some`) — fall through to direct owner.table resolution below.
        }
        let (q_owner, _q_table) = split_dotted(q);
        if q_owner.is_none() {
            // `owner.table` with the cursor on the table: direct resolve,
            // bypassing the system filter like the hover card.
            if !cache.columns_for(Some(q), word).is_empty() {
                return Some((Some(q.to_string()), word.to_string()));
            }
        }
        return None;
    }
    // Bare word: unique visible table only — never guess across schemas.
    let mut matches: Vec<(&String, &String)> = cache
        .tables
        .iter()
        .filter(|t| t.name.eq_ignore_ascii_case(word) && !hidden(&t.owner))
        .map(|t| (&t.owner, &t.name))
        .collect();
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        let (owner, name) = matches[0];
        return Some((Some(owner.clone()), name.clone()));
    }
    None
}

/// `**COL** · TYPE · TABLE [· OWNER]` + comment. Shared by qualified
/// and unique bare column cards. Own-schema tables show bare.
fn column_card(owner: &str, table: &str, col: &ColumnMeta, own_schema: &str) -> String {
    let mut md = format!(
        "**{}** · {}",
        col.name,
        if col.data_type.is_empty() {
            "COLUMN".to_string()
        } else {
            col.data_type.clone()
        }
    );
    md.push_str(&format!("\n\n{table}"));
    if !owner.is_empty() && !owner.eq_ignore_ascii_case(own_schema) {
        md.push_str(&format!(" · {owner}"));
    }
    let c = short_comment(&col.comments);
    if !c.is_empty() {
        md.push_str(&format!("\n\n{c}"));
    }
    md
}

/// `**OWNER.TABLE** — TABLE` (bare `**TABLE**` for the connected user's
/// own schema) + up to 30 `COL — TYPE — comment` lines.
fn table_card(owner: Option<&str>, table: &str, cols: &[ColumnMeta], own_schema: &str) -> String {
    let mut md = String::from("**");
    md.push_str(&display_name(owner, table, own_schema));
    md.push_str("** — TABLE");
    let shown = cols.len().min(30);
    for col in &cols[..shown] {
        md.push_str(&format!(
            "\n{} — {}",
            col.name,
            if col.data_type.is_empty() {
                "?"
            } else {
                col.data_type.as_str()
            }
        ));
        let c = short_comment(&col.comments);
        if !c.is_empty() {
            md.push_str(&format!(" — {c}"));
        }
    }
    if cols.len() > shown {
        md.push_str(&format!("\n… +{} more", cols.len() - shown));
    }
    md
}
/// One-line popup detail for a column comment: trimmed, single-spaced,
/// capped at 80 chars.
pub fn short_comment(comment: &str) -> String {
    let one: String = comment.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one.trim();
    if trimmed.chars().count() <= 80 {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(79).collect::<String>())
    }
}
/// Insert text for a candidate: keywords append a trailing space so the
/// next word starts cleanly (`SELECT |`, `ORDER BY |`); everything else
/// inserts verbatim (tables/columns may be followed by an alias, functions
/// carry their own parens).
pub fn insert_text_for(kind: CandidateKind, label: &str) -> String {
    match kind {
        CandidateKind::Keyword => format!("{label} "),
        _ => label.to_string(),
    }
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
                let tref = TableRef {
                    owner,
                    name: name.clone(),
                };
                // Bare name always addressable (lowercased).
                map.entry(name.to_lowercase())
                    .or_insert_with(|| tref.clone());
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
        "WHERE"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "ON"
            | "USING"
            | "SET"
            | "VALUES"
            | "SELECT"
            | "FROM"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "CROSS"
            | "UNION"
            | "MINUS"
            | "INTERSECT"
            | "LIMIT"
            | "FETCH"
            | "FOR"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
            | "AND"
            | "OR"
            | "INTO"
            | "UPDATE"
            | "BY"
            | "("
            | ")"
            | "AS"
    )
}

/// Resolve a qualifier (`e`, `emp`, `scott.emp`) through the alias map.
/// Returns the concrete table (owner may be None).
pub fn resolve_qualifier(qualifier: &str, aliases: &HashMap<String, TableRef>) -> Option<TableRef> {
    let (owner, name) = split_dotted(qualifier);
    if let Some(o) = owner {
        return Some(TableRef {
            owner: Some(o),
            name,
        });
    }
    aliases
        .get(&name.to_lowercase())
        .cloned()
        .or(Some(TableRef { owner: None, name }))
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
    find_last_keyword(&clean, "AND", 0).is_some() || find_last_keyword(&clean, "OR", 0).is_some()
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
            let pairs = fk_pairs(fk, left, right)
                .or_else(|| fk_pairs(fk, right, left).map(|(rcols, lcols)| (lcols, rcols)));
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

/// Statement starters for empty statement heads.
pub const STMT_KEYWORDS: &[&str] = &[
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "DROP", "ALTER", "TRUNCATE",
    "DESCRIBE", "EXPLAIN", "COMMIT", "ROLLBACK", "BEGIN", "DECLARE",
];

/// Expression keywords for select lists.
pub const EXPR_KEYWORDS: &[&str] = &[
    "DISTINCT", "ALL", "CASE", "WHEN", "THEN", "ELSE", "NOT", "NULL",
];

/// Predicate keywords for WHERE/GROUP/ORDER/HAVING/ON conditions.
pub const PRED_KEYWORDS: &[&str] = &[
    "AND", "OR", "NOT", "IN", "LIKE", "BETWEEN", "EXISTS", "IS", "NULL", "CASE", "WHEN",
];

/// Clause-transition keywords valid right after a select list
/// (`SELECT * fro|` must offer FROM — strictness is about the prefix
/// matching, not about hiding transitions).
pub const SELECT_FOLLOW: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "INTERSECT",
    "MINUS",
    "INTO",
    "FETCH",
];

/// Clause-transition keywords valid after a predicate
/// (`WHERE x=1 ord|` must offer ORDER).
pub const PRED_FOLLOW: &[&str] = &[
    "ORDER",
    "GROUP",
    "HAVING",
    "LIMIT",
    "UNION",
    "INTERSECT",
    "MINUS",
    "FETCH",
    "OFFSET",
];

/// Oracle keywords worth completing (v1 set — statements, clauses, common
/// functions, sequence pseudo-columns). Uppercase; matching is case-insensitive.
pub const ORACLE_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "ON",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "UNION",
    "UNION ALL",
    "MINUS",
    "INTERSECT",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "MERGE",
    "USING",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "CASE",
    "AND",
    "OR",
    "NOT",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "IS",
    "NULL",
    "DISTINCT",
    "ALL",
    "AS",
    "ASC",
    "DESC",
    "WITH",
    "CONNECT",
    "START",
    "PRIOR",
    "SIBLINGS",
    "ROWNUM",
    "ROWID",
    "NEXTVAL",
    "CURRVAL",
    "SYSDATE",
    "SYSTIMESTAMP",
    "DUAL",
    "COMMIT",
    "ROLLBACK",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "SEQUENCE",
    "DESCRIBE",
    "EXPLAIN",
    "GRANT",
    "ORDER",
    "SIBLINGS",
    "BEGIN",
    "DECLARE",
    "LIMIT",
    "OFFSET",
    "FETCH",
];

/// Built-in functions: `(NAME, signature shown in the detail pane)`.
/// Labels complete as `NAME()` — the kit has no snippet engine (`$1` would
/// insert literally) and no post-accept hook, so the cursor lands after the
/// closing paren (one Left-arrow into the args). True tab-stops need kit
/// support. Names here must not repeat in [`ORACLE_KEYWORDS`].
pub const ORACLE_FUNCTIONS: &[(&str, &str)] = &[
    ("ADD_MONTHS", "ADD_MONTHS(date, n)"),
    ("AVG", "AVG([DISTINCT | ALL] expr)"),
    ("CAST", "CAST(expr AS type)"),
    ("COALESCE", "COALESCE(expr, …)"),
    ("COUNT", "COUNT(* | [DISTINCT | ALL] expr)"),
    ("DECODE", "DECODE(expr, search, result [, …] [, default])"),
    ("DENSE_RANK", "DENSE_RANK() OVER (…)"),
    ("EXTRACT", "EXTRACT(field FROM src)"),
    ("INITCAP", "INITCAP(char)"),
    ("INSTR", "INSTR(str, substr [, pos [, nth]])"),
    ("LAST_DAY", "LAST_DAY(date)"),
    ("LENGTH", "LENGTH(char)"),
    (
        "LISTAGG",
        "LISTAGG(expr [, delim]) WITHIN GROUP (ORDER BY …)",
    ),
    ("LOWER", "LOWER(char)"),
    ("MAX", "MAX([DISTINCT | ALL] expr)"),
    ("MIN", "MIN([DISTINCT | ALL] expr)"),
    ("MOD", "MOD(n, m)"),
    ("MONTHS_BETWEEN", "MONTHS_BETWEEN(d1, d2)"),
    ("NULLIF", "NULLIF(expr1, expr2)"),
    ("NVL", "NVL(expr1, expr2)"),
    ("NVL2", "NVL2(expr, v1, v2)"),
    ("RANK", "RANK() OVER (…)"),
    ("ROUND", "ROUND(n [, m])"),
    ("ROW_NUMBER", "ROW_NUMBER() OVER (…)"),
    ("SUBSTR", "SUBSTR(char, pos [, len])"),
    ("SUM", "SUM([DISTINCT | ALL] expr)"),
    ("TO_CHAR", "TO_CHAR(n | date [, fmt [, nls]])"),
    ("TO_DATE", "TO_DATE(char [, fmt [, nls]])"),
    ("TO_NUMBER", "TO_NUMBER(char [, fmt [, nls]])"),
    ("TRIM", "TRIM([LEAD|TRAIL|BOTH] [char] FROM src)"),
    ("TRUNC", "TRUNC(n [, m] | date [, fmt])"),
    ("UPPER", "UPPER(char)"),
];

/// Insert text for a function: call form with empty args.
pub fn function_insert(name: &str) -> String {
    format!("{name}()")
}

/// System schemas hidden by default (toggleable). Covers the Oracle
/// catalogs a DBA account sees but never queries: core, spatial/text,
/// vault/audit/label-security, GSM, APEX/FLOWS, and maintenance accounts.
/// Shared with `metadata.rs`, which excludes them at the SQL level.
pub const SYSTEM_SCHEMAS: &[&str] = &[
    "SYS",
    "SYSTEM",
    "XDB",
    "MDSYS",
    "MDDATA",
    "CTXSYS",
    "ORDSYS",
    "ORDDATA",
    "ORDPLUGINS",
    "OLAPSYS",
    "SI_INFORMTN_SCHEMA",
    "WMSYS",
    "DBSNMP",
    "OUTLN",
    "EXFSYS",
    "DIP",
    "TSMSYS",
    "DMSYS",
    "ODM",
    "ODM_MTR",
    "ANONYMOUS",
    "APPQOSSYS",
    "AUDSYS",
    "DVSYS",
    "DVF",
    "DV_MONITOR",
    "LBACSYS",
    "GSMADMIN_INTERNAL",
    "GSMCATUSER",
    "GSMROOTUSER",
    "GSMUSER",
    "REMOTE_SCHEDULER_AGENT",
    "SYSBACKUP",
    "SYSDG",
    "SYSKM",
    "SYSRAC",
    "ORACLE_OCM",
    "SPATIAL_WFS_ADMIN_USR",
    "SPATIAL_CSW_ADMIN_USR",
    "XS$NULL",
    "OJVMSYS",
    "SYSMAN",
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
        CandidateKind::Function => 5,
        CandidateKind::Keyword => 6,
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
mod tests;
