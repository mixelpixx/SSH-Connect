//! Host inventory loaded from `hosts.toml`, with environment-variable overrides.
//!
//! A host entry names a device and the parameters needed to reach it. Secrets
//! (passwords) are best supplied via env vars rather than committed to the
//! file. For a host named `core-sw-01`, the password override variable is
//! `SSHCONNECT_CORE_SW_01_PASSWORD` (name upper-cased, non-alphanumerics → `_`).

use crate::transport::Protocol;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One device in the inventory. Fields mirror the `connect` tool arguments so a
/// caller can `connect` by `name` alone and have the rest filled in.
#[derive(Debug, Clone, Deserialize)]
pub struct HostEntry {
    pub name: String,
    #[serde(default)]
    pub host: Option<String>,
    pub protocol: Protocol,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub key_path: Option<PathBuf>,
    /// Serial-only: COM port name (e.g. "COM3").
    #[serde(default)]
    pub serial_port: Option<String>,
    /// Serial-only: baud rate (default 9600 applied at connect time).
    #[serde(default)]
    pub baud: Option<u32>,
    /// Optional explicit prompt regex override for this device.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawInventory {
    #[serde(default, rename = "host")]
    hosts: Vec<HostEntry>,
}

/// The loaded inventory, indexed by host name.
#[derive(Debug, Default)]
pub struct Inventory {
    hosts: HashMap<String, HostEntry>,
}

impl Inventory {
    /// Load from the default location: `$SSHCONNECT_HOSTS` if set, else
    /// `hosts.toml` in the current working directory. A missing file yields an
    /// empty inventory (not an error).
    pub fn load_default() -> Self {
        let path = std::env::var("SSHCONNECT_HOSTS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("hosts.toml"));
        match Self::load_from(&path) {
            Ok(inv) => inv,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "no host inventory loaded");
                Inventory::default()
            }
        }
    }

    /// Load from an explicit path, applying env-var overrides.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let raw: RawInventory = toml::from_str(&text)?;
        let mut hosts = HashMap::new();
        for mut entry in raw.hosts {
            apply_env_overrides(&mut entry);
            hosts.insert(entry.name.clone(), entry);
        }
        Ok(Self { hosts })
    }

    pub fn get(&self, name: &str) -> Option<&HostEntry> {
        self.hosts.get(name)
    }

    #[allow(dead_code)] // part of the inventory API
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    #[allow(dead_code)] // companion to len(); part of the inventory API
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// Names + protocols for the `list_hosts` tool, sorted by name.
    pub fn summaries(&self) -> Vec<&HostEntry> {
        let mut v: Vec<&HostEntry> = self.hosts.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

/// Normalise a host name into the env-var stem: upper-case, every run of
/// non-alphanumeric characters collapsed to a single underscore.
pub fn env_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_us = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Apply `SSHCONNECT_<STEM>_<FIELD>` overrides to a single entry.
fn apply_env_overrides(entry: &mut HostEntry) {
    let stem = env_stem(&entry.name);
    let var = |field: &str| std::env::var(format!("SSHCONNECT_{stem}_{field}")).ok();

    if let Some(v) = var("PASSWORD") {
        entry.password = Some(v);
    }
    if let Some(v) = var("USERNAME") {
        entry.username = Some(v);
    }
    if let Some(v) = var("HOST") {
        entry.host = Some(v);
    }
    if let Some(v) = var("KEY_PATH") {
        entry.key_path = Some(PathBuf::from(v));
    }
    if let Some(v) = var("PORT").and_then(|s| s.parse().ok()) {
        entry.port = Some(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_stem_normalises() {
        assert_eq!(env_stem("core-sw-01"), "CORE_SW_01");
        assert_eq!(env_stem("Console A"), "CONSOLE_A");
        assert_eq!(env_stem("a..b"), "A_B");
        assert_eq!(env_stem("_edge_"), "EDGE");
    }

    #[test]
    fn parses_toml_inventory() {
        let toml = r#"
            [[host]]
            name = "core-sw-01"
            host = "10.0.0.2"
            protocol = "ssh"
            username = "admin"

            [[host]]
            name = "console-a"
            protocol = "serial"
            serial_port = "COM3"
            baud = 9600
        "#;
        let raw: RawInventory = toml::from_str(toml).unwrap();
        assert_eq!(raw.hosts.len(), 2);
        assert_eq!(raw.hosts[0].protocol, Protocol::Ssh);
        assert_eq!(raw.hosts[1].protocol, Protocol::Serial);
        assert_eq!(raw.hosts[1].serial_port.as_deref(), Some("COM3"));
    }
}
