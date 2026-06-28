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
use crate::state::exec_command;

// ── helpers ───────────────────────────────────────────────────────────────────

fn internal_err(msg: impl ToString) -> ErrorData {
    ErrorData::internal_error(msg.to_string(), None)
}

fn invalid_err(msg: impl ToString) -> ErrorData {
    ErrorData::invalid_params(msg.to_string(), None)
}

fn default_true() -> bool {
    true
}

// ── parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct NginxControlParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: start | stop | restart | status | reload | check-config
    action: String,
    /// Run with sudo (default: true)
    #[serde(default = "default_true")]
    sudo: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct UpdatePackagesParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Only install security updates (default: false)
    #[serde(default)]
    security_only: bool,
    /// Run apt-get upgrade after update (default: false)
    #[serde(default)]
    upgrade: bool,
    /// Run apt-get autoremove after upgrade (default: false)
    #[serde(default)]
    autoremove: bool,
    /// Run with sudo (default: true)
    #[serde(default = "default_true")]
    sudo: bool,
    /// Override the upgrade-step timeout in milliseconds (default: 300000). Raise
    /// for slow mirrors; for very long upgrades prefer running apt detached.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SslCertParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: issue | renew | status | list
    action: String,
    /// Domain name (required for issue)
    domain: Option<String>,
    /// Email address for Let's Encrypt notifications (required for issue)
    email: Option<String>,
    /// Webroot path for HTTP-01 challenge (default: /var/www/html)
    webroot: Option<String>,
    /// Run with sudo (default: true)
    #[serde(default = "default_true")]
    sudo: bool,
    /// Override the certbot timeout in milliseconds (default: 120000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DeployParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: deploy | backup | restore
    action: String,
    /// Local path for deploy (file or directory)
    local_path: Option<String>,
    /// Remote destination path
    remote_path: Option<String>,
    /// Path for the backup archive (tar.gz)
    backup_path: Option<String>,
    /// Create a backup before deploying (default: false)
    #[serde(default)]
    create_backup: bool,
    /// Run with sudo (default: true)
    #[serde(default = "default_true")]
    sudo: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FirewallParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: enable | disable | status | allow | deny | delete | reset
    action: String,
    /// Port number (for allow/deny/delete actions)
    port: Option<u16>,
    /// Protocol: tcp | udp (default: tcp for deny/delete)
    protocol: Option<String>,
    /// Source IP or CIDR for allow-from rules
    from: Option<String>,
    /// Run with sudo (default: true)
    #[serde(default = "default_true")]
    sudo: bool,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = ubuntu_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Control the Nginx web server on Ubuntu via systemctl. Actions: start, stop, restart, status, reload, check-config.")]
    async fn ubuntu_nginx_control(
        &self,
        Parameters(params): Parameters<NginxControlParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| invalid_err(format!("No active connection: '{}'", params.connection_id)))?;

        let s = if params.sudo { "sudo " } else { "" };

        let cmd = match params.action.as_str() {
            "start" | "stop" | "restart" | "status" | "reload" => {
                format!("{}systemctl {} nginx 2>&1", s, params.action)
            }
            "check-config" => format!("{}nginx -t 2>&1", s),
            other => {
                return Err(invalid_err(format!(
                    "Invalid action '{}'. Use: start|stop|restart|status|reload|check-config",
                    other
                )))
            }
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 30_000)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        let output = if stdout.is_empty() { stderr } else { stdout };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "nginx {} (exit {})\n{}",
            params.action, exit_code, output
        ))]))
    }

    #[tool(description = "Update apt packages on Ubuntu. Runs apt-get update, then optionally upgrade or security-only updates, and optionally autoremove.")]
    async fn ubuntu_update_packages(
        &self,
        Parameters(params): Parameters<UpdatePackagesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| invalid_err(format!("No active connection: '{}'", params.connection_id)))?;

        let s = if params.sudo { "sudo " } else { "" };
        let upgrade_timeout = params.timeout_ms.unwrap_or(300_000);
        let mut results: Vec<String> = Vec::new();
        let mut conn = conn_arc.lock().await;

        // Step 1: always refresh package lists
        let (code, out, err) =
            exec_command(&mut conn.handle, &format!("{}apt-get update 2>&1", s), 120_000)
                .await
                .map_err(|e| internal_err(e.to_string()))?;
        results.push(format!(
            "apt-get update (exit {}):\n{}",
            code,
            if out.is_empty() { &err } else { &out }
        ));

        // Step 2: upgrade
        if params.security_only {
            let cmd = format!(
                r#"{s}DEBIAN_FRONTEND=noninteractive apt-get -y --only-upgrade install \
$(apt-get -s dist-upgrade 2>/dev/null | grep '^Inst' | grep -i securi | awk '{{print $2}}') 2>&1"#,
                s = s
            );
            let (code, out, err) = exec_command(&mut conn.handle, &cmd, upgrade_timeout)
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            results.push(format!(
                "security-only upgrade (exit {}):\n{}",
                code,
                if out.is_empty() { &err } else { &out }
            ));
        } else if params.upgrade {
            let cmd = format!(
                "{}DEBIAN_FRONTEND=noninteractive apt-get -y upgrade 2>&1",
                s
            );
            let (code, out, err) = exec_command(&mut conn.handle, &cmd, upgrade_timeout)
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            results.push(format!(
                "apt-get upgrade (exit {}):\n{}",
                code,
                if out.is_empty() { &err } else { &out }
            ));
        }

        // Step 3: autoremove
        if params.autoremove {
            let (code, out, err) = exec_command(
                &mut conn.handle,
                &format!("{}apt-get -y autoremove 2>&1", s),
                60_000,
            )
            .await
            .map_err(|e| internal_err(e.to_string()))?;
            results.push(format!(
                "apt-get autoremove (exit {}):\n{}",
                code,
                if out.is_empty() { &err } else { &out }
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(results.join("\n\n"))]))
    }

    #[tool(description = "Manage Let's Encrypt SSL certificates via certbot. Actions: issue (requires domain+email), renew, status, list.")]
    async fn ubuntu_ssl_certificate(
        &self,
        Parameters(params): Parameters<SslCertParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| invalid_err(format!("No active connection: '{}'", params.connection_id)))?;

        let s = if params.sudo { "sudo " } else { "" };
        let ssl_timeout = params.timeout_ms.unwrap_or(120_000);

        let cmd = match params.action.as_str() {
            "issue" => {
                let domain = params
                    .domain
                    .ok_or_else(|| invalid_err("'domain' is required for issue action"))?;
                let email = params
                    .email
                    .ok_or_else(|| invalid_err("'email' is required for issue action"))?;
                let webroot = params
                    .webroot
                    .unwrap_or_else(|| "/var/www/html".to_string());

                // Auto-install certbot if not present
                format!(
                    "which certbot || {s}apt-get install -y certbot && \
                     {s}certbot certonly --webroot -w {webroot} -d {domain} \
                     -m {email} --agree-tos --non-interactive 2>&1",
                    s = s,
                    webroot = shell_quote(&webroot),
                    domain = shell_quote(&domain),
                    email = shell_quote(&email)
                )
            }
            "renew" => format!("{}certbot renew 2>&1", s),
            "status" | "list" => format!("{}certbot certificates 2>&1", s),
            other => {
                return Err(invalid_err(format!(
                    "Invalid action '{}'. Use: issue|renew|status|list",
                    other
                )))
            }
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, ssl_timeout)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        let output = if stdout.is_empty() { stderr } else { stdout };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "certbot {} (exit {}):\n{}",
            params.action, exit_code, output
        ))]))
    }

    #[tool(description = "Deploy, backup, or restore website files on the remote server. 'deploy' uploads local files via SFTP. 'backup' and 'restore' use tar archives on the remote.")]
    async fn ubuntu_website_deployment(
        &self,
        Parameters(params): Parameters<DeployParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| invalid_err(format!("No active connection: '{}'", params.connection_id)))?;

        let s = if params.sudo { "sudo " } else { "" };
        let mut conn = conn_arc.lock().await;
        let mut results: Vec<String> = Vec::new();

        match params.action.as_str() {
            "backup" => {
                let backup = params
                    .backup_path
                    .ok_or_else(|| invalid_err("'backupPath' required for backup action"))?;
                let remote = params
                    .remote_path
                    .ok_or_else(|| invalid_err("'remotePath' required for backup action"))?;

                let cmd = format!(
                    r#"{s}tar -czf {backup} -C "$(dirname {remote})" "$(basename {remote})" 2>&1"#,
                    s = s,
                    backup = shell_quote(&backup),
                    remote = shell_quote(&remote)
                );
                let (code, out, err) = exec_command(&mut conn.handle, &cmd, 120_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                results.push(format!(
                    "backup to '{}' (exit {}):\n{}",
                    backup,
                    code,
                    if out.is_empty() { &err } else { &out }
                ));
            }

            "restore" => {
                let backup = params
                    .backup_path
                    .ok_or_else(|| invalid_err("'backupPath' required for restore action"))?;
                let remote = params
                    .remote_path
                    .ok_or_else(|| invalid_err("'remotePath' required for restore action"))?;

                let cmd = format!(
                    r#"{s}tar -xzf {backup} -C "$(dirname {remote})" 2>&1"#,
                    s = s,
                    backup = shell_quote(&backup),
                    remote = shell_quote(&remote)
                );
                let (code, out, err) = exec_command(&mut conn.handle, &cmd, 120_000)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                results.push(format!(
                    "restore from '{}' (exit {}):\n{}",
                    backup,
                    code,
                    if out.is_empty() { &err } else { &out }
                ));
            }

            "deploy" => {
                let local = params
                    .local_path
                    .ok_or_else(|| invalid_err("'localPath' required for deploy action"))?;
                let remote = params
                    .remote_path
                    .ok_or_else(|| invalid_err("'remotePath' required for deploy action"))?;

                // Optional pre-deploy backup
                if params.create_backup {
                    if let Some(ref backup) = params.backup_path {
                        let cmd = format!(
                            r#"{s}tar -czf {backup} -C "$(dirname {remote})" "$(basename {remote})" 2>&1"#,
                            s = s,
                            backup = shell_quote(backup),
                            remote = shell_quote(&remote)
                        );
                        let (code, out, err) = exec_command(&mut conn.handle, &cmd, 120_000)
                            .await
                            .map_err(|e| internal_err(e.to_string()))?;
                        results.push(format!(
                            "pre-deploy backup (exit {}):\n{}",
                            code,
                            if out.is_empty() { &err } else { &out }
                        ));
                    }
                }

                // Upload files via SFTP
                let channel = conn
                    .handle
                    .channel_open_session()
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                channel
                    .request_subsystem(true, "sftp")
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                let sftp = SftpSession::new(channel.into_stream())
                    .await
                    .map_err(|e| internal_err(format!("SFTP init: {e}")))?;

                let count = upload_path(&sftp, std::path::Path::new(&local), &remote)
                    .await
                    .map_err(|e| internal_err(format!("Upload error: {e}")))?;

                results.push(format!("deployed {} file(s) → '{}'", count, remote));
            }

            other => {
                return Err(invalid_err(format!(
                    "Invalid action '{}'. Use: deploy|backup|restore",
                    other
                )))
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            results.join("\n\n"),
        )]))
    }

    #[tool(description = "Manage the UFW firewall on Ubuntu. Actions: enable, disable, status, allow, deny, delete, reset. Use port and protocol for port rules; use from for IP-based allow rules.")]
    async fn ubuntu_ufw_firewall(
        &self,
        Parameters(params): Parameters<FirewallParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| invalid_err(format!("No active connection: '{}'", params.connection_id)))?;

        let s = if params.sudo { "sudo " } else { "" };

        let cmd = match params.action.as_str() {
            "enable" => format!("{}ufw --force enable 2>&1", s),
            "disable" => format!("{}ufw disable 2>&1", s),
            "status" => format!("{}ufw status verbose 2>&1", s),
            "reset" => format!("{}ufw --force reset 2>&1", s),

            "allow" => match (params.port, &params.protocol, &params.from) {
                (Some(port), Some(proto), _) => {
                    format!("{}ufw allow {}/{} 2>&1", s, port, shell_quote(proto))
                }
                (Some(port), None, _) => format!("{}ufw allow {} 2>&1", s, port),
                (None, _, Some(from)) => {
                    format!("{}ufw allow from {} 2>&1", s, shell_quote(from))
                }
                _ => {
                    return Err(invalid_err(
                        "allow action requires 'port' or 'from' parameter",
                    ))
                }
            },

            "deny" => {
                let port = params
                    .port
                    .ok_or_else(|| invalid_err("deny action requires 'port'"))?;
                let proto = params.protocol.as_deref().unwrap_or("tcp");
                format!("{}ufw deny {}/{} 2>&1", s, port, shell_quote(proto))
            }

            "delete" => {
                let port = params
                    .port
                    .ok_or_else(|| invalid_err("delete action requires 'port'"))?;
                let proto = params.protocol.as_deref().unwrap_or("tcp");
                format!("{}ufw delete allow {}/{} 2>&1", s, port, shell_quote(proto))
            }

            other => {
                return Err(invalid_err(format!(
                    "Invalid action '{}'. Use: enable|disable|status|allow|deny|delete|reset",
                    other
                )))
            }
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, 30_000)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        let output = if stdout.is_empty() { stderr } else { stdout };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "ufw {} (exit {}):\n{}",
            params.action, exit_code, output
        ))]))
    }
}

/// Wrap a string in single quotes, escaping any embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ── SFTP directory upload helper ──────────────────────────────────────────────

/// Recursively upload a local file or directory to a remote path via SFTP.
/// Returns the number of files uploaded.
async fn upload_path(
    sftp: &SftpSession,
    local: &std::path::Path,
    remote: &str,
) -> anyhow::Result<usize> {
    if local.is_file() {
        let data = tokio::fs::read(local).await?;
        let mut f = sftp
            .create(remote)
            .await
            .map_err(|e| anyhow::anyhow!("SFTP create '{}': {}", remote, e))?;
        f.write_all(&data).await?;
        f.shutdown().await.ok();
        Ok(1)
    } else {
        // Create the remote directory (ignore error if already exists)
        sftp.create_dir(remote).await.ok();
        let mut count = 0usize;
        let mut entries = tokio::fs::read_dir(local).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().replace('\\', "/");
            let remote_child = format!("{}/{}", remote.trim_end_matches('/'), name);
            let local_child = entry.path();
            // Box::pin for async recursion
            count += Box::pin(upload_path(sftp, &local_child, &remote_child)).await?;
        }
        Ok(count)
    }
}
