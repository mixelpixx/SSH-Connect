//! Error types shared across the interactive-console side of the server.
//!
//! Interactive tool handlers return [`ToolResult`]; the tool wrappers map a
//! [`ToolError`] into an `rmcp::ErrorData` (or a structured error
//! `CallToolResult`) at the boundary. `ToolError` carries a machine-readable
//! `kind` so the caller (Claude) can react, plus a human-readable message.

use serde::Serialize;
use std::fmt;

/// Categorises a tool failure so callers can branch on the cause without
/// parsing free text.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// A named session was requested but does not exist. Caller should `connect` first.
    NoSuchSession,
    /// A session with that name already exists.
    SessionExists,
    /// Authentication failed (bad password / key / username).
    AuthFailed,
    /// Could not reach the host (timeout, refused, DNS, port closed).
    ConnectFailed,
    /// A serial port was missing or already in use.
    SerialUnavailable,
    /// The command ran but the prompt was not seen before the timeout.
    Timeout,
    /// File transfer failed (path missing, permission, remote error).
    TransferFailed,
    /// The request was malformed (bad/missing arguments).
    BadRequest,
    /// Configuration problem (hosts.toml parse / missing host).
    Config,
    /// Anything not covered above.
    Internal,
}

/// An error returned from an interactive tool handler.
#[derive(Debug, Clone, Serialize)]
pub struct ToolError {
    pub kind: ErrorKind,
    pub message: String,
    /// Optional hint — e.g. the list of available COM ports when a serial open fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ToolError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), hint: None }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    // Convenience constructors for the common cases.
    pub fn no_such_session(name: &str) -> Self {
        Self::new(
            ErrorKind::NoSuchSession,
            format!("no session named '{name}'; call connect first"),
        )
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, msg)
    }
    #[allow(dead_code)] // part of the public constructor set
    pub fn config(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, msg)
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.kind, self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " (hint: {h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ToolError {}

impl From<anyhow::Error> for ToolError {
    fn from(e: anyhow::Error) -> Self {
        ToolError::internal(e.to_string())
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::internal(e.to_string())
    }
}

/// Map an interactive [`ToolError`] into an `rmcp` error result. Successful tool
/// results carry structured JSON; failures carry the structured error payload
/// as pretty JSON text with `isError` set.
impl From<ToolError> for rmcp::ErrorData {
    fn from(e: ToolError) -> Self {
        let data = serde_json::to_value(&e).ok();
        rmcp::ErrorData::internal_error(e.to_string(), data)
    }
}

pub type ToolResult<T> = std::result::Result<T, ToolError>;
