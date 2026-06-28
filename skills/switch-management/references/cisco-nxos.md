# Cisco NX-OS reference

Nexus (3k/5k/7k/9k). Prompt: `switch#` (no separate user mode; RBAC roles instead
of enable). Config: `configure terminal` → `(config)#`.

## Access
- No `enable` step — log in (use `login`) and you're at `#` per your role.
- Disable paging: `terminal length 0`.
- Save config: `copy running-config startup-config` (alias `copy run start`).
- Features are modular: `show feature` / `feature <name>` before some commands work.

## Health / diagnostics
| Area | Command |
|------|---------|
| Version / uptime | `show version` |
| Inventory | `show inventory` |
| Environment | `show environment` (fans/temp/power) |
| System resources (CPU/mem) | `show system resources` |
| Process CPU | `show processes cpu sort` |
| Interfaces | `show interface status` / `show interface brief` |
| Interface errors | `show interface counters errors` |
| MAC table | `show mac address-table [count]` |
| VLANs | `show vlan brief` |
| Spanning tree | `show spanning-tree summary` |
| Port-channel | `show port-channel summary` |
| FHRP | `show hsrp brief` / `show vrrp` |
| Neighbors | `show cdp neighbors` / `show lldp neighbors` |
| vPC (if used) | `show vpc` / `show vpc consistency-parameters` |

## Logging
- `show logging last <N>` — newest N lines directly (no oldest-first problem).
- `show logging logfile` — full buffer; `show logging nvram` — persisted.
- Filter: `show logging last 200 | include <pattern>`.
