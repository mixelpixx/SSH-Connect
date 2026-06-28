use std::{collections::HashMap, path::PathBuf, sync::Arc};

use russh::client::Handle;
use tokio::sync::Mutex;

/// How to treat the server's host key on connect.
///
/// Default is [`HostKeyPolicy::AcceptAny`] so existing flows keep working. Set
/// the `SSHCONNECT_HOST_KEY_CHECK` env var (or the legacy `VPS_HOST_KEY_CHECK`)
/// to `tofu` (or `1`/`true`/`on`/`yes`) to enable trust-on-first-use
/// verification against `~/.ssh-connect/known_hosts`: the first key seen for a
/// host is recorded, and any later mismatch refuses the connection (possible
/// MITM).
#[derive(Clone)]
pub enum HostKeyPolicy {
    AcceptAny,
    Tofu(PathBuf),
}

/// Resolve the host-key policy from the environment, once per connect.
pub fn host_key_policy() -> HostKeyPolicy {
    let enabled = |var: &str| {
        std::env::var(var)
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "tofu" | "1" | "true" | "on" | "yes"))
            .unwrap_or(false)
    };
    if enabled("SSHCONNECT_HOST_KEY_CHECK") || enabled("VPS_HOST_KEY_CHECK") {
        HostKeyPolicy::Tofu(known_hosts_path())
    } else {
        HostKeyPolicy::AcceptAny
    }
}

fn known_hosts_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh-connect")
        .join("known_hosts")
}

/// SSH client handler. Carries the target identity and host-key policy so
/// `check_server_key` can do trust-on-first-use when enabled.
pub struct VpsClientHandler {
    pub host: String,
    pub port: u16,
    pub policy: HostKeyPolicy,
}

impl russh::client::Handler for VpsClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let path = match &self.policy {
            HostKeyPolicy::AcceptAny => return Ok(true),
            HostKeyPolicy::Tofu(p) => p,
        };

        // Serialize the key to its authorized_keys form for storage/comparison.
        let key_line = match server_public_key.to_openssh() {
            Ok(k) => k.trim().to_string(),
            Err(_) => return Ok(false), // unencodable key — refuse rather than guess
        };
        let id = format!("{}:{}", self.host, self.port);

        // Look for an existing pin for this host:port.
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((stored_id, stored_key)) = line.split_once(' ') {
                    if stored_id == id {
                        if stored_key.trim() == key_line {
                            return Ok(true);
                        }
                        tracing::error!(
                            host = %id,
                            "host key MISMATCH — refusing connection (possible MITM). \
                             Remove the stale line from {} if this change is expected.",
                            path.display()
                        );
                        return Ok(false);
                    }
                }
            }
        }

        // First time we've seen this host — learn it (TOFU).
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{} {}", id, key_line);
        }
        let fp = server_public_key.fingerprint(Default::default());
        tracing::info!(host = %id, fingerprint = %fp, "learned new host key (TOFU)");
        Ok(true)
    }
}

/// A single live SSH session.
#[allow(dead_code)]
pub struct SshConnection {
    pub handle: Handle<VpsClientHandler>,
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// Thread-safe pool of active SSH connections.
///
/// Outer Mutex guards the map (insert/remove).
/// Inner Mutex guards one connection during SSH operations.
/// This allows concurrent tool calls on different connections
/// without blocking each other.
#[derive(Clone, Default)]
pub struct ConnectionPool {
    inner: Arc<Mutex<HashMap<String, Arc<Mutex<SshConnection>>>>>,
}

impl ConnectionPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, id: String, conn: SshConnection) {
        self.inner
            .lock()
            .await
            .insert(id, Arc::new(Mutex::new(conn)));
    }

    pub async fn get(&self, id: &str) -> Option<Arc<Mutex<SshConnection>>> {
        self.inner.lock().await.get(id).cloned()
    }

    pub async fn remove(&self, id: &str) -> bool {
        self.inner.lock().await.remove(id).is_some()
    }
}

/// Execute a command on a live SSH handle and return (exit_code, stdout, stderr).
/// Each call opens a fresh session channel, runs the command, and closes the channel.
///
/// On timeout this **terminates the remote command** (best-effort `SIGTERM` then
/// channel EOF/close) and returns an `Err` — it does NOT silently return an empty
/// `Ok`. That distinction matters: a silent timeout previously let long commands
/// (e.g. `certbot renew`, `apt upgrade`) keep running server-side and hold locks
/// while the caller saw an empty success. Note that OpenSSH commonly ignores
/// `signal` requests on a non-PTY exec channel, so termination is best-effort;
/// for genuinely long operations prefer the run-detached-and-poll pattern
/// (see the vps-management skill) over a large timeout.
pub async fn exec_command(
    handle: &mut Handle<VpsClientHandler>,
    command: &str,
    timeout_ms: u64,
) -> anyhow::Result<(i32, String, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = 0i32;
    let mut timed_out = false;

    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        match tokio::time::timeout_at(deadline, channel.wait()).await {
            Err(_elapsed) => {
                timed_out = true;
                break;
            }
            Ok(None) => break,
            Ok(Some(russh::ChannelMsg::Data { ref data })) => {
                stdout.push_str(&String::from_utf8_lossy(data));
            }
            Ok(Some(russh::ChannelMsg::ExtendedData { ref data, ext })) if ext == 1 => {
                stderr.push_str(&String::from_utf8_lossy(data));
            }
            Ok(Some(russh::ChannelMsg::ExitStatus { exit_status })) => {
                exit_code = exit_status as i32;
            }
            Ok(Some(russh::ChannelMsg::Eof)) => {}
            Ok(Some(_)) => {}
        }
    }

    if timed_out {
        // Best-effort terminate the remote command so it can't linger and hold
        // locks, then tear the channel down.
        let _ = channel.signal(russh::Sig::TERM).await;
        let _ = channel.eof().await;
        let _ = channel.close().await;
        anyhow::bail!(
            "command timed out after {timeout_ms} ms (remote command signaled to terminate; \
             for long-running operations run them detached and poll a log file)"
        );
    }

    Ok((
        exit_code,
        stdout.trim().to_string(),
        stderr.trim().to_string(),
    ))
}
