//! Session-lifecycle and discovery tools for the interactive-console family:
//! `connect`, `disconnect`, `list_sessions`, `list_com_ports`, `list_hosts`.
//!
//! These operate on the [`SessionManager`](crate::transport::SessionManager)
//! registry — distinct from the server-ops SSH `pool` used by `ssh_connect` /
//! `ssh_exec` and the `vps_*` tools.

use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::CallToolResult,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

use crate::discovery::ports;
use crate::error::ToolError;
use crate::server::SshConnectServer;
use crate::tools::util::json_result;
use crate::transport::serial::SerialTransport;
use crate::transport::ssh::{SshAuth, SshTransport};
use crate::transport::telnet::TelnetTransport;
use crate::transport::{Protocol, Session, Transport, DEFAULT_PROMPT};
use regex::Regex;

// ── parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectParams {
    /// Session name (also looked up in the inventory hosts.toml for defaults).
    pub name: String,
    /// Protocol: "ssh", "telnet", or "serial". Falls back to the inventory entry.
    pub protocol: Option<String>,
    /// Hostname or IP (ssh/telnet).
    pub host: Option<String>,
    /// TCP port. Defaults: ssh 22, telnet 23.
    pub port: Option<u16>,
    /// Login user (ssh).
    pub username: Option<String>,
    /// Password (ssh/telnet). Prefer an env override for inventory hosts.
    pub password: Option<String>,
    /// Path to an SSH private key (ssh; alternative to password).
    pub key_path: Option<String>,
    /// Passphrase for an encrypted private key.
    pub passphrase: Option<String>,
    /// COM/serial port name, e.g. COM3 (serial).
    pub serial_port: Option<String>,
    /// Serial baud rate (default 9600).
    pub baud: Option<u32>,
    /// Serial: probe common baud rates instead of using a fixed `baud`.
    pub baud_auto: Option<bool>,
    /// Serial: send a carriage return on connect and return the banner/prompt.
    pub wake: Option<bool>,
    /// Optional regex overriding device prompt detection.
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DisconnectParams {
    /// Name of the session to close.
    pub session: String,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = session_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Open a named interactive session to a switch/host over SSH, Telnet, or serial. If `name` matches a host in the inventory (hosts.toml), its parameters are used as defaults and explicit arguments override them. The session persists across tool calls until `disconnect`. Use this (with run_command/enable/login) for network devices and consoles; use ssh_connect for server-ops with exit codes.")]
    async fn connect(
        &self,
        Parameters(params): Parameters<ConnectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = params.name.clone();
        let entry = self.inventory.get(&name).cloned();

        // Resolve protocol from args, falling back to the inventory entry.
        let protocol = match params.protocol.as_deref() {
            Some(s) => parse_protocol(s)?,
            None => entry.as_ref().map(|e| e.protocol).ok_or_else(|| {
                ToolError::bad_request(format!(
                    "no 'protocol' given and no inventory host named '{name}'"
                ))
            })?,
        };

        let host = params.host.clone().or_else(|| entry.as_ref().and_then(|e| e.host.clone()));
        let port = params.port.or_else(|| entry.as_ref().and_then(|e| e.port));
        let username = params
            .username
            .clone()
            .or_else(|| entry.as_ref().and_then(|e| e.username.clone()));
        let password = params
            .password
            .clone()
            .or_else(|| entry.as_ref().and_then(|e| e.password.clone()));
        let key_path = params
            .key_path
            .clone()
            .map(PathBuf::from)
            .or_else(|| entry.as_ref().and_then(|e| e.key_path.clone()));
        let passphrase = params.passphrase.clone();
        let serial_port = params
            .serial_port
            .clone()
            .or_else(|| entry.as_ref().and_then(|e| e.serial_port.clone()));
        let baud = params
            .baud
            .or_else(|| entry.as_ref().and_then(|e| e.baud))
            .unwrap_or(9600);
        let prompt_src = params
            .prompt
            .clone()
            .or_else(|| entry.as_ref().and_then(|e| e.prompt.clone()));
        let prompt = Regex::new(prompt_src.as_deref().unwrap_or(DEFAULT_PROMPT))
            .map_err(|e| ToolError::bad_request(format!("invalid prompt regex: {e}")))?;

        let (transport, target): (Box<dyn Transport>, String) = match protocol {
            Protocol::Ssh => {
                let host = host.ok_or_else(|| ToolError::bad_request("ssh requires 'host'"))?;
                let username =
                    username.ok_or_else(|| ToolError::bad_request("ssh requires 'username'"))?;
                let port = port.unwrap_or(22);
                let t = SshTransport::connect(SshAuth {
                    host: &host,
                    port,
                    username: &username,
                    password: password.as_deref(),
                    key_path: key_path.as_deref(),
                    passphrase: passphrase.as_deref(),
                })
                .await?;
                (Box::new(t), format!("{host}:{port}"))
            }
            Protocol::Telnet => {
                let host = host.ok_or_else(|| ToolError::bad_request("telnet requires 'host'"))?;
                let port = port.unwrap_or(23);
                let t = TelnetTransport::connect(&host, port).await?;
                (Box::new(t), format!("{host}:{port}"))
            }
            Protocol::Serial => {
                let sp = serial_port
                    .ok_or_else(|| ToolError::bad_request("serial requires 'serial_port'"))?;
                if params.baud_auto.unwrap_or(false) {
                    let (t, detected) = SerialTransport::auto_baud(&sp)
                        .await
                        .map_err(|e| e.with_hint(ports::ports_hint()))?;
                    (Box::new(t), format!("{sp}@{detected}"))
                } else {
                    let t = SerialTransport::open(&sp, baud)
                        .map_err(|e| e.with_hint(ports::ports_hint()))?;
                    (Box::new(t), format!("{sp}@{baud}"))
                }
            }
        };

        let mut session = Session::new(name.clone(), target.clone(), prompt, transport);

        // Optional wake: send a carriage return and capture the banner/prompt.
        let mut banner = None;
        if params.wake.unwrap_or(false) {
            let w = session.run_command("", Duration::from_secs(4)).await?;
            banner = w.matched_prompt.clone().or(Some(w.output));
        }

        self.sessions.insert(session).await?;

        Ok(json_result(json!({
            "connected": true,
            "session": name,
            "protocol": protocol.to_string(),
            "target": target,
            "banner": banner,
        })))
    }

    #[tool(description = "Close a named interactive session and remove it from the registry.")]
    async fn disconnect(
        &self,
        Parameters(params): Parameters<DisconnectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.sessions.remove(&params.session).await?;
        let _ = handle.lock().await.close().await;
        Ok(json_result(json!({ "disconnected": true, "session": params.session })))
    }

    #[tool(description = "List all currently open interactive sessions with their protocol and target.")]
    async fn list_sessions(&self) -> Result<CallToolResult, ErrorData> {
        let sessions = self.sessions.list().await;
        Ok(json_result(json!({ "count": sessions.len(), "sessions": sessions })))
    }

    #[tool(description = "Enumerate serial/COM ports visible to the OS, with description, manufacturer, product, serial number, USB vid:pid (e.g. FTDI 0403:6001, Prolific 067b:2303, Silicon Labs 10c4:ea60, CH340 1a86:7523), and a free/in_use status from a non-destructive open test.")]
    async fn list_com_ports(&self) -> Result<CallToolResult, ErrorData> {
        let ports = ports::list_com_ports().await?;
        Ok(json_result(json!({ "count": ports.len(), "ports": ports })))
    }

    #[tool(description = "List named hosts from the inventory (hosts.toml).")]
    async fn list_hosts(&self) -> Result<CallToolResult, ErrorData> {
        let hosts: Vec<_> = self
            .inventory
            .summaries()
            .into_iter()
            .map(|h| {
                json!({
                    "name": h.name,
                    "protocol": h.protocol.to_string(),
                    "host": h.host,
                    "serial_port": h.serial_port,
                })
            })
            .collect();
        Ok(json_result(json!({ "count": hosts.len(), "hosts": hosts })))
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_protocol(s: &str) -> Result<Protocol, ToolError> {
    match s.to_ascii_lowercase().as_str() {
        "ssh" => Ok(Protocol::Ssh),
        "telnet" => Ok(Protocol::Telnet),
        "serial" => Ok(Protocol::Serial),
        other => Err(ToolError::bad_request(format!(
            "unknown protocol '{other}' (expected ssh, telnet, or serial)"
        ))),
    }
}
