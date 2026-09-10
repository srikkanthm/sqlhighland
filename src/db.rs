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
    fn run_query(&mut self, sql: &str, max_rows: usize) -> Result<QueryResult, DbError>;
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
    pub fn start_query(
        &mut self,
        sql: &str,
        first_n: usize,
    ) -> Result<(Vec<ColumnInfo>, FetchPage, u64), DbError> {
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DbError("not connected".to_string()))?;
        let sql = sanitize_statement(sql)?;
        let cursor = conn.query(sql, &[]).map_err(DbError::from)?;

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
    pub fn fetch_more(&mut self, query_id: u64, n: usize) -> Result<FetchPage, DbError> {
        if query_id != self.query_id || self.conn.is_none() {
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

    /// Pull up to `limit` converted rows; probe one extra row to learn
    /// end-of-data without losing it (stashed in `pending`).
    fn pull_locked(&mut self, limit: usize) -> Result<PulledRows, DbError> {
        assert!(limit > 0, "fetch limit must be positive");
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
                Some(Err(e)) => return Err(DbError::from(e)),
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        if !exhausted {
            match cursor.next() {
                Some(Ok(row)) => self.pending = Some(row),
                Some(Err(e)) => return Err(DbError::from(e)),
                None => exhausted = true,
            }
        }
        Ok((rows, exhausted))
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
    fn run_query(&mut self, sql: &str, max_rows: usize) -> Result<QueryResult, DbError> {
        let started = Instant::now();
        let (columns, page, _) = self.start_query(sql, max_rows)?;
        self.cursor = None;
        self.pending = None;
        Ok(QueryResult {
            columns,
            rows: page.rows,
            elapsed_ms: started.elapsed().as_millis(),
            truncated: !page.exhausted,
        })
    }
}

/// Strip editor-style statement terminators (`;`) and reject empty input.
///
/// Only trailing semicolons are removed, so `SELECT ';' FROM dual;`
/// correctly keeps the one inside the string literal.
fn sanitize_statement(sql: &str) -> Result<&str, DbError> {
    let mut s = sql.trim();
    while let Some(stripped) = s.strip_suffix(';') {
        s = stripped.trim_end();
    }
    if s.is_empty() {
        return Err(DbError("empty statement".to_string()));
    }
    Ok(s)
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
            .map(|v| format!("{v:?}")),
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
    const CAP: usize = 300;
    let s = format!("{v:?}");
    if s.len() > CAP {
        format!("{}…", s.chars().take(CAP).collect::<String>())
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
}
