//! SSH transport via russh (pure Rust — no native deps). Opens an interactive
//! shell channel so switch CLIs that expect a PTY behave normally. File
//! transfer (`upload_file`/`download_file`) runs over SFTP on a side channel.
//!
//! This is the interactive-console SSH path (persistent PTY shell), distinct
//! from the exec-per-channel path in `state.rs` used by the server-ops tools.

use super::{Protocol, Transport};
use crate::error::{ErrorKind, ToolError, ToolResult};
use async_trait::async_trait;
use russh::client::{self, AuthResult, Handle, Msg};
use russh::keys::{Algorithm, EcdsaCurve, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{cipher, kex, Channel, ChannelMsg, Disconnect, Preferred};
use russh_sftp::client::SftpSession;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// Algorithm preferences with legacy fallback for old network gear.
///
/// russh's defaults are modern-only, but 2015–2017-era IOS-XE (e.g. Catalyst
/// 3850 on 3.x) offers only `diffie-hellman-group14-sha1`/`group1-sha1` key
/// exchange, an `ssh-rsa` host key, and CTR/CBC ciphers — the default config
/// refuses the handshake outright. Modern algorithms stay FIRST in every list,
/// so current devices still negotiate strong crypto; the legacy entries are
/// only used when they're all the server has.
fn legacy_compatible_preferred() -> Preferred {
    Preferred {
        kex: Cow::Borrowed(&[
            kex::CURVE25519,
            kex::CURVE25519_PRE_RFC_8731,
            kex::DH_G16_SHA512,
            kex::DH_G14_SHA256,
            // Legacy fallback (old IOS/IOS-XE, ancient appliances):
            kex::DH_G14_SHA1,
            kex::DH_G1_SHA1,
            kex::EXTENSION_SUPPORT_AS_CLIENT,
            kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
        ]),
        key: Cow::Borrowed(&[
            Algorithm::Ed25519,
            Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 },
            Algorithm::Ecdsa { curve: EcdsaCurve::NistP521 },
            Algorithm::Rsa { hash: Some(HashAlg::Sha256) },
            Algorithm::Rsa { hash: Some(HashAlg::Sha512) },
            // Legacy fallback: SHA-1-signed RSA host keys (only host-key type
            // old IOS-XE presents) — `ssh-rsa`.
            Algorithm::Rsa { hash: None },
        ]),
        cipher: Cow::Borrowed(&[
            cipher::CHACHA20_POLY1305,
            cipher::AES_256_GCM,
            cipher::AES_256_CTR,
            cipher::AES_192_CTR,
            cipher::AES_128_CTR,
            // Legacy fallback:
            cipher::AES_256_CBC,
            cipher::AES_192_CBC,
            cipher::AES_128_CBC,
            cipher::TRIPLE_DES_CBC,
        ]),
        // Default MAC order already ends with hmac-sha1; default compression is fine.
        ..Preferred::DEFAULT
    }
}

/// russh event handler. We accept any host key (trust-on-first-use): switches
/// in a management network are reached by address, and host-key pinning is out
/// of scope for the interactive-console path (the server-ops path in `state.rs`
/// offers optional TOFU pinning via `VPS_HOST_KEY_CHECK`).
struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Parameters needed to establish an SSH session.
pub struct SshAuth<'a> {
    pub host: &'a str,
    pub port: u16,
    pub username: &'a str,
    pub password: Option<&'a str>,
    pub key_path: Option<&'a Path>,
    pub passphrase: Option<&'a str>,
}

pub struct SshTransport {
    handle: Handle<Client>,
    channel: Channel<Msg>,
    closed: bool,
}

impl SshTransport {
    pub async fn connect(auth: SshAuth<'_>) -> ToolResult<Self> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(3600)),
            preferred: legacy_compatible_preferred(),
            ..Default::default()
        });

        let mut handle = tokio::time::timeout(
            Duration::from_secs(15),
            client::connect(config, (auth.host, auth.port), Client),
        )
        .await
        .map_err(|_| {
            ToolError::new(
                ErrorKind::ConnectFailed,
                format!("ssh connect to {}:{} timed out", auth.host, auth.port),
            )
        })?
        .map_err(|e| {
            ToolError::new(
                ErrorKind::ConnectFailed,
                format!("ssh connect to {}:{}: {e}", auth.host, auth.port),
            )
        })?;

        // Authenticate: prefer key if supplied, else password.
        let result = if let Some(key_path) = auth.key_path {
            let pem = tokio::fs::read_to_string(key_path).await.map_err(|e| {
                ToolError::new(
                    ErrorKind::AuthFailed,
                    format!("read private key '{}': {e}", key_path.display()),
                )
            })?;
            let private_key = PrivateKey::from_openssh(pem.as_bytes()).map_err(|e| {
                ToolError::new(
                    ErrorKind::AuthFailed,
                    format!("parse private key '{}': {e}", key_path.display()),
                )
            })?;
            let private_key = if private_key.is_encrypted() {
                let pass = auth.passphrase.ok_or_else(|| {
                    ToolError::new(
                        ErrorKind::AuthFailed,
                        "private key is encrypted but no passphrase was provided",
                    )
                })?;
                private_key.decrypt(pass.as_bytes()).map_err(|e| {
                    ToolError::new(ErrorKind::AuthFailed, format!("decrypt private key: {e}"))
                })?
            } else {
                private_key
            };
            handle
                .authenticate_publickey(
                    auth.username,
                    PrivateKeyWithHashAlg::new(Arc::new(private_key), None),
                )
                .await
                .map_err(|e| ToolError::new(ErrorKind::AuthFailed, e.to_string()))?
        } else if let Some(pw) = auth.password {
            handle
                .authenticate_password(auth.username, pw)
                .await
                .map_err(|e| ToolError::new(ErrorKind::AuthFailed, e.to_string()))?
        } else {
            return Err(ToolError::new(
                ErrorKind::AuthFailed,
                "no password or key_path supplied for SSH",
            ));
        };

        if result != AuthResult::Success {
            return Err(ToolError::new(
                ErrorKind::AuthFailed,
                "SSH authentication rejected (bad username/password/key)",
            ));
        }

        let channel = handle.channel_open_session().await.map_err(|e| {
            ToolError::internal(format!("open ssh session channel: {e}"))
        })?;
        channel
            .request_pty(false, "xterm", 200, 50, 0, 0, &[])
            .await
            .map_err(|e| ToolError::internal(format!("request pty: {e}")))?;
        channel
            .request_shell(true)
            .await
            .map_err(|e| ToolError::internal(format!("request shell: {e}")))?;

        Ok(Self { handle, channel, closed: false })
    }

    /// Open a fresh SFTP session on a side channel of the same connection.
    async fn sftp(&self) -> ToolResult<SftpSession> {
        let channel = self.handle.channel_open_session().await.map_err(|e| {
            ToolError::new(ErrorKind::TransferFailed, format!("open sftp channel: {e}"))
        })?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| ToolError::new(ErrorKind::TransferFailed, format!("request sftp: {e}")))?;
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| ToolError::new(ErrorKind::TransferFailed, format!("start sftp: {e}")))
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn write_all(&mut self, data: &[u8]) -> ToolResult<()> {
        // `Channel::data` takes any AsyncRead; a byte slice qualifies.
        self.channel
            .data(data)
            .await
            .map_err(|e| ToolError::internal(format!("ssh write: {e}")))
    }

    async fn read_chunk(&mut self, timeout: Duration) -> ToolResult<Vec<u8>> {
        if self.closed {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(r) if !r.is_zero() => r,
                _ => return Ok(Vec::new()),
            };
            match tokio::time::timeout(remaining, self.channel.wait()).await {
                Err(_) => return Ok(Vec::new()), // idle tick
                Ok(None) => {
                    self.closed = true;
                    return Ok(Vec::new());
                }
                Ok(Some(msg)) => match msg {
                    ChannelMsg::Data { data } => return Ok(data.to_vec()),
                    ChannelMsg::ExtendedData { data, .. } => return Ok(data.to_vec()),
                    ChannelMsg::Eof | ChannelMsg::Close => {
                        self.closed = true;
                        return Ok(Vec::new());
                    }
                    // ExitStatus, WindowAdjusted, Success, etc. — keep waiting.
                    _ => continue,
                },
            }
        }
    }

    async fn close(&mut self) -> ToolResult<()> {
        let _ = self.channel.eof().await;
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "", "en")
            .await;
        self.closed = true;
        Ok(())
    }

    fn protocol(&self) -> Protocol {
        Protocol::Ssh
    }

    async fn upload_file(&mut self, local: &Path, remote: &str) -> ToolResult<u64> {
        let sftp = self.sftp().await?;
        let mut src = tokio::fs::File::open(local).await.map_err(|e| {
            ToolError::new(
                ErrorKind::TransferFailed,
                format!("open local file '{}': {e}", local.display()),
            )
        })?;
        let mut dst = sftp.create(remote).await.map_err(|e| {
            ToolError::new(ErrorKind::TransferFailed, format!("create remote '{remote}': {e}"))
        })?;
        let n = tokio::io::copy(&mut src, &mut dst)
            .await
            .map_err(|e| ToolError::new(ErrorKind::TransferFailed, format!("upload copy: {e}")))?;
        dst.flush()
            .await
            .map_err(|e| ToolError::new(ErrorKind::TransferFailed, format!("upload flush: {e}")))?;
        Ok(n)
    }

    async fn download_file(&mut self, remote: &str, local: &Path) -> ToolResult<u64> {
        let sftp = self.sftp().await?;
        let mut src = sftp.open(remote).await.map_err(|e| {
            ToolError::new(ErrorKind::TransferFailed, format!("open remote '{remote}': {e}"))
        })?;
        let mut dst = tokio::fs::File::create(local).await.map_err(|e| {
            ToolError::new(
                ErrorKind::TransferFailed,
                format!("create local file '{}': {e}", local.display()),
            )
        })?;
        let n = tokio::io::copy(&mut src, &mut dst)
            .await
            .map_err(|e| ToolError::new(ErrorKind::TransferFailed, format!("download copy: {e}")))?;
        dst.flush()
            .await
            .map_err(|e| ToolError::new(ErrorKind::TransferFailed, format!("download flush: {e}")))?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old IOS-XE (Catalyst 3850 on 3.x) needs SHA-1 DH kex, ssh-rsa host keys,
    /// and CBC fallback ciphers. Guard that the preference lists keep them, and
    /// that modern algorithms still come first.
    #[test]
    fn preferred_includes_legacy_algorithms_after_modern_ones() {
        let p = legacy_compatible_preferred();

        let kex: Vec<&str> = p.kex.iter().map(|n| n.as_ref()).collect();
        assert!(kex.contains(&"diffie-hellman-group14-sha1"));
        assert!(kex.contains(&"diffie-hellman-group1-sha1"));
        assert_eq!(kex[0], "curve25519-sha256", "modern kex must stay first");

        let ciphers: Vec<&str> = p.cipher.iter().map(|n| n.as_ref()).collect();
        assert!(ciphers.contains(&"aes128-cbc"));
        assert!(ciphers.contains(&"3des-cbc"));
        assert_eq!(ciphers[0], "chacha20-poly1305@openssh.com");
    }
}
