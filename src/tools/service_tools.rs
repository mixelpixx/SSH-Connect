use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use russh_sftp::client::SftpSession;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::server::SshConnectServer;
use crate::state::{VpsClientHandler, exec_command};

fn internal_err(msg: impl ToString) -> ErrorData {
    ErrorData::internal_error(msg.to_string(), None)
}

fn invalid_err(msg: impl ToString) -> ErrorData {
    ErrorData::invalid_params(msg.to_string(), None)
}

fn default_lines_50() -> u32 {
    50
}

fn default_sudo_true() -> bool {
    true
}

// ── param structs ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ServiceControlParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// systemd unit name e.g. mysql, php8.2-fpm, redis-server, cron
    service: String,
    /// Action: start | stop | restart | reload | status | enable | disable | is-active | is-enabled | logs
    action: String,
    /// Lines of journalctl output (only for 'logs' action, default: 50)
    #[serde(default = "default_lines_50")]
    lines: u32,
    /// Prefix with sudo (default: true)
    #[serde(default = "default_sudo_true")]
    sudo: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct NginxConfigParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: list | enable | disable | view | write | test
    action: String,
    /// Site filename e.g. "orchis.ai" or "default"
    site: Option<String>,
    /// Full nginx config text (required for write action)
    config: Option<String>,
    /// Prefix with sudo (default: true)
    #[serde(default = "default_sudo_true")]
    sudo: bool,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = service_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Control any systemd service. action: start | stop | restart | reload | status | enable | disable | is-active | is-enabled | logs. Examples: mysql, php8.2-fpm, redis-server, cron, nginx.")]
    async fn ubuntu_service_control(
        &self,
        Parameters(params): Parameters<ServiceControlParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };
        let svc = shell_quote(&params.service);

        let cmd = if params.action == "logs" {
            format!(
                "{sudo}journalctl -u {} -n {} --no-pager 2>&1",
                svc, params.lines
            )
        } else {
            match params.action.as_str() {
                "start" | "stop" | "restart" | "reload" | "status"
                | "enable" | "disable" | "is-active" | "is-enabled" => {
                    format!("{sudo}systemctl {} {}", params.action, svc)
                }
                other => return Err(invalid_err(format!(
                    "Unknown action '{}'. Use: start | stop | restart | reload | status | enable | disable | is-active | is-enabled | logs",
                    other
                ))),
            }
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

    #[tool(description = "Manage nginx virtual host configs. action: list (sites-available/enabled) | enable | disable | view | write (via SFTP) | test. 'site' is the filename in sites-available e.g. 'orchis.ai'.")]
    async fn vps_nginx_config(
        &self,
        Parameters(params): Parameters<NginxConfigParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        let sudo = if params.sudo { "sudo " } else { "" };

        // Helpers that require a site name
        let require_site = |p: &NginxConfigParams| -> Result<String, ErrorData> {
            p.site.clone().ok_or_else(|| invalid_err("'site' is required for this action"))
        };

        match params.action.as_str() {
            "list" => {
                let cmd = "ls -1 /etc/nginx/sites-available && echo '---enabled---' && ls -1 /etc/nginx/sites-enabled";
                let mut conn = conn_arc.lock().await;
                let (_, stdout, stderr) = exec_command(&mut conn.handle, cmd, 15_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                let out = if stderr.is_empty() { stdout } else { format!("{}\n[stderr]\n{}", stdout, stderr) };
                Ok(CallToolResult::success(vec![Content::text(out)]))
            }

            "enable" => {
                let site = require_site(&params)?;
                let avail = shell_quote(&format!("/etc/nginx/sites-available/{site}"));
                let enabled = shell_quote(&format!("/etc/nginx/sites-enabled/{site}"));
                let cmd = format!(
                    "{sudo}ln -sf {avail} {enabled} && \
                     {sudo}nginx -t 2>&1 && {sudo}systemctl reload nginx"
                );
                let mut conn = conn_arc.lock().await;
                let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 30_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "exit_code: {}\n{}{}",
                    exit_code, stdout,
                    if stderr.is_empty() { String::new() } else { format!("\n[stderr]\n{}", stderr) }
                ))]))
            }

            "disable" => {
                let site = require_site(&params)?;
                let enabled = shell_quote(&format!("/etc/nginx/sites-enabled/{site}"));
                let cmd = format!(
                    "{sudo}rm -f {enabled} && \
                     {sudo}nginx -t 2>&1 && {sudo}systemctl reload nginx"
                );
                let mut conn = conn_arc.lock().await;
                let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 30_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "exit_code: {}\n{}{}",
                    exit_code, stdout,
                    if stderr.is_empty() { String::new() } else { format!("\n[stderr]\n{}", stderr) }
                ))]))
            }

            "view" => {
                let site = require_site(&params)?;
                let cmd = format!("cat {}", shell_quote(&format!("/etc/nginx/sites-available/{site}")));
                let mut conn = conn_arc.lock().await;
                let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 15_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                if exit_code != 0 && stdout.is_empty() {
                    return Err(internal_err(format!("Cannot read site '{}': {}", site, stderr)));
                }
                Ok(CallToolResult::success(vec![Content::text(stdout)]))
            }

            "write" => {
                let site = require_site(&params)?;
                let config = params.config.as_deref().ok_or_else(|| {
                    invalid_err("'config' is required for write action")
                })?;

                let remote_path = format!("/etc/nginx/sites-available/{}", site);
                let tmp_path = format!("/tmp/.nginx_cfg_{}", site);

                // Write to /tmp first, then sudo-copy to sites-available
                let mut conn = conn_arc.lock().await;

                let sftp = open_sftp(&mut conn.handle).await?;
                let mut f = sftp.create(&tmp_path).await
                    .map_err(|e| internal_err(format!("Cannot create temp file: {e}")))?;
                f.write_all(config.as_bytes()).await
                    .map_err(|e| internal_err(format!("Write failed: {e}")))?;
                f.shutdown().await
                    .map_err(|e| internal_err(format!("Flush failed: {e}")))?;
                drop(sftp);

                let cmd = format!(
                    "{sudo}cp {} {} && rm -f {}",
                    shell_quote(&tmp_path),
                    shell_quote(&remote_path),
                    shell_quote(&tmp_path)
                );
                let (exit_code, _stdout, stderr) = exec_command(&mut conn.handle, &cmd, 15_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;

                if exit_code != 0 {
                    return Err(internal_err(format!("Copy failed: {}", stderr)));
                }
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Written to {}\nRun 'test' then 'enable' to apply.",
                    remote_path
                ))]))
            }

            "test" => {
                let cmd = format!("{sudo}nginx -t 2>&1");
                let mut conn = conn_arc.lock().await;
                let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 15_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                let combined = format!("{}{}", stdout, if stderr.is_empty() { String::new() } else { format!("\n{}", stderr) });
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "exit_code: {}\n{}", exit_code, combined
                ))]))
            }

            other => Err(invalid_err(format!(
                "Unknown action '{}'. Use: list | enable | disable | view | write | test", other
            ))),
        }
    }
}

async fn open_sftp(
    handle: &mut russh::client::Handle<VpsClientHandler>,
) -> Result<SftpSession, ErrorData> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| internal_err(format!("Cannot open SSH channel: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| internal_err(format!("Cannot start SFTP subsystem: {e}")))?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| internal_err(format!("SFTP session init failed: {e}")))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
