//! Telnet transport: a TCP stream with inline IAC option negotiation handled
//! transparently. We decline most options and accept Suppress-Go-Ahead so the
//! stream behaves like a clean character pipe — which is all a switch CLI needs.

use super::{Protocol, Transport};
use crate::error::{ErrorKind, ToolError, ToolResult};
use async_trait::async_trait;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// Telnet command bytes (RFC 854).
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

// Options we are willing to engage.
const OPT_SGA: u8 = 3; // Suppress Go Ahead

/// Incremental IAC parser state, preserved across reads so escape sequences may
/// span TCP segment boundaries.
#[derive(Clone, Copy, PartialEq)]
enum State {
    Data,
    Iac,
    Negotiate(u8), // saw IAC + (DO|DONT|WILL|WONT); awaiting the option byte
    Subneg,        // inside IAC SB ... ; discard until IAC SE
    SubnegIac,     // inside subneg, saw IAC; SE ends it
}

/// Pure telnet IAC state machine — no I/O, so it is unit-testable on its own.
struct IacParser {
    state: State,
}

impl IacParser {
    fn new() -> Self {
        Self { state: State::Data }
    }

    /// Process a raw inbound buffer: extract application `data_out` and any
    /// negotiation `replies` that must be written back to the server.
    fn process(&mut self, input: &[u8], data_out: &mut Vec<u8>, replies: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                State::Data => {
                    if b == IAC {
                        self.state = State::Iac;
                    } else {
                        data_out.push(b);
                    }
                }
                State::Iac => match b {
                    IAC => {
                        data_out.push(IAC); // escaped literal 0xFF
                        self.state = State::Data;
                    }
                    DO | DONT | WILL | WONT => self.state = State::Negotiate(b),
                    SB => self.state = State::Subneg,
                    _ => self.state = State::Data, // standalone command (GA, NOP, …)
                },
                State::Negotiate(cmd) => {
                    Self::reply_negotiation(cmd, b, replies);
                    self.state = State::Data;
                }
                State::Subneg => {
                    if b == IAC {
                        self.state = State::SubnegIac;
                    }
                }
                State::SubnegIac => {
                    self.state = if b == SE { State::Data } else { State::Subneg };
                }
            }
        }
    }

    /// Decide our response to a server option negotiation.
    fn reply_negotiation(cmd: u8, opt: u8, replies: &mut Vec<u8>) {
        let response = match cmd {
            // Server asks us to enable an option.
            DO => {
                if opt == OPT_SGA {
                    WILL
                } else {
                    WONT
                }
            }
            // Server tells us to disable — comply.
            DONT => WONT,
            // Server offers to enable an option on its side.
            WILL => {
                if opt == OPT_SGA {
                    DO
                } else {
                    DONT
                }
            }
            // Server says it won't — acknowledge.
            WONT => DONT,
            _ => return,
        };
        replies.extend_from_slice(&[IAC, response, opt]);
    }
}

pub struct TelnetTransport {
    stream: TcpStream,
    parser: IacParser,
}

impl TelnetTransport {
    pub async fn connect(host: &str, port: u16) -> ToolResult<Self> {
        let target = format!("{host}:{port}");
        let stream =
            tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host, port)))
                .await
                .map_err(|_| {
                    ToolError::new(
                        ErrorKind::ConnectFailed,
                        format!("telnet connect to {target} timed out"),
                    )
                })?
                .map_err(|e| {
                    ToolError::new(
                        ErrorKind::ConnectFailed,
                        format!("telnet connect to {target}: {e}"),
                    )
                })?;
        Ok(Self { stream, parser: IacParser::new() })
    }
}

#[async_trait]
impl Transport for TelnetTransport {
    async fn write_all(&mut self, data: &[u8]) -> ToolResult<()> {
        // Escape any literal 0xFF bytes per the telnet protocol.
        if data.contains(&IAC) {
            let mut escaped = Vec::with_capacity(data.len() + 4);
            for &b in data {
                escaped.push(b);
                if b == IAC {
                    escaped.push(IAC);
                }
            }
            self.stream.write_all(&escaped).await
        } else {
            self.stream.write_all(data).await
        }
        .map_err(|e| ToolError::internal(format!("telnet write: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| ToolError::internal(format!("telnet flush: {e}")))?;
        Ok(())
    }

    async fn read_chunk(&mut self, timeout: Duration) -> ToolResult<Vec<u8>> {
        let mut buf = [0u8; 4096];
        let n = match tokio::time::timeout(timeout, self.stream.read(&mut buf)).await {
            Err(_) => return Ok(Vec::new()), // idle tick
            Ok(Ok(0)) => return Ok(Vec::new()),
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(ToolError::internal(format!("telnet read: {e}"))),
        };

        let mut data_out = Vec::with_capacity(n);
        let mut replies = Vec::new();
        self.parser.process(&buf[..n], &mut data_out, &mut replies);

        if !replies.is_empty() {
            let _ = self.stream.write_all(&replies).await;
            let _ = self.stream.flush().await;
        }
        Ok(data_out)
    }

    async fn close(&mut self) -> ToolResult<()> {
        let _ = self.stream.shutdown().await;
        Ok(())
    }

    fn protocol(&self) -> Protocol {
        Protocol::Telnet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_iac_negotiation_and_replies() {
        let mut p = IacParser::new();
        let mut data = Vec::new();
        let mut replies = Vec::new();
        // "AB" + IAC DO SGA + "C"
        let input = [b'A', b'B', IAC, DO, OPT_SGA, b'C'];
        p.process(&input, &mut data, &mut replies);
        assert_eq!(data, b"ABC");
        assert_eq!(replies, vec![IAC, WILL, OPT_SGA]); // we accept SGA
    }

    #[test]
    fn declines_unknown_option_and_handles_escaped_iac() {
        let mut p = IacParser::new();
        let mut data = Vec::new();
        let mut replies = Vec::new();
        // IAC WILL 99 (unknown) then IAC IAC (literal 0xFF) then 'Z'
        let input = [IAC, WILL, 99, IAC, IAC, b'Z'];
        p.process(&input, &mut data, &mut replies);
        assert_eq!(data, vec![IAC, b'Z']);
        assert_eq!(replies, vec![IAC, DONT, 99]);
    }

    #[test]
    fn subnegotiation_is_discarded() {
        let mut p = IacParser::new();
        let mut data = Vec::new();
        let mut replies = Vec::new();
        // 'X' + IAC SB <stuff> IAC SE + 'Y'
        let input = [b'X', IAC, SB, 24, 0, b'a', b'b', IAC, SE, b'Y'];
        p.process(&input, &mut data, &mut replies);
        assert_eq!(data, b"XY");
        assert!(replies.is_empty());
    }

    #[test]
    fn negotiation_split_across_chunks() {
        let mut p = IacParser::new();
        let (mut d, mut r) = (Vec::new(), Vec::new());
        p.process(&[IAC, DO], &mut d, &mut r); // option byte arrives next chunk
        assert!(r.is_empty());
        p.process(&[OPT_SGA], &mut d, &mut r);
        assert_eq!(r, vec![IAC, WILL, OPT_SGA]);
        assert!(d.is_empty());
    }
}
