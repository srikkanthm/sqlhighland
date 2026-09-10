//! One live Oracle session per saved connection, shared by tabs.
//!
//! Sessions are created lazily and keyed by connection id. Two tabs bound to
//! the same connection share its session (and its single open cursor — the
//! generation guards in `db.rs` keep paging honest when they interleave).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::db::{DbClient, OracledbSession};

#[derive(Default)]
pub struct SessionPool {
    sessions: HashMap<String, Arc<Mutex<OracledbSession>>>,
}

impl SessionPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the session for a connection, creating it on first use.
    pub fn get_or_create(&mut self, connection_id: &str) -> Arc<Mutex<OracledbSession>> {
        self.sessions
            .entry(connection_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(OracledbSession::new())))
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
            .and_then(|s| s.lock().ok())
            .map(|s| s.is_connected())
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
        let a = pool.get_or_create("db1");
        let b = pool.get_or_create("db1");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(pool.len(), 1);
        assert!(!pool.is_live("db1")); // created, not connected
    }

    #[test]
    fn remove_drops_session() {
        let mut pool = SessionPool::new();
        pool.get_or_create("db1");
        pool.remove("db1");
        assert!(pool.is_empty());
        assert!(!pool.is_live("db1"));
    }
}
