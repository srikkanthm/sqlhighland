//! Crash-safe, permission-aware file writes for app-owned state.
//!
//! App state under `~/.config/sqlhighland` can hold credentials
//! (`connections.toml`), so it is written to a sibling temp file, `fsync`ed,
//! renamed into place, and restricted to the owner (`0700` dirs, `0600`
//! files). User files (`filetab`) use the same crash-safe path but keep
//! whatever permissions the OS would otherwise assign.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Atomically replace `path` with `text`.
///
/// The text is written to a sibling temp file, flushed with `fsync`, then
/// renamed over the target; the parent directory is `fsync`ed too so the
/// rename itself survives a crash. When `secret` is set, the directory is
/// created/updated to `0700` and the file to `0600` — use it for anything
/// that may contain a password or connection details.
pub fn write_atomic(path: &Path, text: &str, secret: bool) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        if secret {
            let _ = restrict(parent, 0o700);
        }
    }
    let tmp = temp_path(path);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        if secret {
            opts.mode(0o600);
        }
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    // A stale temp from a previous crash may predate the mode: re-apply it
    // before any secret lands on disk.
    if secret {
        let _ = restrict(&tmp, 0o600);
    }
    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", tmp.display()))?;
    drop(file);

    std::fs::rename(&tmp, path).with_context(|| format!("moving {}", path.display()))?;
    // Make the rename durable, not just the file contents.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Best-effort tighten of an existing path's permissions (e.g. a config
/// file written by an older, umask-respecting version). Missing paths and
/// non-Unix platforms are ignored.
pub fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Temp path beside the target. Appending (not replacing) the extension
/// keeps `a.sql` and `a.toml` temps distinct.
fn temp_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".tmp");
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_round_trips_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("sqlhighland-fsutil-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("state.toml");
        write_atomic(&path, "hello", true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file must be renamed away");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn secret_writes_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("sqlhighland-fsutil-perm-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("connections.toml");
        write_atomic(&path, "password = \"secret\"", true).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A file written earlier with looser perms is tightened in place.
        restrict(&path, 0o644).unwrap();
        restrict(&path, 0o600).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
