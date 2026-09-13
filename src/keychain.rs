//! OS keychain storage for connection passwords.
//!
//! The public surface is `get`/`set`/`delete`; each platform provides its own
//! backend. Today that is the macOS login keychain (via `security-framework`,
//! Apple-only). Windows (Credential Manager) and Linux (Secret Service) can be
//! added behind the same calls — until then those platforms compile a stub that
//! reports "unsupported", so the app still builds and runs there.
//!
//! All errors are strings; callers decide how to surface them (status line,
//! dialog, silent skip).

#[cfg(target_vendor = "apple")]
mod platform {
    use security_framework::os::macos::keychain::SecKeychain;

    const SERVICE: &str = "SQLHighland";

    fn default_keychain() -> Result<SecKeychain, String> {
        SecKeychain::default().map_err(|e| format!("keychain unavailable: {e}"))
    }

    /// Stored password for a connection id, if any.
    pub fn get(account: &str) -> Result<Option<String>, String> {
        let kc = default_keychain()?;
        match kc.find_generic_password(SERVICE, account) {
            Ok((pw, _)) => String::from_utf8(pw.as_ref().to_vec())
                .map(Some)
                .map_err(|_| "keychain entry is not valid UTF-8".to_string()),
            Err(e) => {
                // errSecItemNotFound (-25300) just means "never stored".
                if e.code() == -25300 {
                    Ok(None)
                } else {
                    Err(format!("keychain lookup failed: {e}"))
                }
            }
        }
    }

    /// Insert or overwrite the password for a connection id.
    pub fn set(account: &str, password: &str) -> Result<(), String> {
        let kc = default_keychain()?;
        match kc.find_generic_password(SERVICE, account) {
            Ok((_, mut item)) => item
                .set_password(password.as_bytes())
                .map_err(|e| format!("keychain update failed: {e}")),
            Err(_) => kc
                .add_generic_password(SERVICE, account, password.as_bytes())
                .map_err(|e| format!("keychain store failed: {e}")),
        }
    }

    /// Best-effort removal (connection deleted). Missing entries are fine.
    pub fn delete(account: &str) {
        if let Ok(kc) = default_keychain() {
            if let Ok((_, item)) = kc.find_generic_password(SERVICE, account) {
                item.delete();
            }
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
mod platform {
    /// No backend yet: behave as "nothing stored" so callers prompt instead of
    /// hard-failing when a connection is in Keychain mode.
    pub fn get(_account: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// No backend yet; surface it if the user picks Keychain mode.
    pub fn set(_account: &str, _password: &str) -> Result<(), String> {
        Err("OS keychain is not supported on this platform yet".to_string())
    }

    /// Best-effort removal; nothing to do without a backend.
    pub fn delete(_account: &str) {}
}

pub use platform::{delete, get, set};
