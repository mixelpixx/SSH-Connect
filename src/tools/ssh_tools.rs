use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use russh::client;
use russh_sftp::client::SftpSession;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::server::SshConnectServer;
use crate::ssh_config::{self, Credentials};
use crate::state::{SshConnection, VpsClientHandler, exec_command, host_key_policy};

// ── helpers ──────────────────────────────────────────────────────────────────

fn internal_err(msg: impl ToString) -> ErrorData {
    ErrorData::internal_error(msg.to_string(), None)
}

fn invalid_err(msg: impl ToString) -> ErrorData {
    ErrorData::invalid_params(msg.to_string(), None)
}

fn default_timeout() -> u64 {
    60_000
}

// ── parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SshConnectParams {
    /// Hostname, IP, or a Host alias from ~/.ssh/config. Whatever the local
    /// `ssh -G` resolves for it (HostName/User/Port/IdentityFile) is used, so
    /// `ssh <alias>` and this tool reach the same machine the same way.
    host: String,
    /// SSH port. Overrides the alias; defaults to 22.
    port: Option<u16>,
    /// SSH username. Optional when the alias resolves a User.
    username: Option<String>,
    /// Password for authentication (omit to use a key or ssh-agent)
    password: Option<String>,
    /// Path to a private key file. Overrides the alias's IdentityFile.
    private_key_path: Option<String>,
    /// Passphrase to decrypt the private key (if encrypted)
    passphrase: Option<String>,
    /// ssh-agent identities are offered automatically; pass false to disable.
    use_agent: Option<bool>,
    /// Custom connection ID; auto-generated if omitted
    connection_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SshExecParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Shell command to execute on the remote server
    command: String,
    /// Working directory; prepended as `cd <cwd> && <command>`
    cwd: Option<String>,
    /// Timeout in milliseconds (default: 60000)
    #[serde(default = "default_timeout")]
    timeout: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SshUploadParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Local file path to upload
    local_path: String,
    /// Destination path on the remote server
    remote_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SshDownloadParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Source path on the remote server
    remote_path: String,
    /// Local destination path
    local_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SshListParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Remote directory path to list
    remote_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SshDisconnectParams {
    /// Connection ID to close
    connection_id: String,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = ssh_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Connect to a remote server via SSH. Returns a connectionId used in all subsequent commands. `host` may be a Host alias from ~/.ssh/config: HostName/User/Port/IdentityFile come from the local `ssh -G`, so username defaults to the local account exactly as with `ssh`. Authenticates with password, private key, or ssh-agent (tried in that order).")]
    async fn ssh_connect(
        &self,
        Parameters(params): Parameters<SshConnectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // ~/.ssh/config supplies whatever the caller left out; explicit
        // arguments always win.
        let defaults = ssh_config::lookup(&params.host).await;
        let host = defaults.hostname.unwrap_or_else(|| params.host.clone());
        let port = params.port.or(defaults.port).unwrap_or(22);
        let username = params
            .username
            .or(defaults.username)
            .ok_or_else(|| {
                invalid_err(format!(
                    "no username for '{}': pass 'username', set User for this host in ~/.ssh/config, or set USER/USERNAME in the environment",
                    params.host
                ))
            })?;

        // Explicit key first, then the alias's IdentityFile entries.
        let mut identity_files: Vec<PathBuf> = Vec::new();
        if let Some(path) = params.private_key_path.as_deref() {
            identity_files.push(PathBuf::from(path));
        }
        identity_files.extend(defaults.identity_files);

        let config = Arc::new(client::Config::default());
        let addr = format!("{host}:{port}");

        let handler = VpsClientHandler {
            host: host.clone(),
            port,
            policy: host_key_policy(),
        };

        let mut session = client::connect(config, addr.as_str(), handler)
            .await
            .map_err(|e| internal_err(format!("SSH connect to {addr}: {e}")))?;

        let used = ssh_config::authenticate(
            &mut session,
            Credentials {
                username: username.clone(),
                password: params.password,
                identity_files,
                passphrase: params.passphrase,
                use_agent: params.use_agent.unwrap_or(true),
            },
        )
        .await
        .map_err(internal_err)?;

        let conn_id = params.connection_id.unwrap_or_else(|| {
            let id = uuid::Uuid::new_v4().to_string();
            format!("ssh-{}", &id[..8])
        });

        tracing::info!(conn_id = %conn_id, host = %host, auth = %used, "SSH connected");

        self.pool
            .insert(
                conn_id.clone(),
                SshConnection {
                    handle: session,
                    host: host.clone(),
                    port,
                    username: username.clone(),
                },
            )
            .await;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Connected to {username}@{host}:{port} using {used}\nConnection ID: {conn_id}"
        ))]))
    }

    #[tool(description = "Execute a shell command on the remote server. Returns exit code, stdout, and stderr. Use cwd to set the working directory.")]
    async fn ssh_exec(
        &self,
        Parameters(params): Parameters<SshExecParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| {
                invalid_err(format!("No active connection: '{}'", params.connection_id))
            })?;

        let command = match &params.cwd {
            Some(cwd) => format!("cd {} && {}", cwd, params.command),
            None => params.command.clone(),
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) =
            exec_command(&mut conn.handle, &command, params.timeout)
                .await
                .map_err(|e| internal_err(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
            exit_code, stdout, stderr
        ))]))
    }

    #[tool(description = "Upload a local file to the remote server via SFTP.")]
    async fn ssh_upload_file(
        &self,
        Parameters(params): Parameters<SshUploadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| {
                invalid_err(format!("No active connection: '{}'", params.connection_id))
            })?;

        let local_data = tokio::fs::read(&params.local_path)
            .await
            .map_err(|e| internal_err(format!("Cannot read '{}': {}", params.local_path, e)))?;

        let mut conn = conn_arc.lock().await;
        let sftp = open_sftp(&mut conn.handle).await?;

        let mut remote_file = sftp
            .create(&params.remote_path)
            .await
            .map_err(|e| internal_err(format!("Cannot create remote '{}': {}", params.remote_path, e)))?;

        remote_file
            .write_all(&local_data)
            .await
            .map_err(|e| internal_err(format!("Write failed: {e}")))?;
        remote_file
            .shutdown()
            .await
            .map_err(|e| internal_err(format!("Flush failed: {e}")))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Uploaded '{}' ({} bytes) → '{}'",
            params.local_path,
            local_data.len(),
            params.remote_path
        ))]))
    }

    #[tool(description = "Download a file from the remote server via SFTP. Creates local parent directories as needed.")]
    async fn ssh_download_file(
        &self,
        Parameters(params): Parameters<SshDownloadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| {
                invalid_err(format!("No active connection: '{}'", params.connection_id))
            })?;

        let mut conn = conn_arc.lock().await;
        let sftp = open_sftp(&mut conn.handle).await?;

        let mut remote_file = sftp
            .open(&params.remote_path)
            .await
            .map_err(|e| internal_err(format!("Cannot open remote '{}': {}", params.remote_path, e)))?;

        let mut buf = Vec::new();
        remote_file
            .read_to_end(&mut buf)
            .await
            .map_err(|e| internal_err(format!("Read failed: {e}")))?;

        if let Some(parent) = Path::new(&params.local_path).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| internal_err(format!("Cannot create local dir: {e}")))?;
            }
        }

        tokio::fs::write(&params.local_path, &buf)
            .await
            .map_err(|e| internal_err(format!("Cannot write '{}': {}", params.local_path, e)))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Downloaded '{}' ({} bytes) → '{}'",
            params.remote_path,
            buf.len(),
            params.local_path
        ))]))
    }

    #[tool(description = "List files and directories in a remote path via SFTP. Returns name, type, and size for each entry.")]
    async fn ssh_list_files(
        &self,
        Parameters(params): Parameters<SshListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| {
                invalid_err(format!("No active connection: '{}'", params.connection_id))
            })?;

        let mut conn = conn_arc.lock().await;
        let sftp = open_sftp(&mut conn.handle).await?;

        let entries = sftp
            .read_dir(&params.remote_path)
            .await
            .map_err(|e| internal_err(format!("Cannot list '{}': {}", params.remote_path, e)))?;

        let mut lines = vec![format!("Contents of '{}':", params.remote_path)];
        for entry in entries {
            let meta = entry.metadata();
            let kind = if meta.is_dir() { "dir " } else { "file" };
            let size = meta.size.unwrap_or(0);
            lines.push(format!("  {} {:>12}  {}", kind, size, entry.file_name()));
        }

        Ok(CallToolResult::success(vec![Content::text(lines.join("\n"))]))
    }

    #[tool(description = "Disconnect and close an active SSH connection.")]
    async fn ssh_disconnect(
        &self,
        Parameters(params): Parameters<SshDisconnectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.pool.remove(&params.connection_id).await {
            tracing::info!(conn_id = %params.connection_id, "SSH disconnected");
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Disconnected: {}",
                params.connection_id
            ))]))
        } else {
            Err(invalid_err(format!(
                "No active connection: '{}'",
                params.connection_id
            )))
        }
    }
}

// ── SFTP helper ───────────────────────────────────────────────────────────────

/// Open an SFTP subsystem session on an existing SSH handle.
/// Each call opens a new SSH channel for the SFTP subsystem.
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
