# SSH-Connect

A single Rust [MCP](https://modelcontextprotocol.io) server that unifies **remote
server administration** and **interactive network-device / console management** over
**SSH, Telnet, and Serial** — with bundled Claude skills and agents.

SSH-Connect is the Rust successor to the TypeScript `SSH-MCP` tool. It merges two
prior Rust projects:

- **vps-manager-rust** — Ubuntu/VPS server operations (nginx, SSL, UFW, packages,
  deploy, systemd, MySQL, Redis, Docker, logs, processes, files, cron) with
  exec-per-channel SSH that captures exit codes, plus SFTP and optional
  trust-on-first-use host-key checking.
- **PuTTy-MCP** — prompt-aware interactive console management for switches/routers
  over SSH (PTY), Telnet, and Serial, with a Windows named-pipe broker so multiple
  MCP clients share one set of live sessions and serial ports.

Built on the `rmcp` Rust MCP SDK. Pure Rust SSH via `russh` (no OpenSSL/C deps).

## Two interaction models, one server

| You want… | Use | Connect with | Run with |
|-----------|-----|--------------|----------|
| Linux/VPS admin, scripts, exit codes | **server-ops** tools | `ssh_connect` | `ssh_exec`, `vps_*`, `ubuntu_*` |
| Switches, routers, consoles, serial | **interactive console** tools | `connect` | `run_command`, `enable`, `login`, … |

Both families share one process and the broker, so sessions are shared across all
your Claude windows.

### Tools

**Session / discovery (interactive):** `connect`, `disconnect`, `list_sessions`,
`list_com_ports`, `list_hosts`
**Interactive command:** `run_command`, `run_commands`, `enable`, `login`,
`expect_send`, `run_on_fleet`, `upload_config`, `download_config`
**Switch convenience:** `switch_backup_config`, `switch_network_diagnostics`
**SSH server-ops:** `ssh_connect`, `ssh_exec`, `ssh_upload_file`,
`ssh_download_file`, `ssh_list_files`, `ssh_disconnect`
**VPS / Ubuntu:** `vps_system_stats`, `vps_logs`, `vps_process`, `vps_file_read`,
`vps_file_ops`, `vps_cron`, `ubuntu_service_control`, `vps_nginx_config`,
`ubuntu_nginx_control`, `ubuntu_update_packages`, `ubuntu_ssl_certificate`,
`ubuntu_website_deployment`, `ubuntu_ufw_firewall`, `ubuntu_mysql`, `vps_redis`,
`vps_docker`, `vps_health_check` (one-call structured health battery)

> **Long-running commands:** `ssh_exec` (and the `vps_*`/`ubuntu_*` tools) terminate the
> remote command and return a clear error when their timeout elapses (best-effort — OpenSSH
> may ignore signals on a non-PTY exec). `ubuntu_update_packages`, `ubuntu_ssl_certificate`,
> and `vps_docker` accept an optional `timeoutMs`. For multi-minute operations, prefer the
> run-detached-and-poll-a-logfile pattern (see the `vps-management` skill) over a large timeout.

### Skills (`skills/`)

- **switch-management** — operational playbook for switches/routers (vendor idioms,
  pagination, enable/config, saving, fleet ops, firmware upgrades, console→SSH setup).
- **vps-management** — safe-change discipline + standard workflows (patch+reboot, SSL
  troubleshooting, service recovery, long-op polling) for Linux/Ubuntu server ops.
- **vps-health-report** — standardized HTML **server** health report + JSON sidecar + delta,
  driven by `vps_health_check`.
- **health-report** — standardized HTML network-device health report + JSON sidecar.

### Agents (`.claude/agents/`)

`vps-troubleshooter`, `switch-config-auditor`, `deployment-runner`,
`fleet-health-reporter`, `vps-health-reporter`.

## Build

Requires a recent stable Rust toolchain. Dependencies (including the `rmcp` MCP SDK,
`rmcp = "1.8"` from crates.io) are fetched by Cargo — a fresh clone builds with no
extra setup.

```pwsh
cargo build --release
# Binary: target/release/ssh-connect.exe   (.exe on Windows)
```

## Register with Claude Code / Claude Desktop

```pwsh
claude mcp add ssh-connect --scope user -- C:\repo\SSH-connect\SSH-Connect\target\release\ssh-connect.exe
```

Or in an MCP settings file:

```json
{
  "mcpServers": {
    "ssh-connect": {
      "command": "C:\\repo\\SSH-connect\\SSH-Connect\\target\\release\\ssh-connect.exe"
    }
  }
}
```

The server speaks MCP over stdio; all logging goes to stderr (`RUST_LOG=info` for
broker/role logs).

## Configuration

- **Inventory:** copy `hosts.example.toml` to `hosts.toml` and edit. The `connect`
  tool can then reach a device by `name` alone. Point elsewhere with
  `SSHCONNECT_HOSTS`.
- **Secrets:** prefer env vars over committing passwords. For host `core-sw-01` set
  `SSHCONNECT_CORE_SW_01_PASSWORD` (name upper-cased, non-alphanumerics → `_`); same
  for `USERNAME`, `HOST`, `PORT`, `KEY_PATH`.
- **Host-key checking (server-ops SSH):** default accepts any key. Set
  `SSHCONNECT_HOST_KEY_CHECK=tofu` (legacy `VPS_HOST_KEY_CHECK` also accepted) to
  enable trust-on-first-use against `~/.ssh-connect/known_hosts` (refuses later key
  mismatches).

## Broker (multi-instance session sharing, Windows)

On Windows, the first instance becomes the **owner** of a named pipe
(`\\.\pipe\ssh-connect-broker-v1`) and holds all live connections and COM ports.
Later instances become **proxies** that forward `tools/call` to the owner, so
`connect` in one window and `run_command` in another act on the same device — and
two windows can't collide on the same serial port. Tool *schemas* are served
locally by every instance. On non-Windows platforms every instance is a local
owner.

> Known limitation: if the owner process exits, existing proxies do not currently
> re-elect a new owner mid-session — restart the affected client. A fresh instance
> always elects correctly.

## Tests

```pwsh
cargo test --bin ssh-connect   # unit tests: telnet IAC, prompt/ANSI parsing, inventory, ports, legacy SSH algorithms
```

Protocol/broker behavior is verified by piping an `initialize` + `tools/list`
sequence to the binary and by launching two instances to confirm owner/proxy
election (see the project notes).

## License

MIT.
