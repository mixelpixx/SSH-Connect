# Log-signature decoder

What common switch log messages mean and what to check. Patterns are Cisco
IOS/IOS-XE/NX-OS unless noted; Arista/Junos analogues in parentheses.

## Layer 2 / switching

- **`%SW_MATM-4-MACFLAP_NOTIF: Host <mac> in vlan N is flapping between port A and port B`**
  A MAC is being learned alternately on two ports → a Layer-2 loop or a dual-homed
  device/path in that VLAN. Map the VLAN's topology; one of A/B is usually a core
  uplink (`Po1`) and the other a local access trunk. On a paired core, the *same*
  MACs flap on both switches (different local ports) — it's a fabric-wide loop, not
  a single-switch fault.
- **`%SPANTREE-2-RECV_PVID_ERR` / `%SPANTREE-2-BLOCK_PVID_LOCAL`**
  Native-VLAN mismatch across a trunk (peer's PVID ≠ local). The port is auto-blocked
  to prevent a loop. Align the native VLAN on both ends before re-enabling.
- **`%SPANTREE-2-ROOTGUARD_*` / `LOOPGUARD_*` / `BLOCK_BPDUGUARD`**
  An STP guard fired (unexpected superior BPDU, unidirectional link, or a BPDU on a
  PortFast edge). Find what was plugged in.

## First-hop redundancy (HSRP / VRRP)

- **`%HSRP-4-DIFFVIP1: Vlan N Grp G active routers virtual IP X is different to the
  locally configured address Y`**
  The two HSRP peers disagree on the group's virtual IP (X vs Y). Gateway redundancy
  is broken and the standby logs this repeatedly (often every ~30 s). Fix: make both
  peers' `standby G ip` identical (and in the SVI's subnet). Verify with
  `show standby brief` (active/standby/VIP per group).
- **HSRP group-number mismatch** (no single log; seen via `show standby brief`):
  both peers Active for the same VIP under *different* group numbers → they aren't
  coordinating. Standardize the group number.
- **`%HSRP-5-STATECHANGE`** — normal on link/peer transitions; a storm of them means
  flapping uplinks or an unstable peer.

## Physical / link

- **`%LINK-3-UPDOWN` + `%LINEPROTO-5-UPDOWN`** — interface flapping. Check the SFP,
  fiber/cable, and the far end. Repeated cycles on one port = marginal optics.
- **`%PLATFORM_PM-6-MODULE_INSERTED/REMOVED`** — an SFP was (un)plugged. Correlate
  with maintenance windows.

## Interpreting counters (not logs)

- High **`OutDiscards`** with zero FCS/CRC/align/collision errors = egress queue
  drops (congestion / QoS policing), not a cabling fault. A frozen OutDiscards
  counter across two checks means the condition has stopped.
- Any nonzero **FCS-Err / Align-Err / Runts** = physical problem (SFP, fiber, EMI).
