//! Oracle access behind a small trait.
//!
//! `oracledb` 26.x is beta and its API will churn; everything Oracle-specific
//! stays in this module. All calls are blocking — callers must run them on a
//! GPUI background executor, never on the UI thread.

use std::time::Instant;

use crate::model::{ColumnInfo, ConnectionConfig, QueryResult};

/// Rows per on-demand fetch (initial load fetches one chunk too).
pub const FETCH_CHUNK: usize = 1000;
/// Safety ceiling on total buffered rows per query.
pub const FETCH_CAP: usize = 100_000;

/// Pulled page: converted rows plus whether the server is exhausted.
type PulledRows = (Vec<Vec<Option<String>>>, bool);

/// One page pulled from a held-open cursor.
#[derive(Debug)]
pub struct FetchPage {
    pub rows: Vec<Vec<Option<String>>>,
    /// True when the server has no more rows.
    pub exhausted: bool,
    /// False when a newer query superseded this one (or we disconnected).
    /// Stale pages must be discarded, never appended.
    pub current: bool,
}

pub trait DbClient {
    fn connect(&mut self, cfg: &ConnectionConfig) -> Result<(), DbError>;
    fn is_connected(&self) -> bool;
    fn disconnect(&mut self);
    fn run_query(
        &mut self,
        sql: &str,
        max_rows: usize,
        binds: &[BindParam],
    ) -> Result<QueryResult, DbError>;
    /// Execute a non-query statement (DML/DDL/PL/SQL). Returns rows affected.
    /// The driver does not autocommit — call `commit` explicitly.
    fn exec(&mut self, sql: &str, binds: &[BindParam]) -> Result<(u64, u128), DbError>;
    fn commit(&mut self) -> Result<(), DbError>;
    fn rollback(&mut self) -> Result<(), DbError>;
}

/// One native bind value. `name` is the placeholder without the colon
/// (`"id"` for `:id`, `"1"` for `:1`); all values are sent as strings
/// (VARCHAR2) — SQL Developer parity. Callers wrap with
/// `TO_NUMBER`/`TO_DATE` for other types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindParam {
    pub name: String,
    pub value: String,
}

impl BindParam {
    pub fn s(name: &str, value: &str) -> Self {
        Self {
            name: name.to_string(),
            value: value.to_string(),
        }
    }
}

/// Thin-driver session holding one connection plus at most one open cursor.
///
/// The cursor stays open server-side between `start_query` and exhaustion (or
/// supersession), which is what makes on-demand fetching possible without
/// re-executing the statement.
pub struct OracledbSession {
    conn: Option<oracledb::Connection>,
    cursor: Option<oracledb::Cursor>,
    columns: Vec<ColumnInfo>,
    /// Lookahead row consumed to detect end-of-data; prepended to next page.
    pending: Option<oracledb::Row>,
    query_id: u64,
}

impl OracledbSession {
    pub fn new() -> Self {
        Self {
            conn: None,
            cursor: None,
            columns: Vec::new(),
            pending: None,
            query_id: 0,
        }
    }

    /// Generation of the currently open cursor. Fetch completions must check
    /// this (see [`FetchPage::current`]).
    pub fn query_id(&self) -> u64 {
        self.query_id
    }

    /// Execute and pull the first page, holding the cursor open for more.
    /// Returns the columns, the first page, and the new generation id.
    /// Supersedes any previously open cursor (it is dropped/closed).
    /// `Cursor` has no close API in beta.3 — drop is the only release, so a
    /// superseded server cursor is reaped server-side; the poison-drop in
    /// [`OracledbSession::pull_locked`] covers a desync mid-flight.
    /// `binds` are native `:name` values, all sent as VARCHAR2 strings.
    pub fn start_query(
        &mut self,
        sql: &str,
        first_n: usize,
        binds: &[BindParam],
    ) -> Result<(Vec<ColumnInfo>, FetchPage, u64), DbError> {
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DbError("not connected".to_string()))?;
        let sql = sanitize_statement(sql)?;
        // DESCRIBE is a SQL*Plus client command, not SQL — the server
        // rejects it (ORA-00900). Emulate it via ALL_TAB_COLUMNS.
        let rewritten;
        let sql = match rewrite_describe(sql) {
            Some(q) => {
                rewritten = q;
                rewritten.as_str()
            }
            None => sql,
        };
        let cursor = query_with_binds(conn, sql, binds).map_err(|e| {
            // A TTC desync poisons the connection: the driver and server no
            // longer agree on the response stream, so every later run on this
            // pooled session would fail too. Drop it; the next run reconnects
            // fresh (the pool's lazy-connect path). Plain ORA- errors leave
            // the connection usable and must NOT disconnect.
            if is_poisoned(&e) {
                self.disconnect();
            }
            DbError::from(e)
        })?;

        let columns: Vec<ColumnInfo> = cursor
            .columns()
            .iter()
            .map(|m| ColumnInfo {
                name: m.name().to_string(),
                db_type: m.db_type().name().to_string(),
            })
            .collect();

        self.query_id = self.query_id.wrapping_add(1);
        self.cursor = Some(cursor);
        self.columns = columns.clone();
        self.pending = None;

        let (rows, exhausted) = self.pull_locked(first_n)?;
        Ok((
            columns,
            FetchPage {
                rows,
                exhausted,
                current: true,
            },
            self.query_id,
        ))
    }

    /// Pull the next page from the open cursor. Stale generations and a
    /// missing connection yield a non-current page the UI must discard.
    /// Protocol-class fetch failures disconnect the session (see
    /// [`OracledbSession::pull_locked`]); ORA- errors keep it for retry.
    pub fn fetch_more(&mut self, query_id: u64, n: usize) -> Result<FetchPage, DbError> {        if query_id != self.query_id || self.conn.is_none() {
            return Ok(FetchPage {
                rows: Vec::new(),
                exhausted: true,
                current: false,
            });
        }
        if self.cursor.is_none() {
            return Ok(FetchPage {
                rows: Vec::new(),
                exhausted: true,
                current: true,
            });
        }
        let (rows, exhausted) = self.pull_locked(n)?;
        if exhausted {
            // Server is done: release its resources now, not at next query.
            self.cursor = None;
        }
        Ok(FetchPage {
            rows,
            exhausted,
            current: true,
        })
    }

    /// Execute a DML/DDL/PL-SQL statement. Returns `(rows_affected, elapsed_ms)`.
    /// Supersedes any open cursor (an execute invalidates server-side state
    /// sibling tabs may be paging through — the UI marks them exhausted).
    /// `binds` are native `:name` values, all sent as VARCHAR2 strings.
    pub fn exec(&mut self, sql: &str, binds: &[BindParam]) -> Result<(u64, u128), DbError> {
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DbError("not connected".to_string()))?;
        let sql = sanitize_statement(sql)?;
        let started = Instant::now();
        let result = exec_with_binds(conn, sql, binds).map_err(|e| {
            if is_poisoned(&e) {
                self.disconnect();
            }
            DbError::from(e)
        })?;
        self.cursor = None;
        self.pending = None;
        self.query_id = self.query_id.wrapping_add(1);
        Ok((result.rows_affected(), started.elapsed().as_millis()))
    }

    pub fn commit(&mut self) -> Result<(), DbError> {
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DbError("not connected".to_string()))?;
        conn.commit().map_err(DbError::from)
    }

    pub fn rollback(&mut self) -> Result<(), DbError> {
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DbError("not connected".to_string()))?;
        conn.rollback().map_err(DbError::from)
    }

    /// Pull up to `limit` converted rows; probe one extra row to learn
    /// end-of-data without losing it (stashed in `pending`).
    ///
    /// A protocol-class fetch failure (TTC desync) poisons the whole session,
    /// so it is dropped — the next run reconnects fresh. Plain ORA- errors
    /// keep cursor + connection alive for scroll-to-retry.
    fn pull_locked(&mut self, limit: usize) -> Result<PulledRows, DbError> {
        assert!(limit > 0, "fetch limit must be positive");
        let outcome: Result<PulledRows, oracledb::Error> = (|| {
            let cursor = self
                .cursor
                .as_mut()
                .expect("pull_locked with no open cursor");
            let mut rows: Vec<Vec<Option<String>>> = Vec::new();
            if let Some(row) = self.pending.take() {
                rows.push(convert_row(row, &self.columns));
            }
            let mut exhausted = false;
            while rows.len() < limit {
                match cursor.next() {
                    Some(Ok(row)) => rows.push(convert_row(row, &self.columns)),
                    Some(Err(e)) => return Err(e),
                    None => {
                        exhausted = true;
                        break;
                    }
                }
            }
            if !exhausted {
                match cursor.next() {
                    Some(Ok(row)) => self.pending = Some(row),
                    Some(Err(e)) => return Err(e),
                    None => exhausted = true,
                }
            }
            Ok((rows, exhausted))
        })();
        match outcome {
            Ok(page) => Ok(page),
            Err(e) => {
                if is_poisoned(&e) {
                    self.disconnect();
                }
                Err(DbError::from(e))
            }
        }
    }
}

/// Database error with the full Oracle message text (preserves `ORA-xxxxx`).
#[derive(Debug, Clone)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DbError {}

impl From<oracledb::Error> for DbError {
    fn from(err: oracledb::Error) -> Self {
        Self(err.to_string())
    }
}

impl Default for OracledbSession {
    fn default() -> Self {
        Self::new()
    }
}

impl DbClient for OracledbSession {
    fn connect(&mut self, cfg: &ConnectionConfig) -> Result<(), DbError> {
        let ora_cfg = oracledb::Config::default()
            .set_credentials(&cfg.user, &cfg.password)
            .set_connect_string(&cfg.connect_string())
            .map_err(DbError::from)?;
        let conn = oracledb::connect(ora_cfg).map_err(DbError::from)?;
        self.conn = Some(conn);
        // Fresh connection: no cursor can be open.
        self.cursor = None;
        self.pending = None;
        self.columns.clear();
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    fn disconnect(&mut self) {
        // `Connection` has no explicit close in beta.3; drop ends the session.
        // Dropping the cursor first releases server-side resources promptly.
        self.cursor = None;
        self.pending = None;
        self.columns.clear();
        self.conn = None;
    }

    /// Convenience for non-incremental callers (and tests): run to `max_rows`
    /// like the old implementation, then close the cursor.
    fn run_query(
        &mut self,
        sql: &str,
        max_rows: usize,
        binds: &[BindParam],
    ) -> Result<QueryResult, DbError> {
        let started = Instant::now();
        let (columns, page, _) = self.start_query(sql, max_rows, binds)?;
        self.cursor = None;
        self.pending = None;
        Ok(QueryResult {
            columns,
            rows: page.rows,
            elapsed_ms: started.elapsed().as_millis(),
            truncated: !page.exhausted,
        })
    }

    fn exec(&mut self, sql: &str, binds: &[BindParam]) -> Result<(u64, u128), DbError> {
        OracledbSession::exec(self, sql, binds)
    }

    fn commit(&mut self) -> Result<(), DbError> {
        OracledbSession::commit(self)
    }

    fn rollback(&mut self) -> Result<(), DbError> {
        OracledbSession::rollback(self)
    }
}

/// Strip editor-style statement terminators (`;`) and reject empty input.
///
/// Only trailing semicolons are removed, so `SELECT ';' FROM dual;`
/// correctly keeps the one inside the string literal. Anonymous PL/SQL
/// blocks are the exception: the server *requires* their trailing `;`,
/// so it is preserved (duplicates collapsed to one).
fn sanitize_statement(sql: &str) -> Result<&str, DbError> {
    use crate::sql::is_plsql_block;
    let mut s = sql.trim();
    if is_plsql_block(s) {
        while s.ends_with(";;") {
            s = s[..s.len() - 1].trim_end();
        }
    } else {
        while let Some(stripped) = s.strip_suffix(';') {
            s = stripped.trim_end();
        }
    }
    if s.is_empty() {
        return Err(DbError("empty statement".to_string()));
    }
    Ok(s)
}
/// True for SQL*Plus DESCRIBE/DESC commands (client-side, emulated via
/// `ALL_TAB_COLUMNS`). A describe that returns zero rows means the object is
/// not visible — every real table has at least one column — so callers show
/// a not-found hint instead of a bare empty grid.
pub fn is_describe_statement(sql: &str) -> bool {
    let mut s = sql.trim();
    while let Some(stripped) = s.strip_suffix(';') {
        s = stripped.trim_end();
    }
    rewrite_describe(s).is_some()
}

/// Rewrite SQL*Plus `DESCRIBE`/`DESC <object>` into a query against
/// `ALL_TAB_COLUMNS`. Returns `None` when the text is not a describe command.
///
/// Accepts schema-qualified and double-quoted names (`scott."Emp"`); unquoted
/// identifiers fold to uppercase like Oracle does. Anything else (extra
/// tokens, garbage) falls through so the server reports the real error.
fn rewrite_describe(sql: &str) -> Option<String> {
    let rest = strip_keyword(sql, "describe").or_else(|| strip_keyword(sql, "desc"))?;
    let (owner, table) = parse_object_name(rest)?;
    let owner_filter = owner
        .map(|o| format!("AND owner = '{}' ", escape_literal(&o)))
        .unwrap_or_default();
    Some(format!(
        "SELECT column_name AS \"Name\", \
            DECODE(nullable, 'N', 'NOT NULL', NULL) AS \"Null?\", \
            CASE \
              WHEN data_type IN ('VARCHAR2','NVARCHAR2','CHAR','NCHAR') \
                THEN data_type || '(' || data_length || ')' \
              WHEN data_type IN ('NUMBER','FLOAT','DECIMAL','NUMERIC') AND data_precision IS NOT NULL \
                THEN data_type || '(' || data_precision || NVL2(data_scale, ',' || data_scale, '') || ')' \
              ELSE data_type \
            END AS \"Type\" \
         FROM all_tab_columns \
         WHERE table_name = '{}' {}ORDER BY owner, column_id",
        escape_literal(&table),
        owner_filter
    ))
}

/// Strip a leading keyword (case-insensitive) requiring a word boundary.
/// Returns the remainder, or `None` on mismatch.
fn strip_keyword<'a>(sql: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = sql.trim_start();
    if trimmed.len() < keyword.len() {
        return None;
    }
    if !trimmed[..keyword.len()].eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &trimmed[keyword.len()..];
    // `descending` is not `desc`; `described` is not `describe`.
    if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '#') {
        return None;
    }
    Some(rest)
}

/// Parse `[owner.]table` with double-quoted identifier support.
/// Returns uppercase-folded names (quoted parts keep case). The remainder
/// after the object name must be whitespace-only.
fn parse_object_name(rest: &str) -> Option<(Option<String>, String)> {
    let rest = rest.trim_start();
    let (first, after_first) = parse_identifier(rest)?;
    let after_first = after_first.trim_start();
    if let Some(after_dot) = after_first.strip_prefix('.') {
        let (second, after_second) = parse_identifier(after_dot.trim_start())?;
        if !after_second.trim().is_empty() {
            return None;
        }
        Some((Some(first), second))
    } else {
        if !after_first.trim().is_empty() {
            return None;
        }
        Some((None, first))
    }
}

/// Parse one identifier: `"quoted"` (with `""` escape) or bare
/// `[A-Za-z][A-Za-z0-9_$#]*`. Bare folds to uppercase; quoted keeps case.
fn parse_identifier(s: &str) -> Option<(String, &str)> {
    if let Some(quoted) = s.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = quoted.char_indices();
        loop {
            let (i, c) = chars.next()?;
            if c == '"' {
                // `""` is an escaped quote; a lone `"` ends the identifier.
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
        let (ident, rest) = s.split_at(end);
        if ident.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        Some((ident.to_ascii_uppercase(), rest))
    }
}

fn escape_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// True for protocol-level failures that leave the connection unusable.
/// A TTC desync means driver and server disagree on the response stream, so
/// the pooled session must be dropped (next run reconnects fresh). Plain
/// ORA- server errors are NOT poisoning — the connection stays usable.
fn is_poisoned(err: &oracledb::Error) -> bool {
    use oracledb::ErrorKind as K;
    matches!(
        err.kind(),
        K::UnknownTtcMessageType(..)
            | K::UnknownServerSidePiggyback(..)
            | K::DeadConnection
            | K::UnableToRecover
            | K::UnexpectedError
    )
}

/// Run a query with native binds. All-numeric placeholders (`:1`, `:2`) go
/// through the positional API (ordered by number); anything else uses named
/// binds. All values are sent as VARCHAR2 strings — SQL Developer parity.
fn query_with_binds(
    conn: &oracledb::Connection,
    sql: &str,
    binds: &[BindParam],
) -> Result<oracledb::Cursor, oracledb::Error> {
    if binds.is_empty() {
        return conn.query(sql, &[]);
    }
    if binds
        .iter()
        .all(|b| !b.name.is_empty() && b.name.bytes().all(|c| c.is_ascii_digit()))
    {
        let mut ordered = binds.to_vec();
        ordered.sort_by_key(|b| b.name.parse::<u32>().unwrap_or(u32::MAX));
        let refs: Vec<&dyn oracledb::ToDbValue> =
            ordered.iter().map(|b| &b.value as &dyn oracledb::ToDbValue).collect();
        conn.query(sql, &refs)
    } else {
        let refs: Vec<(&str, &dyn oracledb::ToDbValue)> = binds
            .iter()
            .map(|b| (b.name.as_str(), &b.value as &dyn oracledb::ToDbValue))
            .collect();
        conn.query_named(sql, &refs)
    }
}

/// Execute a non-query with native binds (same routing as [`query_with_binds`]).
fn exec_with_binds(
    conn: &oracledb::Connection,
    sql: &str,
    binds: &[BindParam],
) -> Result<oracledb::ExecResult, oracledb::Error> {
    if binds.is_empty() {
        return conn.execute(sql, &[]);
    }
    if binds
        .iter()
        .all(|b| !b.name.is_empty() && b.name.bytes().all(|c| c.is_ascii_digit()))
    {
        let mut ordered = binds.to_vec();
        ordered.sort_by_key(|b| b.name.parse::<u32>().unwrap_or(u32::MAX));
        let refs: Vec<&dyn oracledb::ToDbValue> =
            ordered.iter().map(|b| &b.value as &dyn oracledb::ToDbValue).collect();
        conn.execute(sql, &refs)
    } else {
        let refs: Vec<(&str, &dyn oracledb::ToDbValue)> = binds
            .iter()
            .map(|b| (b.name.as_str(), &b.value as &dyn oracledb::ToDbValue))
            .collect();
        conn.execute_named(sql, &refs)
    }
}

/// Convert one owned row to display cells.
fn convert_row(mut row: oracledb::Row, columns: &[ColumnInfo]) -> Vec<Option<String>> {
    columns
        .iter()
        .enumerate()
        .map(|(ix, col)| cell_to_display(&mut row, ix, &col.db_type))
        .collect()
}

/// Convert one cell to its display string. `None` is SQL NULL.
///
 /// The converter is chosen from the column's Oracle type up front (never
/// probe-and-retry: `Row::take` moves the value out, so a failed conversion
/// would destroy the cell for the next attempt).
fn cell_to_display(row: &mut oracledb::Row, ix: usize, db_type: &str) -> Option<String> {
    match db_type {
        "DB_TYPE_NUMBER" => row
            .get::<Option<oracledb::OracleNumber>>(ix)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        "DB_TYPE_BINARY_FLOAT" => row.get::<Option<f32>>(ix).ok().flatten().map(|v| v.to_string()),
        "DB_TYPE_BINARY_DOUBLE" => row.get::<Option<f64>>(ix).ok().flatten().map(|v| v.to_string()),
        "DB_TYPE_BOOLEAN" => row.get::<Option<bool>>(ix).ok().flatten().map(|v| v.to_string()),
        "DB_TYPE_DATE"
        | "DB_TYPE_TIMESTAMP"
        | "DB_TYPE_TIMESTAMP_TZ"
        | "DB_TYPE_TIMESTAMP_LTZ" => row
            .get::<Option<oracledb::OracleTimestamp>>(ix)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        "DB_TYPE_INTERVAL_DS" => row
            .get::<Option<oracledb::OracleIntervalDS>>(ix)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        "DB_TYPE_INTERVAL_YM" => row
            .get::<Option<oracledb::OracleIntervalYM>>(ix)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        "DB_TYPE_RAW" | "DB_TYPE_LONG_RAW" => row
            .get::<Option<Vec<u8>>>(ix)
            .ok()
            .flatten()
            .map(|bytes| hex_preview(&bytes)),
        "DB_TYPE_JSON" => row
            .get::<Option<oracledb::JsonValue>>(ix)
            .ok()
            .flatten()
            .map(|v| capped_debug(&v, 300)),
        "DB_TYPE_VECTOR" => row
            .get::<Option<oracledb::Vector>>(ix)
            .ok()
            .flatten()
            .map(|v| vector_preview(&v)),
        // LOBs have no read API in beta.3; report the size instead.
        // `Lob` requires owned access, hence `take` (row is ours to consume).
        "DB_TYPE_BLOB" => row
            .take::<Option<oracledb::Lob>>(ix)
            .ok()
            .flatten()
            .map(|mut lob| format!("<BLOB, {} bytes>", lob.get_size().unwrap_or(0))),
        "DB_TYPE_CLOB" | "DB_TYPE_NCLOB" => {
            // Small CLOBs may arrive inline as strings; try that first.
            // `get` only borrows, so falling back to `take` is safe.
            match row.get::<Option<String>>(ix) {
                Ok(v) => v,
                Err(_) => row
                    .take::<Option<oracledb::Lob>>(ix)
                    .ok()
                    .flatten()
                    .map(|mut lob| format!("<CLOB, {} chars>", lob.get_size().unwrap_or(0))),
            }
        }
        "DB_TYPE_CURSOR" => Some("<REF CURSOR>".to_string()),
        "DB_TYPE_BFILE" => Some("<BFILE>".to_string()),
        // Strings, CHAR, ROWID/UROWID, and anything else: String conversion
        // covers String + Rowid; unknown types get a placeholder.
        _ => match row.get::<Option<String>>(ix) {
            Ok(v) => v,
            Err(_) => Some(format!("<{db_type}>")),
        },
    }
}

/// Uppercase hex, capped so a 2 MB RAW doesn't freeze the grid.
fn hex_preview(bytes: &[u8]) -> String {
    const CAP: usize = 64;
    let mut s: String = bytes.iter().take(CAP).map(|b| format!("{b:02X}")).collect();
    if bytes.len() > CAP {
        s.push_str(&format!("… ({} bytes)", bytes.len()));
    }
    s
}

fn vector_preview(v: &oracledb::Vector) -> String {
    // `Vector` exposes no public element accessors in beta.3; Debug it is.
    capped_debug(v, 300)
}

/// Debug-format with a char cap so huge values (JSON docs, vectors) don't
/// stall text layout in the grid. Full values come later (cell detail view).
fn capped_debug(v: &impl std::fmt::Debug, cap: usize) -> String {
    let s = format!("{v:?}");
    if s.len() > cap {
        format!("{}…", s.chars().take(cap).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_is_send_for_background_tasks() {
        fn assert_send<T: Send>() {}
        // The UI runs blocking `oracledb` calls on the background executor,
        // which requires all captured state to be `Send`.
        assert_send::<oracledb::Connection>();
        assert_send::<oracledb::Cursor>();
        assert_send::<std::sync::Mutex<OracledbSession>>();
    }

    #[test]
    fn sanitize_strips_editor_terminators() {
        assert_eq!(sanitize_statement("SELECT 1 FROM dual;").unwrap(), "SELECT 1 FROM dual");
        assert_eq!(sanitize_statement("  SELECT 1 FROM dual;;\n").unwrap(), "SELECT 1 FROM dual");
        assert_eq!(sanitize_statement("SELECT 1 FROM dual").unwrap(), "SELECT 1 FROM dual");
        // Semicolons inside literals are preserved.
        assert_eq!(sanitize_statement("SELECT ';' FROM dual;").unwrap(), "SELECT ';' FROM dual");
        assert!(sanitize_statement("   ;  ").is_err());
        assert!(sanitize_statement("").is_err());
    }

    #[test]
    fn describe_statement_detects_client_commands() {
        assert!(is_describe_statement("DESCRIBE emp"));
        assert!(is_describe_statement("  desc scott.emp;  "));
        assert!(is_describe_statement("describe \"MixedCase\";;"));
        assert!(!is_describe_statement("SELECT 1 FROM dual"));
        assert!(!is_describe_statement("describe emp extra"));
        assert!(!is_describe_statement("descending"));
    }

    #[test]
    fn describe_rewrites_to_tab_columns() {
        let q = rewrite_describe("DESCRIBE emp").unwrap();
        assert!(q.contains("FROM all_tab_columns"), "{q}");
        assert!(q.contains("table_name = 'EMP'"), "{q}");
        assert!(!q.contains("owner = "), "{q}");

        let q = rewrite_describe("  desc scott.emp  ").unwrap();
        assert!(q.contains("table_name = 'EMP'"), "{q}");
        assert!(q.contains("owner = 'SCOTT'"), "{q}");

        // Quoted identifiers keep case; unquoted fold to uppercase.
        let q = rewrite_describe("describe \"MixedCase\"").unwrap();
        assert!(q.contains("table_name = 'MixedCase'"), "{q}");

        // Not describe commands: fall through for the server to reject.
        assert_eq!(rewrite_describe("SELECT 1 FROM dual"), None);
        assert_eq!(rewrite_describe("describe"), None);
        assert_eq!(rewrite_describe("describe emp extra"), None);
        assert_eq!(rewrite_describe("descending"), None);
        assert_eq!(rewrite_describe("described emp"), None);
    }
}
