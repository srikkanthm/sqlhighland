//! Dotted names, alias maps, and JOIN…ON detection.
//!
//! Part of the completion engine (see `complete.rs`).

use super::*;

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

/// If `text` ends at `cursor` with `<FUNC>()`, where `FUNC` is a known
/// function name, return the byte offset between the parentheses — where the
/// cursor belongs after accepting a function completion. `None` when the
/// trailing call is not a known function or the cursor is not immediately
/// after `()`.
///
/// The editor kit has no snippet support (`$0`/tabstops are inserted
/// literally), so a function completion leaves the cursor after `()`. Typing
/// the call by hand never produces this shape with the cursor after `()`:
/// auto-close inserts the pair with the cursor already inside, and typing the
/// closer is a pure cursor move that emits no change.
pub fn cursor_inside_call(
    text: &str,
    cursor: usize,
    is_function: impl Fn(&str) -> bool,
) -> Option<usize> {
    let head = text.get(..cursor)?;
    let stem = head.strip_suffix("()")?;
    let start = stem
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '#'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = &stem[start..];
    if name.is_empty() || !is_function(name) {
        return None;
    }
    Some(cursor - 1)
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
pub(super) fn tokenize(s: &str) -> Vec<String> {
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
        } else if c == '\'' {
            // Single-quoted string literal: its contents are data, never
            // tokens (a literal like `'from'` must not switch the clause).
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            loop {
                match chars.next() {
                    None => break,
                    Some('\'') => {
                        if chars.peek() == Some(&'\'') {
                            chars.next(); // escaped `''`
                        } else {
                            break;
                        }
                    }
                    Some(_) => {}
                }
            }
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Detect `JOIN <table> [[AS] alias] ON …` with the cursor in the ON tail.
/// Returns the right side as written, its resolved table, and the byte offset
/// just past the `ON` (so the caller can tell which conditions are already
/// typed and not re-suggest them). Works whether the tail is fresh or already
/// has conditions, so `… ON a = b AND |` can offer the remaining FK links.
/// `head` is buffer text before the cursor; `aliases` resolves `AS` aliases.
pub fn detect_join_on(
    head: &str,
    aliases: &HashMap<String, TableRef>,
) -> Option<(String, TableRef, usize)> {
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
    let tail_start = on_end.min(head.len());
    let tref = match aliases.get(&alias.to_lowercase()) {
        Some(t) => t.clone(),
        None => TableRef { owner, name },
    };
    Some((alias, tref, tail_start))
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
                insert: None,
                label: cond,
                detail: format!("JOIN · {}", fk.name),
                kind: CandidateKind::JoinCondition,
                owner: None,
                usage: 0,
                depth: 0,
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

pub(crate) fn owners_match(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => true,
    }
}
