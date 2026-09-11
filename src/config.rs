//! Saved connections on disk (TOML). Plaintext passwords in v1 — see PLAN.md.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::model::ConnectionConfig;

/// Write-then-rename so a crash mid-write never leaves a truncated file
/// behind (a truncated `tabs.toml` used to read as "no tabs", and the next
/// persist overwrote it — permanent tab loss from one bad shutdown).
fn write_atomic(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("moving {}", path.display()))?;
    Ok(())
}

/// Base config dir. Overridable via `SQLHIGHLAND_CONFIG_DIR` (tests).
fn base_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("SQLHIGHLAND_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join("sqlhighland"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

impl SavedConfig {
    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(base_dir()?.join("connections.toml"))
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
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

    pub fn draft_path(tab_id: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::tabs_dir()?.join(format!("{tab_id}.sql")))
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::manifest_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).context("parsing tabs manifest")
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
                let _ = std::fs::rename(&path, &backup);
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
/// Catppuccin ships all four variants; the rest ship light + dark.
pub const THEME_LIST: [&str; 11] = [
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
];

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
            legacy_family: LegacyFamily::default(),
            legacy_mode: LegacyMode::default(),
            legacy_catppuccin_dark: LegacyCatppuccinDark::default(),
        }
    }
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

    #[test]
    fn preferences_round_trip_with_env_dir() {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("sqlhighland-prefs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        // Missing file → system-follow.
        assert_eq!(Preferences::load(), Preferences::default());
        assert_eq!(Preferences::load().theme_name(), SYSTEM_THEME);
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
    fn tabs_manifest_and_drafts_round_trip() {
        let _guard = env_lock();
        // Single test owns SQLHIGHLAND_CONFIG_DIR (process-global env).
        let dir = std::env::temp_dir().join(format!("sqlhighland-tabs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        unsafe { std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir) };

        let manifest = TabsManifest {
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
