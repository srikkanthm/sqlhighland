use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

pub fn normalize(path: &Path) -> Result<PathBuf> {
    let path = if path.exists() {
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("reading current directory")?
            .join(path)
    };
    Ok(path)
}

pub fn read(path: &Path) -> Result<(String, FileStamp)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading SQL file {}", path.display()))?;
    Ok((text, stamp(path)?))
}

pub fn write(path: &Path, text: &str) -> Result<FileStamp> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Crash-safe, fsynced write. User files keep default (umask) permissions.
    crate::fsutil::write_atomic(path, text, false)?;
    stamp(path)
}

pub fn stamp(path: &Path) -> Result<FileStamp> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    Ok(FileStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

pub fn is_sql(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_extension_is_case_insensitive() {
        assert!(is_sql(Path::new("query.sql")));
        assert!(is_sql(Path::new("query.SQL")));
        assert!(!is_sql(Path::new("query.txt")));
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = std::env::temp_dir().join(format!("sqlhighland-filetab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("query.sql");
        let written = write(&path, "select 1;\n").unwrap();
        let (text, read_stamp) = read(&path).unwrap();
        assert_eq!(text, "select 1;\n");
        assert_eq!(written, read_stamp);
        std::fs::remove_dir_all(dir).ok();
    }
}
