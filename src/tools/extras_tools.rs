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

fn default_port_6379() -> u16 {
    6379
}

fn default_lines_50() -> u32 {
    50
}

// ── param structs ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RedisParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: ping | info | keyspace | flush-cache | get | del | dbsize
    action: String,
    /// Key name (required for get and del actions)
    key: Option<String>,
    /// Redis port (default: 6379)
    #[serde(default = "default_port_6379")]
    port: u16,
    /// Prefix with sudo (default: false)
    #[serde(default)]
    sudo: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DockerParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: ps | start | stop | restart | logs | exec | pull | inspect
    action: String,
    /// Container name or ID (required for most actions)
    container: Option<String>,
    /// Command to run inside container (required for exec action)
    command: Option<String>,
    /// Image name (required for pull action)
    image: Option<String>,
    /// Number of log lines to show (default: 50)
    #[serde(default = "default_lines_50")]
    lines: u32,
    /// Prefix with sudo (default: false)
    #[serde(default)]
    sudo: bool,
    /// Override the timeout in milliseconds for slow actions like 'pull' (default: 300000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = extras_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Redis cache management. action: ping | info | keyspace | flush-cache | get | del | dbsize. Use 'key' for get/del. phpBB3 can use Redis as cache backend.")]
    async fn vps_redis(
        &self,
        Parameters(params): Parameters<RedisParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };
        let port = params.port;

        let redis_cmd = match params.action.as_str() {
            "ping"        => "PING".to_string(),
            "info"        => "INFO".to_string(),
            "keyspace"    => "INFO keyspace".to_string(),
            "flush-cache" => "FLUSHDB".to_string(),
            "dbsize"      => "DBSIZE".to_string(),
            "get" => {
                let key = params.key.as_deref().ok_or_else(|| invalid_err("'key' required for get"))?;
                format!("GET {}", shell_quote(key))
            }
            "del" => {
                let key = params.key.as_deref().ok_or_else(|| invalid_err("'key' required for del"))?;
                format!("DEL {}", shell_quote(key))
            }
            other => return Err(invalid_err(format!(
                "Unknown action '{}'. Use: ping | info | keyspace | flush-cache | get | del | dbsize", other
            ))),
        };

        let cmd = format!("{sudo}redis-cli -p {port} {redis_cmd}");

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

    #[tool(description = "Docker container management. action: ps | start | stop | restart | logs | exec | pull | inspect. Use 'container' for name/ID, 'command' for exec, 'image' for pull.")]
    async fn vps_docker(
        &self,
        Parameters(params): Parameters<DockerParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };

        let require_container = || {
            params.container.as_deref()
                .ok_or_else(|| invalid_err("'container' required for this action"))
        };

        let cmd = match params.action.as_str() {
            "ps" => format!("{sudo}docker ps -a"),
            "start"   => format!("{sudo}docker start {}", shell_quote(require_container()?)),
            "stop"    => format!("{sudo}docker stop {}", shell_quote(require_container()?)),
            "restart" => format!("{sudo}docker restart {}", shell_quote(require_container()?)),
            "inspect" => format!("{sudo}docker inspect {}", shell_quote(require_container()?)),
            "logs" => format!(
                "{sudo}docker logs --tail={} {}",
                params.lines,
                shell_quote(require_container()?)
            ),
            "exec" => {
                let container = require_container()?;
                let command = params.command.as_deref()
                    .ok_or_else(|| invalid_err("'command' required for exec action"))?;
                format!("{sudo}docker exec {} {}", shell_quote(container), command)
            }
            "pull" => {
                let image = params.image.as_deref()
                    .ok_or_else(|| invalid_err("'image' required for pull action"))?;
                format!("{sudo}docker pull {}", shell_quote(image))
            }
            other => return Err(invalid_err(format!(
                "Unknown action '{}'. Use: ps | start | stop | restart | logs | exec | pull | inspect", other
            ))),
        };

        let timeout_ms = params
            .timeout_ms
            .unwrap_or(if params.action == "pull" { 300_000 } else { 60_000 });

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, timeout_ms)
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

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
