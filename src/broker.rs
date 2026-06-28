//! Single-owner broker so multiple `ssh-connect` instances (one spawned per MCP
//! client) cooperate instead of fighting over exclusive resources like a serial
//! port.
//!
//! On startup each instance races to create a Windows named pipe:
//! - The winner is the **owner**: it holds the real connection registries (all
//!   live SSH/Telnet/serial sessions and COM ports) and serves the pipe to
//!   peers. Each connecting peer gets a full MCP server bound to the *same*
//!   Arc-backed state, so `connect` in one window and `run_command` from another
//!   operate on the same device.
//! - The losers are **proxies**: they open the pipe as an MCP *client* and the
//!   stdio-facing server forwards every `tools/call` to the owner.
//!
//! Tool *schemas* are identical across processes (same binary), so `tools/list`
//! is always answered locally — only execution (`tools/call`) is forwarded.
//!
//! On non-Windows platforms the named-pipe machinery is omitted and every
//! instance is simply a local owner.

use crate::server::SshConnectServer;

/// Default machine-wide pipe name. Versioned so mismatched binaries don't
/// cross-talk during an upgrade.
#[cfg(windows)]
pub const DEFAULT_PIPE: &str = r"\\.\pipe\ssh-connect-broker-v1";

/// The outcome of broker election, used by `main` to build the server.
pub enum Election {
    /// This instance owns the hardware. On Windows it also carries the first
    /// pipe-server instance so the accept loop can be spawned.
    #[cfg(windows)]
    Owner(tokio::net::windows::named_pipe::NamedPipeServer),
    /// Owner with no pipe (non-Windows, or standalone fallback).
    OwnerLocal,
    /// This instance forwards to an existing owner over the given client peer.
    #[cfg(windows)]
    Proxy(rmcp::Peer<rmcp::RoleClient>),
}

/// Decide this process's broker role.
#[cfg(windows)]
pub async fn elect() -> Election {
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    // Try to become the owner by creating the first pipe instance.
    match ServerOptions::new().first_pipe_instance(true).create(DEFAULT_PIPE) {
        Ok(server) => {
            tracing::info!(pipe = DEFAULT_PIPE, "ssh-connect: BROKER OWNER (holds devices, serves peers)");
            Election::Owner(server)
        }
        Err(_) => {
            // Someone else owns the pipe — connect as a proxy, retrying briefly
            // to absorb the race where the owner is mid-creation.
            for _ in 0..40 {
                if let Ok(client) = ClientOptions::new().open(DEFAULT_PIPE) {
                    match connect_proxy(client).await {
                        Some(peer) => {
                            tracing::info!(pipe = DEFAULT_PIPE, "ssh-connect: PROXY to existing broker");
                            return Election::Proxy(peer);
                        }
                        None => break,
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            // Could neither own nor reach an owner; run standalone so this
            // instance still works in isolation.
            tracing::warn!("ssh-connect: no broker reachable; running standalone owner");
            Election::OwnerLocal
        }
    }
}

#[cfg(not(windows))]
pub async fn elect() -> Election {
    Election::OwnerLocal
}

/// Complete the MCP client handshake to the owner over a pipe client, returning
/// the client peer used to forward calls. The running client task is kept alive
/// for the process lifetime.
#[cfg(windows)]
async fn connect_proxy(
    client: tokio::net::windows::named_pipe::NamedPipeClient,
) -> Option<rmcp::Peer<rmcp::RoleClient>> {
    use rmcp::ServiceExt;

    match ().serve(client).await {
        Ok(running) => {
            let peer = running.peer().clone();
            // Keep the client service running; if the owner dies the peer's
            // calls will error and the user can restart this client.
            tokio::spawn(async move {
                let _ = running.waiting().await;
            });
            Some(peer)
        }
        Err(e) => {
            tracing::warn!(error = %e, "ssh-connect: proxy handshake to owner failed");
            None
        }
    }
}

/// Owner accept loop: hand each connecting peer its own pipe instance and serve
/// a full MCP server bound to the shared `owner` state on a dedicated task.
#[cfg(windows)]
pub async fn serve_owner(
    first: tokio::net::windows::named_pipe::NamedPipeServer,
    owner: SshConnectServer,
) {
    use rmcp::ServiceExt;
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut server = first;
    loop {
        if let Err(e) = server.connect().await {
            tracing::warn!(error = %e, "broker: pipe accept failed");
            break;
        }
        // Pre-create the next instance for the next peer.
        let next = match ServerOptions::new().create(DEFAULT_PIPE) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "broker: could not create next pipe instance");
                break;
            }
        };
        let conn = std::mem::replace(&mut server, next);
        let peer_server = owner.clone();
        tokio::spawn(async move {
            match peer_server.serve(conn).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(e) => tracing::warn!(error = %e, "broker: peer service ended"),
            }
        });
    }
}

// Referenced by `main` on all platforms to keep the signature stable.
#[cfg(not(windows))]
#[allow(dead_code)]
pub async fn serve_owner(_first: (), _owner: SshConnectServer) {}
