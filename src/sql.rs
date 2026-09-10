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
    let statements = split_statements(sql);
    if statements.is_empty() {
        return None;
    }
    if statements.len() == 1 {
        return Some(statements.into_iter().next().unwrap().text);
    }
    let offset = offset.min(sql.len());
    // Caret immediately after a terminator belongs to the statement just
    // ended — not the one starting there. (Contiguous spans share the
    // boundary, so this must precede the containment check.)
    if let Some(stmt) = statements.iter().find(|s| offset == s.end) {
        return Some(stmt.text.clone());
    }
    // Own segment.
    if let Some(stmt) = statements
        .iter()
        .find(|s| s.start <= offset && offset < s.end)
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

/// How a statement must be sent to Oracle: queries produce a result set
/// (`Connection::query`, paged through a held cursor); everything else —
/// DML, DDL, PL/SQL blocks — goes through `Connection::execute` and reports
/// rows affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Query,
    Execute,
}

/// Classify a single statement by its leading keyword. Comments and
/// whitespace are skipped; `WITH ... SELECT` counts as a query while
/// `WITH ... INSERT/UPDATE/DELETE` counts as execute (CTEs are scanned
/// at paren-depth 0). Unknown/empty input defaults to `Execute` — the
/// server then reports the real error.
pub fn statement_kind(sql: &str) -> StatementKind {
    let bytes = sql.as_bytes();
    let mut i = skip_ws_comments(bytes, 0);
    let (keyword, next) = read_keyword(bytes, i);
    if keyword.eq_ignore_ascii_case("select") {
        return StatementKind::Query;
    }
    if keyword.eq_ignore_ascii_case("with") {
        // Scan top-level keywords past the CTE definitions.
        let mut depth = 0usize;
        i = next;
        loop {
            i = skip_ws_comments(bytes, i);
            if i >= bytes.len() {
                break;
            }
            let b = bytes[i];
            if b == b'(' {
                depth += 1;
                i += 1;
                continue;
            }
            if b == b')' {
                depth = depth.saturating_sub(1);
                i += 1;
                continue;
            }
            if b == b'\'' || b == b'"' {
                i = skip_quoted(bytes, i);
                continue;
            }
            if depth == 0 && (b.is_ascii_alphabetic() || b == b'_') {
                let (kw, _) = read_keyword(bytes, i);
                if kw.eq_ignore_ascii_case("select") {
                    return StatementKind::Query;
                }
                if kw.eq_ignore_ascii_case("insert")
                    || kw.eq_ignore_ascii_case("update")
                    || kw.eq_ignore_ascii_case("delete")
                    || kw.eq_ignore_ascii_case("merge")
                {
                    return StatementKind::Execute;
                }
            }
            i += 1;
        }
        return StatementKind::Execute;
    }
    StatementKind::Execute
}

/// True for a typed COMMIT (false for ROLLBACK, None otherwise). The app
/// clears per-tab pending flags when these run as statements, mirroring the
/// Commit/Rollback buttons.
pub fn txn_end(sql: &str) -> Option<bool> {
    let bytes = sql.as_bytes();
    let i = skip_ws_comments(bytes, 0);
    let (keyword, _) = read_keyword(bytes, i);
    if keyword.eq_ignore_ascii_case("commit") {
        Some(true)
    } else if keyword.eq_ignore_ascii_case("rollback") {
        Some(false)
    } else {
        None
    }
}

/// Human summary for an executed (non-query) statement: "3 rows inserted",
/// "Table EMP created", "PL/SQL block executed". Timing is appended by the
/// caller; unknown statements fall back to "N rows affected".
pub fn exec_summary(sql: &str, affected: u64) -> String {
    let bytes = sql.as_bytes();
    let i = skip_ws_comments(bytes, 0);
    let (first, next) = read_keyword(bytes, i);
    let row = if affected == 1 { "row" } else { "rows" };
    if first.eq_ignore_ascii_case("insert") {
        return format!("{affected} {row} inserted");
    }
    if first.eq_ignore_ascii_case("update") {
        return format!("{affected} {row} updated");
    }
    if first.eq_ignore_ascii_case("delete") {
        return format!("{affected} {row} deleted");
    }
    if first.eq_ignore_ascii_case("merge") {
        return format!("{affected} {row} merged");
    }
    if first.eq_ignore_ascii_case("begin") || first.eq_ignore_ascii_case("declare") {
        return "PL/SQL block executed".into();
    }
    if first.eq_ignore_ascii_case("commit") {
        return "Committed".into();
    }
    if first.eq_ignore_ascii_case("rollback") {
        return "Rolled back".into();
    }
    if ["create", "drop", "alter", "truncate"]
        .iter()
        .any(|k| first.eq_ignore_ascii_case(k))
    {
        if let Some(summary) = ddl_summary(bytes, next, &first) {
            return summary;
        }
    }
    format!("{affected} {row} affected")
}

/// "Table EMP created" from `CREATE TABLE emp ...`. Returns None when the
/// object type/name can't be parsed (caller falls back to generic text).
fn ddl_summary(bytes: &[u8], mut i: usize, verb: &str) -> Option<String> {
    let participle = match verb.to_ascii_lowercase().as_str() {
        "create" => "created",
        "drop" => "dropped",
        "alter" => "altered",
        "truncate" => "truncated",
        _ => return None,
    };
    // Skip CREATE OR REPLACE.
    i = skip_ws_comments(bytes, i);
    let (mut obj_type, mut j) = read_keyword(bytes, i);
    if obj_type.eq_ignore_ascii_case("or") {
        j = skip_ws_comments(bytes, j);
        let (replace, k) = read_keyword(bytes, j);
        if !replace.eq_ignore_ascii_case("replace") {
            return None;
        }
        j = skip_ws_comments(bytes, k);
        let (t, k2) = read_keyword(bytes, j);
        obj_type = t;
        j = k2;
    }
    if obj_type.is_empty() {
        return None;
    }
    let mut type_name = obj_type.to_ascii_lowercase();
    if let Some(first) = type_name.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    // Optional (possibly quoted, schema-qualified) object name.
    let name = read_object_name(bytes, j).map(|(owner, name)| {
        owner.map(|o| format!("{o}.{name}")).unwrap_or(name)
    });
    match name {
        Some(name) => Some(format!("{type_name} {name} {participle}")),
        None => Some(format!("{type_name} {participle}")),
    }
}

/// Read `[owner.]name` with double-quoted support. Returns None when no
/// identifier follows.
fn read_object_name(bytes: &[u8], i: usize) -> Option<(Option<String>, String)> {
    let sql = std::str::from_utf8(bytes).ok()?;
    let rest = sql.get(i..)?.trim_start();
    let (first, after) = read_quoted_or_bare(rest)?;
    let after = after.trim_start();
    if let Some(dotted) = after.strip_prefix('.') {
        let (second, _) = read_quoted_or_bare(dotted.trim_start())?;
        Some((Some(first), second))
    } else {
        Some((None, first))
    }
}

/// Read one bare identifier (case preserved) or `"quoted"` one (with `""`
/// escapes). Returns the name and the remainder.
fn read_quoted_or_bare(s: &str) -> Option<(String, &str)> {
    if let Some(quoted) = s.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = quoted.char_indices();
        loop {
            let (i, c) = chars.next()?;
            if c == '"' {
                if quoted[i + 1..].starts_with('"') {
                    name.push('"');
                    chars.next();
                } else {
                    return Some((name, &quoted[i + 1..]));
                }
            } else {
                name.push(c);
            }
        }
    } else {
        let end = s
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '#'))
            .unwrap_or(s.len());
        if end == 0 {
            return None;
        }
        Some((s[..end].to_string(), &s[end..]))
    }
}

/// True for INSERT/UPDATE/DELETE/MERGE — statements that open a
/// user transaction needing an explicit commit (the driver never autocommits).
pub fn is_dml(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let i = skip_ws_comments(bytes, 0);
    let (keyword, _) = read_keyword(bytes, i);
    keyword.eq_ignore_ascii_case("insert")
        || keyword.eq_ignore_ascii_case("update")
        || keyword.eq_ignore_ascii_case("delete")
        || keyword.eq_ignore_ascii_case("merge")
}

/// True for anonymous PL/SQL blocks (`BEGIN...` / `DECLARE...`), which —
/// unlike plain SQL — *require* their trailing semicolon server-side.
pub fn is_plsql_block(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let i = skip_ws_comments(bytes, 0);
    let (keyword, _) = read_keyword(bytes, i);
    keyword.eq_ignore_ascii_case("begin") || keyword.eq_ignore_ascii_case("declare")
}

/// Skip whitespace, `--` line comments, and `/* */` block comments.
fn skip_ws_comments(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            break;
        }
    }
    i
}

/// Read `[A-Za-z_][A-Za-z0-9_$#]*` at `i`. Returns (keyword, next offset).
fn read_keyword(bytes: &[u8], i: usize) -> (String, usize) {
    let mut j = i;
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$' || bytes[j] == b'#')
    {
        j += 1;
    }
    (
        String::from_utf8_lossy(&bytes[i..j]).into_owned(),
        j,
    )
}

/// If `i` starts a slash-terminator line (`/ `alone, optional surrounding
/// spaces/tabs), return the offset just past its newline (or end of input).
fn slash_line_end(bytes: &[u8], i: usize) -> Option<usize> {
    let mut p = i;
    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
        p += 1;
    }
    if bytes.get(p) != Some(&b'/') {
        return None;
    }
    let mut q = p + 1;
    while q < bytes.len() && (bytes[q] == b' ' || bytes[q] == b'\t') {
        q += 1;
    }
    if q == bytes.len() {
        Some(q)
    } else if bytes[q] == b'\n' {
        Some(q + 1)
    } else {
        None
    }
}

/// Skip a `'...'`/`"..."` literal starting at the quote (handles `''`).
fn skip_quoted(bytes: &[u8], mut i: usize) -> usize {
    let quote = bytes[i];
    i += 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
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
    fn slash_line_terminates_like_sql_developer() {
        // `/` alone on a line terminates, even without `;`. The slash is
        // excluded from the statement text — nothing to strip server-side.
        let stmts = split_statements("SELECT 1\n/\nSELECT 2;");
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert_eq!(stmts[0].text, "SELECT 1");
        assert_eq!(stmts[1].text, "SELECT 2;");

        // Classic SQL Developer shape: PL/SQL block closed by `/`.
        let stmts = split_statements("BEGIN NULL; END;\n/\nSELECT 2;");
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert!(stmts[0].text.ends_with("END;"));

        // Caret on the `/` line itself runs the statement it terminates.
        let sql = "SELECT 1\n/\nSELECT 2;";
        let slash = sql.find('/').unwrap();
        assert_eq!(statement_at(sql, slash).as_deref(), Some("SELECT 1"));

        // Leading / consecutive slash lines vanish, not error.
        let stmts = split_statements("/\n/\nSELECT 1;");
        assert_eq!(stmts.len(), 1, "{stmts:?}");

        // `/` mid-line is ordinary text, not a terminator.
        let stmts = split_statements("SELECT 1/2 FROM dual;");
        assert_eq!(stmts.len(), 1, "{stmts:?}");
    }

    #[test]
    fn statement_at_after_semicolon_runs_current_line() {
        // Caret right after `;` (offset 9) belongs to statement 1, even
        // though statement 2 starts at the same offset.
        let sql = "SELECT 1;\nSELECT 2;";
        assert_eq!(statement_at(sql, 9).as_deref(), Some("SELECT 1;"));
        // Interior positions are unaffected.
        assert_eq!(statement_at(sql, 2).as_deref(), Some("SELECT 1;"));
        assert_eq!(statement_at(sql, 12).as_deref(), Some("SELECT 2;"));
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

    #[test]
    fn statement_kind_routes_queries_and_commands() {
        use StatementKind::*;
        assert_eq!(statement_kind("SELECT 1 FROM dual"), Query);
        assert_eq!(statement_kind("  -- comment\nselect a from t"), Query);
        assert_eq!(
            statement_kind("WITH x AS (SELECT 1 FROM dual) SELECT * FROM x"),
            Query
        );
        assert_eq!(
            statement_kind("WITH x AS (SELECT 1 FROM dual) UPDATE t SET a = 1"),
            Execute
        );
        assert_eq!(statement_kind("INSERT INTO t VALUES (1)"), Execute);
        assert_eq!(statement_kind("UPDATE t SET a = 1"), Execute);
        assert_eq!(statement_kind("DELETE FROM t"), Execute);
        assert_eq!(statement_kind("CREATE TABLE t (a NUMBER)"), Execute);
        assert_eq!(statement_kind("BEGIN NULL; END;"), Execute);
        assert_eq!(statement_kind("DECLARE x NUMBER; BEGIN x := 1; END;"), Execute);
        assert_eq!(statement_kind("COMMIT"), Execute);
        assert_eq!(statement_kind(""), Execute);
    }

    #[test]
    fn plsql_blocks_stay_whole() {
        let stmts = split_statements("BEGIN NULL; DBMS_OUTPUT.PUT_LINE('x;'); END;");
        assert_eq!(stmts.len(), 1, "{stmts:?}");
        assert!(stmts[0].text.ends_with("END;"));

        let stmts = split_statements("DECLARE x NUMBER; BEGIN x := 1; END;\nSELECT 2;");
        assert_eq!(stmts.len(), 2, "{stmts:?}");

        // Nested blocks and END IF / END LOOP don't confuse the depth.
        let sql = "BEGIN IF x THEN y := 1; END IF; BEGIN z := 2; END; END;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 1, "{stmts:?}");

        // CASE...END in plain SQL still splits at the real terminator.
        let stmts = split_statements("SELECT CASE WHEN a THEN 1 END FROM t;");
        assert_eq!(stmts.len(), 1, "{stmts:?}");
    }

    #[test]
    fn is_plsql_block_detects_anonymous_blocks() {
        assert!(is_plsql_block("BEGIN NULL; END;"));
        assert!(is_plsql_block("  declare x number; begin null; end;"));
        assert!(!is_plsql_block("SELECT 1 FROM dual"));
        assert!(!is_plsql_block("CREATE TABLE t (a NUMBER)"));
    }

    #[test]
    fn exec_summary_uses_action_verbs() {
        assert_eq!(exec_summary("INSERT INTO t VALUES (1)", 1), "1 row inserted");
        assert_eq!(exec_summary("insert into t select * from s", 5), "5 rows inserted");
        assert_eq!(exec_summary("UPDATE t SET a = 1", 0), "0 rows updated");
        assert_eq!(exec_summary("-- gone\nDELETE FROM t", 2), "2 rows deleted");
        assert_eq!(exec_summary("MERGE INTO t USING s ON (1=1) WHEN MATCHED THEN UPDATE SET a=1", 1), "1 row merged");
        assert_eq!(exec_summary("BEGIN NULL; END;", 1), "PL/SQL block executed");
        assert_eq!(exec_summary("COMMIT", 0), "Committed");
        assert_eq!(exec_summary("rollback", 0), "Rolled back");
        assert_eq!(exec_summary("GRANT SELECT ON t TO r", 0), "0 rows affected");
    }

    #[test]
    fn exec_summary_names_ddl_objects() {
        assert_eq!(exec_summary("CREATE TABLE emp (a NUMBER)", 0), "Table emp created");
        assert_eq!(exec_summary("create or replace view v as select 1 from dual", 0), "View v created");
        assert_eq!(exec_summary("DROP INDEX \"My Index\"", 0), "Index My Index dropped");
        assert_eq!(exec_summary("ALTER TABLE scott.emp ADD (b NUMBER)", 0), "Table scott.emp altered");
        assert_eq!(exec_summary("TRUNCATE TABLE t", 0), "Table t truncated");
        // Unparseable DDL falls back to generic text, never panics.
        assert_eq!(exec_summary("CREATE", 0), "0 rows affected");
    }

    #[test]
    fn txn_end_detects_commit_rollback() {
        assert_eq!(txn_end("COMMIT"), Some(true));
        assert_eq!(txn_end("  rollback ;"), Some(false));
        assert_eq!(txn_end("SELECT 1 FROM dual"), None);
        assert_eq!(txn_end("COMMITMENT ISSUES"), None);
    }

    #[test]
    fn is_dml_flags_transactional_statements() {
        assert!(is_dml("INSERT INTO t VALUES (1)"));
        assert!(is_dml("  update t set a = 1"));
        assert!(is_dml("-- fix\nDELETE FROM t"));
        assert!(is_dml("MERGE INTO t USING s ON (t.a = s.a) WHEN MATCHED THEN UPDATE SET a = 1"));
        assert!(!is_dml("SELECT 1 FROM dual"));
        assert!(!is_dml("CREATE TABLE t (a NUMBER)"));
        assert!(!is_dml("BEGIN NULL; END;"));
    }
}
