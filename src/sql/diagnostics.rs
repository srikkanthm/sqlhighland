//! Lexical SQL checks: unterminated literals/comments and unbalanced
//! parentheses.
//!
//! Part of the `sql` module (see `sql.rs`). Pure and dependency-free so it
//! runs in the GUI-free core and is trivially testable. The tree-sitter
//! structural pass (gui-only, `crate::sqlparse`) supplements these with
//! parse-tree `ERROR`/`MISSING` nodes; this pass is cheap enough to run on
//! every keystroke and covers the unambiguous faults the parser may recover
//! from silently.

/// How a [`SqlIssue`] renders in the editor (underline color).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Error,
    Warning,
}

/// One detected SQL issue as a **byte** range into the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlIssue {
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub severity: IssueSeverity,
}

impl SqlIssue {
    fn error(start: usize, end: usize, message: impl Into<String>) -> Self {
        Self {
            start,
            end: end.max(start + 1),
            message: message.into(),
            severity: IssueSeverity::Error,
        }
    }
}

/// Scan for unterminated single-quoted strings, double-quoted identifiers,
/// block comments, and unbalanced parentheses. Strings/comments are lexed
/// so markers inside them don't count (`'--'`, `-- don't`, `'''`).
///
/// Issues are ordered by start offset. This mirrors the lexer in
/// `sql::split_statements` / `complete::context` (same scope rules) so the
/// three never disagree about what is "inside" a literal.
pub fn lexical_issues(text: &str) -> Vec<SqlIssue> {
    let b = text.as_bytes();
    let mut out: Vec<SqlIssue> = Vec::new();
    let mut parens: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                let start = i;
                i += 1;
                loop {
                    if i >= b.len() {
                        out.push(SqlIssue::error(
                            start,
                            text.len(),
                            "Unterminated string literal",
                        ));
                        break;
                    }
                    if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2; // escaped ''
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                let start = i;
                i += 1;
                loop {
                    if i >= b.len() {
                        out.push(SqlIssue::error(
                            start,
                            text.len(),
                            "Unterminated quoted identifier",
                        ));
                        break;
                    }
                    if b[i] == b'"' {
                        if b.get(i + 1) == Some(&b'"') {
                            i += 2; // escaped ""
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                let mut closed = false;
                while i + 1 < b.len() {
                    if b[i] == b'*' && b[i + 1] == b'/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    out.push(SqlIssue::error(
                        start,
                        text.len(),
                        "Unterminated block comment",
                    ));
                    i = b.len();
                }
            }
            b'(' => {
                parens.push(i);
                i += 1;
            }
            b')' => {
                if parens.pop().is_none() {
                    out.push(SqlIssue::error(i, i + 1, "Unmatched ')'"));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    for start in parens {
        out.push(SqlIssue::error(start, start + 1, "Unclosed '('"));
    }
    out.sort_by_key(|issue| issue.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(text: &str) -> Vec<(usize, String)> {
        lexical_issues(text)
            .into_iter()
            .map(|i| (i.start, i.message))
            .collect()
    }

    #[test]
    fn clean_sql_has_no_issues() {
        assert!(lexical_issues("SELECT * FROM emp WHERE ename = 'x' AND n = 1").is_empty());
        // Escapes and markers inside literals/comments are inert.
        assert!(lexical_issues("SELECT '-- not a comment', '/* nope */'").is_empty());
        assert!(lexical_issues("-- don't\nSELECT \"Mixed\"\"Name\" FROM t").is_empty());
        assert!(lexical_issues("SELECT (1 + (2 * 3)) FROM dual").is_empty());
    }

    #[test]
    fn unterminated_string() {
        assert_eq!(
            starts("SELECT 'abc"),
            vec![(7, "Unterminated string literal".to_string())]
        );
    }

    #[test]
    fn unterminated_quoted_identifier() {
        assert_eq!(
            starts("SELECT \"abc"),
            vec![(7, "Unterminated quoted identifier".to_string())]
        );
    }

    #[test]
    fn unterminated_block_comment() {
        assert_eq!(
            starts("SELECT 1 /* open"),
            vec![(9, "Unterminated block comment".to_string())]
        );
    }

    #[test]
    fn unbalanced_parens() {
        assert_eq!(
            starts("SELECT (1 + 2"),
            vec![(7, "Unclosed '('".to_string())]
        );
        assert_eq!(
            starts("SELECT 1 + 2)"),
            vec![(12, "Unmatched ')'".to_string())]
        );
        // Balanced nesting is fine.
        assert!(lexical_issues("SELECT ((1))").is_empty());
    }

    #[test]
    fn ignores_balances_inside_literals() {
        assert!(lexical_issues("SELECT ')' FROM dual").is_empty());
        assert!(lexical_issues("SELECT /* ) */ 1").is_empty());
    }
}
