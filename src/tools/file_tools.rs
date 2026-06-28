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

// ── param structs ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FileReadParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Remote file path to read
    path: String,
    /// Limit output to this many lines (optional)
    lines: Option<u32>,
    /// If true and lines is set, tail from end instead of head from start (default: false)
    #[serde(default)]
    from_end: bool,
    /// Prefix with sudo (default: false)
    #[serde(default)]
    sudo: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FileOpsParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Operation: delete | mkdir | chmod | chown | find | copy | move
    action: String,
    /// Source path
    path: String,
    /// Destination path (required for copy and move)
    destination: Option<String>,
    /// Permission bits e.g. "755" or "644" (for chmod)
    permissions: Option<String>,
    /// Owner spec e.g. "www-data:www-data" (for chown)
    owner: Option<String>,
    /// Glob pattern e.g. "*.log" (for find)
    pattern: Option<String>,
    /// Apply recursively (default: false)
    #[serde(default)]
    recursive: bool,
    /// Prefix with sudo (default: false)
    #[serde(default)]
    sudo: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CronParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: list | add | remove
    action: String,
    /// Run as this user (requires sudo); omit for current user
    user: Option<String>,
    /// Full cron entry to add e.g. "*/5 * * * * /usr/bin/php /path/to/cron.php"
    entry: Option<String>,
    /// Fixed-string pattern matching lines to remove (for remove action)
    pattern: Option<String>,
    /// Use sudo for user-targeted crontab (default: false)
    #[serde(default)]
    sudo: bool,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = file_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Read a remote file's contents. Optionally limit to N lines from the start (head) or end (from_end=true, tail). Good for inspecting nginx configs, PHP ini, .env files, phpBB3 config.php.")]
    async fn vps_file_read(
        &self,
        Parameters(params): Parameters<FileReadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };
        let path = shell_quote(&params.path);

        let cmd = match params.lines {
            None => format!("{sudo}cat {path}"),
            Some(n) if params.from_end => format!("{sudo}tail -n {n} {path}"),
            Some(n) => format!("{sudo}head -n {n} {path}"),
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 30_000)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        if exit_code != 0 && stdout.is_empty() {
            return Err(internal_err(format!(
                "Command failed (exit {}): {}",
                exit_code,
                if stderr.is_empty() { "no output" } else { &stderr }
            )));
        }

        Ok(CallToolResult::success(vec![Content::text(stdout)]))
    }

    #[tool(description = "Remote file system operations. action: delete | mkdir | chmod | chown | find | copy | move. Use recursive=true for directory operations. sudo=true for protected paths.")]
    async fn vps_file_ops(
        &self,
        Parameters(params): Parameters<FileOpsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };
        let r = if params.recursive { " -r" } else { "" };
        let r_upper = if params.recursive { " -R" } else { "" };
        let path = shell_quote(&params.path);

        let cmd = match params.action.as_str() {
            "delete" => {
                let flag = if params.recursive { "-rf" } else { "-f" };
                format!("{sudo}rm {flag} {path}")
            }
            "mkdir" => format!("{sudo}mkdir -p {path}"),
            "chmod" => {
                let perms = params.permissions.as_deref().ok_or_else(|| {
                    invalid_err("'permissions' required for chmod")
                })?;
                format!("{sudo}chmod{r_upper} {perms} {path}")
            }
            "chown" => {
                let owner = params.owner.as_deref().ok_or_else(|| {
                    invalid_err("'owner' required for chown")
                })?;
                format!("{sudo}chown{r_upper} {} {path}", shell_quote(owner))
            }
            "find" => {
                let pattern = params.pattern.as_deref().unwrap_or("*");
                format!("find {path} -name {} 2>&1", shell_quote(pattern))
            }
            "copy" => {
                let dest = params.destination.as_deref().ok_or_else(|| {
                    invalid_err("'destination' required for copy")
                })?;
                format!("{sudo}cp{r} {path} {}", shell_quote(dest))
            }
            "move" => {
                let dest = params.destination.as_deref().ok_or_else(|| {
                    invalid_err("'destination' required for move")
                })?;
                format!("{sudo}mv {path} {}", shell_quote(dest))
            }
            other => return Err(invalid_err(format!(
                "Unknown action '{}'. Use: delete | mkdir | chmod | chown | find | copy | move", other
            ))),
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 30_000)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "exit_code: {}\n{}{}",
            exit_code,
            stdout,
            if stderr.is_empty() { String::new() } else { format!("\n[stderr]\n{}", stderr) }
        ))]))
    }

    #[tool(description = "Manage cron jobs. action: list | add | remove. Use 'entry' for the full cron line when adding (e.g. '*/5 * * * * /usr/bin/php /path/cron.php'). Use 'pattern' to match lines for removal.")]
    async fn vps_cron(
        &self,
        Parameters(params): Parameters<CronParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let user_flag = match (&params.user, params.sudo) {
            (Some(u), true) => format!("sudo crontab -u {} ", u),
            (Some(u), false) => format!("crontab -u {} ", u),
            _ => "crontab ".to_string(),
        };

        let cmd = match params.action.as_str() {
            "list" => format!("{user_flag}-l 2>/dev/null || echo '(no crontab)'"),
            "add" => {
                let entry = params.entry.as_deref().ok_or_else(|| {
                    invalid_err("'entry' required for add action")
                })?;
                format!(
                    "({user_flag}-l 2>/dev/null; echo {}) | {user_flag}-",
                    shell_quote(entry)
                )
            }
            "remove" => {
                let pattern = params.pattern.as_deref().ok_or_else(|| {
                    invalid_err("'pattern' required for remove action")
                })?;
                format!(
                    "{user_flag}-l 2>/dev/null | grep -vF {} | {user_flag}-",
                    shell_quote(pattern)
                )
            }
            other => return Err(invalid_err(format!(
                "Unknown action '{}'. Use: list | add | remove", other
            ))),
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
