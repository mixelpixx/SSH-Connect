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

/// Detect a CLI rejection in device output. IOS-style devices answer a command
/// they don't accept (wrong privilege level, bad syntax, failed authorization)
/// with a `%`-prefixed line rather than the requested data. When present, the
/// command did NOT succeed and its "output" is an error message — callers must
/// not treat it as a valid capture. Returns the offending line for context.
fn detect_cli_error(output: &str) -> Option<String> {
    const MARKERS: &[&str] = &[
        "% Invalid input",
        "% Incomplete command",
        "% Ambiguous command",
        "% Unknown command",
        "% Authorization failed",
        "Command authorization failed",
        "% Access denied",
        "% Permission denied",
        "% Unrecognized",
        "% Bad ",
        "% Not enough",
    ];
    for m in MARKERS {
        if output.contains(m) {
            let line = output
                .lines()
                .find(|l| l.contains(m))
                .unwrap_or(m)
                .trim()
                .to_string();
            return Some(line);
        }
    }
    None
}

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
    #[tool(description = "Back up a device's configuration by running a show-config command on an interactive session (default 'show running-config'; paging is auto-handled). Optionally writes the captured text to a local file. Returns { ok, bytes, saved_to, output }; if the device rejects the command (e.g. needs `enable`), ok is false, error_detected names the rejection, and nothing is saved.")]
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

        // The device may reject the command (e.g. `show running-config` needs
        // privilege 15). Don't save the rejection text as if it were a config.
        if let Some(err) = detect_cli_error(&outcome.output) {
            return Ok(json_result(json!({
                "ok": false,
                "session": params.session,
                "command": command,
                "error_detected": err,
                "saved_to": serde_json::Value::Null,
                "bytes": 0,
                "hint": "the device rejected the command (check privilege level with `enable`, or the syntax); nothing was saved",
                "output": outcome.output,
            })));
        }
        // Guard against an empty capture being reported as a successful backup.
        if outcome.output.trim().is_empty() {
            return Ok(json_result(json!({
                "ok": false,
                "session": params.session,
                "command": command,
                "error_detected": "empty output",
                "saved_to": serde_json::Value::Null,
                "bytes": 0,
                "hint": "no output captured — is the session at a usable prompt?",
                "output": outcome.output,
            })));
        }

        let mut saved_to = None;
        if let Some(path) = &params.save_to {
            tokio::fs::write(path, outcome.output.as_bytes())
                .await
                .map_err(|e| ToolError::internal(format!("write backup to '{path}': {e}")))?;
            saved_to = Some(path.clone());
        }

        Ok(json_result(json!({
            "ok": true,
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

        // A CLI rejection (bad syntax / unrecognized host) is a failure; note
        // that a ping reporting "0 percent" success is a valid result, NOT a CLI
        // error, so it is reported with ok=true and surfaced in `output`.
        let cli_error = detect_cli_error(&outcome.output);
        Ok(json_result(json!({
            "ok": cli_error.is_none(),
            "session": params.session,
            "action": verb,
            "target": params.target,
            "command": command,
            "error_detected": cli_error,
            "output": outcome.output,
            "timed_out": outcome.timed_out,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::detect_cli_error;

    #[test]
    fn flags_ios_rejections() {
        // The exact case seen on a live 2960X when `show running-config` was run
        // at privilege level 1.
        let out = "                         ^\n% Invalid input detected at '^' marker.";
        assert_eq!(
            detect_cli_error(out).as_deref(),
            Some("% Invalid input detected at '^' marker.")
        );
        assert!(detect_cli_error("% Access denied").is_some());
        assert!(detect_cli_error("Command authorization failed").is_some());
    }

    #[test]
    fn passes_real_output() {
        // A normal config / ping result must NOT be flagged.
        assert!(detect_cli_error("Building configuration...\n!\nversion 15.2\nhostname SW1").is_none());
        // 0% ping success is a valid result, not a CLI error.
        assert!(detect_cli_error("Sending 5, 100-byte ICMP Echos\n.....\nSuccess rate is 0 percent (0/5)").is_none());
    }
}
