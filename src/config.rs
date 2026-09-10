//! Saved connections on disk (TOML). Plaintext passwords in v1 — see PLAN.md.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::model::ConnectionConfig;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

impl SavedConfig {
    pub fn default_path() -> anyhow::Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("sqlhighland")
            .join("connections.toml"))
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text).context("parsing saved connections")?;
        // Backfill stable ids for entries written before ids existed.
        for conn in &mut cfg.connections {
            conn.ensure_id();
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("encoding saved connections")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let path = std::env::temp_dir().join(format!("sqlhighland-missing-{}.toml", std::process::id()));
        let loaded = SavedConfig::load(&path).unwrap();
        assert!(loaded.connections.is_empty());
    }
}
