---
name: switch-management
description: Use when managing network switches/routers over the SSH-Connect server (SSH, Telnet, or serial console) — opening sessions, running CLI commands, entering enable/config mode, handling pagination, saving configs safely, backing up before changes, running commands across a fleet, upgrading firmware, and converting a console-configured device to SSH. Covers Cisco IOS/NX-OS, Arista EOS, and Juniper Junos idioms.
---

# Switch Management

The playbook for driving the **SSH-Connect** server to manage switches and routers.
The MCP server gives you raw tools (`connect`, `run_command`, `expect_send`, …) plus
two convenience tools (`switch_backup_config`, `switch_network_diagnostics`); this
skill is the operational knowledge for using them safely and correctly.

## The cardinal rule: back up before you change

Before ANY configuration change on a production device:

1. Capture the current running-config with a `show` (or download it via SFTP).
2. Make the change.
3. Verify with a `show` that the change took effect.
4. Save the config (see vendor idioms below) — only after verifying.

A change you didn't back up is a change you can't roll back. Never skip step 1.

## Connecting

Pick the transport by situation:

| Situation | Transport |
|-----------|-----------|
| Normal in-band management | `ssh` (preferred) |
| Legacy device, no SSH | `telnet` |
| Initial setup, password recovery, network down, no IP yet | `serial` |

```
connect { name: "core-sw-01", protocol: "ssh", host: "10.0.0.2", username: "admin", key_path: "..." }
```

If the device is in `hosts.toml`, `connect { name: "core-sw-01" }` alone is enough —
inventory fills in the rest. For serial, first run `list_com_ports` to find the
right port (look for the USB-serial adapter, e.g. an FTDI `vid_pid` of `0403:6001`).
Each port reports a `status` of `free` or `in_use` — if your port is `in_use`,
another program (a terminal, or another Claude window) holds it; close that first.

Serial conveniences on `connect`:
- `baud: "auto"` probes common rates (9600/115200/…) instead of guessing.
- `wake: true` sends a carriage return on connect and returns the banner/prompt,
  so you don't need a manual nudge.

Sessions persist across tool calls **and are shared across all Claude windows**
(single-owner broker). Reuse the session `name`; don't reconnect for every command.
A second `connect` with a name that's already open returns "session exists" — that's
the shared session, just use it.

## Disabling pagination first

Switches paginate long output with a `--More--` prompt. `run_command` now
**auto-advances** the pager (sends a space per page and stitches the output), so it
won't stall — but it's still cleaner to disable paging as your **first command**:

- Cisco IOS / NX-OS / Arista: `terminal length 0`
- Juniper Junos (operational): `set cli screen-length 0`

## Entering privileged / config mode

Cisco-style devices start in user mode (`>`). Use the dedicated **`enable`** tool —
it drives the `enable` → `Password:` handshake in one step and **never echoes the
password** back in the result:

```
enable { session, password: "<enable-pw>" }
```

For a console/vty that prompts for a username and/or password after connecting, use
**`login`** (also non-echoing):

```
login { session, username: "admin", password: "<pw>" }   # username optional
```

> **Never** put a password in `run_command` — it would be echoed in the result.
> Use `enable`/`login`. If `run_command` hits any interactive sub-prompt
> (`Password:`, `[confirm]`, `[yes/no]`, `--More--`) it returns immediately with a
> `sub_prompt` field instead of waiting out the timeout, so you can respond.

The default prompt regex matches `>`, `#`, and `(config)#`, so once privileged,
`run_command` works normally. For config changes: `configure terminal` → change →
`end`. To reply to a confirmation prompt, use `expect_send` (sends verbatim) or
`run_command { raw: true }` (no line ending appended).

## Saving configuration (vendor idioms)

After verifying a change, persist it:

| Platform | Save command |
|----------|--------------|
| Cisco IOS | `copy running-config startup-config` (or `write memory`) |
| Cisco NX-OS | `copy running-config startup-config` |
| Arista EOS | `copy running-config startup-config` (or `write memory`) |
| Juniper Junos | `commit` (changes are not live until committed) |

Cisco `copy run start` may prompt `Destination filename [startup-config]?` —
answer with `expect_send { expect: "Destination filename", send: "\n" }` or send a
bare newline.

> Junos is different: edits in configuration mode are staged and do nothing until
> `commit`. Use `commit confirmed <minutes>` for risky changes — it auto-rolls
> back if you don't `commit` again, protecting you from locking yourself out.

## Backing up configs over SFTP

For SSH sessions you can pull/push files directly:

```
download_config { session, remote_path: "running-config", local_path: "./backups/core-sw-01.cfg" }
upload_config   { session, local_path: "./golden.cfg", remote_path: "flash:restored.cfg" }
```

(Many switches expose the running-config through SFTP under vendor-specific
paths; when in doubt, capture it textually with `show running-config` and save the
`output`.)

The **`switch_backup_config`** convenience tool wraps this: it runs a show-config
command (default `show running-config`, paging auto-handled) and optionally writes
the text to a local file in one step:

```
switch_backup_config { session, save_to: "./backups/core-sw-01.cfg" }
```

For reachability checks from the device itself, **`switch_network_diagnostics`**
runs `ping`/`traceroute` against a target (append vendor-specific options via `args`,
e.g. `args: "repeat 100"` on Cisco):

```
switch_network_diagnostics { session, action: "ping", target: "8.8.8.8" }
```

## Fleet operations — read before you write

`run_on_fleet` runs one command across many sessions in parallel. Use it for
**read-only audits first** to understand the fleet before changing anything:

```
run_on_fleet { sessions: ["sw-01","sw-02","sw-03"], command: "show vlan brief" }
```

Each device returns independently; one failure doesn't abort the rest. Only after
auditing and backing up should you push a change across the fleet — and prefer
doing writes one device at a time so a bad command doesn't break everything at
once.

## Inspecting logs & diagnostics

Log retrieval differs sharply by vendor — and the ordering matters.

**Cisco IOS / IOS-XE prints `show logging` OLDEST-first.** A big buffer can exceed
your timeout, and because it's oldest-first, a timeout truncates the **newest**
(most relevant) lines. Don't just `show logging` on a busy device. Instead:
- **Filter by recency**, e.g. `show logging | include Jun` (current month) — small,
  fast, and shows what's happening *now*. Combine facilities:
  `show logging | include Jun|MACFLAP|DIFFVIP`.
- Raise `timeout_secs` (15–30 s) for full dumps; expect `timed_out`/`truncated`.
- **NX-OS / Junos can tail directly**: `show logging last 100` (NX-OS),
  `show log messages | last 100` (Junos).

**Prefer live state over log-diving for current conditions.** To check first-hop
redundancy, `show standby brief` (HSRP) tells you the *current* active/standby/VIP
in one line — far more reliable than inferring from scattered log events. Likewise
`show interfaces counters errors`, `show spanning-tree summary`, `show etherchannel
summary` give you ground truth.

**Decode common Cisco log signatures** (see `references/log-signatures.md`):
- `%SW_MATM-4-MACFLAP_NOTIF` — a MAC is learned on two ports alternately → L2 loop
  or dual-homed path in that VLAN.
- `%HSRP-4-DIFFVIP1` — two HSRP peers disagree on the virtual IP → broken gateway
  redundancy; reconcile `standby <grp> ip`.
- `%SPANTREE-2-RECV_PVID_ERR` / `BLOCK_PVID_LOCAL` — native-VLAN mismatch on a
  trunk; the port is auto-blocked.
- `%LINK-3-UPDOWN` / `%LINEPROTO-5-UPDOWN` — link flaps; check optics/fiber.

**Per-vendor command sets** (version, environment, CPU, memory, interface errors,
MAC table, FHRP, STP, logging) live in `references/`:
`cisco-ios-iosxe.md`, `cisco-nxos.md`, `arista-eos.md`, `juniper-junos.md`.

## Firmware upgrades (playbook, not a single tool)

Firmware install is deliberately driven from the generic primitives rather than a
rigid tool, because the safe sequence is vendor- and situation-specific. The
disciplined flow:

1. **Pre-flight.** Record the running version and free flash:
   - Cisco IOS/IOS-XE: `show version`, `dir flash:` / `show flash:`
   - NX-OS: `show version`, `dir bootflash:`
   - Arista: `show version`, `dir flash:`
   Confirm there's room for the new image before copying.
2. **Back up first.** `switch_backup_config { session, save_to: "..." }` — never
   upgrade an un-backed-up device.
3. **Transfer the image.** Over an SSH session, push it with SFTP:
   `upload_config { session, local_path: "./img/cat3850.bin", remote_path: "flash:cat3850.bin" }`.
   (For very large images, raise downstream `timeout_secs`.)
4. **Verify integrity.** Check the hash on the device, e.g.
   `verify /md5 flash:cat3850.bin <expected-md5>` (IOS) and compare to the vendor value.
5. **Set boot + save.** Point the boot variable at the new image
   (`boot system flash:cat3850.bin` on IOS; `install all nxos ...` on NX-OS;
   `boot system ...` on Arista), then `copy running-config startup-config`.
6. **Prepare rollback.** Keep the old image on flash and note its boot statement so
   you can revert. For risky changes prefer a confirmed/auto-rollback path where the
   platform supports it.
7. **Reload + verify.** `reload` (respond to the `[confirm]` with `expect_send`),
   wait, reconnect, and `show version` to confirm the new image is running.

Treat every step's output as ground truth — don't assume a copy or boot statement
succeeded; read it back.

## Converting a console-configured device to SSH

A factory-fresh switch reached over `serial` can be brought onto the network and
switched to in-band SSH without a dedicated tool:

1. Connect on `serial` (`connect { protocol: "serial", serial_port: "COM3", baud_auto: true, wake: true }`)
   and `login`/`enable`.
2. Configure a management IP/VLAN and default route; verify with
   `switch_network_diagnostics { action: "ping", target: "<gateway>" }`.
3. Enable SSH: set `hostname` + `ip domain-name`, generate keys
   (`crypto key generate rsa modulus 2048`), create a local user, and set
   `line vty 0 15` → `transport input ssh` + `login local`.
4. Save: `copy running-config startup-config`.
5. **Verify over the new path before trusting it:** open a *second* session with
   `connect { protocol: "ssh", host: "<mgmt-ip>", username, password }` and run a
   `show` — only then consider the console optional. Keep the serial session open
   until SSH is proven, so a mistake doesn't lock you out.

## Timeouts

`run_command` defaults to a 30s timeout. Long operations (`show tech-support`,
large `show running-config`, software installs) need a higher `timeout_secs`. If
`timed_out` comes back `true`, the `output` is partial — raise the timeout and/or
confirm paging is disabled. `truncated: true` means output hit the size cap.

## Quick troubleshooting

| Symptom | Likely cause / fix |
|---------|--------------------|
| `run_command` returns with a `sub_prompt` | Device is waiting at Password:/[confirm]/--More--. Use `enable`/`login`, `expect_send`, or `run_command {raw:true}` to respond. |
| `enable` did nothing / `% Bad secrets` | Wrong enable password, or you sent it via `run_command` (use the `enable` tool). |
| `no_such_session` | The session isn't open (or server restarted). `connect` again. |
| Serial `serial_unavailable` / `in_use` | Wrong or busy port. Run `list_com_ports` and check each port's `status`; close whatever holds it. |
| `auth_failed` | Bad credentials, or the key needs a passphrase (unsupported — use an unencrypted key or password). |
| Garbled output | Wrong baud on serial — try `baud: "auto"` on `connect`, or set a custom `prompt` regex. |
| Empty output on a serial console | It may need a nudge — use `connect {wake: true}` or `run_command {command: ""}`. |
