use serde::{Deserialize, Serialize};

/// Deployment environment tag for a connection. Purely visual (no behavior
/// attached): callers map variants to theme colors at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Environment {
    /// No tag; renders nothing. Default so pre-tag entries on disk load clean.
    #[default]
    Untagged,
    Prod,
    Dev,
    Qa,
    Uat,
}

impl Environment {
    /// All selectable values, in dialog order.
    pub const ALL: [Self; 5] = [Self::Untagged, Self::Dev, Self::Qa, Self::Uat, Self::Prod];

    /// Short uppercase label for the tag pill. Untagged has none.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Untagged => None,
            Self::Prod => Some("PROD"),
            Self::Dev => Some("DEV"),
            Self::Qa => Some("QA"),
            Self::Uat => Some("UAT"),
        }
    }
}

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
    /// Environment tag. Missing on pre-tag entries; defaults to untagged.
    #[serde(default)]
    pub environment: Environment,
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
            environment: Environment::default(),
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

/// Derive a tab label from editor text: first non-empty line, trimmed to
/// 28 chars. Falls back to the provided default (e.g. `Untitled 3`).
pub fn tab_name_from_sql(text: &str, fallback: &str) -> String {
    match text.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => {
            const MAX: usize = 28;
            if line.chars().count() > MAX {
                format!("{}…", line.chars().take(MAX).collect::<String>())
            } else {
                line.to_string()
            }
        }
        None => fallback.to_string(),
    }
}

/// Format one result row as CSV for the clipboard (RFC 4180-style).
/// SQL NULL becomes empty; fields containing a comma, quote, newline, or
/// leading/trailing space are quoted with embedded quotes doubled.
/// No headers, no trailing newline.
///
/// Takes borrowed text so both `String`- and `SharedString`-backed rows work
/// without allocating.
pub fn csv_row<'a>(cells: impl IntoIterator<Item = Option<&'a str>>) -> String {
    cells
        .into_iter()
        .map(|c| csv_field(c.unwrap_or("")))
        .collect::<Vec<_>>()
        .join(",")
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r'])
        || value.starts_with([' ', '\t'])
        || value.ends_with([' ', '\t'])
    {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
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
    fn tab_name_from_first_line() {
        assert_eq!(tab_name_from_sql("\n  SELECT * FROM users;\nSELECT 2;", "Untitled 1"), "SELECT * FROM users;");
        assert_eq!(tab_name_from_sql("", "Untitled 1"), "Untitled 1");
        assert_eq!(tab_name_from_sql("   \n  ", "Untitled 1"), "Untitled 1");
        let long = "SELECT a_very_long_column_list FROM some_table WHERE x = 1;";
        assert_eq!(tab_name_from_sql(long, "U"), "SELECT a_very_long_column_li…");
    }

    #[test]
    fn csv_row_formats_for_clipboard() {
        let cells = [
            Some("42".to_string()),
            None,
            Some("plain".to_string()),
        ];
        assert_eq!(csv_row(cells.iter().map(|c| c.as_deref())), "42,,plain");
        assert_eq!(csv_row([] as [Option<&str>; 0]), "");
        assert_eq!(csv_row([None]), "");
    }

    #[test]
    fn csv_row_quotes_special_fields() {
        let cells = [
            Some("a,b".to_string()),
            Some("say \"hi\"".to_string()),
            Some("line1\nline2".to_string()),
            Some(" padded ".to_string()),
        ];
        assert_eq!(
            csv_row(cells.iter().map(|c| c.as_deref())),
            "\"a,b\",\"say \"\"hi\"\"\",\"line1\nline2\",\" padded \""
        );
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

    #[test]
    fn environment_labels() {
        assert_eq!(Environment::Untagged.label(), None);
        assert_eq!(Environment::Prod.label(), Some("PROD"));
        assert_eq!(Environment::Dev.label(), Some("DEV"));
        assert_eq!(Environment::Qa.label(), Some("QA"));
        assert_eq!(Environment::Uat.label(), Some("UAT"));
        assert_eq!(Environment::default(), Environment::Untagged);
        assert_eq!(Environment::ALL.len(), 5);
    }

    #[test]
    fn environment_survives_toml_round_trip() {
        let cfg = ConnectionConfig {
            environment: Environment::Prod,
            ..Default::default()
        };
        let text = toml::to_string(&cfg).unwrap();
        let back: ConnectionConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.environment, Environment::Prod);
    }

    #[test]
    fn legacy_toml_without_environment_loads_untagged() {
        let back: ConnectionConfig = toml::from_str(
            "id = \"x\"\nname = \"n\"\nhost = \"h\"\nport = 1521\nservice_name = \"s\"\nuser = \"u\"\npassword = \"p\"\n",
        )
        .unwrap();
        assert_eq!(back.environment, Environment::Untagged);
    }
}
