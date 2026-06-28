//! Hybrid switch convenience tools. These are thin wrappers over the interactive
//! session layer for the two highest-value switch workflows. Firmware
//! install/verify/rollback and automated SSH setup are handled through the
//! `switch-management` skill (using the generic `run_command`/`expect_send`/
//! `enable` primitives and `upload_config`) rather than rigid tools.

use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::CallToolResult,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

use crate::error::ToolError;
use crate::server::SshConnectServer;
use crate::tools::util::json_result;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BackupConfigParams {
    /// Target session name (an SSH/Telnet/Serial console session).
    pub session: String,
    /// Command that prints the configuration (default "show running-config").
    pub command: Option<String>,
    /// Optional local file path to write the captured config to.
    pub save_to: Option<String>,
    /// Timeout in seconds for the capture (default 60 — configs can be large).
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetworkDiagnosticsParams {
    /// Target session name.
    pub session: String,
    /// Diagnostic to run: "ping" or "traceroute".
    pub action: String,
    /// Destination host or IP to probe from the device.
    pub target: String,
    /// Optional extra CLI args appended verbatim (vendor-specific, e.g. "repeat 10").
    pub args: Option<String>,
    /// Timeout in seconds (default 60).
    pub timeout_secs: Option<u64>,
}

#[tool_router(router = switch_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Back up a device's configuration by running a show-config command on an interactive session (default 'show running-config'; paging is auto-handled). Optionally writes the captured text to a local file. Returns { bytes, saved_to, output }.")]
    async fn switch_backup_config(
        &self,
        Parameters(params): Parameters<BackupConfigParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let command = params
            .command
            .clone()
            .unwrap_or_else(|| "show running-config".to_string());
        let timeout = Duration::from_secs(params.timeout_secs.unwrap_or(60));

        let handle = self.sessions.get(&params.session).await?;
        let mut guard = handle.lock().await;
        let outcome = guard.run_command(&command, timeout).await?;
        drop(guard);

        let mut saved_to = None;
        if let Some(path) = &params.save_to {
            tokio::fs::write(path, outcome.output.as_bytes())
                .await
                .map_err(|e| ToolError::internal(format!("write backup to '{path}': {e}")))?;
            saved_to = Some(path.clone());
        }

        Ok(json_result(json!({
            "session": params.session,
            "command": command,
            "bytes": outcome.output.len(),
            "saved_to": saved_to,
            "timed_out": outcome.timed_out,
            "truncated": outcome.truncated,
            "output": outcome.output,
        })))
    }

    #[tool(description = "Run a network diagnostic (ping or traceroute) from the device against a target host/IP, over an interactive session. Append vendor-specific options via `args`.")]
    async fn switch_network_diagnostics(
        &self,
        Parameters(params): Parameters<NetworkDiagnosticsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let verb = match params.action.to_ascii_lowercase().as_str() {
            "ping" => "ping",
            "traceroute" | "trace" | "tracert" => "traceroute",
            other => {
                return Err(ToolError::bad_request(format!(
                    "unknown action '{other}' (expected ping or traceroute)"
                ))
                .into());
            }
        };
        let mut command = format!("{verb} {}", params.target);
        if let Some(extra) = &params.args {
            if !extra.trim().is_empty() {
                command.push(' ');
                command.push_str(extra.trim());
            }
        }
        let timeout = Duration::from_secs(params.timeout_secs.unwrap_or(60));

        let handle = self.sessions.get(&params.session).await?;
        let mut guard = handle.lock().await;
        let outcome = guard.run_command(&command, timeout).await?;

        Ok(json_result(json!({
            "session": params.session,
            "action": verb,
            "target": params.target,
            "command": command,
            "output": outcome.output,
            "timed_out": outcome.timed_out,
        })))
    }
}
