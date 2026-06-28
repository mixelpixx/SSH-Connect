# Arista EOS reference

Largely Cisco-IOS-like CLI. Prompt: user `>`, privileged `#`, config `(config)#`.

## Access
- Privileged mode: `enable` (often no password in labs) — use the `enable` tool.
- Disable paging: `terminal length 0` (or session default `terminal length 0`).
- Save config: `copy running-config startup-config` (or `write memory`).
- Bonus: most `show` commands support `| json` for structured output —
  e.g. `show interfaces status | json` (great for parsing).

## Health / diagnostics
| Area | Command |
|------|---------|
| Version / uptime | `show version` |
| Inventory | `show inventory` |
| Environment | `show system environment all` (temp/cooling/power) |
| Resources | `show processes top once` / `show version` (mem) |
| Interfaces | `show interfaces status` |
| Interface errors | `show interfaces counters errors` |
| MAC table | `show mac address-table [count]` |
| VLANs | `show vlan` |
| Spanning tree | `show spanning-tree summary` |
| Port-channel / MLAG | `show port-channel summary` / `show mlag` |
| FHRP | `show standby brief` (VARP: `show ip virtual-router`) |
| Neighbors | `show lldp neighbors` |

## Logging
- `show logging` — buffer, newest last (so the tail is what you want).
- Recent: `show logging last 100` (also `last <time>` e.g. `last 1 hours`).
- Underlying syslog is in `/var/log/messages` if you drop to bash (`bash`).
