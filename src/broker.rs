//! Single-owner broker so multiple `ssh-connect` instances (one spawned per MCP
//! client) cooperate instead of fighting over exclusive resources like a serial
//! port.
//!
//! On startup each instance tries to own a per-machine rendezvous endpoint:
//! - **Windows:** a named pipe (`\\.\pipe\ssh-connect-broker-v1`).
//! - **Unix (Linux/macOS):** a Unix domain socket under `$XDG_RUNTIME_DIR`
//!   (falling back to the temp dir), e.g. `ssh-connect-broker-v1.sock`.
//!
//! The winner is the **owner**: it holds the real connection registries (all
//! live SSH/Telnet/serial sessions and COM ports) and serves the endpoint to
//! peers. Each connecting peer gets a full MCP server bound to the *same*
//! Arc-backed state, so `connect` in one client and `run_command` from another
//! operate on the same device. The losers are **proxies**: they open the
//! endpoint as an MCP *client* and the stdio-facing server forwards every
//! `tools/call` to the owner. Tool *schemas* are identical across processes
//! (same binary), so `tools/list` is always answered locally.
//!
//! On a platform with neither transport, every instance is a standalone owner.

use crate::server::SshConnectServer;

#[cfg(windows)]
pub const DEFAULT_PIPE: &str = r"\\.\pipe\ssh-connect-broker-v1";

/// The outcome of broker election, used by `main` to build the server.
pub enum Election {
    /// Standalone owner with no cross-process sharing (no broker transport).
    OwnerLocal,
    /// This instance owns the broker; carries the platform listener so the
    /// accept loop can be spawned once the server is built.
    #[cfg(any(windows, unix))]
    Owner(OwnerListener),
    /// This instance forwards tool calls to an existing owner over the given peer.
    #[cfg(any(windows, unix))]
    Proxy(rmcp::Peer<rmcp::RoleClient>),
}

/// Opaque wrapper over the platform's accept endpoint, so `main` stays
/// platform-neutral.
#[cfg(windows)]
pub struct OwnerListener(tokio::net::windows::named_pipe::NamedPipeServer);
#[cfg(unix)]
pub struct OwnerListener(tokio::net::UnixListener);

/// Complete the MCP client handshake to the owner over any duplex stream,
/// returning the client peer used to forward calls. The running client task is
/// kept alive for the process lifetime.
#[cfg(any(windows, unix))]
async fn handshake_client<S>(stream: S) -> Option<rmcp::Peer<rmcp::RoleClient>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    use rmcp::ServiceExt;
    match ().serve(stream).await {
        Ok(running) => {
            let peer = running.peer().clone();
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

// ---- Windows: named-pipe broker -------------------------------------------

#[cfg(windows)]
pub async fn elect() -> Election {
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    match ServerOptions::new().first_pipe_instance(true).create(DEFAULT_PIPE) {
        Ok(server) => {
            tracing::info!(pipe = DEFAULT_PIPE, "ssh-connect: BROKER OWNER (holds devices, serves peers)");
            Election::Owner(OwnerListener(server))
        }
        Err(_) => {
            for _ in 0..40 {
                if let Ok(client) = ClientOptions::new().open(DEFAULT_PIPE) {
                    if let Some(peer) = handshake_client(client).await {
                        tracing::info!(pipe = DEFAULT_PIPE, "ssh-connect: PROXY to existing broker");
                        return Election::Proxy(peer);
                    }
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            tracing::warn!("ssh-connect: no broker reachable; running standalone owner");
            Election::OwnerLocal
        }
    }
}

#[cfg(windows)]
pub async fn serve_owner(listener: OwnerListener, owner: SshConnectServer) {
    use rmcp::ServiceExt;
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut server = listener.0;
    loop {
        if let Err(e) = server.connect().await {
            tracing::warn!(error = %e, "broker: pipe accept failed");
            break;
        }
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

// ---- Unix (Linux/macOS): domain-socket broker -----------------------------

#[cfg(unix)]
fn socket_path() -> std::path::PathBuf {
    // Prefer the per-user runtime dir; fall back to a per-user name in the temp
    // dir so distinct users don't collide on a shared /tmp.
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return std::path::Path::new(&dir).join("ssh-connect-broker-v1.sock");
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    std::env::temp_dir().join(format!("ssh-connect-broker-v1-{user}.sock"))
}

#[cfg(unix)]
pub async fn elect() -> Election {
    use tokio::net::{UnixListener, UnixStream};

    let path = socket_path();

    // If a live owner is already listening, attach as a proxy.
    if let Ok(stream) = UnixStream::connect(&path).await {
        if let Some(peer) = handshake_client(stream).await {
            tracing::info!(socket = %path.display(), "ssh-connect: PROXY to existing broker");
            return Election::Proxy(peer);
        }
    }

    // No live owner. Clear any stale socket file left by a crashed owner, then
    // try to bind it ourselves.
    let _ = std::fs::remove_file(&path);
    match UnixListener::bind(&path) {
        Ok(listener) => {
            tracing::info!(socket = %path.display(), "ssh-connect: BROKER OWNER (holds devices, serves peers)");
            Election::Owner(OwnerListener(listener))
        }
        Err(_) => {
            // Lost a race with another starting instance — try once more to attach.
            if let Ok(stream) = UnixStream::connect(&path).await {
                if let Some(peer) = handshake_client(stream).await {
                    tracing::info!(socket = %path.display(), "ssh-connect: PROXY to existing broker");
                    return Election::Proxy(peer);
                }
            }
            tracing::warn!("ssh-connect: no broker reachable; running standalone owner");
            Election::OwnerLocal
        }
    }
}

#[cfg(unix)]
pub async fn serve_owner(listener: OwnerListener, owner: SshConnectServer) {
    use rmcp::ServiceExt;

    let listener = listener.0;
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let peer_server = owner.clone();
                tokio::spawn(async move {
                    match peer_server.serve(stream).await {
                        Ok(running) => {
                            let _ = running.waiting().await;
                        }
                        Err(e) => tracing::warn!(error = %e, "broker: peer service ended"),
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "broker: socket accept failed");
                break;
            }
        }
    }
}

// ---- Platforms with neither transport -------------------------------------

#[cfg(not(any(windows, unix)))]
pub async fn elect() -> Election {
    Election::OwnerLocal
}
