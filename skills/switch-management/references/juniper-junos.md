# Juniper Junos reference

EX/QFX/SRX/MX. Two modes: **operational** (`user@host>`) and **configuration**
(`user@host#`, entered with `configure`). Very different from Cisco.

## Access
- Login (use `login`) lands in operational mode `>`. There is **no `enable`** —
  privilege is by user class. Enter config with `configure` (or `configure
  exclusive`).
- Disable paging: `set cli screen-length 0` (operational).
- **Commit model:** edits in config mode do nothing until `commit`. Use
  `commit confirmed <min>` for risky changes (auto-rollback if not confirmed),
  `commit check` to validate, `rollback` to discard. `show | compare` shows the diff.

## Health / diagnostics
| Area | Command (operational) |
|------|------------------------|
| Version | `show version` |
| Hardware / inventory | `show chassis hardware` |
| Environment | `show chassis environment` (temp/fans/power) |
| Routing engine CPU/mem | `show chassis routing-engine` |
| Interfaces (brief) | `show interfaces terse` |
| Interface detail/errors | `show interfaces <if> extensive` (input/output errors) |
| Optics | `show interfaces diagnostics optics` |
| Ethernet switching / MAC | `show ethernet-switching table` |
| VLANs | `show vlans` |
| Spanning tree | `show spanning-tree bridge` / `… interface` |
| LAG | `show lacp interfaces` / `show interfaces ae0` |
| FHRP (VRRP) | `show vrrp summary` |
| Neighbors | `show lldp neighbors` |

## Logging
- `show log messages` — system log; **limit with** `| last 100` (newest) or
  `| match <pattern>`.
- List logs: `show log`. Follow live: `monitor start messages` / `monitor stop`.
- Config history: `show system commit`; who/what changed: `show system commit | match …`.
