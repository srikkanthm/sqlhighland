//! Test-only helpers shared across the crate's unit tests.
//!
//! `SQLHIGHLAND_CONFIG_DIR` is process-global, so any unit test that reads or
//! writes app state under it (config, metadata caches) must serialize through
//! [`config_dir_lock`] — otherwise one test's `set_var` redirects another's
//! lookups mid-run. Integration-test binaries have their own process and
//! their own locks; this covers the `src` unit tests only.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialize unit tests that touch the process-global config dir.
pub(crate) fn config_dir_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
