//! SQL text helpers: formatting and statement splitting.

/// Format a SQL string for display in the editor.
///
/// Uses `sqlformat` with uppercased keywords and 2-space indent.
pub fn format_sql(sql: &str) -> String {
    sqlformat::format(
        sql,
        &sqlformat::QueryParams::None,
        &sqlformat::FormatOptions {
            uppercase: Some(true),
            ..Default::default()
        },
    )
}

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
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        match state {
            State::Normal => match b {
                b'\'' => state = State::SingleQuote,
                b'"' => state = State::DoubleQuote,
                b';' => {
                    spans.push((seg_start, i + 1));
                    seg_start = i + 1;
                }
                b'-' if next == b'-' => state = State::LineComment,
                b'/' if next == b'*' => state = State::BlockComment,
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
    let statements = split_statements(sql);
    if statements.is_empty() {
        return None;
    }
    if statements.len() == 1 {
        return Some(statements.into_iter().next().unwrap().text);
    }
    let offset = offset.min(sql.len());
    // Own segment first.
    if let Some(stmt) = statements
        .iter()
        .find(|s| s.start <= offset && offset < s.end)
        .or_else(|| statements.iter().find(|s| offset == s.end))
    {
        return Some(stmt.text.clone());
    }
    // Whitespace-only gap: prefer the next statement, else the previous.
    statements
        .iter()
        .find(|s| s.start >= offset)
        .or_else(|| statements.iter().rev().find(|s| s.end <= offset))
        .map(|s| s.text.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_basic_select() {
        let out = format_sql("select a, b from t where a = 1;");
        assert!(out.contains("SELECT"), "keywords uppercased:\n{out}");
        assert!(out.contains('\n'), "multiline:\n{out}");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(format_sql("").trim(), "");
    }

    #[test]
    fn splits_on_top_level_semicolons() {
        let stmts = split_statements("SELECT 1;\nSELECT 2;");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].text, "SELECT 1;");
        assert_eq!(stmts[1].text, "SELECT 2;");
    }

    #[test]
    fn ignores_semicolons_in_strings_and_comments() {
        let sql = "SELECT ';' AS a, \"b;c\" FROM t; -- trailing;\n/* block; */\nSELECT 2;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert!(stmts[0].text.starts_with("SELECT ';'"));
        // Trailing comments attach to the following statement; crucially the
        // `;` inside strings/comments did not split further.
        assert!(stmts[1].text.ends_with("SELECT 2;"), "{stmts:?}");
    }

    #[test]
    fn handles_escaped_quotes() {
        let stmts = split_statements("SELECT 'it''s; fine';\nSELECT 2;");
        assert_eq!(stmts.len(), 2, "{stmts:?}");
    }

    #[test]
    fn statement_at_picks_statement_under_caret() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        assert_eq!(statement_at(sql, 2).as_deref(), Some("SELECT 1;"));
        assert_eq!(statement_at(sql, 12).as_deref(), Some("SELECT 2;"));
        assert_eq!(statement_at(sql, 22).as_deref(), Some("SELECT 3;"));
    }

    #[test]
    fn statement_at_past_end_reruns_last() {
        let sql = "SELECT 1;\nSELECT 2;   ";
        assert_eq!(
            statement_at(sql, sql.len()).as_deref(),
            Some("SELECT 2;")
        );
    }

    #[test]
    fn statement_at_blank_line_prefers_next() {
        let sql = "SELECT 1;\n\n\nSELECT 2;";
        let blank = sql.find("\n\n").unwrap() + 1;
        assert_eq!(statement_at(sql, blank).as_deref(), Some("SELECT 2;"));
    }

    #[test]
    fn statement_at_empty_is_none() {
        assert_eq!(statement_at("", 0), None);
        assert_eq!(statement_at("  \n;  ", 3), None);
    }

    #[test]
    fn single_statement_ignores_caret() {
        assert_eq!(
            statement_at("SELECT 1;", 99).as_deref(),
            Some("SELECT 1;")
        );
    }
}
