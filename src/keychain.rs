//! OS keychain storage for connection passwords.
//!
//! Backed by the `keyring` crate, which selects the platform store: macOS
//! Keychain Services, Windows Credential Manager, or Linux Secret Service
//! (D-Bus). One generic-password entry per connection id under our service
//! name. All errors are strings; callers decide how to surface them (status
//! line, dialog, silent skip).

const SERVICE: &str = "SQLHighland";

/// Build the keyring entry for a connection id, mapping store-init failures
/// (no Secret Service, unusable keychain) to a string.
fn entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account).map_err(|e| format!("keychain unavailable: {e}"))
}

/// Stored password for a connection id, if any.
pub fn get(account: &str) -> Result<Option<String>, String> {
    match entry(account)?.get_password() {
        Ok(pw) => Ok(Some(pw)),
        // A missing entry is the normal "never stored" case, not an error.
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keychain lookup failed: {e}")),
    }
}

/// Insert or overwrite the password for a connection id.
pub fn set(account: &str, password: &str) -> Result<(), String> {
    entry(account)?
        .set_password(password)
        .map_err(|e| format!("keychain store failed: {e}"))
}

/// Best-effort removal (connection deleted). Missing entries are fine.
pub fn delete(account: &str) {
    if let Ok(entry) = entry(account) {
        let _ = entry.delete_credential();
    }
}
