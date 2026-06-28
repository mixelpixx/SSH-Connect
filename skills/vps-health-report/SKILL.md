---
name: vps-health-report
description: Use when asked for a health check, health report, server report, or re-check of a Linux/Ubuntu VPS or server reached through the SSH-Connect server. Runs the standard read-only diagnostic battery (system & services, security hardening, SSL & web reachability, patch & maintenance) and generates a standardized HTML report plus a JSON sidecar in reports/. Automatically produces a delta (Resolved / Still open / New) when a previous report exists for the same host.
---

# VPS Health Report

Produces a **standardized, comparable** health report for a Linux/Ubuntu server, the
server-side counterpart to the switch `health-report` skill. The format is frozen so
every report (and every re-check) looks the same and can be diffed over time.

## When to use

Any request like "health check / health report / status report / re-check" for a VPS or
Linux server. For network switches/routers use the `health-report` skill instead.

## Inputs

- A reachable server (host + credentials). If an `ssh_connect` connection is already open,
  reuse its `connectionId`; otherwise connect first (prefer key auth).
- Optional: a prior report in `reports/` for the same host → the run becomes a **re-check**
  with a delta.

## Procedure

1. **Connect** (read-only intent). `ssh_connect` → `connectionId`.
2. **Collect** the standard battery in one call:
   `vps_health_check { connectionId, domains?: [...] }` → structured JSON for the four
   categories (system, security, ssl_web, maintenance). If `vps_health_check` is
   unavailable, run the equivalent commands from `rubric.md` over `ssh_exec`.
3. **Score** strictly per `rubric.md`: derive findings (with stable kebab-case ids),
   the 10 security checks (pass/review/fail), the 8 metric cards, and the deterministic
   status word. Do **not** improvise severity.
4. **Research mitigations** (3–6, prioritized NOW / NEXT WINDOW / PLAN), citing sources —
   distro EoL, CVEs in pending packages, hardening guidance. If web is unavailable, note it.
5. **Delta**: if a previous `reports/<HOST>-health-*.json` exists, compare by finding id →
   Resolved / Still open / New, and fill the `BEGIN:DELTA` block (otherwise delete it).
6. **Render** `template.html`: replace every `{{slot}}` (grep for `{{` before finishing —
   none may remain), duplicate `BEGIN:REPEAT` blocks per item, and keep/fill or delete the
   conditional `BEGIN:DELTA` block. Do not alter the CSS or section structure.
7. **Write** outputs to `reports/`.

## Output paths (deterministic)

- HTML: `reports/<HOST>-health-<YYYY-MM-DD>.html`
- JSON: `reports/<HOST>-health-<YYYY-MM-DD>.json` (schema in `rubric.md`)

If a file for today already exists, suffix `-recheck`, then `-recheck2`, … `<HOST>` is the
short hostname (or IPv4 if no name).

## Discipline

- **Strictly read-only.** A health check never changes the server — only `vps_health_check`,
  `vps_*` read tools, and `show`/`status`-style `ssh_exec`. No restarts, no edits.
- **Score deterministically** from `rubric.md` — the report's value is that it's comparable.
- **Evidence, not vibes.** Every finding cites the metric/log line it came from.
- **Never echo secrets** captured along the way.

Companion: the `vps-health-reporter` agent automates this end to end. For the operational
*fixes* that findings imply (patch+reboot, SSL repair, service recovery), see the
`vps-management` skill — but apply changes only with explicit approval.
