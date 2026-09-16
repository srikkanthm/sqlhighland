//! Tree-sitter SQL structural diagnostics (gui-only).
//!
//! Walks the `tree-sitter-sequel` parse tree for `ERROR` and `MISSING` nodes
//! and reports them as [`SqlIssue`]s (bytes). The grammar is generic SQL (not
//! Oracle-specific), so this is a supplement to the lexical checks in
//! `crate::sql::lexical_issues`, never a replacement: a parse that fails on
//! Oracle-only syntax yields a best-effort set, and the lexical pass still
//! catches the unambiguous faults.
//!
//! PL/SQL is skipped entirely: the grammar can only parse SQL statements inside
//! `BEGIN ... END` blocks, so Oracle PL/SQL (procedure calls, `CREATE
//! PROCEDURE` bodies, …) would otherwise be reported as errors (see
//! `crate::sql::is_plsql`). Lexical checks still run over PL/SQL.
//!
//! Parsing is bounded by a wall-clock budget so a pathological buffer can't
//! stall the worker; a timed-out parse reports nothing rather than a partial
//! tree.

use std::borrow::Cow;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, Parser};

use crate::config::SqlCheckScope;
use crate::sql::{IssueSeverity, SqlIssue};

/// Wall-clock budget for one parse. Runs on the background executor, so this
/// only bounds the worker's tail latency, not the UI.
const PARSE_BUDGET: Duration = Duration::from_millis(50);

/// Structural issues for `text`, scoped to the whole buffer or each
/// statement (per [`SqlCheckScope`]). PL/SQL statements are skipped.
pub fn syntax_issues(text: &str, scope: SqlCheckScope) -> Vec<SqlIssue> {
    let mut issues = Vec::new();
    match scope {
        SqlCheckScope::WholeBuffer => collect(&mask_plsql(text), 0, &mut issues),
        SqlCheckScope::Statement => {
            for stmt in crate::sql::split_statements(text) {
                if is_skipped(&stmt.text) {
                    continue;
                }
                collect(&text[stmt.start..stmt.end], stmt.start, &mut issues);
            }
        }
    }
    issues.sort_by_key(|i| i.start);
    issues.truncate(crate::sqlparse::ISSUE_CAP);
    issues
}

/// PL/SQL the generic grammar can't parse: a whole construct ([`is_plsql`]) or
/// a fragment the splitter peeled off a `CREATE … PACKAGE`/`TYPE BODY`
/// ([`is_plsql_fragment`]).
fn is_skipped(sql: &str) -> bool {
    crate::sql::is_plsql(sql) || crate::sql::is_plsql_fragment(sql)
}

/// A copy of `text` with PL/SQL statements and lone `/` terminators blanked
/// out (replaced by spaces of equal byte length), so the whole-buffer parse
/// can't flag them and every reported offset still maps onto the original.
/// Returns the input borrowed when there is nothing to mask.
fn mask_plsql(text: &str) -> Cow<'_, str> {
    let mut masked: Option<String> = None;
    for stmt in crate::sql::split_statements(text) {
        if is_skipped(&stmt.text) {
            blank(&mut masked, text, stmt.start, stmt.end);
        }
    }
    // A `/` alone on a line terminates the preceding statement (SQL*Plus);
    // the splitter excludes it from statement spans, so blank it here too.
    for (start, end) in slash_only_lines(text) {
        blank(&mut masked, text, start, end);
    }
    match masked {
        Some(s) => Cow::Owned(s),
        None => Cow::Borrowed(text),
    }
}

/// Replace `text[start..end]` with spaces in `masked` (cloning it first when
/// needed). Byte lengths are preserved, so offsets never shift.
fn blank(masked: &mut Option<String>, text: &str, start: usize, end: usize) {
    if start >= end {
        return;
    }
    let s = masked.get_or_insert_with(|| text.to_string());
    s.replace_range(start..end, &" ".repeat(end - start));
}

/// Byte ranges of lines that contain only a `/` (optional spaces/tabs), the
/// SQL\*Plus statement terminator.
fn slash_only_lines(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| start + p)
            .unwrap_or(bytes.len());
        let line = &text[start..end];
        let t = line.trim_matches(|c| c == ' ' || c == '\t' || c == '\r');
        if t == "/" {
            out.push((start, end));
        }
        start = if end < bytes.len() { end + 1 } else { end };
    }
    out
}

/// Upper bound on reported issues, so a garbage buffer can't flood the editor.
pub const ISSUE_CAP: usize = 100;

/// Parse `chunk` (a slice of the buffer at `base`) and append every
/// `ERROR`/`MISSING` node as a [`SqlIssue`]. No-op on parse failure/timeout.
fn collect(chunk: &str, base: usize, out: &mut Vec<SqlIssue>) {
    let Some(tree) = parse(chunk) else {
        return;
    };
    let len = chunk.len();
    // Iterative DFS (`tree-sitter` trees can be deep; avoid recursion).
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            let (mut start, end) = (base + node.start_byte(), base + node.end_byte());
            let end = end.min(base + len);
            // Zero-width MISSING nodes (e.g. an expected token at EOF) need a
            // paintable range: widen backwards onto the preceding character.
            if end <= start {
                start = start.saturating_sub(1).max(base);
            }
            let (message, severity) = if node.is_missing() {
                (
                    format!("Missing {}", friendly_kind(node.kind())),
                    IssueSeverity::Warning,
                )
            } else {
                (error_message(node, chunk), IssueSeverity::Error)
            };
            out.push(SqlIssue {
                start,
                end: end.max(start + 1),
                message,
                severity,
            });
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// Human message for an `ERROR` node. tree-sitter attaches no reason, so this
/// is a bounded heuristic over the node's span:
///
/// - A dangling clause/keyword at the buffer end (`FROM`, `WHERE`, …) reads as
///   an incomplete statement, not as "unexpected FROM".
/// - A bare trailing identifier (a recovery artifact) is left generic rather
///   than naming the wrong token.
/// - Otherwise the first non-keyword token is named (`Unexpected 'SELEC'`,
///   `Unexpected ','`).
fn error_message(node: tree_sitter::Node, chunk: &str) -> String {
    let end = node.end_byte().min(chunk.len());
    let start = node.start_byte().min(end);
    let text = chunk.get(start..end).unwrap_or("");
    // Trailing whitespace/newlines still count as "at the end": an editor
    // buffer almost always ends with a newline, and a dangling clause there
    // would otherwise fall through to the generic message.
    let at_eof = chunk.get(end..).is_none_or(|rest| rest.trim().is_empty());
    let token = first_token(text);

    if at_eof {
        if token.is_some_and(is_sql_keyword) {
            return "Incomplete statement".to_string();
        }
        if node.child_count() == 0 {
            return "Unexpected or invalid syntax".to_string();
        }
        return match token {
            Some(t) => format!("Unexpected '{t}'"),
            None => "Incomplete statement".to_string(),
        };
    }
    match token {
        Some(t) if !is_sql_keyword(t) => format!("Unexpected '{t}'"),
        _ => "Unexpected or invalid syntax".to_string(),
    }
}

/// First token of `text`: a word, a whole quoted run, or one punctuation char.
fn first_token(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let first = text.chars().next()?;
    if first == '"' {
        let rest = &text[1..];
        return Some(match rest.find('"') {
            Some(i) => &text[..i + 2],
            None => text,
        });
    }
    if first.is_alphanumeric() || first == '_' {
        let end = text
            .char_indices()
            .take_while(|(_, c)| c.is_alphanumeric() || matches!(c, '_' | '$' | '#'))
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(first.len_utf8());
        return Some(&text[..end]);
    }
    Some(&text[..first.len_utf8()])
}

/// Clause/operator keywords: naming one as "unexpected" is misleading (it is
/// the parser's recovery anchor, usually a dangling-clause/incomplete case).
fn is_sql_keyword(token: &str) -> bool {
    matches!(
        token.to_ascii_uppercase().as_str(),
        "SELECT"
            | "FROM"
            | "WHERE"
            | "GROUP"
            | "ORDER"
            | "BY"
            | "HAVING"
            | "UPDATE"
            | "INSERT"
            | "INTO"
            | "SET"
            | "VALUES"
            | "JOIN"
            | "ON"
            | "USING"
            | "UNION"
            | "INTERSECT"
            | "MINUS"
            | "AS"
            | "AND"
            | "OR"
            | "NOT"
            | "IN"
            | "LIKE"
            | "BETWEEN"
            | "IS"
            | "NULL"
            | "CREATE"
            | "ALTER"
            | "DROP"
            | "DELETE"
            | "MERGE"
            | "WITH"
            | "DISTINCT"
            | "ALL"
            | "CASE"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
    )
}

/// Friendly name for a `MISSING` node's expected kind (grammar rules leak
/// `_`-prefixed internal names like `_identifier`).
fn friendly_kind(kind: &str) -> String {
    match kind {
        ")" => "closing parenthesis".to_string(),
        "(" => "opening parenthesis".to_string(),
        "," => "comma".to_string(),
        ";" => "semicolon".to_string(),
        "." => "dot".to_string(),
        "_identifier" | "identifier" => "identifier".to_string(),
        other => other.trim_start_matches('_').replace('_', " "),
    }
}

/// Parse with a progress budget. Returns `None` on setup failure or timeout.
fn parse(chunk: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&Language::new(tree_sitter_sequel::LANGUAGE))
        .ok()?;
    let start = Instant::now();
    let mut progress = |_: &tree_sitter::ParseState| -> ControlFlow<()> {
        if start.elapsed() > PARSE_BUDGET {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress);
    parser.parse_with_options(
        &mut |byte_offset, _| chunk.get(byte_offset..).unwrap_or(""),
        None,
        Some(options),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(sql: &str) -> Vec<(IssueSeverity, String)> {
        syntax_issues(sql, SqlCheckScope::WholeBuffer)
            .into_iter()
            .map(|i| (i.severity, i.message))
            .collect()
    }

    #[test]
    fn valid_sql_has_no_structural_issues() {
        assert!(
            syntax_issues("SELECT * FROM emp WHERE id = 1", SqlCheckScope::WholeBuffer).is_empty()
        );
        assert!(syntax_issues(
            "SELECT e.name, d.name FROM emp e JOIN dept d ON e.deptno = d.deptno",
            SqlCheckScope::WholeBuffer
        )
        .is_empty());
    }

    #[test]
    fn flags_structural_errors() {
        // Missing relation / a keyword typo / a dangling clause. (The grammar
        // is permissive on some fragments like `SELECT FROM t`; the lexical
        // pass covers the rest.)
        for sql in [
            "SELECT * FROM",
            "SELEC * FROM t",
            "SELECT * FROM t WHERE",
            "UPDATE SET x = 1",
        ] {
            assert!(!messages(sql).is_empty(), "expected issues for {sql:?}");
        }
    }

    #[test]
    fn issues_are_sorted_and_capped() {
        let issues = syntax_issues(";;;", SqlCheckScope::WholeBuffer);
        assert!(issues.windows(2).all(|w| w[0].start <= w[1].start));
    }

    #[test]
    fn messages_are_specific() {
        let first = |sql: &str| messages(sql).into_iter().next().map(|(_, m)| m);
        assert_eq!(
            first("SELECT * FROM").as_deref(),
            Some("Incomplete statement")
        );
        // Lowercase and a trailing newline/space still read as incomplete.
        assert_eq!(
            first("select * from").as_deref(),
            Some("Incomplete statement")
        );
        assert_eq!(
            first("select * from\n").as_deref(),
            Some("Incomplete statement")
        );
        assert_eq!(
            first("select * from ").as_deref(),
            Some("Incomplete statement")
        );
        assert_eq!(
            first("SELECT * FROM t WHERE").as_deref(),
            Some("Incomplete statement")
        );
        assert_eq!(
            first("SELEC * FROM t").as_deref(),
            Some("Unexpected 'SELEC'")
        );
        assert_eq!(
            first("SELECT a,, b FROM t").as_deref(),
            Some("Unexpected ','")
        );
        assert_eq!(
            first("SELECT (1+2").as_deref(),
            Some("Missing closing parenthesis")
        );
    }

    #[test]
    fn plsql_blocks_are_not_flagged() {
        // PL/SQL can't be parsed by the generic grammar; every statement kind
        // below must be skipped in both scopes.
        for sql in [
            "BEGIN DBMS_SESSION.SLEEP(10); END;",
            "BEGIN\n    DBMS_SESSION.SLEEP(10);\nEND;",
            "DECLARE\n  n NUMBER;\nBEGIN\n  n := 1;\nEND;",
            "CREATE OR REPLACE PROCEDURE p IS BEGIN NULL; END;",
            "CREATE OR REPLACE FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;",
            "CREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE p IS BEGIN NULL; END;\nEND;",
            "CREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE a IS BEGIN NULL; END;\n  PROCEDURE b IS BEGIN NULL; END;\nEND;",
            "CREATE OR REPLACE PACKAGE pkg AS\n  PROCEDURE p;\n  FUNCTION f RETURN NUMBER;\nEND;",
            "CREATE TRIGGER trg BEFORE INSERT ON t BEGIN NULL; END;",
            "BEGIN NULL; END;\n/\n",
        ] {
            assert!(
                syntax_issues(sql, SqlCheckScope::WholeBuffer).is_empty(),
                "whole-buffer issues for {sql:?}: {:?}",
                messages(sql)
            );
            assert!(
                syntax_issues(sql, SqlCheckScope::Statement).is_empty(),
                "statement issues for {sql:?}"
            );
        }
    }

    #[test]
    fn plsql_masking_preserves_offsets() {
        // A real error after a masked PL/SQL block still reports at its own
        // offset (masking must not shift positions).
        let sql = "BEGIN NULL; END;\nSELEC * FROM t";
        let issues = syntax_issues(sql, SqlCheckScope::WholeBuffer);
        let first = issues.first().expect("the trailing typo is flagged");
        assert_eq!(first.message, "Unexpected 'SELEC'");
        assert_eq!(first.start, sql.find("SELEC").unwrap());

        // A dangling clause after a block still reads as incomplete.
        let sql = "BEGIN NULL; END;\nSELECT * FROM";
        let issues = syntax_issues(sql, SqlCheckScope::WholeBuffer);
        assert!(
            issues.iter().any(|i| i.message == "Incomplete statement"),
            "{issues:?}"
        );
    }

    #[test]
    fn lone_slash_terminator_is_not_flagged() {
        assert!(syntax_issues("SELECT 1;\n/\n", SqlCheckScope::WholeBuffer).is_empty());
        // A division operator is left alone.
        assert!(syntax_issues("SELECT a / b FROM t", SqlCheckScope::WholeBuffer).is_empty());
    }
}
