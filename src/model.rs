use serde::{Deserialize, Serialize};

/// Connection parameters for a single Oracle database.
/// Password is stored in plaintext in v1 (see PLAN.md debt note).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Stable identity. Empty for pre-id entries on disk; backfilled on load.
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub service_name: String,
    pub user: String,
    pub password: String,
}

impl ConnectionConfig {
    /// EZCONNECT string: `host:port/service_name`.
    pub fn connect_string(&self) -> String {
        format!("{}:{}/{}", self.host, self.port, self.service_name)
    }

    /// Assign a fresh id unless one is already set. Returns whether it changed.
    pub fn ensure_id(&mut self) -> bool {
        if self.id.is_empty() {
            self.id = uuid::Uuid::new_v4().to_string();
            true
        } else {
            false
        }
    }
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "Highland local".to_string(),
            host: "localhost".to_string(),
            port: 1521,
            service_name: "highlandpdb".to_string(),
            user: "system".to_string(),
            password: String::new(),
        }
    }
}

/// One result column: display name + Oracle type name (e.g. `DB_TYPE_NUMBER`).
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub db_type: String,
}

/// Fully materialized query result. Cells are display strings; `None` is SQL
/// NULL. Materializing keeps lifetimes out of the UI layer.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Option<String>>>,
    pub elapsed_ms: u128,
    /// True when more rows existed than `max_rows` allowed.
    pub truncated: bool,
}

impl QueryResult {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// One-line summary for the status bar, e.g. `1,000 rows · 12 ms`.
    pub fn summary(&self, max_rows: usize) -> String {
        let mut s = format!("{} rows · {} ms", self.row_count(), self.elapsed_ms);
        if self.truncated {
            s.push_str(&format!(" · truncated at {max_rows}"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_string_is_ezconnect() {
        let cfg = ConnectionConfig {
            host: "db.example.com".to_string(),
            port: 1522,
            service_name: "ORCLPDB".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.connect_string(), "db.example.com:1522/ORCLPDB");
    }

    #[test]
    fn ensure_id_assigns_unique_stable_ids() {
        let mut a = ConnectionConfig::default();
        let mut b = ConnectionConfig::default();
        assert!(a.ensure_id());
        assert!(b.ensure_id());
        assert!(!a.id.is_empty() && !b.id.is_empty() && a.id != b.id);
        // Existing ids are preserved.
        assert!(!a.ensure_id());
    }

    #[test]
    fn result_summary() {
        let r = QueryResult {
            columns: vec![],
            rows: vec![vec![], vec![]],
            elapsed_ms: 12,
            truncated: false,
        };
        assert_eq!(r.summary(1000), "2 rows · 12 ms");
        let t = QueryResult { truncated: true, ..r };
        assert_eq!(t.summary(1000), "2 rows · 12 ms · truncated at 1000");
    }
}
