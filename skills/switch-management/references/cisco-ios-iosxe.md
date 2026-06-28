# Cisco IOS / IOS-XE reference

Catalyst (e.g. 3850/9300), ISR/ASR, classic IOS. Prompts: user `>`, privileged `#`,
config `(config)#`.

## Access
- Privileged mode: use the `enable` tool (`enable {session, password}`).
- Console/vty login: `login {session, username?, password}`.
- Disable paging: `terminal length 0`.
- Save config: `copy running-config startup-config` (or `write memory`). May prompt
  `Destination filename [startup-config]?` → reply with `expect_send` (bare CR) or
  `run_command {raw:true, command:"\r"}`.

## Health / diagnostics
| Area | Command |
|------|---------|
| Version / uptime / serial | `show version` |
| Inventory / SFPs | `show inventory` |
| Environment (fans, temp, PSU) | `show environment all` |
| CPU | `show processes cpu sorted | exclude 0.0` |
| Memory | `show processes memory sorted` (header has Total/Used/Free) |
| Interfaces (oper) | `show interfaces status` |
| L3 interfaces | `show ip interface brief` |
| Interface errors | `show interfaces counters errors` |
| MAC table | `show mac address-table` (count: `… count`) |
| VLANs | `show vlan brief` |
| Spanning tree | `show spanning-tree summary` (root + blocked count) |
| EtherChannel | `show etherchannel summary` (look for `(P)` bundled) |
| HSRP | `show standby brief` |
| Neighbors | `show cdp neighbors [detail]` / `show lldp neighbors` |
| Stack | `show switch` / `show redundancy` |

## Logging
- `show logging` prints **OLDEST first** — a timeout truncates the newest lines.
- Recent only: `show logging | include <Mon>` (e.g. `| include Jun`), optionally with
  facilities: `| include Jun|MACFLAP|DIFFVIP|LINK-3`.
- Config-change audit: `show logging | include CONFIG_I`.

## Notes
- `show license summary` is invalid on older 3.x IOS-XE (use `show license`).
- Many 3.x boxes lack `show platform software status control-processor` /
  `show memory statistics` — use the commands above instead.
