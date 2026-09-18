//! Saved connections on disk (TOML). The legacy `File` password mode is
//! plaintext — see `docs/HISTORY.md`.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::model::ConnectionConfig;

/// Write-then-rename so a crash mid-write never leaves a truncated file
/// behind (a truncated `tabs.toml` used to read as "no tabs", and the next
/// persist overwrote it — permanent tab loss from one bad shutdown).
///
/// These files may hold connection passwords, so they are written owner-only
/// (`0600`, directory `0700`) and fsynced (see [`crate::fsutil`]).
fn write_atomic(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    crate::fsutil::write_atomic(path, text, true)
}

/// Bring an existing config file (and its directory) down to owner-only
/// permissions. Older builds wrote `connections.toml` with the process
/// umask, leaving plaintext passwords world-readable on multi-user machines.
fn tighten_owned(path: &std::path::Path) {
    if path.exists() {
        let _ = crate::fsutil::restrict(path, 0o600);
    }
    if let Some(parent) = path.parent() {
        if parent.exists() {
            let _ = crate::fsutil::restrict(parent, 0o700);
        }
    }
}

/// Base config dir. Overridable via `SQLHIGHLAND_CONFIG_DIR` (tests).
///
/// Windows uses `%APPDATA%\sqlhighland`; everywhere else `~/.config/sqlhighland`.
fn base_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("SQLHIGHLAND_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    // Windows-only in practice; checked unconditionally so the branch compiles
    // and is exercised on every platform.
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(appdata).join("sqlhighland"));
    }
    let home = crate::fsutil::home_dir()
        .context("could not determine the home directory (HOME/USERPROFILE unset)")?;
    Ok(home.join(".config").join("sqlhighland"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

/// One persisted usage row: how often `label` was used on a connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageRow {
    conn: String,
    label: String,
    count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UsageStats {
    #[serde(default)]
    rows: Vec<UsageRow>,
}

/// Load the persisted usage counts used by completion ranking. Missing or
/// unreadable/corrupt files load as empty (ranking just starts fresh).
pub fn load_usage() -> std::collections::HashMap<(String, String), u64> {
    let Ok(path) = base_dir().map(|d| d.join("usage.toml")) else {
        return std::collections::HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return std::collections::HashMap::new();
    };
    let stats: UsageStats = toml::from_str(&text).unwrap_or_default();
    stats
        .rows
        .into_iter()
        .map(|r| ((r.conn, r.label), r.count))
        .collect()
}

/// Persist usage counts. Sorted for a stable file (and diff-friendly).
pub fn save_usage(map: &std::collections::HashMap<(String, String), u64>) -> anyhow::Result<()> {
    let path = base_dir()?.join("usage.toml");
    let mut rows: Vec<UsageRow> = map
        .iter()
        .map(|((conn, label), count)| UsageRow {
            conn: conn.clone(),
            label: label.clone(),
            count: *count,
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.conn.as_str(), a.label.as_str()).cmp(&(b.conn.as_str(), b.label.as_str()))
    });
    let text = toml::to_string_pretty(&UsageStats { rows }).context("encoding usage stats")?;
    crate::fsutil::write_atomic(&path, &text, false)
}

impl SavedConfig {
    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(base_dir()?.join("connections.toml"))
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        tighten_owned(path);
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text).context("parsing saved connections")?;
        // Backfill stable ids for entries written before ids existed.
        for conn in &mut cfg.connections {
            conn.ensure_id();
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self).context("encoding saved connections")?;
        write_atomic(path, &text)
    }
}

/// One open query tab: identity, connection binding, and optional external
/// SQL path. Tabs without a path keep editor text in a sibling draft file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTab {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabsManifest {
    /// Id of the tab that was active on last quit (query tabs only; a focused
    /// viewer falls back to the nearest query tab). Declared before `tabs` so
    /// TOML emits it above the `[[tabs]]` array — a bare key after an
    /// array-of-tables would be absorbed into that table.
    #[serde(default)]
    pub active_tab: Option<String>,
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
}

impl TabsManifest {
    pub fn manifest_path() -> anyhow::Result<PathBuf> {
        Ok(base_dir()?.join("tabs.toml"))
    }

    pub fn tabs_dir() -> anyhow::Result<PathBuf> {
        Ok(base_dir()?.join("tabs"))
    }

    /// True when `id` is safe to use as a single path component for a draft.
    /// Generated ids are UUIDs and adopted orphan stems come from real
    /// directory entries, but `tabs.toml` is user-editable and could carry
    /// `../` or a Windows drive spec — reject separators and `.`/`..`.
    fn is_safe_id(id: &str) -> bool {
        !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', ':', '\0'])
    }

    pub fn draft_path(tab_id: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(Self::is_safe_id(tab_id), "unsafe tab id {tab_id:?}");
        Ok(Self::tabs_dir()?.join(format!("{tab_id}.sql")))
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::manifest_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        tighten_owned(&path);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut manifest: Self = toml::from_str(&text).context("parsing tabs manifest")?;
        // Rekey unsafe ids (hand-edited or corrupt): their draft is unreadable
        // by design, so the tab restores empty rather than escaping the dir.
        for tab in &mut manifest.tabs {
            if !Self::is_safe_id(&tab.id) {
                tab.id = uuid::Uuid::new_v4().to_string();
            }
        }
        Ok(manifest)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::manifest_path()?;
        let text = toml::to_string_pretty(self).context("encoding tabs manifest")?;
        write_atomic(&path, &text)
    }

    /// Load, preserving evidence on corruption: a present-but-unparseable
    /// manifest is renamed aside (`tabs.corrupt-<epoch>.toml`) instead of
    /// silently defaulting (the old `unwrap_or_default` + next persist
    /// overwrote it — permanent loss). Missing file → default, as before.
    pub fn load_preserving() -> Self {
        let path = match Self::manifest_path() {
            Ok(p) => p,
            Err(_) => return Self::default(),
        };
        if !path.exists() {
            return Self::default();
        }
        match Self::load() {
            Ok(m) => m,
            Err(_) => {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = path.with_extension(format!("corrupt-{secs}.toml"));
                if std::fs::rename(&path, &backup).is_ok() {
                    crate::logging::warn(format!(
                        "unparseable tabs manifest preserved as {}",
                        backup.display()
                    ));
                }
                Self::default()
            }
        }
    }

    /// Drafts on disk with no manifest entry (`<id>.sql` files): leftovers
    /// of a lost manifest, adopted as recovered tabs on next launch.
    /// Sorted by filename for deterministic order.
    pub fn orphan_drafts(known_ids: &std::collections::HashSet<String>) -> Vec<(String, String)> {
        let dir = match Self::tabs_dir() {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut orphans = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|x| x.to_str()) != Some("sql") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if !known_ids.contains(stem) {
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        orphans.push((stem.to_string(), text));
                    }
                }
            }
        }
        orphans.sort();
        orphans
    }

    pub fn read_draft(tab_id: &str) -> String {
        Self::draft_path(tab_id)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    }

    pub fn write_draft(tab_id: &str, text: &str) -> anyhow::Result<()> {
        let path = Self::draft_path(tab_id)?;
        write_atomic(&path, text)
    }

    pub fn delete_draft(tab_id: &str) {
        if let Ok(path) = Self::draft_path(tab_id) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Theme selection: either one of the flat registry names below, or
/// [`SYSTEM_THEME`] to follow the OS (resolving to the stock Default pair).
pub const SYSTEM_THEME: &str = "System";

/// Every selectable theme: the stock Default pair plus our bundled families.
/// Catppuccin ships all four variants; Ayu adds Mirage; the rest ship
/// light + dark.
pub const THEME_LIST: [&str; 16] = [
    SYSTEM_THEME,
    "Default Light",
    "Default Dark",
    "Nord Light",
    "Nord Dark",
    "Catppuccin Latte",
    "Catppuccin Frappé",
    "Catppuccin Macchiato",
    "Catppuccin Mocha",
    "Solarized Light",
    "Solarized Dark",
    "Gruvbox Dark",
    "Gruvbox Light",
    "Ayu Dark",
    "Ayu Light",
    "Ayu Mirage",
];

/// Monospace font families offered in Settings → Editor → Font. The empty
/// value follows the active theme's font. Filterable in the dropdown.
pub const FONT_FAMILIES: &[(&str, &str)] = &[
    ("Theme default", ""),
    ("SF Mono", "SF Mono"),
    ("Menlo", "Menlo"),
    ("Monaco", "Monaco"),
    ("JetBrains Mono", "JetBrains Mono"),
    ("Fira Code", "Fira Code"),
    ("Hack", "Hack"),
    ("IBM Plex Mono", "IBM Plex Mono"),
    ("Source Code Pro", "Source Code Pro"),
    ("Cascadia Code", "Cascadia Code"),
    ("Victor Mono", "Victor Mono"),
    ("Roboto Mono", "Roboto Mono"),
    ("PT Mono", "PT Mono"),
    ("Courier New", "Courier New"),
];

/// True when `family` may be offered as an editor font. The empty sentinel
/// ("Theme default") always may; any other name must be reported by the text
/// system's installed list — which includes families registered with
/// `add_fonts`, so the embedded fonts pass on every machine. Best-effort: the
/// list also carries GPUI's fallback-stack names, so a match means
/// "resolvable", not "a real file exists".
pub fn font_family_available(family: &str, installed: &[String]) -> bool {
    family.is_empty() || installed.iter().any(|name| name == family)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// Exact registry theme name, or [`SYSTEM_THEME`]. Empty (or unknown)
    /// normalizes to system-follow on load.
    #[serde(default)]
    pub theme: String,
    /// Suggestion popup behavior. Missing (pre-autocomplete files) → Auto.
    #[serde(default)]
    pub completion: CompleteMode,
    /// Include SYS/SYSTEM/etc. objects in suggestions. Default hidden.
    #[serde(default)]
    pub show_system_schemas: bool,
    /// Global interface density. Default Compact.
    #[serde(default)]
    pub ui_density: UiDensity,
    /// Show table/column detail cards when hovering the editor. Off by
    /// default (the cards are informative but can be noisy).
    #[serde(default)]
    pub hover_details: bool,
    /// Live SQL syntax/structure checking in the editor. Default on.
    #[serde(default = "default_true")]
    pub sql_diagnostics: bool,
    /// How much of the buffer the syntax check parses. Default whole buffer.
    #[serde(default)]
    pub sql_check_scope: SqlCheckScope,
    /// Results-grid row cap (exports stay uncapped). Default 100k; `0`
    /// means unlimited (page until the cursor is exhausted).
    #[serde(default = "default_result_cap")]
    pub result_cap: usize,
    /// Rows fetched per page for the grid: the initial load and each scroll
    /// fetch. Clamped to [`FETCH_SIZE_MIN`]..=[`FETCH_SIZE_MAX`].
    #[serde(default = "default_fetch_size")]
    pub fetch_size: usize,
    /// Rows fetched per page when draining an export. Independent of the
    /// grid's [`fetch_size`](Self::fetch_size): a large page keeps a big
    /// export to few round trips. Clamped like the grid size.
    #[serde(default = "default_export_fetch_size")]
    pub export_fetch_size: usize,
    /// Results-grid row height in points (compactness). Clamped to
    /// [`GRID_ROW_HEIGHT_MIN`]..=[`GRID_ROW_HEIGHT_MAX`] on load/use.
    #[serde(default = "default_grid_row_height")]
    pub grid_row_height: u32,
    /// CSV delimiter for file exports ("," ";" tab "|" presets).
    #[serde(default = "default_csv_delimiter")]
    pub csv_delimiter: String,
    /// Header row in CSV file exports. Default on.
    #[serde(default = "default_true")]
    pub csv_header: bool,
    /// Editor font family. Empty = follow the active theme.
    #[serde(default)]
    pub font_family: String,
    /// Editor font size in points. Explicit (editors keep size across
    /// themes); stepper-clamped 10..24 in Settings.
    #[serde(default = "default_font_size")]
    pub font_size: u32,
    /// Query timeout in seconds (0 = unlimited). Applied per call on
    /// every round trip; missing (older files) → 60.
    #[serde(default = "default_query_timeout")]
    pub query_timeout_secs: u64,
    /// Suggestions/dictionary cache TTL in seconds. 0 = never expire (the
    /// cache refreshes only on reconnect or a manual refresh). Default 0.
    #[serde(default = "default_metadata_ttl")]
    pub metadata_ttl_secs: u64,
    // --- Legacy family+mode matrix (pre-flat themes). Still parsed (via
    // the original key names) so old files migrate instead of resetting;
    // never written back. ---
    #[serde(default, skip_serializing, alias = "theme_family")]
    legacy_family: LegacyFamily,
    #[serde(default, skip_serializing, alias = "theme_mode")]
    legacy_mode: LegacyMode,
    #[serde(default, skip_serializing, alias = "catppuccin_dark")]
    legacy_catppuccin_dark: LegacyCatppuccinDark,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: SYSTEM_THEME.to_string(),
            completion: CompleteMode::default(),
            show_system_schemas: false,
            ui_density: UiDensity::default(),
            hover_details: false,
            sql_diagnostics: true,
            sql_check_scope: SqlCheckScope::default(),
            result_cap: default_result_cap(),
            fetch_size: default_fetch_size(),
            export_fetch_size: default_export_fetch_size(),
            grid_row_height: default_grid_row_height(),
            csv_delimiter: default_csv_delimiter(),
            csv_header: default_true(),
            font_family: String::new(),
            font_size: default_font_size(),
            query_timeout_secs: default_query_timeout(),
            metadata_ttl_secs: default_metadata_ttl(),
            legacy_family: LegacyFamily::default(),
            legacy_mode: LegacyMode::default(),
            legacy_catppuccin_dark: LegacyCatppuccinDark::default(),
        }
    }
}

fn default_result_cap() -> usize {
    100_000
}

/// Bounds for the results fetch size (rows per page). A page too small makes
/// paging chatty; one too large stalls the first paint and buffers more.
pub const FETCH_SIZE_MIN: usize = 1;
pub const FETCH_SIZE_MAX: usize = 10_000;

fn default_fetch_size() -> usize {
    50
}

fn default_export_fetch_size() -> usize {
    1000
}

/// Clamp a fetch size into the supported range (blank/0 → default).
pub fn clamp_fetch_size(n: usize) -> usize {
    if n == 0 {
        default_fetch_size()
    } else {
        n.clamp(FETCH_SIZE_MIN, FETCH_SIZE_MAX)
    }
}

/// Clamp an export fetch size into the supported range (blank/0 → default).
pub fn clamp_export_fetch_size(n: usize) -> usize {
    if n == 0 {
        default_export_fetch_size()
    } else {
        n.clamp(FETCH_SIZE_MIN, FETCH_SIZE_MAX)
    }
}

/// Compactness bounds for the results grid (row height in points).
pub const GRID_ROW_HEIGHT_MIN: u32 = 18;
pub const GRID_ROW_HEIGHT_MAX: u32 = 34;

fn default_grid_row_height() -> u32 {
    22
}

fn default_csv_delimiter() -> String {
    ",".to_string()
}

fn default_true() -> bool {
    true
}

fn default_font_size() -> u32 {
    13
}

fn default_query_timeout() -> u64 {
    60
}

/// Suggestions cache TTL default: 0 = never expire while connected.
fn default_metadata_ttl() -> u64 {
    0
}

/// Suggestion popup behavior for the query editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CompleteMode {
    /// Popup automatically while typing (2+ chars, `.` forces columns).
    #[default]
    Auto,
    /// Popup only on the manual shortcut (ctrl-space).
    Manual,
}

/// Global interface density: how tightly tabs, toolbars, the sidebar, the
/// status bar, and padding are sized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UiDensity {
    /// Tight sizing across the shell (the default).
    #[default]
    Compact,
    /// The roomier legacy sizing.
    Comfortable,
}

impl UiDensity {
    pub fn label(self) -> &'static str {
        match self {
            UiDensity::Compact => "Compact",
            UiDensity::Comfortable => "Comfortable",
        }
    }
}

/// How much of the buffer the live SQL syntax check parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SqlCheckScope {
    /// Parse the whole buffer (default).
    #[default]
    WholeBuffer,
    /// Parse only the statement under the cursor.
    Statement,
}

impl SqlCheckScope {
    pub fn label(self) -> &'static str {
        match self {
            SqlCheckScope::WholeBuffer => "Whole buffer",
            SqlCheckScope::Statement => "Current statement",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum LegacyFamily {
    #[default]
    Default,
    Nord,
    Catppuccin,
    Solarized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum LegacyCatppuccinDark {
    Frappé,
    Macchiato,
    #[default]
    Mocha,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum LegacyMode {
    #[default]
    System,
    Light,
    Dark,
}

impl Preferences {
    /// Effective theme name, migrating legacy files: an explicit `theme`
    /// wins; otherwise the old family+mode matrix maps to its concrete
    /// variant (System stays system-follow).
    pub fn theme_name(&self) -> String {
        if !self.theme.is_empty() && THEME_LIST.contains(&self.theme.as_str()) {
            return self.theme.clone();
        }
        if !self.theme.is_empty() {
            // Unknown name (e.g. hand-edited): fall back to system-follow
            // rather than a stale or missing theme.
            return SYSTEM_THEME.to_string();
        }
        let (light, dark) = match self.legacy_family {
            LegacyFamily::Default => ("Default Light", "Default Dark"),
            LegacyFamily::Nord => ("Nord Light", "Nord Dark"),
            LegacyFamily::Catppuccin => (
                "Catppuccin Latte",
                match self.legacy_catppuccin_dark {
                    LegacyCatppuccinDark::Frappé => "Catppuccin Frappé",
                    LegacyCatppuccinDark::Macchiato => "Catppuccin Macchiato",
                    LegacyCatppuccinDark::Mocha => "Catppuccin Mocha",
                },
            ),
            LegacyFamily::Solarized => ("Solarized Light", "Solarized Dark"),
        };
        match self.legacy_mode {
            LegacyMode::System => SYSTEM_THEME.to_string(),
            LegacyMode::Light => light.to_string(),
            LegacyMode::Dark => dark.to_string(),
        }
    }

    /// Suggestions/dictionary cache TTL, or `None` when the cache never
    /// expires (the setting is 0).
    pub fn metadata_ttl(&self) -> Option<std::time::Duration> {
        if self.metadata_ttl_secs == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(self.metadata_ttl_secs))
        }
    }
}

impl Preferences {
    pub fn path() -> anyhow::Result<PathBuf> {
        Ok(base_dir()?.join("preferences.toml"))
    }

    pub fn load() -> Self {
        Self::path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path()?;
        let text = toml::to_string_pretty(self).context("encoding preferences")?;
        write_atomic(&path, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// `SQLHIGHLAND_CONFIG_DIR` is process-global: tests using it must hold
    /// this lock to avoid racing each other.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("sqlhighland-test-{}", std::process::id()));
        let path = dir.join("connections.toml");
        let cfg = SavedConfig {
            connections: vec![ConnectionConfig::default()],
        };
        cfg.save(&path).unwrap();
        let loaded = SavedConfig::load(&path).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].port, 1521);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_gives_empty_config() {
        let path =
            std::env::temp_dir().join(format!("sqlhighland-missing-{}.toml", std::process::id()));
        let loaded = SavedConfig::load(&path).unwrap();
        assert!(loaded.connections.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn load_tightens_loose_connections_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("sqlhighland-tighten-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connections.toml");
        std::fs::write(&path, "[[connections]]\nid = \"c1\"\nname = \"n\"\nhost = \"h\"\nport = 1521\nservice_name = \"s\"\nuser = \"u\"\npassword = \"p\"\n").unwrap();
        crate::fsutil::restrict(&path, 0o644).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        // Loading migrates the file to owner-only.
        let _ = SavedConfig::load(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preferences_round_trip_with_env_dir() {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("sqlhighland-prefs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        // Missing file → system-follow.
        assert_eq!(Preferences::load(), Preferences::default());
        assert_eq!(Preferences::load().theme_name(), SYSTEM_THEME);
        // Query timeout defaults to 60s (0 = unlimited when set).
        assert_eq!(Preferences::load().query_timeout_secs, 60);
        let prefs = Preferences {
            theme: "Catppuccin Macchiato".to_string(),
            ..Default::default()
        };
        prefs.save().unwrap();
        assert_eq!(Preferences::load(), prefs);
        assert_eq!(Preferences::load().theme_name(), "Catppuccin Macchiato");
        // Unknown names fall back to system-follow, never error.
        std::fs::write(Preferences::path().unwrap(), "theme = \"Nope\"\n").unwrap();
        assert_eq!(Preferences::load().theme_name(), SYSTEM_THEME);
        // Legacy family+mode files migrate to their concrete variant.
        std::fs::write(
            Preferences::path().unwrap(),
            "theme_family = \"Nord\"\ntheme_mode = \"Light\"\n",
        )
        .unwrap();
        assert_eq!(Preferences::load().theme_name(), "Nord Light");
        std::fs::write(
            Preferences::path().unwrap(),
            "theme_family = \"Catppuccin\"\ntheme_mode = \"Dark\"\ncatppuccin_dark = \"Frappé\"\n",
        )
        .unwrap();
        assert_eq!(Preferences::load().theme_name(), "Catppuccin Frappé");
        std::fs::write(
            Preferences::path().unwrap(),
            "theme_family = \"Solarized\"\ntheme_mode = \"System\"\n",
        )
        .unwrap();
        assert_eq!(Preferences::load().theme_name(), SYSTEM_THEME);

        unsafe { std::env::remove_var("SQLHIGHLAND_CONFIG_DIR") };
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_pref_defaults() {
        // Fresh defaults: 100k grid cap, comma CSVs with headers, theme
        // font at 13pt. Missing keys in old files resolve the same way.
        let p = Preferences::default();
        assert_eq!(p.result_cap, 100_000);
        assert_eq!(p.fetch_size, 50);
        assert_eq!(p.export_fetch_size, 1000);
        assert_eq!(clamp_fetch_size(0), 50);
        assert_eq!(clamp_export_fetch_size(0), 1000);
        assert_eq!(p.csv_delimiter, ",");
        assert!(p.csv_header);
        assert_eq!(p.font_family, "");
        assert_eq!(p.font_size, 13);
        assert_eq!(p.query_timeout_secs, 60);
        assert!(!p.hover_details);
        assert!(p.sql_diagnostics);
        assert_eq!(p.sql_check_scope, SqlCheckScope::WholeBuffer);
        assert_eq!(p.grid_row_height, 22);
        // Suggestions cache never expires by default.
        assert_eq!(p.metadata_ttl_secs, 0);
        assert!(p.metadata_ttl().is_none());
        assert_eq!(p.ui_density, UiDensity::Compact);
        let p: Preferences = toml::from_str("theme = \"Nord Dark\"\n").unwrap();
        assert_eq!(p.result_cap, 100_000);
        assert_eq!(p.csv_delimiter, ",");
        assert!(p.csv_header);
        assert_eq!(p.font_size, 13);
    }

    #[test]
    fn old_connection_entries_default_new_options() {
        // Pre-role/SSL/password-mode files load with safe defaults: plain
        // login, service name, no TLS, legacy file password.
        let dir = std::env::temp_dir().join(format!("sqlhighland-oldconn-{}", std::process::id()));
        let path = dir.join("connections.toml");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "[[connections]]\nid = \"c1\"\nname = \"old\"\nhost = \"h\"\nport = 1521\nservice_name = \"s\"\nuser = \"u\"\npassword = \"p\"\n",
        )
        .unwrap();
        let loaded = SavedConfig::load(&path).unwrap();
        let c = &loaded.connections[0];
        assert_eq!(c.role, crate::model::OracleRole::Default);
        assert_eq!(c.service_kind, crate::model::ServiceKind::ServiceName);
        assert!(!c.ssl);
        assert_eq!(c.password_mode, crate::model::PasswordMode::File);
        assert_eq!(c.engine, crate::schema::DbEngine::Oracle);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn usage_stats_round_trip() {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("sqlhighland-usage-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        let mut map = std::collections::HashMap::new();
        map.insert(("c1".to_string(), "EMP".to_string()), 7);
        map.insert(("c1".to_string(), "DEPT".to_string()), 2);
        save_usage(&map).unwrap();
        assert_eq!(load_usage(), map);

        // Missing file loads empty.
        std::fs::remove_file(dir.join("usage.toml")).unwrap();
        assert!(load_usage().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tabs_manifest_and_drafts_round_trip() {
        let _guard = env_lock();
        // Single test owns SQLHIGHLAND_CONFIG_DIR (process-global env).
        let dir = std::env::temp_dir().join(format!("sqlhighland-tabs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        let manifest = TabsManifest {
            active_tab: Some("tab-2".to_string()),
            tabs: vec![
                SavedTab {
                    id: "tab-1".to_string(),
                    name: "Users".to_string(),
                    connection_id: Some("conn-1".to_string()),
                    path: Some(PathBuf::from("/tmp/users.sql")),
                },
                SavedTab {
                    id: "tab-2".to_string(),
                    name: "Untitled 2".to_string(),
                    connection_id: None,
                    path: None,
                },
            ],
        };
        manifest.save().unwrap();
        TabsManifest::write_draft("tab-1", "SELECT 1;").unwrap();

        let loaded = TabsManifest::load().unwrap();
        assert_eq!(loaded.tabs.len(), 2);
        assert_eq!(loaded.active_tab.as_deref(), Some("tab-2"));
        assert_eq!(loaded.tabs[0].connection_id.as_deref(), Some("conn-1"));
        assert_eq!(
            loaded.tabs[0].path.as_deref(),
            Some(std::path::Path::new("/tmp/users.sql"))
        );
        assert_eq!(TabsManifest::read_draft("tab-1"), "SELECT 1;");
        assert_eq!(TabsManifest::read_draft("missing"), "");

        TabsManifest::delete_draft("tab-1");
        assert_eq!(TabsManifest::read_draft("tab-1"), "");

        unsafe { std::env::remove_var("SQLHIGHLAND_CONFIG_DIR") };
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_manifest_is_backed_up_not_lost() {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("sqlhighland-corrupt-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        // Garbage on disk: load_preserving renames it aside, returns default.
        let path = TabsManifest::manifest_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[[not toml").unwrap();
        let loaded = TabsManifest::load_preserving();
        assert!(loaded.tabs.is_empty());
        assert!(!path.exists(), "corrupt file moved aside");
        let backup: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("tabs.corrupt-"))
            .collect();
        assert_eq!(backup.len(), 1, "evidence preserved");

        unsafe { std::env::remove_var("SQLHIGHLAND_CONFIG_DIR") };
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsafe_tab_ids_are_rejected_and_rekeyed() {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("sqlhighland-badid-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        // draft_path refuses traversal / separators.
        assert!(TabsManifest::draft_path("../escape").is_err());
        assert!(TabsManifest::draft_path("a/b").is_err());
        assert!(TabsManifest::draft_path("..").is_err());
        assert!(TabsManifest::draft_path("ok-id").is_ok());

        // A manifest carrying a traversal id is rekeyed on load.
        let manifest = TabsManifest {
            active_tab: None,
            tabs: vec![SavedTab {
                id: "../../etc/passwd".to_string(),
                name: "evil".to_string(),
                connection_id: None,
                path: None,
            }],
        };
        manifest.save().unwrap();
        let loaded = TabsManifest::load().unwrap();
        assert_eq!(loaded.tabs.len(), 1);
        assert_ne!(loaded.tabs[0].id, "../../etc/passwd");
        assert!(TabsManifest::is_safe_id(&loaded.tabs[0].id));

        unsafe { std::env::remove_var("SQLHIGHLAND_CONFIG_DIR") };
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn orphan_drafts_are_found_sorted() {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("sqlhighland-orphan-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        TabsManifest::write_draft("zzz", "SELECT 3;").unwrap();
        TabsManifest::write_draft("aaa", "SELECT 1;").unwrap();
        // Non-sql files and known ids are ignored.
        std::fs::write(TabsManifest::tabs_dir().unwrap().join("note.txt"), "x").unwrap();
        let mut known = std::collections::HashSet::new();
        known.insert("aaa".to_string());
        let orphans = TabsManifest::orphan_drafts(&known);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].0, "zzz");
        assert_eq!(orphans[0].1, "SELECT 3;");

        unsafe { std::env::remove_var("SQLHIGHLAND_CONFIG_DIR") };
        std::fs::remove_dir_all(&dir).ok();
    }
}
