//! Session abstraction: a uniform interface over SSH, Telnet, and serial
//! transports, plus the registry that owns live interactive sessions.

pub mod manager;
pub mod serial;
pub mod ssh;
pub mod telnet;

pub use manager::{SessionManager, DEFAULT_PROMPT};

use crate::error::{ErrorKind, ToolError, ToolResult};
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Safety cap on a single command's accumulated output, so a runaway device or
/// an enormous dump (e.g. `show tech-support`) can't grow memory unbounded.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum `--More--` pages auto-advanced for a single command (backstop against
/// a never-ending pager). Prefer `terminal length 0` to disable paging entirely.
const MAX_PAGES: u32 = 4096;

/// True if a detected sub-prompt is the CLI pager (`--More--`).
fn is_pager(sub_prompt: &str) -> bool {
    let s = sub_prompt.to_lowercase();
    s.contains("more")
}

/// Remove `--More--` pager markers (and trailing spaces) from a page of output.
fn strip_pager(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"--\s*[Mm]ore\s*--\s*").expect("valid pager regex"));
    re.replace_all(s, "").into_owned()
}

/// Common interactive sub-prompts a device emits mid-command that are NOT the
/// normal CLI prompt: password/username challenges, confirmations, and the
/// pager. Detecting these lets `run_command` return immediately instead of
/// waiting out the whole timeout for a prompt that will never come.
fn subprompt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(password:|username:|login:|\[confirm\]|\[yes/no\]|\(yes/no\)|--\s*more\s*--)\s*$",
        )
        .expect("valid subprompt regex")
    })
}

/// Which wire protocol a session speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Ssh,
    Telnet,
    Serial,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Protocol::Ssh => "ssh",
            Protocol::Telnet => "telnet",
            Protocol::Serial => "serial",
        };
        f.write_str(s)
    }
}

impl Protocol {
    /// The line terminator a command should be sent with for this transport.
    ///
    /// Serial console lines (and many switch CLIs over a raw console) treat a
    /// bare carriage return as Enter and ignore a lone line feed; sending a
    /// trailing `\n` after the `\r` is also dangerous because it can be consumed
    /// as an empty entry at a sub-prompt (e.g. submitting a blank enable
    /// password). So serial uses CR only. Telnet's NVT standard is CR-LF. SSH
    /// runs through a PTY that expects LF.
    pub fn line_ending(&self) -> &'static str {
        match self {
            Protocol::Serial => "\r",
            Protocol::Telnet => "\r\n",
            Protocol::Ssh => "\n",
        }
    }
}

/// The result of reading device output up to (or until timing out waiting for)
/// the prompt.
#[derive(Debug, Clone, Serialize)]
pub struct ReadOutcome {
    /// Cleaned output (ANSI stripped, echoed command and trailing prompt removed).
    pub output: String,
    /// The device CLI prompt text that terminated the read, if matched.
    pub matched_prompt: Option<String>,
    /// An interactive sub-prompt that terminated the read (e.g. "Password:",
    /// "[confirm]", "--More--"), if one was detected instead of the CLI prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_prompt: Option<String>,
    /// True if neither prompt nor sub-prompt was seen before the timeout —
    /// `output` is partial.
    pub timed_out: bool,
    /// True if the output hit the size cap and was truncated.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Lowest-level transport contract. Each protocol implements raw byte write and
/// a timeout-bounded read; the shared [`read_until_prompt`] loop builds command
/// semantics on top.
#[async_trait]
pub trait Transport: Send {
    /// Write raw bytes to the device.
    async fn write_all(&mut self, data: &[u8]) -> ToolResult<()>;

    /// Read whatever data is available, waiting at most `timeout`. Returns an
    /// empty vec on an idle tick (no data yet) — not an error. Returns an error
    /// only if the connection is broken.
    async fn read_chunk(&mut self, timeout: Duration) -> ToolResult<Vec<u8>>;

    /// Close the underlying connection.
    async fn close(&mut self) -> ToolResult<()>;

    fn protocol(&self) -> Protocol;

    /// Upload a local file to the device. Default: unsupported (only SSH/SFTP
    /// implements this). Returns bytes transferred.
    async fn upload_file(&mut self, _local: &Path, _remote: &str) -> ToolResult<u64> {
        Err(ToolError::new(
            ErrorKind::TransferFailed,
            "file transfer is only supported over SSH (SFTP)",
        ))
    }

    /// Download a remote file to a local path. Default: unsupported. Returns
    /// bytes transferred.
    async fn download_file(&mut self, _remote: &str, _local: &Path) -> ToolResult<u64> {
        Err(ToolError::new(
            ErrorKind::TransferFailed,
            "file transfer is only supported over SSH (SFTP)",
        ))
    }
}

/// A live, named session: a boxed transport plus the metadata describing it.
pub struct Session {
    pub name: String,
    pub protocol: Protocol,
    /// Human-readable target (host:port or COM port) for `list_sessions`.
    pub target: String,
    /// Prompt regex used to detect end-of-output.
    pub prompt: Regex,
    transport: Box<dyn Transport>,
}

impl Session {
    pub fn new(
        name: String,
        target: String,
        prompt: Regex,
        transport: Box<dyn Transport>,
    ) -> Self {
        let protocol = transport.protocol();
        Self { name, protocol, target, prompt, transport }
    }

    /// Write `text` followed by the protocol's line ending.
    pub async fn send_line(&mut self, text: &str) -> ToolResult<()> {
        let mut line = text.to_string();
        line.push_str(self.protocol.line_ending());
        self.transport.write_all(line.as_bytes()).await
    }

    /// Send a command (the protocol's line ending is appended) and read the
    /// device's response up to the prompt or `timeout`. A `--More--` pager is
    /// auto-advanced (space) and the pages are stitched together.
    pub async fn run_command(&mut self, command: &str, timeout: Duration) -> ToolResult<ReadOutcome> {
        self.run_command_inner(command, timeout, false).await
    }

    /// As `run_command`, but `raw` sends the command bytes verbatim with no line
    /// ending appended (for control characters or precise sub-prompt replies).
    pub async fn run_command_raw(
        &mut self,
        command: &str,
        timeout: Duration,
        raw: bool,
    ) -> ToolResult<ReadOutcome> {
        self.run_command_inner(command, timeout, raw).await
    }

    async fn run_command_inner(
        &mut self,
        command: &str,
        timeout: Duration,
        raw: bool,
    ) -> ToolResult<ReadOutcome> {
        // Drain any unsolicited banner/output already buffered so it doesn't
        // contaminate this command's response. Best-effort, short budget.
        let _ = self.transport.read_chunk(Duration::from_millis(50)).await;

        if raw {
            self.transport.write_all(command.as_bytes()).await?;
        } else {
            self.send_line(command).await?;
        }

        let mut acc = String::new();
        let mut pages = 0u32;
        loop {
            let outcome =
                read_until_prompt(self.transport.as_mut(), &self.prompt, timeout, true).await?;
            let is_more = outcome
                .sub_prompt
                .as_deref()
                .map(is_pager)
                .unwrap_or(false);

            if is_more && pages < MAX_PAGES {
                pages += 1;
                acc.push_str(&strip_pager(&outcome.output));
                // Advance the pager with a space (no line ending).
                self.transport.write_all(b" ").await?;
                continue;
            }

            // Terminal page (CLI prompt, real sub-prompt, timeout, or page cap).
            acc.push_str(&strip_pager(&outcome.output));
            let strip_prompt = outcome.matched_prompt.is_some();
            return Ok(ReadOutcome {
                output: clean_output(&acc, command, strip_prompt),
                ..outcome
            });
        }
    }

    /// Enter privileged (enable) mode, supplying `password` if challenged. The
    /// password is never echoed back. Returns the landing outcome.
    pub async fn enable(&mut self, password: &str, timeout: Duration) -> ToolResult<ReadOutcome> {
        let first = self.run_command("enable", timeout).await?;
        // Already at a CLI prompt → already privileged or no password required.
        if first.matched_prompt.is_some() {
            return Ok(first);
        }
        // Only proceed if the device actually asked for a password.
        let wants_pw = first
            .sub_prompt
            .as_deref()
            .map(|s| s.to_lowercase().contains("password"))
            .unwrap_or(false);
        if !wants_pw {
            return Ok(first);
        }
        self.send_line(password).await?;
        read_until_prompt(self.transport.as_mut(), &self.prompt, timeout, true).await
    }

    /// Log in over a console/vty that prompts for username and/or password.
    /// Secrets are never echoed back. Returns the landing outcome.
    pub async fn login(
        &mut self,
        username: Option<&str>,
        password: &str,
        timeout: Duration,
    ) -> ToolResult<ReadOutcome> {
        // Nudge the line to elicit a login prompt.
        self.send_line("").await?;
        let mut outcome =
            read_until_prompt(self.transport.as_mut(), &self.prompt, timeout, true).await?;

        // Username challenge.
        let asks_user = outcome
            .sub_prompt
            .as_deref()
            .map(|s| {
                let s = s.to_lowercase();
                s.contains("username") || s.contains("login")
            })
            .unwrap_or(false);
        if asks_user {
            self.send_line(username.unwrap_or("")).await?;
            outcome = read_until_prompt(self.transport.as_mut(), &self.prompt, timeout, true).await?;
        }

        // Password challenge.
        let asks_pw = outcome
            .sub_prompt
            .as_deref()
            .map(|s| s.to_lowercase().contains("password"))
            .unwrap_or(false);
        if asks_pw {
            self.send_line(password).await?;
            outcome = read_until_prompt(self.transport.as_mut(), &self.prompt, timeout, true).await?;
        }
        Ok(outcome)
    }

    /// Wait for `expect` to appear, then send `send` (no trailing newline added;
    /// caller controls it). Used for enable passwords, `[confirm]`, pagination.
    pub async fn expect_send(
        &mut self,
        expect: &Regex,
        send: &str,
        timeout: Duration,
    ) -> ToolResult<ReadOutcome> {
        // expect patterns may appear mid-line (e.g. "[confirm]"), so search the
        // whole buffer rather than only the tail line.
        let outcome = read_until_prompt(self.transport.as_mut(), expect, timeout, false).await?;
        if !outcome.timed_out {
            self.transport.write_all(send.as_bytes()).await?;
        }
        Ok(outcome)
    }

    /// Upload a local file to the device (SSH/SFTP only). Returns bytes sent.
    pub async fn upload(&mut self, local: &Path, remote: &str) -> ToolResult<u64> {
        self.transport.upload_file(local, remote).await
    }

    /// Download a remote file to a local path (SSH/SFTP only). Returns bytes received.
    pub async fn download(&mut self, remote: &str, local: &Path) -> ToolResult<u64> {
        self.transport.download_file(remote, local).await
    }

    pub async fn close(&mut self) -> ToolResult<()> {
        self.transport.close().await
    }
}

/// Read from a transport, accumulating output until `pattern` matches or
/// `overall_timeout` elapses.
///
/// When `anchor_tail` is true the pattern is tested only against the current
/// last line (the device's re-displayed prompt); this avoids false hits on an
/// earlier output line that happens to look like a prompt. When false the whole
/// buffer is searched — used by `expect_send` for mid-stream patterns.
pub async fn read_until_prompt(
    transport: &mut dyn Transport,
    pattern: &Regex,
    overall_timeout: Duration,
    anchor_tail: bool,
) -> ToolResult<ReadOutcome> {
    let deadline = Instant::now() + overall_timeout;
    let mut raw = String::new();

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(ReadOutcome {
                output: strip_ansi(&raw),
                matched_prompt: None,
                sub_prompt: None,
                timed_out: true,
                truncated: false,
            });
        }
        // Poll in short slices so we re-check the pattern promptly after each
        // burst of data, while never overrunning the overall deadline.
        let slice = std::cmp::min(deadline - now, Duration::from_millis(300));
        let chunk = transport.read_chunk(slice).await?;
        if !chunk.is_empty() {
            raw.push_str(&String::from_utf8_lossy(&chunk));
        }

        // Output cap: stop accumulating if a command floods us.
        if raw.len() > MAX_OUTPUT_BYTES {
            return Ok(ReadOutcome {
                output: strip_ansi(&raw),
                matched_prompt: None,
                sub_prompt: None,
                timed_out: false,
                truncated: true,
            });
        }

        let cleaned = strip_ansi(&raw);
        // Owned so it doesn't borrow `cleaned` across the moves below.
        let tail = cleaned.rsplit('\n').next().unwrap_or("").to_string();

        // Primary pattern: tail-anchored for the CLI prompt (run_command), or
        // anywhere in the buffer for a caller-supplied expect pattern.
        let hit = if anchor_tail {
            if tail.trim().is_empty() {
                None
            } else {
                pattern.find(&tail).map(|m| m.as_str().trim().to_string())
            }
        } else {
            pattern.find(&cleaned).map(|m| m.as_str().trim().to_string())
        };
        if let Some(matched) = hit {
            return Ok(ReadOutcome {
                output: cleaned,
                matched_prompt: Some(matched),
                sub_prompt: None,
                timed_out: false,
                truncated: false,
            });
        }

        // Sub-prompt detection (only on the command path): if the device is
        // waiting at a Password:/[confirm]/--More-- prompt, return now rather
        // than burning the whole timeout waiting for a CLI prompt.
        if anchor_tail {
            if let Some(m) = subprompt_re().find(&tail) {
                return Ok(ReadOutcome {
                    output: cleaned,
                    matched_prompt: None,
                    sub_prompt: Some(m.as_str().trim().to_string()),
                    timed_out: false,
                    truncated: false,
                });
            }
        }
    }
}

/// Remove ANSI/VT100 escape sequences and carriage returns that switches emit.
pub fn strip_ansi(s: &str) -> String {
    // CSI sequences: ESC [ ... final-byte ; plus lone ESC sequences.
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '\u{1b}' {
            // Skip until a letter (CSI final byte) or end.
            i += 1;
            if i < bytes.len() && bytes[i] == '[' {
                i += 1;
                while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // consume final byte
                }
            } else {
                // Lone ESC or other — skip one extra char.
                if i < bytes.len() {
                    i += 1;
                }
            }
        } else if c == '\r' || c == '\u{8}' {
            i += 1; // drop carriage returns and backspaces (the pager erases with \b)
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Strip the echoed command (first line, if it equals the command) and,
/// when `strip_prompt` is true, the trailing CLI prompt line.
///
/// `strip_prompt` must be false when the read stopped at a sub-prompt
/// (e.g. `Password:`, `--More--`) or on timeout — otherwise that text would be
/// discarded and the caller would see confusing empty output.
pub fn clean_output(cleaned: &str, command: &str, strip_prompt: bool) -> String {
    let mut lines: Vec<&str> = cleaned.split('\n').collect();

    // Remove a leading echoed command line.
    if let Some(first) = lines.first() {
        if first.trim() == command.trim() {
            lines.remove(0);
        }
    }

    if strip_prompt {
        // Remove the trailing prompt line the device re-displayed. read_until_prompt
        // stops right after the prompt, so drop the final non-empty line.
        while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            lines.pop();
        }
        if !lines.is_empty() {
            lines.pop(); // the prompt line itself
        }
    }
    lines.join("\n").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_cr() {
        let s = "\u{1b}[2J\u{1b}[Hhello\r\nworld\r\n";
        assert_eq!(strip_ansi(s), "hello\nworld\n");
    }

    #[test]
    fn cleans_echo_and_prompt() {
        let cleaned = "show version\nCisco IOS Software\nuptime 5 days\nswitch#";
        let out = clean_output(cleaned, "show version", true);
        assert_eq!(out, "Cisco IOS Software\nuptime 5 days");
    }

    #[test]
    fn keeps_subprompt_text_when_not_stripping() {
        // The "enable" case: device echoed the command then asked for a password.
        // With strip_prompt=false the Password: line must be preserved.
        let cleaned = "enable\nPassword: ";
        let out = clean_output(cleaned, "enable", false);
        assert_eq!(out, "Password:");
    }

    #[test]
    fn subprompt_regex_matches_interactive_prompts() {
        let re = super::subprompt_re();
        assert!(re.is_match("Password: "));
        assert!(re.is_match("Username:"));
        assert!(re.is_match("Proceed with reload? [confirm]"));
        assert!(re.is_match("Save? [yes/no]"));
        assert!(re.is_match(" --More-- "));
        // Must NOT fire on ordinary output that merely mentions the word.
        assert!(!re.is_match("the password was changed yesterday"));
        assert!(!re.is_match("switch#"));
    }

    #[test]
    fn line_endings_are_protocol_specific() {
        // Serial must be CR-only: a trailing LF can submit a blank entry at a
        // sub-prompt (e.g. an empty enable password).
        assert_eq!(Protocol::Serial.line_ending(), "\r");
        assert_eq!(Protocol::Telnet.line_ending(), "\r\n");
        assert_eq!(Protocol::Ssh.line_ending(), "\n");
    }

    #[test]
    fn default_prompt_matches_common_devices() {
        let re = Regex::new(crate::transport::manager::DEFAULT_PROMPT).unwrap();
        assert!(re.is_match("\nswitch#"));
        assert!(re.is_match("\nRouter>"));
        assert!(re.is_match("\nuser@host:~$ "));
        assert!(re.is_match("\ncore-sw-01(config)#"));
    }
}
