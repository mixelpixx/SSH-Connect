//! Registry of live interactive sessions.
//!
//! The outer map is locked only briefly to look up or mutate membership; each
//! session sits behind its own mutex so independent sessions can run commands
//! concurrently (needed for `run_on_fleet`).

use super::Session;
use crate::error::{ErrorKind, ToolError, ToolResult};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Default prompt regex: matches a typical device prompt at the end of a line —
/// a run of name characters followed by `#`, `>`, or `$`. Covers Cisco IOS
/// (`switch#`, `Router>`, `core-sw-01(config)#`), Arista/NX-OS, and shell
/// (`user@host:~$`).
pub const DEFAULT_PROMPT: &str = r"[\w.@:~()/\\-]+[#>$]\s*$";

#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<Session>>>>>,
}

/// A lightweight snapshot of a session for `list_sessions`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub protocol: super::Protocol,
    pub target: String,
}

impl SessionManager {
    pub fn new() -> Self {
        Self { sessions: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Register a new session. Errors if the name is already in use.
    pub async fn insert(&self, session: Session) -> ToolResult<()> {
        let name = session.name.clone();
        let mut map = self.sessions.lock().await;
        if map.contains_key(&name) {
            return Err(ToolError::new(
                ErrorKind::SessionExists,
                format!("a session named '{name}' is already open"),
            ));
        }
        map.insert(name, Arc::new(Mutex::new(session)));
        Ok(())
    }

    /// Get a handle to a session by name.
    pub async fn get(&self, name: &str) -> ToolResult<Arc<Mutex<Session>>> {
        self.sessions
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::no_such_session(name))
    }

    /// Remove a session from the registry, returning it so the caller can close it.
    pub async fn remove(&self, name: &str) -> ToolResult<Arc<Mutex<Session>>> {
        self.sessions
            .lock()
            .await
            .remove(name)
            .ok_or_else(|| ToolError::no_such_session(name))
    }

    /// Snapshot of all open sessions, sorted by name.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let map = self.sessions.lock().await;
        let mut infos = Vec::with_capacity(map.len());
        for handle in map.values() {
            let s = handle.lock().await;
            infos.push(SessionInfo {
                name: s.name.clone(),
                protocol: s.protocol,
                target: s.target.clone(),
            });
        }
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
