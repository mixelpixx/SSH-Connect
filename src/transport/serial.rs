//! Serial (COM port) transport via tokio-serial. Used for console-cable access
//! to switches — initial setup, password recovery, or out-of-band management.

use super::{Protocol, Transport};
use crate::error::{ErrorKind, ToolError, ToolResult};
use async_trait::async_trait;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

pub struct SerialTransport {
    stream: SerialStream,
}

impl SerialTransport {
    /// Open a COM port at the given baud rate (8N1, the switch-console default).
    pub fn open(port: &str, baud: u32) -> ToolResult<Self> {
        let stream = tokio_serial::new(port, baud)
            .data_bits(tokio_serial::DataBits::Eight)
            .parity(tokio_serial::Parity::None)
            .stop_bits(tokio_serial::StopBits::One)
            .flow_control(tokio_serial::FlowControl::None)
            .timeout(Duration::from_millis(100))
            .open_native_async()
            .map_err(|e| {
                ToolError::new(
                    ErrorKind::SerialUnavailable,
                    format!("cannot open serial port '{port}' @ {baud}: {e}"),
                )
            })?;
        Ok(Self { stream })
    }

    /// Common console baud rates, ordered by likelihood for switch consoles.
    pub const COMMON_BAUDS: [u32; 5] = [9600, 115200, 38400, 57600, 19200];

    /// Try common baud rates: open, send a CR, and accept the rate whose reply is
    /// mostly printable (garbage indicates a baud mismatch). Returns the open
    /// transport and the detected rate. Falls back to 9600 if nothing answers.
    pub async fn auto_baud(port: &str) -> ToolResult<(Self, u32)> {
        for &baud in &Self::COMMON_BAUDS {
            let mut t = match Self::open(port, baud) {
                Ok(t) => t,
                // If the port can't be opened at all (busy/missing), stop early.
                Err(e) => return Err(e),
            };
            // Nudge and read a short reply.
            let _ = t.write_all(b"\r").await;
            let reply = t.read_chunk(Duration::from_millis(600)).await.unwrap_or_default();
            if reply.is_empty() {
                continue; // device silent at this rate; try the next
            }
            // Mostly-printable (ASCII text / common control) ⇒ correct baud.
            let printable = reply
                .iter()
                .filter(|&&b| b == b'\r' || b == b'\n' || b == b'\t' || (0x20..=0x7e).contains(&b))
                .count();
            if printable * 10 >= reply.len() * 8 {
                return Ok((t, baud));
            }
        }
        // Nothing answered cleanly — default to 9600 so the caller can still try.
        Ok((Self::open(port, 9600)?, 9600))
    }
}

#[async_trait]
impl Transport for SerialTransport {
    async fn write_all(&mut self, data: &[u8]) -> ToolResult<()> {
        self.stream
            .write_all(data)
            .await
            .map_err(|e| ToolError::internal(format!("serial write: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| ToolError::internal(format!("serial flush: {e}")))?;
        Ok(())
    }

    async fn read_chunk(&mut self, timeout: Duration) -> ToolResult<Vec<u8>> {
        let mut buf = [0u8; 4096];
        match tokio::time::timeout(timeout, self.stream.read(&mut buf)).await {
            Err(_) => Ok(Vec::new()),         // idle tick — no data this slice
            Ok(Ok(0)) => Ok(Vec::new()),      // nothing available
            Ok(Ok(n)) => Ok(buf[..n].to_vec()),
            Ok(Err(e)) => Err(ToolError::internal(format!("serial read: {e}"))),
        }
    }

    async fn close(&mut self) -> ToolResult<()> {
        // The port is closed when the stream is dropped with the session.
        Ok(())
    }

    fn protocol(&self) -> Protocol {
        Protocol::Serial
    }
}
