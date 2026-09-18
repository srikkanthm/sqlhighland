//! Caret-context classification (what can be suggested where).
//!
//! Part of the completion engine (see `complete.rs`).

use super::*;

/// True for identifier characters (matches `sql.rs` var rules minus `&`/`:`).
pub(super) fn is_word_char(c: char) -> bool {
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
        Clause::FromTail(origin) => CompleteContext::FromTail(origin),
        Clause::CaseStart => CompleteContext::CaseStart,
        Clause::CaseCondition => CompleteContext::CaseCondition,
        Clause::CaseResult => CompleteContext::CaseResult,
        Clause::SubqueryStart => CompleteContext::SubqueryStart,
        Clause::CastType => CompleteContext::CastType,
        Clause::Window => CompleteContext::WindowClause,
        Clause::UsingColumns => CompleteContext::UsingColumns,
        Clause::Predicate => CompleteContext::Predicate,
        Clause::Bare => CompleteContext::BareWord,
    }
}

/// Which clause introduced the table reference the cursor is past. Drives the
/// continuation keywords offered after a table (`FROM t ` → `WHERE/JOIN/…`,
/// `UPDATE t ` → `SET`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FromOrigin {
    /// `FROM t` / `DELETE FROM t`.
    From,
    /// `JOIN t`.
    Join,
    /// `INSERT INTO t`.
    Into,
    /// `MERGE INTO t`.
    Merge,
    /// `UPDATE t`.
    Update,
    /// `CREATE TABLE t` (or other DDL table).
    Table,
    /// `MERGE … USING t`.
    Using,
}

/// Clause scope from a token scan (see `scan_clause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    Start,
    Select,
    From,
    /// Past a table reference: the table-introducing clause is remembered so
    /// the caller offers the right continuations instead of every keyword.
    FromTail(FromOrigin),
    /// Just after `CASE` — the next keyword is `WHEN`.
    CaseStart,
    /// In a `WHEN` condition.
    CaseCondition,
    /// In a `THEN`/`ELSE` result.
    CaseResult,
    /// A fresh query begins: after `(` following `FROM`/`IN`/`EXISTS`/`JOIN`,
    /// or after a set operator (`UNION`/`INTERSECT`/`MINUS`).
    SubqueryStart,
    /// `CAST(expr AS |` — data types.
    CastType,
    /// `OVER (|` — window clause keywords.
    Window,
    /// `USING (|` — columns common to the joined relations.
    UsingColumns,
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

/// Origin of a table reference from its introducing keyword (`prev` is the
/// token before it, used to tell `INSERT INTO` from `MERGE INTO`).
fn from_origin(up: &str, prev: Option<&str>) -> FromOrigin {
    match up {
        "JOIN" => FromOrigin::Join,
        "INTO" if prev.is_some_and(|p| p.eq_ignore_ascii_case("MERGE")) => FromOrigin::Merge,
        "INTO" => FromOrigin::Into,
        "UPDATE" => FromOrigin::Update,
        "TABLE" => FromOrigin::Table,
        "USING" => FromOrigin::Using,
        _ => FromOrigin::From,
    }
}

/// Walk tokens back from the cursor word: commas start a fresh list item
/// (skip the completed word before them), DISTINCT/ALL/AS are transparent,
/// `(` opens expression scope, `)` and misc keywords bail to Bare, and a
/// `.` skips its qualifier. A bare identifier between the cursor and a
/// SELECT means the select-list tail (`Select`); before a table-introducing
/// keyword it is the table tail (`FromTail`), which the caller narrows to the
/// valid continuations. `CASE … END` nesting is tracked so its keywords
/// resolve (and so the cursor after `END` returns to the enclosing clause).
fn scan_clause(toks: &[String]) -> Clause {
    if toks.is_empty() {
        return Clause::Start;
    }
    let mut saw_ident = false;
    let mut skip_word = false;
    let mut depth = 0u32;
    let mut case_depth = 0u32;
    for (i, t) in toks.iter().enumerate().rev() {
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
                // What precedes the `(` decides what it opens.
                let before = i
                    .checked_sub(1)
                    .and_then(|j| toks.get(j))
                    .map(|s| s.to_ascii_uppercase());
                return match before.as_deref() {
                    Some("CAST") => Clause::CastType,
                    Some("OVER") => Clause::Window,
                    Some("USING") => Clause::UsingColumns,
                    // A relation/subquery is expected here: `FROM (`, `IN (`,
                    // `EXISTS (`, `JOIN (`.
                    Some("IN") | Some("EXISTS") | Some("FROM") | Some("JOIN") => {
                        Clause::SubqueryStart
                    }
                    _ => Clause::Select,
                };
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
        // A PL/SQL block's BEGIN/DECLARE makes clause scanning meaningless;
        // bail out rather than guess (its END would otherwise look like a
        // CASE close).
        if up == "BEGIN" || up == "DECLARE" {
            return Clause::Bare;
        }
        // CASE keywords come before the clause table (which maps WHEN to a
        // predicate). `END` opens a region to skip; its matching `CASE` closes
        // it and scanning continues, so the cursor after `END` resolves to the
        // enclosing clause.
        match up.as_str() {
            "END" => {
                case_depth += 1;
                continue;
            }
            "CASE" => {
                if case_depth == 0 {
                    return Clause::CaseStart;
                }
                case_depth -= 1;
                continue;
            }
            "WHEN" => {
                if case_depth == 0 {
                    return Clause::CaseCondition;
                }
                continue;
            }
            "THEN" | "ELSE" => {
                if case_depth == 0 {
                    return Clause::CaseResult;
                }
                continue;
            }
            _ => {}
        }
        // A set operation starts a new query: the next keyword is SELECT/WITH.
        if matches!(up.as_str(), "UNION" | "INTERSECT" | "MINUS") {
            return Clause::SubqueryStart;
        }
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
            // An identifier already seen just means we are still inside the
            // list, not that the context is unknown.
            Some(Clause::Select) => return Clause::Select,
            Some(Clause::From) => {
                if saw_ident {
                    let prev = i
                        .checked_sub(1)
                        .and_then(|j| toks.get(j))
                        .map(|s| s.as_str());
                    return Clause::FromTail(from_origin(up.as_str(), prev));
                }
                return Clause::From;
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
        Clause::Select
            | Clause::From
            | Clause::FromTail(_)
            | Clause::CaseStart
            | Clause::CaseCondition
            | Clause::CaseResult
            | Clause::SubqueryStart
            | Clause::CastType
            | Clause::Window
            | Clause::UsingColumns
            | Clause::Predicate
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
