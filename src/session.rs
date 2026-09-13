//! One live session per saved connection, shared by tabs.
//!
//! Sessions are created lazily and keyed by connection id. Two tabs bound to
//! the same connection share its session (and its single open cursor — the
//! generation guards in `db.rs` keep paging honest when they interleave).
//!
//! The pool is engine-agnostic: it stores `Box<dyn DbClient>` and constructs
//! the concrete session for a connection's [`DbEngine`](crate::schema::DbEngine)
//! lazily, so a second engine plugs in by adding an arm to [`new_session`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::db::{DbClient, OracledbSession, SharedSession};
use crate::schema::DbEngine;

/// Poison-tolerant guard for session-adjacent locks (session, grid data,
/// metadata cache): a panic while holding one must degrade to stale data
/// on next access, never a crash loop — `lock().expect(..)` would panic
/// on every later touch until restart.
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Construct the session for an engine. Add a match arm per
/// [`DbEngine`] variant (the compiler enforces it).
fn new_session(engine: DbEngine) -> Box<dyn DbClient> {
    match engine {
        DbEngine::Oracle => Box::new(OracledbSession::new()),
    }
}

#[derive(Default)]
pub struct SessionPool {
    sessions: HashMap<String, SharedSession>,
}

impl SessionPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the session for a connection, creating it on first use.
    pub fn get_or_create(&mut self, connection_id: &str, engine: DbEngine) -> SharedSession {
        self.sessions
            .entry(connection_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(new_session(engine))))
            .clone()
    }

    /// Drop a connection's session, disconnecting first. Never blocks: if a
    /// query holds the session lock (e.g. disconnect clicked mid-run), the
    /// entry is dropped and the worker's own Arc keeps its session alive
    /// until it finishes — its results are still guarded by run tokens.
    pub fn remove(&mut self, connection_id: &str) {
        if let Some(session) = self.sessions.remove(connection_id) {
            if let Ok(mut guard) = session.try_lock() {
                guard.disconnect();
            }
        }
    }

    /// True when the connection has a live, connected session.
    pub fn is_live(&self, connection_id: &str) -> bool {
        self.sessions
            .get(connection_id)
            .map(|s| lock(s).is_connected())
            .unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_id_returns_shared_session() {
        let mut pool = SessionPool::new();
        let a = pool.get_or_create("db1", DbEngine::default());
        let b = pool.get_or_create("db1", DbEngine::default());
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(pool.len(), 1);
        assert!(!pool.is_live("db1")); // created, not connected
    }

    #[test]
    fn remove_drops_session() {
        let mut pool = SessionPool::new();
        pool.get_or_create("db1", DbEngine::default());
        pool.remove("db1");
        assert!(pool.is_empty());
        assert!(!pool.is_live("db1"));
    }

    #[test]
    fn lock_recovers_from_poison() {
        let m = Mutex::new(41u32);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("simulated holder panic");
        }));
        assert!(m.is_poisoned());
        // Helper recovers the inner value instead of panicking.
        assert_eq!(*lock(&m), 41);
        *lock(&m) = 42;
        assert_eq!(*lock(&m), 42);
    }
}
