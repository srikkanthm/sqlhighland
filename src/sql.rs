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
        && (bytes[j].is_ascii_alphanumeric()
            || bytes[j] == b'_'
            || bytes[j] == b'$'
            || bytes[j] == b'#')
    {
        j += 1;
    }
    (String::from_utf8_lossy(&bytes[i..j]).into_owned(), j)
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

/// One SQL*Plus substitution variable reference (`&name` / `&&name`).
/// `double` is true when first seen (or ever seen) with `&&`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubVar {
    pub name: String,
    pub double: bool,
}

fn is_var_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#'
}

/// Find substitution variables (`&name`, `&&name`, `&1`) in first-occurrence
/// order, deduped. Scans string literals too (SQL*Plus substitutes inside
/// `'...'`), but skips `--` / `/* */` comments. `\&` is an escape (literal
/// `&`, no prompt). A single trailing `.` after the name is a separator
/// (`SET CONCAT`) and does not join the name.
pub fn find_substitution_vars(sql: &str) -> Vec<SubVar> {
    let bytes = sql.as_bytes();
    let mut out: Vec<SubVar> = Vec::new();
    let mut i = 0usize;
    // Scanner states; strings are still scanned for `&`, comments are not.
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    // Helper records one `&` reference starting at `i`; returns new offset.
    macro_rules! record_sub {
        ($i:expr) => {{
            let mut j = $i + 1;
            let mut double = false;
            if bytes.get(j) == Some(&b'&') {
                double = true;
                j += 1;
            }
            let start = j;
            while j < bytes.len() && is_var_char(bytes[j]) {
                j += 1;
            }
            if start == j {
                $i += 1;
            } else {
                let name = sql[start..j].to_string();
                if bytes.get(j) == Some(&b'.') {
                    j += 1;
                }
                match out.iter_mut().find(|v| v.name == name) {
                    Some(existing) => {
                        existing.double |= double;
                    }
                    None => out.push(SubVar { name, double }),
                }
                $i = j;
            }
        }};
    }
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    in_single = false;
                    i += 1;
                }
            } else if b == b'\\' && bytes.get(i + 1) == Some(&b'&') {
                i += 2;
            } else if b == b'&' {
                // SQL*Plus substitutes inside string literals too.
                record_sub!(i);
            } else {
                i += 1;
            }
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_block_comment = true;
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'&') => {
                i += 2; // escaped ampersand: literal, no prompt
            }
            b'&' => {
                record_sub!(i);
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// Find native bind variables (`:name`, `:1`) in first-occurrence order,
/// deduped. Skips strings, quoted identifiers, and comments. Skips `:=`
/// (PL/SQL assignment), `::`, and the trigger pseudo-binds `:NEW`, `:OLD`,
/// `:PARENT` (case-insensitive).
pub fn find_bind_vars(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    in_single = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_block_comment = true;
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b':') => {
                i += 2; // escaped colon: literal
            }
            b':' => {
                let next = bytes.get(i + 1).copied().unwrap_or(0);
                if next == b'=' || next == b':' {
                    i += 2;
                    continue;
                }
                let start = i + 1;
                let mut j = start;
                if next.is_ascii_digit() {
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                } else {
                    while j < bytes.len() && is_var_char(bytes[j]) {
                        j += 1;
                    }
                }
                if start == j {
                    i += 1;
                    continue;
                }
                let name = sql[start..j].to_string();
                if name.eq_ignore_ascii_case("new")
                    || name.eq_ignore_ascii_case("old")
                    || name.eq_ignore_ascii_case("parent")
                {
                    i = j;
                    continue;
                }
                if !out.iter().any(|n| n == &name) {
                    out.push(name);
                }
                i = j;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// Apply substitution values textually (non-recursive: values are never
/// rescanned). Missing names are left in place. `\&` unescapes to `&`.
/// Comments are passed through untouched; strings are substituted.
pub fn apply_substitutions(
    sql: &str,
    values: &std::collections::HashMap<String, String>,
) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 32);
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            out.push(b as char);
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                out.push_str("*/");
                in_block_comment = false;
                i += 2;
            } else {
                out.push(b as char);
                i += 1;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    out.push_str("''");
                    i += 2;
                } else {
                    out.push('\'');
                    in_single = false;
                    i += 1;
                }
            } else if b == b'\\' && bytes.get(i + 1) == Some(&b'&') {
                // Escaped inside strings too: value or literal?
                // `\&name` never prompts, so it never has a value; emit `&`.
                out.push('&');
                i += 2;
            } else if b == b'&' {
                let (name, j, dot) = read_sub_name(sql, bytes, i);
                match name {
                    Some(name) => match values.get(&name) {
                        Some(v) => {
                            out.push_str(v);
                            i = j + usize::from(dot);
                        }
                        None => {
                            out.push_str(if sql[i + 1..].starts_with('&') {
                                "&&"
                            } else {
                                "&"
                            });
                            out.push_str(&name);
                            if dot {
                                out.push('.');
                            }
                            i = j + usize::from(dot);
                        }
                    },
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            } else {
                out.push(b as char);
                i += 1;
            }
            continue;
        }
        if in_double {
            out.push(b as char);
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                out.push('\'');
                in_single = true;
                i += 1;
            }
            b'"' => {
                out.push('"');
                in_double = true;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                out.push_str("--");
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.push_str("/*");
                in_block_comment = true;
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'&') => {
                out.push('&');
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b':') => {
                out.push(':');
                i += 2;
            }
            b'&' => {
                let (name, j, dot) = read_sub_name(sql, bytes, i);
                match name {
                    Some(name) => match values.get(&name) {
                        Some(v) => {
                            out.push_str(v);
                            i = j + usize::from(dot);
                        }
                        None => {
                            out.push_str(if sql[i + 1..].starts_with('&') {
                                "&&"
                            } else {
                                "&"
                            });
                            out.push_str(&name);
                            if dot {
                                out.push('.');
                            }
                            i = j + usize::from(dot);
                        }
                    },
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Read `&` / `&&` name at byte offset `i` (which is `&`). Returns
/// (name, end-of-name offset, had-trailing-dot).
fn read_sub_name(sql: &str, bytes: &[u8], i: usize) -> (Option<String>, usize, bool) {
    let mut j = i + 1;
    if bytes.get(j) == Some(&b'&') {
        j += 1;
    }
    let start = j;
    while j < bytes.len() && is_var_char(bytes[j]) {
        j += 1;
    }
    if start == j {
        return (None, i + 1, false);
    }
    let mut dot = false;
    if bytes.get(j) == Some(&b'.') {
        dot = true;
    }
    (Some(sql[start..j].to_string()), j, dot)
}

/// An `@` / `@@` / `START` script directive parsed from a single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtDirective {
    /// True for `@@` (resolve against the including file's directory).
    pub double_at: bool,
    /// Raw path token as typed (quotes stripped).
    pub path: String,
}

/// Parse one line as a script directive. Matches SQL*Plus/SQL Developer:
/// `@path`, `@@path`, or `START path`, with the path optionally
/// single/double-quoted (for spaces). A trailing `;` on a bare token is
/// stripped. Returns `None` for ordinary SQL lines.
pub fn parse_at_directive(line: &str) -> Option<AtDirective> {
    let t = line.trim_start();
    // `@@` must precede `@`.
    let (double_at, rest) = if let Some(r) = t.strip_prefix("@@") {
        (true, r)
    } else if let Some(r) = t.strip_prefix('@') {
        (false, r)
    } else {
        // `START path` alias (case-insensitive, whitespace-separated).
        // `START` alone is not a directive.
        let first: String = t.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        if !first.eq_ignore_ascii_case("start") {
            return None;
        }
        let rest = &t[first.len()..];
        // Require whitespace after START (`STARTUP` is not a directive).
        if !rest.starts_with(|c: char| c.is_whitespace()) || rest.trim().is_empty() {
            return None;
        }
        (false, rest)
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        return None;
    }
    let path = if let Some(q) = rest.strip_prefix('"') {
        q.split('"').next().unwrap_or("").to_string()
    } else if let Some(q) = rest.strip_prefix('\'') {
        q.split('\'').next().unwrap_or("").to_string()
    } else {
        // Bare token: up to whitespace or `;`. Extra trailing args
        // (`@seed.sql arg1`) are ignored for v1.
        rest.split(|c: char| c.is_whitespace() || c == ';')
            .next()
            .unwrap_or("")
            .to_string()
    };
    if path.is_empty() {
        return None;
    }
    Some(AtDirective { double_at, path })
}

/// The editor line containing byte `offset` (clamped). Used to spot an
/// `@`-directive under the caret without involving statement splitting
/// (a bare `@file` line has no terminator, so the splitter would merge
/// it with whatever follows).
pub fn line_at(text: &str, offset: usize) -> String {
    let mut offset = offset.min(text.len());
    // Clamp to a char boundary (cursor offsets are byte offsets).
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    // Past-the-end belongs to the last line.
    if offset == text.len() && offset > 0 {
        offset -= 1;
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
    }
    // A caret sitting exactly on a newline belongs to the line it ends.
    if offset > 0 && text.as_bytes().get(offset) == Some(&b'\n') {
        offset -= 1;
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
    }
    let start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(text.len());
    text.get(start..end).unwrap_or("").to_string()
}

/// Expanded script: full text with every directive spliced in, plus the
/// files involved (entry first) for summaries and cycle reports.
#[derive(Debug, Clone)]
pub struct ExpandedScript {
    pub text: String,
    pub files: Vec<std::path::PathBuf>,
}

/// Max `@` nesting depth (deploy → schema → …). Past this, report an
/// error rather than recursing forever.
pub const MAX_SCRIPT_DEPTH: usize = 10;

/// Expand `@` / `@@` / `START` directive lines in `entry_text`, reading
/// files relative to `base_dir` (the tab file's dir when file-backed,
/// else the process working directory). `@@` resolves against the
/// *including* file's directory, so nested trees work from anywhere;
/// `@`/`START` resolve against `base_dir` at the top level and against
/// the includer's directory below it (SQL*Plus parity for nesting).
/// Absolute paths pass through verbatim; `~` expands to `$HOME`; a
/// missing extension tries `+.sql` (SQL Developer parity); quoted paths
/// carry spaces. Only full-line directives expand (leading whitespace
/// allowed) — an `@` mid-statement or inside `--` / `/* */` comments is
/// left alone. Errors name the offending file.
pub fn expand_at_directives(
    entry_text: &str,
    base_dir: &std::path::Path,
) -> Result<ExpandedScript, String> {
    let mut stack: Vec<std::path::PathBuf> = Vec::new();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let text = expand_text(entry_text, base_dir, &mut stack, &mut files, 0)?;
    Ok(ExpandedScript { text, files })
}

/// Expand starting from a raw directive path (as typed on an `@` line):
/// resolve against `base_dir` (absolute verbatim, `~` → `$HOME`), try
/// `+.sql` when extensionless (SQL Developer parity), read, then expand
/// nested directives against the entry file's own directory. The entry
/// file leads `files`, so summaries can name it.
pub fn expand_script_file(
    raw: &str,
    base_dir: &std::path::Path,
) -> Result<ExpandedScript, String> {
    let resolved = resolve_script_path(raw, base_dir);
    let candidate = with_sql_extension_fallback(&resolved);
    let content = std::fs::read_to_string(&candidate)
        .map_err(|_| format!("Cannot open script file: {}", candidate.display()))?;
    let canon = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.clone());
    let mut stack = vec![canon];
    let mut files = vec![candidate.clone()];
    let child_dir: std::path::PathBuf = candidate
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| base_dir.to_path_buf());
    let text = expand_text(&content, &child_dir, &mut stack, &mut files, 0)?;
    Ok(ExpandedScript { text, files })
}

fn expand_text(
    text: &str,
    base_dir: &std::path::Path,
    stack: &mut Vec<std::path::PathBuf>,
    files: &mut Vec<std::path::PathBuf>,
    depth: usize,
) -> Result<String, String> {
    if depth > MAX_SCRIPT_DEPTH {
        return Err(format!(
            "Script nesting too deep (>{MAX_SCRIPT_DEPTH} levels) — cyclic includes?"
        ));
    }
    let mut out = String::with_capacity(text.len() + 64);
    let mut in_block_comment = false;
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        // Track `/* */` across lines so directives inside block comments
        // are never expanded. (Line comments and strings are handled by
        // only matching directives at line start — see below.)
        let mut scan = body;
        if in_block_comment {
            if let Some(end) = scan.find("*/") {
                scan = &scan[end + 2..];
                in_block_comment = false;
            } else {
                out.push_str(line);
                continue;
            }
        }
        // A `/*` before any directive token opens a comment; a directive
        // before any `/*` is real. (`--` to end-of-line likewise kills it.)
        let code = strip_line_comment(scan);
        let (directive_part, opens_block) = match code.find("/*") {
            Some(i) => (&code[..i], true),
            None => (code, false),
        };
        if opens_block {
            in_block_comment = true;
        }
        if let Some(d) = parse_at_directive(directive_part) {
            // Resolve: absolute verbatim; `@@` against the includer's dir
            // (== base_dir below the top level); `@` against base_dir.
            // (Both coincide below the top level — SQL*Plus parity.)
            let resolved = resolve_script_path(&d.path, base_dir);
            let candidate = with_sql_extension_fallback(&resolved);
            let content = std::fs::read_to_string(&candidate).map_err(|_| {
                format!("Cannot open script file: {}", candidate.display())
            })?;
            // Cycle check on the canonical path when available.
            let canon = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.clone());
            if stack.contains(&canon) {
                return Err(format!(
                    "Cyclic script include: {} includes itself",
                    canon.display()
                ));
            }
            files.push(candidate.clone());
            stack.push(canon);
            let child_dir: std::path::PathBuf = candidate
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| base_dir.to_path_buf());
            let expanded = expand_text(&content, &child_dir, stack, files, depth + 1)?;
            stack.pop();
            out.push_str(&expanded);
            if !expanded.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
            let _ = newline;
        }
    }
    Ok(out)
}

/// Strip a `--` line comment (outside quotes is overkill here: the caller
/// only feeds the pre-`/*` fragment, and a `--` inside a quoted `@` path
/// is vanishingly rare; a trailing comment after a directive is common).
fn strip_line_comment(s: &str) -> &str {
    match s.find("--") {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Resolve a raw directive path: `~` → `$HOME`, absolute verbatim,
/// relative against `base_dir`.
fn resolve_script_path(raw: &str, base_dir: &std::path::Path) -> std::path::PathBuf {
    let expanded = if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        match std::env::var("HOME") {
            Ok(home) => std::path::PathBuf::from(home).join(rest),
            Err(_) => std::path::PathBuf::from(raw),
        }
    } else if raw == "~" {
        match std::env::var("HOME") {
            Ok(home) => std::path::PathBuf::from(home),
            Err(_) => std::path::PathBuf::from(raw),
        }
    } else {
        std::path::PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// SQL Developer parity: `@clean_tables` finds `clean_tables.sql`.
/// Tries the path as-is first, then with `.sql` appended.
fn with_sql_extension_fallback(p: &std::path::Path) -> std::path::PathBuf {
    if p.exists() {
        return p.to_path_buf();
    }
    if p.extension().is_none() {
        let with_ext = p.with_extension("sql");
        if with_ext.exists() {
            return with_ext;
        }
    }
    p.to_path_buf()
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
    fn statement_at_trailing_space_runs_current_line() {
        // End of line 1 with trailing spaces still belongs to statement 1.
        let sql = "SELECT 1;  \nSELECT 2;";
        let eol = sql.find('\n').unwrap();
        assert_eq!(statement_at(sql, eol).as_deref(), Some("SELECT 1;"));
        // Same line, two statements: the gap after `;` runs the first.
        let sql = "SELECT 1; SELECT 2;";
        assert_eq!(statement_at(sql, 9).as_deref(), Some("SELECT 1;"));
        assert_eq!(statement_at(sql, 10).as_deref(), Some("SELECT 1;"));
        assert_eq!(statement_at(sql, 11).as_deref(), Some("SELECT 2;"));
    }

    #[test]
    fn statement_at_next_line_indent_runs_next() {
        // Leading indentation of the next line belongs to the next statement.
        let sql = "SELECT 1;\n   SELECT 2;";
        let indent = sql.find("SELECT 2").unwrap() - 1;
        assert_eq!(statement_at(sql, indent).as_deref(), Some("SELECT 2;"));
    }

    #[test]
    fn statement_at_picks_statement_under_caret() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        assert_eq!(statement_at(sql, 2).as_deref(), Some("SELECT 1;"));
        assert_eq!(statement_at(sql, 12).as_deref(), Some("SELECT 2;"));
        assert_eq!(statement_at(sql, 22).as_deref(), Some("SELECT 3;"));
    }

    #[test]
    fn statement_at_range_matches_splice() {
        // Ranges cover the raw span (leading whitespace included); the
        // text is trimmed. Format must splice on the range while keeping
        // the surrounding whitespace so statements never join or drift.
        let sql = "select 1;\nselect 2;";
        let (text, start, end) = statement_at_range(sql, 12).unwrap();
        assert_eq!(text, "select 2;");
        let span = &sql[start..end];
        assert_eq!(span, "\nselect 2;");
        let lead = span.len() - span.trim_start().len();
        let trail = span.len() - span.trim_end().len();
        let formatted = format_sql(span.trim()).trim().to_string();
        let rebuilt = format!(
            "{}{}{}{}{}",
            &sql[..start],
            &span[..lead],
            formatted,
            &span[span.len() - trail..],
            &sql[end..]
        );
        assert_eq!(rebuilt, format!("select 1;\n{formatted}"));
        assert!(rebuilt.contains('\n'));
    }

    #[test]
    fn statement_at_past_end_reruns_last() {
        let sql = "SELECT 1;\nSELECT 2;   ";
        assert_eq!(statement_at(sql, sql.len()).as_deref(), Some("SELECT 2;"));
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
        assert_eq!(statement_at("SELECT 1;", 99).as_deref(), Some("SELECT 1;"));
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
        assert_eq!(
            statement_kind("DECLARE x NUMBER; BEGIN x := 1; END;"),
            Execute
        );
        assert_eq!(statement_kind("COMMIT"), Execute);
        assert_eq!(statement_kind(""), Execute);
        // DESCRIBE rides the query path (emulated via ALL_TAB_COLUMNS).
        assert_eq!(statement_kind("DESCRIBE emp"), Query);
        assert_eq!(statement_kind("  desc scott.emp;"), Query);
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
        assert_eq!(
            exec_summary("INSERT INTO t VALUES (1)", 1),
            "1 row inserted"
        );
        assert_eq!(
            exec_summary("insert into t select * from s", 5),
            "5 rows inserted"
        );
        assert_eq!(exec_summary("UPDATE t SET a = 1", 0), "0 rows updated");
        assert_eq!(exec_summary("-- gone\nDELETE FROM t", 2), "2 rows deleted");
        assert_eq!(
            exec_summary(
                "MERGE INTO t USING s ON (1=1) WHEN MATCHED THEN UPDATE SET a=1",
                1
            ),
            "1 row merged"
        );
        assert_eq!(exec_summary("BEGIN NULL; END;", 1), "PL/SQL block executed");
        assert_eq!(exec_summary("COMMIT", 0), "Committed");
        assert_eq!(exec_summary("rollback", 0), "Rolled back");
        assert_eq!(exec_summary("GRANT SELECT ON t TO r", 0), "0 rows affected");
    }

    #[test]
    fn exec_summary_names_ddl_objects() {
        assert_eq!(
            exec_summary("CREATE TABLE emp (a NUMBER)", 0),
            "Table emp created"
        );
        assert_eq!(
            exec_summary("create or replace view v as select 1 from dual", 0),
            "View v created"
        );
        assert_eq!(
            exec_summary("DROP INDEX \"My Index\"", 0),
            "Index My Index dropped"
        );
        assert_eq!(
            exec_summary("ALTER TABLE scott.emp ADD (b NUMBER)", 0),
            "Table scott.emp altered"
        );
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
        assert!(is_dml(
            "MERGE INTO t USING s ON (t.a = s.a) WHEN MATCHED THEN UPDATE SET a = 1"
        ));
        assert!(!is_dml("SELECT 1 FROM dual"));
        assert!(!is_dml("CREATE TABLE t (a NUMBER)"));
        assert!(!is_dml("BEGIN NULL; END;"));
    }

    #[test]
    fn sub_vars_detect_named_positional_and_double() {
        let vars = find_substitution_vars("SELECT * FROM &tab WHERE a = &1 AND b = &&tab");
        assert_eq!(
            vars,
            vec![
                SubVar {
                    name: "tab".into(),
                    double: true
                },
                SubVar {
                    name: "1".into(),
                    double: false
                },
            ]
        );
    }

    #[test]
    fn sub_vars_include_strings_skip_comments_and_escape() {
        // Inside '...' still prompts (SQL*Plus parity); comments don't; \& doesn't.
        let vars = find_substitution_vars(
            "SELECT '&dept' FROM t; -- &ignored\n/* &also_ignored */ SELECT \\&lit, &real",
        );
        assert_eq!(
            vars,
            vec![
                SubVar {
                    name: "dept".into(),
                    double: false
                },
                SubVar {
                    name: "real".into(),
                    double: false
                },
            ]
        );
    }

    #[test]
    fn sub_vars_trailing_dot_is_separator() {
        let vars = find_substitution_vars("SELECT * FROM &schema.emp");
        assert_eq!(
            vars,
            vec![SubVar {
                name: "schema".into(),
                double: false
            }]
        );
        let mut map = std::collections::HashMap::new();
        map.insert("schema".to_string(), "scott".to_string());
        assert_eq!(
            apply_substitutions("SELECT * FROM &schema.emp", &map),
            "SELECT * FROM scottemp"
        );
    }

    #[test]
    fn sub_apply_is_raw_and_non_recursive() {
        let mut map = std::collections::HashMap::new();
        map.insert("d".to_string(), "&e".to_string());
        // Value containing `&` is NOT re-expanded.
        assert_eq!(
            apply_substitutions("SELECT &d FROM dual", &map),
            "SELECT &e FROM dual"
        );
        // Missing names stay in place; escapes unescape.
        assert_eq!(
            apply_substitutions("SELECT &missing, \\&lit FROM dual", &map),
            "SELECT &missing, &lit FROM dual"
        );
    }

    #[test]
    fn bind_vars_detect_and_skip_pseudo() {
        assert_eq!(
            find_bind_vars("SELECT * FROM t WHERE a = :id AND b = :1"),
            vec!["id", "1"]
        );
        // Dedup, `:=` skipped, trigger pseudo-binds skipped.
        assert_eq!(
            find_bind_vars("BEGIN x := :v; IF :v > 0 THEN :NEW.x := 1; END; -- :c\n/* :d */"),
            vec!["v"]
        );
        assert!(find_bind_vars("SELECT ':not_a_bind', \":neither\" FROM dual").is_empty());
        assert_eq!(
            find_bind_vars("SELECT :OLD, :old, :Parent FROM dual"),
            Vec::<String>::new()
        );
    }

    // --- @-script directives ------------------------------------------------

    fn script_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sqlhighland-script-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_at_directive_forms() {
        assert_eq!(
            parse_at_directive("@/tmp/seed.sql"),
            Some(AtDirective { double_at: false, path: "/tmp/seed.sql".into() })
        );
        assert_eq!(
            parse_at_directive("  @@schema/tables.sql  "),
            Some(AtDirective { double_at: true, path: "schema/tables.sql".into() })
        );
        assert_eq!(
            parse_at_directive("@\"my dir/seed.sql\";"),
            Some(AtDirective { double_at: false, path: "my dir/seed.sql".into() })
        );
        assert_eq!(
            parse_at_directive("@'my dir/seed.sql'"),
            Some(AtDirective { double_at: false, path: "my dir/seed.sql".into() })
        );
        assert_eq!(
            parse_at_directive("START deploy.sql"),
            Some(AtDirective { double_at: false, path: "deploy.sql".into() })
        );
        assert_eq!(
            parse_at_directive("start  ./a.sql"),
            Some(AtDirective { double_at: false, path: "./a.sql".into() })
        );
        // Non-directives.
        assert_eq!(parse_at_directive("SELECT 1 FROM dual"), None);
        assert_eq!(parse_at_directive("@"), None);
        assert_eq!(parse_at_directive("@@"), None);
        assert_eq!(parse_at_directive("START"), None);
        assert_eq!(parse_at_directive("STARTUP costs"), None);
        assert_eq!(parse_at_directive(""), None);
    }

    #[test]
    fn line_at_picks_caret_line() {
        let text = "@a.sql\nSELECT 1;\n";
        assert_eq!(line_at(text, 0), "@a.sql");
        assert_eq!(line_at(text, 3), "@a.sql");
        assert_eq!(line_at(text, 7), "SELECT 1;");
        assert_eq!(line_at(text, 999), "SELECT 1;");
    }

    #[test]
    fn expand_single_and_sql_fallback() {
        let dir = script_dir("single");
        std::fs::write(dir.join("seed.sql"), "SELECT 1 FROM dual;\n").unwrap();
        // Extensionless ref finds seed.sql.
        let out = expand_at_directives("@seed\nSELECT 2 FROM dual;", &dir).unwrap();
        assert!(out.text.contains("SELECT 1 FROM dual;"), "{}", out.text);
        assert!(out.text.contains("SELECT 2 FROM dual;"), "{}", out.text);
        assert_eq!(out.files.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_double_at_resolves_against_includer() {
        let dir = script_dir("nest");
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(dir.join("schema").join("t.sql"), "CREATE TABLE t (a NUMBER);\n").unwrap();
        std::fs::write(dir.join("deploy.sql"), "@@schema/t.sql\n").unwrap();
        // Launched from elsewhere (base = dir itself here, but the nested
        // ref resolves against deploy.sql's dir either way).
        let entry = std::fs::read_to_string(dir.join("deploy.sql")).unwrap();
        let child_dir = dir.clone();
        let out = expand_at_directives(&entry, &child_dir).unwrap();
        assert!(out.text.contains("CREATE TABLE t"), "{}", out.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_missing_file_errors() {
        let dir = script_dir("missing");
        let err = expand_at_directives("@nope.sql\n", &dir).unwrap_err();
        assert!(err.contains("Cannot open script file"), "{err}");
        assert!(err.contains("nope.sql"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_cycle_errors() {
        let dir = script_dir("cycle");
        std::fs::write(dir.join("a.sql"), "@b.sql\n").unwrap();
        std::fs::write(dir.join("b.sql"), "@a.sql\n").unwrap();
        let err = expand_at_directives("@a.sql\n", &dir).unwrap_err();
        assert!(err.contains("Cyclic"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_ignores_comments() {
        let dir = script_dir("comments");
        std::fs::write(dir.join("real.sql"), "SELECT 1 FROM dual;\n").unwrap();
        let entry = "-- @real.sql\n/* @real.sql */\nSELECT 2 FROM dual;\n";
        let out = expand_at_directives(entry, &dir).unwrap();
        assert!(!out.text.contains("SELECT 1 FROM dual"), "{}", out.text);
        assert!(out.files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
