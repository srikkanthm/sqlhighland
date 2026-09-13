//! Minimal logging shim.
//!
//! One place to send diagnostics (currently stderr), so call sites don't
//! sprinkle raw `eprintln!`. No external dependency and no global state; when
//! a real backend lands, only [`emit`] changes. Messages carry a level prefix
//! so stderr stays greppable.

use std::fmt::Display;

/// Log a recoverable problem: a theme that didn't register, an object row the
/// schema browser couldn't parse, and similar "degrade, don't crash" cases.
pub fn warn(msg: impl Display) {
    emit("warn", msg);
}

/// Log a failure the user should know about, e.g. a preferences save that did
/// not land. Callers usually surface the same error in the status bar too.
pub fn error(msg: impl Display) {
    emit("error", msg);
}

fn emit(level: &str, msg: impl Display) {
    eprintln!("[{level}] {msg}");
}
