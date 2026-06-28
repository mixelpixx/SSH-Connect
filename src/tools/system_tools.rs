use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::server::SshConnectServer;
use crate::state::exec_command;

fn internal_err(msg: impl ToString) -> ErrorData {
    ErrorData::internal_error(msg.to_string(), None)
}

fn invalid_err(msg: impl ToString) -> ErrorData {
    ErrorData::invalid_params(msg.to_string(), None)
}

fn default_lines_100() -> u32 {
    100
}

fn default_sudo_true() -> bool {
    true
}

// ── param structs ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SystemStatsParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct LogsParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Log source: nginx-access | nginx-error | syslog | auth | mysql | php | journalctl
    service: String,
    /// Number of lines to tail (default: 100)
    #[serde(default = "default_lines_100")]
    lines: u32,
    /// Case-insensitive grep filter (optional)
    filter: Option<String>,
    /// journalctl unit name (only used when service = "journalctl")
    unit: Option<String>,
    /// Prefix commands with sudo (default: true)
    #[serde(default = "default_sudo_true")]
    sudo: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ProcessParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: list | find | kill
    action: String,
    /// Process name (for find or kill-by-name)
    name: Option<String>,
    /// PID to kill (for kill-by-pid)
    pid: Option<u32>,
    /// Signal to send (default: TERM)
    signal: Option<String>,
    /// Prefix kill commands with sudo (default: false)
    #[serde(default)]
    sudo: bool,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = system_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Get disk usage, memory, CPU count, load average, and OS info in a single call. Good first check for any VPS issue.")]
    async fn vps_system_stats(
        &self,
        Parameters(params): Parameters<SystemStatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let cmd = "echo '===DISK===' && df -h && \
                   echo '===MEM===' && free -m && \
                   echo '===CPU===' && nproc && uname -m && \
                   echo '===LOAD===' && uptime && \
                   echo '===OS===' && grep PRETTY_NAME /etc/os-release";

        let mut conn = conn_arc.lock().await;
        let (_, stdout, stderr) = exec_command(&mut conn.handle, cmd, 15_000)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        let out = if stderr.is_empty() {
            stdout
        } else {
            format!("{}\n[stderr]\n{}", stdout, stderr)
        };
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Tail a log file or journalctl unit. service: nginx-access | nginx-error | syslog | auth | mysql | php | journalctl. Use 'unit' for journalctl service name. Use 'filter' for grep pattern.")]
    async fn vps_logs(
        &self,
        Parameters(params): Parameters<LogsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };
        let n = params.lines;

        let base_cmd = match params.service.as_str() {
            "nginx-access" => format!("{sudo}tail -n {n} /var/log/nginx/access.log 2>&1"),
            "nginx-error"  => format!("{sudo}tail -n {n} /var/log/nginx/error.log 2>&1"),
            "syslog"       => format!("{sudo}tail -n {n} /var/log/syslog 2>&1"),
            "auth"         => format!("{sudo}tail -n {n} /var/log/auth.log 2>&1"),
            "mysql"        => format!("{sudo}tail -n {n} /var/log/mysql/error.log 2>&1"),
            "php"          => format!(
                "{sudo}tail -n {n} $(ls /var/log/php*/error.log 2>/dev/null | head -1) 2>&1"
            ),
            "journalctl" => {
                let unit = params.unit.as_deref().unwrap_or("");
                if unit.is_empty() {
                    return Err(invalid_err("'unit' is required when service = 'journalctl'"));
                }
                format!("{sudo}journalctl -u {unit} -n {n} --no-pager 2>&1")
            }
            other => return Err(invalid_err(format!("Unknown service '{}'. Use: nginx-access | nginx-error | syslog | auth | mysql | php | journalctl", other))),
        };

        let cmd = match &params.filter {
            Some(f) => format!("{} | grep -i {}", base_cmd, shell_quote(f)),
            None => base_cmd,
        };

        let mut conn = conn_arc.lock().await;
        let (_, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 30_000)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        let out = if stderr.is_empty() || stdout.contains(&*stderr) {
            stdout
        } else {
            format!("{}\n[stderr]\n{}", stdout, stderr)
        };
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "List, find, or kill processes. action: list (top 30 by CPU) | find (by name) | kill (by pid or name). signal: TERM | KILL | HUP | USR1 | USR2 (default: TERM).")]
    async fn vps_process(
        &self,
        Parameters(params): Parameters<ProcessParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };
        let signal = params.signal.as_deref().unwrap_or("TERM");

        let cmd = match params.action.as_str() {
            "list" => "ps aux --sort=-%cpu | head -30".to_string(),
            "find" => {
                let name = params.name.as_deref().ok_or_else(|| invalid_err("'name' required for find"))?;
                format!("ps aux | grep -i {} | grep -v grep", shell_quote(name))
            }
            "kill" => {
                if let Some(pid) = params.pid {
                    format!("{sudo}kill -{signal} {pid}")
                } else if let Some(ref name) = params.name {
                    format!("{sudo}pkill -{signal} {}", shell_quote(name))
                } else {
                    return Err(invalid_err("'pid' or 'name' required for kill"));
                }
            }
            other => return Err(invalid_err(format!("Unknown action '{}'. Use: list | find | kill", other))),
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 15_000)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "exit_code: {}\n{}{}",
            exit_code,
            stdout,
            if stderr.is_empty() { String::new() } else { format!("\n[stderr]\n{}", stderr) }
        ))]))
    }
}

/// Wrap a string in single quotes, escaping any embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
