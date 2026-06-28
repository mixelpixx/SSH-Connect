//! COM / serial port enumeration. Wraps `serialport::available_ports` and
//! flattens the platform-specific metadata into a JSON-friendly shape for the
//! `list_com_ports` tool.

use crate::error::{ToolError, ToolResult};
use serde::Serialize;
use std::time::Duration;

/// How the port is physically attached.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    Usb,
    Pci,
    Bluetooth,
    Unknown,
}

/// Whether the port can currently be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortStatus {
    /// The port opened successfully (and was released immediately) — available.
    Free,
    /// Another process holds the port exclusively (e.g. a terminal/another MCP instance).
    InUse,
    /// Could not determine (open failed for some other reason); name still valid.
    Unknown,
}

/// A discovered serial port with whatever identifying metadata the OS exposes.
#[derive(Debug, Clone, Serialize)]
pub struct PortInfo {
    /// The name to pass to `connect` (e.g. "COM3" on Windows, "/dev/ttyUSB0" on Linux).
    pub port_name: String,
    pub kind: PortKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    /// USB vendor:product id as hex (e.g. "0403:6001"), when a USB port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vid_pid: Option<String>,
    /// Whether the port is currently free or in use (from a non-destructive open test).
    pub status: PortStatus,
}

/// Enumerate serial ports (metadata only, `status` left as `Unknown`).
fn enumerate_basic() -> ToolResult<Vec<PortInfo>> {
    let ports = serialport::available_ports()
        .map_err(|e| ToolError::internal(format!("enumerate serial ports: {e}")))?;

    let mut out = Vec::with_capacity(ports.len());
    for p in ports {
        let info = match p.port_type {
            serialport::SerialPortType::UsbPort(usb) => PortInfo {
                port_name: p.port_name,
                kind: PortKind::Usb,
                // A friendly description: prefer product, fall back to manufacturer.
                description: usb.product.clone().or_else(|| usb.manufacturer.clone()),
                manufacturer: usb.manufacturer,
                product: usb.product,
                serial_number: usb.serial_number,
                vid_pid: Some(format!("{:04x}:{:04x}", usb.vid, usb.pid)),
                status: PortStatus::Unknown,
            },
            serialport::SerialPortType::PciPort => simple(p.port_name, PortKind::Pci),
            serialport::SerialPortType::BluetoothPort => simple(p.port_name, PortKind::Bluetooth),
            serialport::SerialPortType::Unknown => simple(p.port_name, PortKind::Unknown),
        };
        out.push(info);
    }
    out.sort_by(|a, b| a.port_name.cmp(&b.port_name));
    Ok(out)
}

/// Enumerate all serial ports and probe each (concurrently, non-destructively)
/// to mark it `free` or `in_use`.
pub async fn list_com_ports() -> ToolResult<Vec<PortInfo>> {
    let mut ports = enumerate_basic()?;

    // Probe each port on the blocking pool so the open-test doesn't stall the
    // async runtime; all probes run concurrently.
    let handles: Vec<_> = ports
        .iter()
        .map(|p| {
            let name = p.port_name.clone();
            tokio::task::spawn_blocking(move || probe_status(&name))
        })
        .collect();

    for (p, h) in ports.iter_mut().zip(handles) {
        if let Ok(status) = h.await {
            p.status = status;
        }
    }
    Ok(ports)
}

/// Non-destructively test whether a port can be opened: open it (no writes) and
/// immediately release it. An exclusive-access error means another process
/// holds it.
pub fn probe_status(port_name: &str) -> PortStatus {
    match serialport::new(port_name, 9600)
        .timeout(Duration::from_millis(50))
        .open()
    {
        Ok(port) => {
            drop(port); // release immediately
            PortStatus::Free
        }
        Err(e) => {
            let m = e.to_string().to_lowercase();
            if m.contains("denied")
                || m.contains("in use")
                || m.contains("busy")
                || m.contains("access")
            {
                PortStatus::InUse
            } else {
                PortStatus::Unknown
            }
        }
    }
}

/// Build a names-only `PortInfo` for non-USB ports.
fn simple(port_name: String, kind: PortKind) -> PortInfo {
    PortInfo {
        port_name,
        kind,
        description: None,
        manufacturer: None,
        product: None,
        serial_number: None,
        vid_pid: None,
        status: PortStatus::Unknown,
    }
}

/// A short, human-readable hint listing available ports — embedded in serial
/// connection errors so the caller can pick a valid port. Sync + metadata-only.
pub fn ports_hint() -> String {
    match enumerate_basic() {
        Ok(ports) if !ports.is_empty() => {
            let names: Vec<&str> = ports.iter().map(|p| p.port_name.as_str()).collect();
            format!("available ports: {}", names.join(", "))
        }
        Ok(_) => "no serial ports detected".to_string(),
        Err(_) => "could not enumerate serial ports".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_nonexistent_port_is_not_free() {
        // A bogus port name must never report Free.
        let st = probe_status("COM_DOES_NOT_EXIST_999");
        assert_ne!(st, PortStatus::Free);
    }
}
