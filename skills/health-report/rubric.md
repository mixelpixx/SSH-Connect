# Health-report rubric

The rules that make every report comparable. The filling agent follows these
exactly — severity and status are not judgment calls.

## Severity criteria

| Severity | Class | Criteria (any one suffices) |
|----------|-------|------------------------------|
| **Critical** | `crit` | Redundancy actively broken (FHRP peers disagree on VIP / both-active conflict); device or service at imminent risk; continuous error storm in logs (≥1 event/min sustained); hardware fault (failed PSU/fan, overtemp) |
| **Warning** | `warn` | Active fault or degradation: ongoing MAC flapping, unsynchronized NTP, heavy egress drops (>1M OutDiscards and growing), member link of a redundant pair down, recurring link flaps |
| **Advisory** | `info` | Worth confirming but not provably wrong: off-subnet VIP that may be intentional, single PSU in a core role, transient guard events that self-cleared, ports out of service |
| **Healthy** | `ok` | Explicitly verified good — always include ONE summary "Healthy" finding listing what checked out (environment, CPU, error counters, STP, LAG) |

## Status word (banner)

Deterministic, from the worst finding present:

| Worst finding | status_word | status_class |
|---------------|-------------|--------------|
| Any critical | `Action needed` | `status-crit` |
| Any warning (no critical) | `Action advised` | `status-warn` |
| Advisories/healthy only | `Healthy` | `status-ok` |

## Finding IDs (for delta matching)

Stable kebab-case slugs: `<subsystem>-<specific>` — e.g. `hsrp-vip-vlan200`,
`macflap-vlan123`, `ntp-unsync`, `outdiscards-gi1-0-2`, `psu-single`,
`link-flap-gi1-0-1`. Reuse the same id across runs for the same condition; the
delta is computed by id:

- in baseline, not now → **Resolved**
- in both → **Still open**
- now only → **New**

A "resolved" verdict needs evidence (e.g. error counter frozen between runs, the
log noise absent in the current window, live state now consistent) — note it.

## Standard command battery

Run after `terminal length 0` (or vendor equivalent). Vendor syntax in
`../switch-management/references/`. Per command, `timeout_secs` ≥ 10; logs ≥ 20.

| # | Purpose | Cisco IOS/IOS-XE form |
|---|---------|------------------------|
| 1 | Identity | `show version` |
| 2 | Inventory / SFPs | `show inventory` |
| 3 | Environment | `show environment all` |
| 4 | CPU | `show processes cpu sorted \| exclude 0.0` |
| 5 | Interfaces | `show interfaces status` |
| 6 | L3 | `show ip interface brief` |
| 7 | Errors | `show interfaces counters errors` |
| 8 | VLANs | `show vlan brief` |
| 9 | STP | `show spanning-tree summary` |
| 10 | LAG | `show etherchannel summary` |
| 11 | FHRP live state | `show standby brief` |
| 12 | Topology | `show cdp neighbors` |
| 13 | Clock/NTP | `show clock` + `show ntp status` |
| 14 | Recent logs | `show logging \| include <current-month>` (oldest-first gotcha — filter, don't dump) |
| 15 | Stack/redundancy | `show switch` + `show redundancy` (platform-permitting) |

### Deep-stats extension (run after the core 15)

| # | Purpose | Cisco IOS/IOS-XE form |
|---|---------|------------------------|
| 16 | Optics health (DOM) | `show interfaces transceiver detail` (Tx/Rx power, temp — flag near-threshold) |
| 17 | PoE budget | `show power inline` (n/a on SFP-only boxes → "N/A") |
| 18 | MAC table size | `show mac address-table count` |
| 19 | ARP size | `show ip arp summary` (or count `show ip arp` lines) |
| 20 | Route table | `show ip route summary` |
| 21 | Flash free (upgrade headroom) | `dir flash: \| include bytes` |
| 22 | Logged-in users | `show users` |
| 23 | Security config greps | see Security checks below |

Commands invalid on the platform → record "not available on this platform" for
that section; never invent data.

## Security checks (Security Posture section)

All read-only (`show` only). Every check appears in the report — passes included.
Status: `pass` (green) / `review` (yellow) / `fail` (red). Gather with:
`show ip ssh`, `show running-config | include ^ip http|^snmp-server community|^service password|^ntp server|^logging host|vstack`,
`show running-config | section line vty`, `show running-config | include ^banner|^aaa new-model`.

| Check | pass | review | fail |
|-------|------|--------|------|
| Software lifecycle | OS current & platform supported | OS train EoL but platform still supported | Platform past Last Day of Support (no patches ever again) |
| SSH version | v2 only (`SSH Enabled - version 2.0`) | — | v1 allowed (`1.99` or `1.5`) |
| VTY transport | `transport input ssh` | `telnet ssh` (both) | `telnet` only / `all` |
| HTTP server | both `ip http server`/`secure-server` absent or ACL-gated | HTTPS only, un-gated | plain `ip http server` enabled |
| Smart Install | `vstack` disabled/absent | — | `vstack` enabled (CVE-2018-0171 class) |
| SNMP communities | none, or v3 only | non-default v2c communities with ACL | `public`/`private` or un-ACL'd v2c |
| AAA | `aaa new-model` with central auth | local users only | line passwords only (no per-user auth) |
| NTP | synchronized | configured but unsynchronized | none configured |
| Central syslog | `logging host` configured | buffer only | logging disabled |
| Password handling | secrets (type 8/9) | `service password-encryption` (type 7) | plaintext passwords in config |
| Login banner | warning banner present | — | none (compliance expectation in clinical envs) |

Each failed/review check also becomes a finding (severity: fail→warning or
critical per the severity criteria; review→advisory) so it feeds delta tracking.

## Mitigations & Remediation section

Research-backed, not boilerplate. The agent uses web search for the exact
platform + version: vendor EoL/EoS bulletins, significant CVEs affecting the
running release, and current hardening guidance. 3–6 items, each with priority
**NOW** (active risk, no window needed — e.g. disable Smart Install), **NEXT
WINDOW** (needs a change window — e.g. VTY to ssh-only, OS update), or **PLAN**
(procurement/project — e.g. hardware refresh past LDoS). Every item cites its
source(s) as links. If web research is unavailable, say so in `mitigation_note`
and limit items to config-level fixes evidenced by the security checks.

## Metric cards (standard set, in order)

1. Uptime · 2. CPU (5 min) · 3. Temperature/Fans/PSU rollup · 4. Ports up (x/y) ·
5. Link errors (FCS/CRC total) · 6. Open findings count · 7–8. device-specific
(e.g. Po1 OutDiscards, STP role). In delta reports, use `.was` for the previous
value (`was 84 · ▲ +5` style).

## JSON sidecar schema

Written next to the HTML as `<HOST>-health-<YYYY-MM-DD>.json`:

```json
{
  "host": "NORTON-CORE-B-3850-12S",
  "date": "2026-06-08",
  "model": "WS-C3850-12S",
  "serial": "FOC2427X0WT",
  "version": "03.07.05E",
  "status": "action_advised",
  "findings": [
    {
      "id": "hsrp-vip-vlan200",
      "severity": "critical",
      "title": "VLAN 200 HSRP virtual-IP mismatch",
      "detail": "one-paragraph evidence",
      "recommendation": "one-sentence fix"
    }
  ],
  "metrics": {
    "uptime_weeks": 51, "cpu_5min_pct": 6, "fans_ok": true,
    "psu_present": 1, "psu_slots": 2, "ports_up": 8, "ports_total": 12,
    "link_errors": 0, "open_findings": 4,
    "mac_entries": 1243, "arp_entries": 312, "ip_routes": 87, "flash_free_mb": 680
  },
  "security": [
    { "id": "sec-ssh-version", "check": "SSH version", "status": "fail",
      "evidence": "SSH Enabled - version 1.99 (v1 accepted)" }
  ],
  "lifecycle": {
    "os_build_date": "2017-02-10",
    "platform_status": "past_ldos",
    "note": "WS-C3850 Last Day of Support 2025-10-31"
  }
}
```

`security[].status`: `pass` | `review` | `fail` — same ids across runs so the
delta also tracks security regressions/improvements.

`status` is the snake_case status word: `healthy` | `action_advised` | `action_needed`.
The `healthy` summary finding is NOT included in `findings` (it would always
"resolve"); findings hold only crit/warn/info items.
