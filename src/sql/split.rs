//! Statement splitting and caret-based statement selection.
//!
//! Part of the `sql` module (see `sql.rs`).

use super::*;

/// One statement within a multi-statement script. `start`/`end` are byte
/// offsets into the original text; `end` includes the terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// Split script text into statements on top-level `;`.
///
/// Semicolons inside single/double-quoted strings and `--` / `/* */`
/// comments do not split. Empty segments are dropped.
///
/// Like SQL Developer/SQL*Plus, a `/` alone on a line also terminates the
/// preceding statement (required after PL/SQL blocks, allowed after plain
/// SQL even without `;`). The slash line itself is excluded from the
/// statement text, so there is nothing to strip before sending.
pub fn split_statements(sql: &str) -> Vec<Statement> {
    #[derive(PartialEq)]
    enum State {
        Normal,
        SingleQuote,
        DoubleQuote,
        LineComment,
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut seg_start = 0usize;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    // PL/SQL block depth: BEGIN/DECLARE raise it, END lowers it (except
    // END IF/LOOP/CASE). Semicolons only split at depth 0, so anonymous
    // blocks and trigger/procedure bodies stay whole. DECLARE doesn't nest:
    // it opens the block its BEGIN closes (`declare_open` keeps the BEGIN
    // from double-counting).
    let mut depth = 0usize;
    let mut declare_open = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        // A `/` alone on a line terminates the preceding statement
        // (SQL Developer/SQL*Plus behavior). Only at block depth 0 — a
        // slash inside a PL/SQL body is ordinary text, not a terminator.
        if state == State::Normal && depth == 0 && (i == 0 || bytes[i - 1] == b'\n') {
            if let Some(line_end) = slash_line_end(bytes, i) {
                spans.push((seg_start, i));
                seg_start = line_end;
                i = line_end;
                continue;
            }
        }
        match state {
            State::Normal => match b {
                b'\'' => state = State::SingleQuote,
                b'"' => state = State::DoubleQuote,
                b';' => {
                    if depth == 0 {
                        spans.push((seg_start, i + 1));
                        seg_start = i + 1;
                    }
                }
                b'-' if next == b'-' => state = State::LineComment,
                b'/' if next == b'*' => state = State::BlockComment,
                _ if b.is_ascii_alphabetic() => {
                    let mut j = i + 1;
                    while j < bytes.len()
                        && (bytes[j].is_ascii_alphanumeric()
                            || bytes[j] == b'_'
                            || bytes[j] == b'$'
                            || bytes[j] == b'#')
                    {
                        j += 1;
                    }
                    let word = &sql[i..j];
                    if word.eq_ignore_ascii_case("declare") {
                        depth += 1;
                        declare_open = true;
                    } else if word.eq_ignore_ascii_case("begin") {
                        if declare_open {
                            declare_open = false;
                        } else {
                            depth += 1;
                        }
                    } else if word.eq_ignore_ascii_case("end") && depth > 0 {
                        // END IF / END LOOP / END CASE close inner constructs,
                        // not the block. (A Ws-only lookahead is enough —
                        // comments between END and IF are vanishingly rare.)
                        let mut k = j;
                        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        let mut m = k;
                        while m < bytes.len() && bytes[m].is_ascii_alphabetic() {
                            m += 1;
                        }
                        let follower = &sql[k..m];
                        if !(follower.eq_ignore_ascii_case("if")
                            || follower.eq_ignore_ascii_case("loop")
                            || follower.eq_ignore_ascii_case("case"))
                        {
                            depth -= 1;
                            declare_open = false;
                        }
                    }
                    i = j;
                    continue;
                }
                _ => {}
            },
            State::SingleQuote => {
                if b == b'\'' {
                    if next == b'\'' {
                        i += 1; // '' escape stays inside the string
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::DoubleQuote => {
                if b == b'"' {
                    state = State::Normal;
                }
            }
            State::LineComment => {
                if b == b'\n' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                if b == b'*' && next == b'/' {
                    state = State::Normal;
                    i += 1;
                }
            }
        }
        i += 1;
    }
    spans.push((seg_start, bytes.len()));

    spans
        .into_iter()
        .filter_map(|(start, end)| {
            let text = sql.get(start..end).unwrap_or("").trim();
            // Drop whitespace-only and lone-`;` (empty statement) segments.
            if text.trim_matches(';').trim().is_empty() {
                None
            } else {
                Some(Statement {
                    text: text.to_string(),
                    start,
                    end,
                })
            }
        })
        .collect()
}

/// Return the statement under the given byte `offset`.
///
/// A caret past the end re-runs the last statement; a caret in
/// statement-free whitespace prefers the following statement, else the
/// preceding one. Returns `None` when there is no statement at all.
pub fn statement_at(sql: &str, offset: usize) -> Option<String> {
    statement_at_range(sql, offset).map(|(text, _, _)| text)
}

/// Same statement resolution as [`statement_at`], plus the statement's byte
/// range — for in-place rewrite (Format scopes to the cursor's statement
/// instead of the whole buffer). Returns `(text, start, end)`.
pub fn statement_at_range(sql: &str, offset: usize) -> Option<(String, usize, usize)> {
    let statements = split_statements(sql);
    if statements.is_empty() {
        return None;
    }
    if statements.len() == 1 {
        return statements
            .into_iter()
            .next()
            .map(|s| (s.text, s.start, s.end));
    }
    let offset = offset.min(sql.len());
    // Caret immediately after a terminator belongs to the statement just
    // ended — not the one starting there. (Contiguous spans share the
    // boundary, so this must precede the containment check.)
    if let Some(stmt) = statements.iter().find(|s| offset == s.end) {
        return Some((stmt.text.clone(), stmt.start, stmt.end));
    }
    // Caret in trailing spaces/tabs on the terminator's own line also
    // belongs to the statement just ended (`SELECT 1;␣` with the caret at
    // end of line runs statement 1, not 2). Newlines are excluded: a caret
    // past a line break — empty lines, next-line indentation — keeps the
    // existing next-statement preference below.
    if let Some(prev) = statements.iter().rev().find(|s| s.end <= offset) {
        let gap = sql.get(prev.end..offset).unwrap_or("");
        if !gap.is_empty() && gap.chars().all(|c| c == ' ' || c == '\t') {
            return Some((prev.text.clone(), prev.start, prev.end));
        }
    }
    // Own segment.
    if let Some(stmt) = statements
        .iter()
        .find(|s| s.start <= offset && offset < s.end)
    {
        return Some((stmt.text.clone(), stmt.start, stmt.end));
    }
    // Whitespace-only gap: prefer the next statement, else the previous.
    statements
        .iter()
        .find(|s| s.start >= offset)
        .or_else(|| statements.iter().rev().find(|s| s.end <= offset))
        .map(|s| (s.text.clone(), s.start, s.end))
}
