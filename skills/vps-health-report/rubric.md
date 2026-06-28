# VPS health-report rubric

The rules that make every VPS report comparable. The filling agent follows these
exactly — severity and status are not judgment calls. Mirrors the switch
`../health-report/rubric.md` pattern, adapted for Linux/Ubuntu servers.

## Severity criteria

| Severity | Class | Criteria (any one suffices) |
|----------|-------|------------------------------|
| **Critical** | `crit` | A service that should be up is **down/failed** (nginx, the DB, php-fpm, a published app); root filesystem or inodes **≥95%**; out-of-memory / no free memory and no swap; an SSL cert **expired or <7 days** to expiry; a 5xx on a primary site; clear compromise indicator |
| **Warning** | `warn` | Disk/inode **≥85%**; load average **> CPU cores** sustained; cert **<30 days**; **pending security updates**; **reboot-required** flag set; a non-critical failed unit; fail2ban inactive on an internet-facing host; MySQL bound to `0.0.0.0`; SSH root login with password enabled |
| **Advisory** | `info` | Non-security updates pending; no swap configured; a backup older than its expected cadence; a 3xx where 2xx was expected |
| **Healthy** | `ok` | Explicitly verified good — always include ONE summary "Healthy" finding listing what checked out (resources, services, certs, security) |

## Status word (banner)

Deterministic, from the worst finding present:

| Worst finding | status_word | status_class |
|---------------|-------------|--------------|
| Any critical | `Action needed` | `status-crit` |
| Any warning (no critical) | `Action advised` | `status-warn` |
| Advisories/healthy only | `Healthy` | `status-ok` |

## Finding IDs (for delta matching)

Stable kebab-case slugs: `<subsystem>-<specific>` — e.g. `svc-nginx-down`,
`disk-root-92pct`, `cert-orchis-ai-expiring`, `updates-security-pending`,
`reboot-required`, `ssh-root-password`, `mysql-bind-public`. Reuse the same id across
runs for the same condition; the delta is computed by id:

- in baseline, not now → **Resolved**
- in both → **Still open**
- now only → **New**

A "resolved" verdict needs evidence (service now active, disk back under threshold,
cert renewed, updates applied) — note it.

## Standard command battery

Gather via the **`vps_health_check`** tool (preferred — one call returns all four
categories as JSON), or run the equivalent commands manually over `ssh_exec`. The
four categories and what each contributes:

| # | Category | Signals |
|---|----------|---------|
| 1 | **System & services** | uptime; `df -P /` (disk %) + `df -Pi /` (inode %); `free -m`; `/proc/loadavg` vs `nproc`; `systemctl --failed`; `is-active` for nginx/mariadb/mysql/php*-fpm/docker/redis-server/fail2ban; top processes; distro + kernel |
| 2 | **Security hardening** | `sshd -T` PermitRootLogin / PasswordAuthentication; `ufw status`; fail2ban active; MySQL `bind-address`; count of pending **security** updates |
| 3 | **SSL & web reachability** | per-cert days-to-expiry (from `/etc/letsencrypt/live/*/fullchain.pem`); per-vhost HTTP(S) status code |
| 4 | **Patch & maintenance** | pending apt updates count; `/var/run/reboot-required` flag + pkgs; recent error-log scan (nginx-error + `journalctl -p err`); backup presence/freshness |

Data not available on the host → record "not available" for that item; never invent.

## Security checks (Security Posture section)

All read-only. Every check appears in the report — passes included. Status:
`pass` (green) / `review` (yellow) / `fail` (red).

| Check | pass | review | fail |
|-------|------|--------|------|
| SSH root login | `PermitRootLogin no` (or `prohibit-password`/key-only) | `prohibit-password` on a root-only box | `yes` with password auth enabled |
| SSH password auth | `PasswordAuthentication no` (keys only) | passwords on for some users | passwords on for all incl. root |
| Firewall (UFW) | `Status: active` with sane rules | active but very permissive | inactive on an internet-facing host |
| fail2ban | active with jails | installed but inactive | absent on a public SSH host |
| DB exposure | MySQL/MariaDB bound to `127.0.0.1`/socket | bound to a private LAN IP | bound to `0.0.0.0` (public) |
| Security updates | none pending | a few pending | many pending / unattended-upgrades off |
| Reboot pending | no reboot-required | reboot-required (non-kernel) | reboot-required after a kernel/security update |
| TLS validity | all certs >30 days | a cert 7–30 days | any cert expired or <7 days |
| Web exposure | expected codes (200/301) on all vhosts | unexpected 3xx/403 | 5xx or connection failure on a primary site |
| Backups | present and fresh | present but stale | none found |

Each failed/review check also becomes a finding (fail→warning or critical per the
severity criteria; review→advisory) so it feeds delta tracking.

## Mitigations & Remediation section

Research-backed, not boilerplate. Use web search for the specifics: the distro's EoL
status, CVEs in pending packages, and current hardening guidance. 3–6 items, each with
priority **NOW** (active risk — e.g. expired cert, public DB), **NEXT WINDOW** (needs a
maintenance window — e.g. reboot for kernel, apt upgrade), or **PLAN** (project — e.g.
distro upgrade before LTS EoL). Cite sources as links. If web research is unavailable,
say so in `mitigation_note` and limit items to evidence from the security checks.

## Metric cards (standard set, in order)

1. **Uptime** · 2. **Load** (1m, with `/cores`) · 3. **Memory used %** · 4. **Disk used %**
(root) · 5. **Failed services** (count) · 6. **Pending updates** (count, note security) ·
7. **Nearest cert** (days to soonest expiry) · 8. **Open findings** (count). In delta
reports, use `.was` for the previous value (`was 89 · ▼ -3` style).

## JSON sidecar schema

Written next to the HTML as `<HOST>-health-<YYYY-MM-DD>.json`:

```json
{
  "host": "srv850061.hstgr.cloud",
  "date": "2026-06-28",
  "ipv4": "31.97.133.47",
  "distro": "Ubuntu 24.04.4 LTS",
  "kernel": "6.8.0-124-generic",
  "status": "action_advised",
  "findings": [
    {
      "id": "updates-security-pending",
      "severity": "warning",
      "title": "Pending security updates",
      "detail": "one-paragraph evidence",
      "recommendation": "one-sentence fix"
    }
  ],
  "metrics": {
    "uptime_days": 67, "load_1m": 0.12, "cpu_count": 2,
    "mem_used_pct": 9, "disk_used_pct": 4, "inode_used_pct": 1,
    "failed_units": 0, "pending_updates": 14, "security_updates": 3,
    "reboot_required": true, "nearest_cert_days": 22, "open_findings": 3
  },
  "security": [
    { "id": "sec-ssh-root", "check": "SSH root login", "status": "review",
      "evidence": "PermitRootLogin prohibit-password (key-only root)" }
  ],
  "lifecycle": {
    "distro": "Ubuntu 24.04 LTS",
    "lts_eol": "2029-04",
    "note": "Standard support until 2029; ESM to 2034"
  }
}
```

`security[].status`: `pass` | `review` | `fail` — same ids across runs so the delta
also tracks security regressions/improvements.

`status` is the snake_case status word: `healthy` | `action_advised` | `action_needed`.
The `healthy` summary finding is NOT included in `findings` (it would always "resolve");
findings hold only crit/warn/info items.
