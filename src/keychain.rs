//! macOS login-keychain storage for connection passwords.
//!
//! GUI-free thin wrapper over `security-framework`: one generic-password
//! entry per connection id under our service name. All errors are strings;
//! callers decide how to surface them (status line, dialog, silent skip).

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
