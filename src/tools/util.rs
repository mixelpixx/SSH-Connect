//! Shared helpers for the interactive-console tool family (session/interactive/
//! switch tools), which work with structured JSON results like the original
//! PuTTY-MCP dispatcher.

use rmcp::model::{CallToolResult, Content};
use serde_json::Value;
use std::time::Duration;

/// Default per-command timeout if the caller does not specify one.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Wrap structured data into a successful MCP tool result, rendered as pretty
/// JSON text so the model can read it directly.
pub fn json_result(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    CallToolResult::success(vec![Content::text(text)])
}

/// Resolve a per-command timeout from an optional `timeout_secs` argument.
pub fn timeout_or_default(timeout_secs: Option<u64>) -> Duration {
    Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS))
}
