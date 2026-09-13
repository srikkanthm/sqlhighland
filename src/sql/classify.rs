//! Statement classification and execution summaries.
//!
//! Part of the `sql` module (see `sql.rs`).

use super::*;

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
    // SQL*Plus DESCRIBE/DESC: not server SQL, but our emulation answers it
    // with a result set (see `db::rewrite_describe`), so it must ride the
    // query path — the execute path sends it raw and Oracle answers ORA-00900.
    if keyword.eq_ignore_ascii_case("describe") || keyword.eq_ignore_ascii_case("desc") {
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
    let name = read_object_name(bytes, j)
        .map(|(owner, name)| owner.map(|o| format!("{o}.{name}")).unwrap_or(name));
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
