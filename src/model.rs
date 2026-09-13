use serde::{Deserialize, Serialize};

use crate::schema::DbEngine;

/// Oracle connect role (`CONNECT AS`). SYSDEFAULT is a plain login;
/// SYSDBA/SYSOPER set the driver auth mode. (There is no XA auth mode
/// in the driver — XA is a transaction protocol, not a login role.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OracleRole {
    #[default]
    Default,
    Sysdba,
    Sysoper,
}

impl OracleRole {
    pub const ALL: [OracleRole; 3] = [OracleRole::Default, OracleRole::Sysdba, OracleRole::Sysoper];

    pub fn label(self) -> &'static str {
        match self {
            OracleRole::Default => "SYSDEFAULT",
            OracleRole::Sysdba => "SYSDBA",
            OracleRole::Sysoper => "SYSOPER",
        }
    }
}

/// Whether `service_name` is a service name or a SID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ServiceKind {
    #[default]
    ServiceName,
    Sid,
}

impl ServiceKind {
    pub const ALL: [ServiceKind; 2] = [ServiceKind::ServiceName, ServiceKind::Sid];

    pub fn label(self) -> &'static str {
        match self {
            ServiceKind::ServiceName => "Service",
            ServiceKind::Sid => "SID",
        }
    }
}

/// Password handling per connection. `File` is today's behavior
/// (plaintext in connections.toml); `Keychain` moves the secret to the
/// macOS login keychain on save; `Ask` never stores and prompts per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PasswordMode {
    /// Legacy: plaintext in the config file.
    #[default]
    File,
    /// macOS login keychain, keyed by connection id.
    Keychain,
    /// Prompt every time; never persisted.
    Ask,
}

impl PasswordMode {
    pub const ALL: [PasswordMode; 3] = [
        PasswordMode::File,
        PasswordMode::Keychain,
        PasswordMode::Ask,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PasswordMode::File => "File",
            PasswordMode::Keychain => "Keychain",
            PasswordMode::Ask => "Ask every time",
        }
    }
}

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
///
/// `Debug` is implemented by hand to redact `password`: a derived `Debug`
/// would leak the secret through any `{:?}`, panic message, or log line.
#[derive(Clone, Serialize, Deserialize)]
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
    /// Database engine. Missing on older entries; defaults to Oracle.
    /// The day a second engine plugs into `SchemaProvider`, this (plus the
    /// session pool) is what it keys off — nothing else changes shape.
    #[serde(default)]
    pub engine: DbEngine,
    /// Oracle connect role. Missing on older entries; plain login.
    #[serde(default)]
    pub role: OracleRole,
    /// Whether `service_name` is a service name or a SID.
    #[serde(default)]
    pub service_kind: ServiceKind,
    /// TLS via the `tcps://` EZCONNECT scheme. Wallet-based mTLS is a
    /// later step; this toggles transport encryption only.
    #[serde(default)]
    pub ssl: bool,
    /// Password handling. Missing on older entries; legacy file behavior.
    #[serde(default)]
    pub password_mode: PasswordMode,
}

impl ConnectionConfig {
    /// EZCONNECT string: `host:port/service_name`, `host:port:SID` for
    /// SID entries, `tcps://`-prefixed when SSL is on.
    pub fn connect_string(&self) -> String {
        let base = match self.service_kind {
            ServiceKind::ServiceName => {
                format!("{}:{}/{}", self.host, self.port, self.service_name)
            }
            ServiceKind::Sid => format!("{}:{}:{}", self.host, self.port, self.service_name),
        };
        if self.ssl {
            format!("tcps://{base}")
        } else {
            base
        }
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
            engine: DbEngine::default(),
            role: OracleRole::default(),
            service_kind: ServiceKind::default(),
            ssl: false,
            password_mode: PasswordMode::default(),
        }
    }
}

/// Hand-written to keep the password out of `{:?}` output. Everything else
/// mirrors a derived `Debug`.
impl std::fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("service_name", &self.service_name)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("environment", &self.environment)
            .field("engine", &self.engine)
            .field("role", &self.role)
            .field("service_kind", &self.service_kind)
            .field("ssl", &self.ssl)
            .field("password_mode", &self.password_mode)
            .finish()
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
    csv_row_with(cells, ',')
}

/// Same as [`csv_row`] with an explicit delimiter (export settings).
/// Quoting triggers on the delimiter, quotes, newlines, and padded
/// whitespace — whichever delimiter is chosen.
pub fn csv_row_with<'a>(cells: impl IntoIterator<Item = Option<&'a str>>, delim: char) -> String {
    let sep: String = std::iter::once(delim).collect();
    cells
        .into_iter()
        .map(|c| csv_field(c.unwrap_or(""), delim))
        .collect::<Vec<_>>()
        .join(&sep)
}

fn csv_field(value: &str, delim: char) -> String {
    if value.contains([delim, '"', '\n', '\r'])
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
    fn debug_redacts_password() {
        let cfg = ConnectionConfig {
            password: "hunter2-secret".to_string(),
            ..Default::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("hunter2-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        // SavedConfig aggregates connections: the secret must not leak there
        // either.
        let saved = crate::config::SavedConfig {
            connections: vec![cfg],
        };
        let rendered = format!("{saved:?}");
        assert!(!rendered.contains("hunter2-secret"), "{rendered}");
    }

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
        assert_eq!(
            tab_name_from_sql("\n  SELECT * FROM users;\nSELECT 2;", "Untitled 1"),
            "SELECT * FROM users;"
        );
        assert_eq!(tab_name_from_sql("", "Untitled 1"), "Untitled 1");
        assert_eq!(tab_name_from_sql("   \n  ", "Untitled 1"), "Untitled 1");
        let long = "SELECT a_very_long_column_list FROM some_table WHERE x = 1;";
        assert_eq!(
            tab_name_from_sql(long, "U"),
            "SELECT a_very_long_column_li…"
        );
    }

    #[test]
    fn csv_row_formats_for_clipboard() {
        let cells = [Some("42".to_string()), None, Some("plain".to_string())];
        assert_eq!(csv_row(cells.iter().map(|c| c.as_deref())), "42,,plain");
        assert_eq!(csv_row([] as [Option<&str>; 0]), "");
        assert_eq!(csv_row([None]), "");
    }

    #[test]
    fn connect_string_variants() {
        let base = ConnectionConfig {
            host: "db".to_string(),
            port: 1521,
            service_name: "orcl".to_string(),
            ..Default::default()
        };
        assert_eq!(base.connect_string(), "db:1521/orcl");
        let sid = ConnectionConfig {
            service_kind: ServiceKind::Sid,
            ..base.clone()
        };
        assert_eq!(sid.connect_string(), "db:1521:orcl");
        let ssl = ConnectionConfig {
            ssl: true,
            ..base.clone()
        };
        assert_eq!(ssl.connect_string(), "tcps://db:1521/orcl");
        let both = ConnectionConfig {
            service_kind: ServiceKind::Sid,
            ssl: true,
            ..base
        };
        assert_eq!(both.connect_string(), "tcps://db:1521:orcl");
    }

    #[test]
    fn connection_option_labels() {
        assert_eq!(
            OracleRole::ALL.map(OracleRole::label),
            ["SYSDEFAULT", "SYSDBA", "SYSOPER"]
        );
        assert_eq!(
            ServiceKind::ALL.map(ServiceKind::label),
            ["Service", "SID"]
        );
        assert_eq!(
            PasswordMode::ALL.map(PasswordMode::label),
            ["File", "Keychain", "Ask every time"]
        );
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
        let t = QueryResult {
            truncated: true,
            ..r
        };
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
