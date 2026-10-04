//! OpenSSH client-config (`~/.ssh/config`) resolution and ssh-agent authentication.
//!
//! `russh` implements the SSH protocol and nothing around it: it does not read
//! the user's client config, and it does not talk to ssh-agent. Both tool
//! families therefore used to demand an explicit host, username and either a
//! password or a key path — so a host that plain `ssh <alias>` reaches with no
//! arguments at all could not be reached here without restating its whole
//! configuration.
//!
//! This module supplies the two missing pieces and is shared by the server-ops
//! path (`ssh_connect`) and the interactive-console path (`connect`):
//!
//! * [`lookup`] resolves a `Host` alias to HostName/User/Port/IdentityFile by
//!   asking the local `ssh` for its own answer (`ssh -G`), rather than
//!   re-implementing OpenSSH's config semantics.
//! * [`authenticate`] runs the auth ladder — password, then each candidate
//!   identity file, then every identity held by the agent.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use russh::client::{AuthResult, Handle, Handler};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use std::sync::Arc;

/// Connection defaults resolved for one `Host` alias.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostDefaults {
    /// `HostName` — the address to actually dial.
    pub hostname: Option<String>,
    /// `Port`.
    pub port: Option<u16>,
    /// `User`.
    pub username: Option<String>,
    /// `IdentityFile` entries, in the order OpenSSH would try them.
    pub identity_files: Vec<PathBuf>,
}

/// Resolve `alias` the way `ssh` itself would, by asking it: `ssh -G <alias>`
/// prints the effective configuration after evaluating `Host` *and* `Match`
/// blocks, `Include`s, percent tokens, canonicalization and the built-in
/// defaults — including `User` = the local account, which is why an alias
/// with no `User` line still connects.
///
/// Never fails: no `ssh` on PATH, or a config `ssh` refuses to load, yields
/// the local account name and nothing else, so an explicit host/username
/// still connects.
pub async fn lookup(alias: &str) -> HostDefaults {
    ssh_g(alias, None).await
}

/// [`lookup`] against an explicit config file (`ssh -G -F <path>`).
#[cfg(test)]
pub async fn lookup_in(alias: &str, path: &Path) -> HostDefaults {
    ssh_g(alias, Some(path)).await
}

/// The account this process runs as, used only when `ssh` cannot be asked.
/// `USER` on unix, `USERNAME` on Windows.
fn local_username() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty())
}

async fn ssh_g(alias: &str, config: Option<&Path>) -> HostDefaults {
    let mut cmd = tokio::process::Command::new("ssh");
    if let Some(path) = config {
        cmd.arg("-F").arg(path);
    }
    // `--` keeps an alias that starts with '-' from being read as an option.
    cmd.arg("-G").arg("--").arg(alias);
    // CREATE_NO_WINDOW: without it every connect flashes a console window when
    // the MCP client is a GUI app.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let out = match cmd.output().await {
        // Exit 255 means ssh rejected the config file itself; there is nothing
        // to resolve and plain `ssh` would fail the same way.
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            tracing::warn!(
                alias,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "ssh -G failed; connecting with explicit arguments only"
            );
            return HostDefaults { username: local_username(), ..Default::default() };
        }
        Err(e) => {
            tracing::debug!(alias, error = %e, "no ssh binary; skipping client-config lookup");
            return HostDefaults { username: local_username(), ..Default::default() };
        }
    };

    let mut defaults = HostDefaults::default();
    for line in String::from_utf8_lossy(&out).lines() {
        // One `key value` pair per line, value verbatim: it is already
        // unquoted, and paths may contain spaces.
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "hostname" => defaults.hostname = Some(value.to_string()),
            "user" => defaults.username = Some(value.to_string()),
            "port" => defaults.port = value.parse().ok(),
            // Repeated, most specific first; ssh -G leaves `~` unexpanded.
            "identityfile" => defaults.identity_files.push(expand_home(value)),
            _ => {}
        }
    }
    defaults
}

/// Expand a leading `~/`; russh opens these paths itself and does no globbing.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/").and_then(|rest| Some((dirs::home_dir()?, rest))) {
        Some((home, rest)) => home.join(rest),
        None => PathBuf::from(path),
    }
}

/// Everything needed to authenticate one session.
///
/// Owned rather than borrowed on purpose: the auth ladder is awaited from
/// inside `#[tool]` handlers, whose futures must be `Send` for *any* lifetime,
/// and a borrowed `&[PathBuf]` here makes that higher-ranked bound
/// unprovable. A handful of clones per connect is free next to a TCP + KEX
/// handshake.
pub struct Credentials {
    pub username: String,
    pub password: Option<String>,
    /// Candidate private keys, most specific first. Entries that do not exist
    /// on disk are skipped — a `Host *` block routinely names keys the user
    /// does not have.
    pub identity_files: Vec<PathBuf>,
    pub passphrase: Option<String>,
    /// Offer the keys held by ssh-agent. On by default; callers expose this so
    /// a user with a large agent can force one specific credential.
    pub use_agent: bool,
}

/// Which credential the server accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthUsed {
    Password,
    Key(PathBuf),
    /// ssh-agent identity, carrying the agent's comment for that key.
    Agent(String),
}

impl std::fmt::Display for AuthUsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthUsed::Password => write!(f, "password"),
            AuthUsed::Key(p) => write!(f, "key {}", p.display()),
            AuthUsed::Agent(c) if c.is_empty() => write!(f, "ssh-agent"),
            AuthUsed::Agent(c) => write!(f, "ssh-agent identity '{c}'"),
        }
    }
}

/// Try every supplied credential in OpenSSH-like order and return the one that
/// worked. The error lists each method that was attempted and why it failed —
/// "authentication rejected" alone is useless when four keys were offered.
///
/// Returns a boxed future rather than being an `async fn`: callers await this
/// from inside `#[tool]` handlers, and with a bare generic `async fn` rustc
/// tries to prove `Send` for every possible lifetime at the call site and
/// gives up ("implementation of Send is not general enough", rust#102211).
/// Boxing pins the obligation here, where it is provable.
pub fn authenticate<'a, H: Handler>(
    handle: &'a mut Handle<H>,
    creds: Credentials,
) -> Pin<Box<dyn Future<Output = Result<AuthUsed, String>> + Send + 'a>> {
    Box::pin(async move {
        let user = creds.username;
        let mut tried: Vec<String> = Vec::new();

        if let Some(password) = creds.password {
            match handle.authenticate_password(&user, &password).await {
                Ok(AuthResult::Success) => return Ok(AuthUsed::Password),
                Ok(_) => tried.push("password: rejected".to_string()),
                Err(e) => tried.push(format!("password: {e}")),
            }
        }

        for path in creds.identity_files {
            if !path.exists() {
                continue;
            }
            match try_identity_file(handle, &user, path.clone(), creds.passphrase.clone()).await {
                Ok(true) => return Ok(AuthUsed::Key(path)),
                Ok(false) => tried.push(format!("key {}: rejected", path.display())),
                Err(e) => tried.push(format!("key {}: {e}", path.display())),
            }
        }

        if creds.use_agent {
            match agent_auth(handle, &user).await {
                Ok(Some(comment)) => return Ok(AuthUsed::Agent(comment)),
                Ok(None) => {}
                Err(e) => tried.push(format!("ssh-agent: {e}")),
            }
        }

        Err(if tried.is_empty() {
            format!(
                "no credentials for user '{user}': supply a password or key, load a key into \
                 ssh-agent, or set IdentityFile for this host in ~/.ssh/config"
            )
        } else {
            format!("authentication failed for user '{user}' ({})", tried.join("; "))
        })
    })
}

/// Load one private key and offer it. `Ok(false)` means the server rejected the
/// key; `Err` means the key could not be used at all (unreadable, encrypted
/// without a passphrase, unsupported format).
async fn try_identity_file<H: Handler>(
    handle: &mut Handle<H>,
    user: &str,
    path: PathBuf,
    passphrase: Option<String>,
) -> Result<bool, String> {
    // load_secret_key is blocking (file IO + KDF for encrypted keys).
    let key = tokio::task::spawn_blocking(move || load_secret_key(&path, passphrase.as_deref()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    let hash_alg = match key.algorithm() {
        Algorithm::Rsa { .. } => best_rsa_hash(handle).await,
        _ => None,
    };

    handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg))
        .await
        .map(|r| r == AuthResult::Success)
        .map_err(|e| e.to_string())
}

type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// Offer every identity the agent holds. `Ok(None)` means there is no usable
/// agent — an ordinary situation, not an error worth reporting.
async fn agent_auth<H: Handler>(
    handle: &mut Handle<H>,
    user: &str,
) -> Result<Option<String>, String> {
    let Some(mut agent) = connect_agent().await else {
        return Ok(None);
    };

    let identities = agent
        .request_identities()
        .await
        .map_err(|e| format!("cannot list identities: {e}"))?;
    if identities.is_empty() {
        return Err("agent holds no identities (`ssh-add -l`)".to_string());
    }

    let mut rejected = 0usize;
    for identity in identities {
        let (public_key, comment) = match &identity {
            AgentIdentity::PublicKey { key, comment } => (Some(key.clone()), comment.clone()),
            AgentIdentity::Certificate {
                certificate,
                comment,
            } => (
                PublicKey::try_from(certificate.public_key().clone()).ok(),
                comment.clone(),
            ),
        };
        let Some(public_key) = public_key else {
            continue;
        };

        let hash_alg = match public_key.algorithm() {
            Algorithm::Rsa { .. } => best_rsa_hash(handle).await,
            _ => None,
        };

        // Boxed with an explicit `Send` bound: `authenticate_publickey_with`
        // takes the signer's `auth_sign` future, whose lifetime is
        // higher-ranked, and rustc cannot otherwise prove the enclosing tool
        // handler future is `Send` (rust#102211).
        let attempt: Pin<Box<dyn Future<Output = _> + Send>> = Box::pin(
            handle.authenticate_publickey_with(user, public_key, hash_alg, &mut agent),
        );
        match attempt.await {
            Ok(AuthResult::Success) => return Ok(Some(comment)),
            Ok(_) => rejected += 1,
            Err(e) => return Err(e.to_string()),
        }
    }

    Err(format!("{rejected} identit{} rejected", if rejected == 1 { "y" } else { "ies" }))
}

#[cfg(unix)]
async fn connect_agent() -> Option<DynAgent> {
    let socket = std::env::var("SSH_AUTH_SOCK").ok()?;
    match AgentClient::connect_uds(&socket).await {
        Ok(client) => Some(client.dynamic()),
        Err(e) => {
            tracing::debug!(socket, error = %e, "ssh-agent unavailable");
            None
        }
    }
}

#[cfg(windows)]
async fn connect_agent() -> Option<DynAgent> {
    // Windows OpenSSH exposes the agent as a named pipe. SSH_AUTH_SOCK is
    // honoured first so WSL/Git-Bash style bridges keep working.
    let pipe = std::env::var("SSH_AUTH_SOCK")
        .unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".to_string());
    match AgentClient::connect_named_pipe(&pipe).await {
        Ok(client) => Some(client.dynamic()),
        Err(e) => {
            tracing::debug!(pipe, error = %e, "ssh-agent unavailable");
            None
        }
    }
}

/// RSA signature algorithm the server actually accepts.
///
/// OpenSSH 8.8+ refuses SHA-1 `ssh-rsa` signatures, which is what russh sends
/// when no hash is given, so an RSA key silently fails everywhere modern
/// without this. The answer comes from the server's `server-sig-algs`
/// extension; legacy gear never sends one, hence the timeout and the SHA-1
/// fallback (`None`) that such devices still want.
async fn best_rsa_hash<H: Handler>(handle: &Handle<H>) -> Option<HashAlg> {
    tokio::time::timeout(Duration::from_secs(5), handle.best_supported_rsa_hash())
        .await
        .ok()?
        .ok()?
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `body` to a uniquely named temp file and hand back its path.
    fn temp_config(body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ssh-connect-test-{}-{:?}.config",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    /// The config tests drive the real `ssh` binary; a machine without one
    /// exercises only the fallback path (covered by its own test).
    fn ssh_missing() -> bool {
        std::process::Command::new("ssh").arg("-V").output().is_err()
    }

    #[tokio::test]
    async fn resolves_alias_to_connection_parameters() {
        if ssh_missing() {
            return;
        }
        let path = temp_config(
            "Host prod-web\n\
             \tHostName 203.0.113.10\n\
             \tUser deploy\n\
             \tPort 2222\n\
             \tIdentityFile ~/.ssh/id_deploy\n",
        );
        let resolved = lookup_in("prod-web", &path).await;
        std::fs::remove_file(&path).ok();

        assert_eq!(resolved.hostname.as_deref(), Some("203.0.113.10"));
        assert_eq!(resolved.username.as_deref(), Some("deploy"));
        assert_eq!(resolved.port, Some(2222));
        // ~ must be expanded: russh opens the path itself, and ssh -G prints
        // identity files unexpanded.
        let key = resolved.identity_files.first().unwrap();
        assert!(key.is_absolute(), "identity file not expanded: {}", key.display());
        assert!(key.ends_with(".ssh/id_deploy"));
    }

    #[tokio::test]
    async fn unknown_alias_still_picks_up_wildcard_defaults() {
        if ssh_missing() {
            return;
        }
        let path = temp_config(
            "Host prod-web\n\
             \tUser deploy\n\
             \n\
             Host *\n\
             \tUser fallback\n\
             \tPort 2022\n",
        );
        let resolved = lookup_in("some-other-box", &path).await;
        std::fs::remove_file(&path).ok();

        // No HostName anywhere: ssh dials the name it was given.
        assert_eq!(resolved.hostname.as_deref(), Some("some-other-box"));
        assert_eq!(resolved.username.as_deref(), Some("fallback"));
        assert_eq!(resolved.port, Some(2022));
    }

    #[tokio::test]
    async fn host_specific_value_wins_over_later_wildcard() {
        if ssh_missing() {
            return;
        }
        let path = temp_config(
            "Host prod-web\n\
             \tUser deploy\n\
             \n\
             Host *\n\
             \tUser fallback\n",
        );
        let resolved = lookup_in("prod-web", &path).await;
        std::fs::remove_file(&path).ok();

        assert_eq!(resolved.username.as_deref(), Some("deploy"));
    }

    #[tokio::test]
    async fn match_block_applies_to_its_own_host_only() {
        if ssh_missing() {
            return;
        }
        let path = temp_config(
            "Host bastion\n\
             \tUser deploy\n\
             \n\
             Match host secret-box\n\
             \tUser root\n",
        );
        let matched = lookup_in("secret-box", &path).await;
        let untouched = lookup_in("bastion", &path).await;
        std::fs::remove_file(&path).ok();

        assert_eq!(matched.username.as_deref(), Some("root"));
        // The Match body must not leak onto the preceding Host block.
        assert_eq!(untouched.username.as_deref(), Some("deploy"));
    }

    #[tokio::test]
    async fn real_directives_outside_our_field_set_are_harmless() {
        if ssh_missing() {
            return;
        }
        let path = temp_config(
            "Host gw\n\
             \tHostName 198.51.100.7\n\
             \tControlMaster auto\n\
             \tAddKeysToAgent yes\n",
        );
        let resolved = lookup_in("gw", &path).await;
        std::fs::remove_file(&path).ok();

        assert_eq!(resolved.hostname.as_deref(), Some("198.51.100.7"));
    }

    #[tokio::test]
    async fn unusable_config_degrades_to_the_local_account() {
        // ssh exits 255 on a config it cannot read or parse; resolution must
        // still hand back something connectable.
        let path = std::env::temp_dir().join("ssh-connect-definitely-absent.config");
        let resolved = lookup_in("anything", &path).await;

        assert_eq!(resolved.hostname, None);
        assert_eq!(resolved.username, local_username());
    }

    #[tokio::test]
    async fn alias_without_user_falls_back_to_local_account() {
        if ssh_missing() || local_username().is_none() {
            return;
        }
        let path = temp_config("Host lonely\n\tHostName 203.0.113.99\n");
        let resolved = lookup_in("lonely", &path).await;
        std::fs::remove_file(&path).ok();

        assert_eq!(resolved.username, local_username());
    }

    #[test]
    fn auth_used_renders_for_tool_output() {
        assert_eq!(AuthUsed::Password.to_string(), "password");
        assert_eq!(
            AuthUsed::Agent("id_ed25519_yubikey".into()).to_string(),
            "ssh-agent identity 'id_ed25519_yubikey'"
        );
        assert_eq!(AuthUsed::Agent(String::new()).to_string(), "ssh-agent");
        assert_eq!(
            AuthUsed::Key(PathBuf::from("/home/u/.ssh/id_ed25519")).to_string(),
            "key /home/u/.ssh/id_ed25519"
        );
    }
}
