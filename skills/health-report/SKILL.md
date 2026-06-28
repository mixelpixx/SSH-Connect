---
name: health-report
description: Use when asked for a health check, health report, device report, or re-check of a network switch/router — spawns a report agent that runs the standard read-only diagnostic battery over SSH-Connect and generates a standardized HTML report (plus JSON sidecar) in reports/. Automatically produces a delta (Resolved/Still open/New) when a previous report exists for the same device.
---

# Health Report

Generates the **standardized** device health report. Every report uses the frozen
[template.html](template.html) and the severity/status rules in [rubric.md](rubric.md) —
same look, same sections, same scoring logic, every time.

## When this triggers

"Run a health check on X", "health report", "full checkup", "re-check X",
"compare to last time". One device per report (fleet roll-ups are out of scope).

## Orchestration (main conversation does this)

1. **Resolve the target.** Device name + how to reach it (serial port / SSH/Telnet
   host, credentials). If a SSH-Connect session is already open to it, the agent will
   reuse it — sessions are shared via the broker.
2. **Find the baseline.** Newest `reports/<HOST>-health-*.json` for this hostname
   (match loosely on the device's short name). If found → delta mode; pass its path.
3. **Spawn ONE general-purpose agent** with a prompt containing, verbatim:
   - The connection parameters (and that the password must only be sent via the
     `enable` / `login` tools — never `run_command`).
   - Full paths to `health-report/template.html`, `health-report/rubric.md`, and
     the vendor reference for the platform (`switch-management/references/`).
   - The baseline JSON path (or "none — snapshot mode").
   - Output paths: `reports/<HOST>-health-<YYYY-MM-DD>.html` + matching `.json`
     (suffix `-recheck`, `-recheck2`… if the file for today already exists).
   - The Agent Contract below, pasted whole.
4. **On return:** relay the agent's summary, open the HTML in the browser
   (`Start-Process <path>`), and offer to commit.

## Agent Contract (paste into the spawned agent's prompt)

> You are generating a standardized device health report. Follow exactly:
>
> **Collect (read-only).** Connect with SSH-Connect (or reuse the named session).
> Log in / enter privileged mode ONLY via the `login` / `enable` tools (they don't
> echo secrets). First command: disable paging (`terminal length 0` or vendor
> equivalent). Then run the FULL battery from `rubric.md`: the core 15, the
> deep-stats extension (optics DOM, PoE, MAC/ARP/route counts, flash free,
> users), and the security-check gathering commands. Use `timeout_secs` ≥ 10
> (≥ 20 for logs). You change NOTHING on the device — `show` commands and
> terminal settings only. Filter logs by the current month (`| include <Mon>`) —
> never dump an unfiltered buffer.
>
> **Assess.** Derive findings strictly per `rubric.md` severity criteria. Assign
> each finding a stable kebab-case id per the rubric. Evaluate EVERY security
> check in rubric.md §"Security checks" as pass/review/fail with one line of
> evidence each — failed/review checks also become findings. Determine the
> software-lifecycle status (OS build date from `show version` + platform
> EoL/LDoS from research). Compute the status word deterministically (worst
> severity). Include exactly one "Healthy" summary finding (not in the JSON).
>
> **Research mitigations (web).** Search for the exact platform + running
> version: vendor end-of-life/EoS bulletins, significant CVEs affecting this
> release, and current hardening guidance. Budget ~3–5 searches. Produce 3–6
> prioritized mitigation items (NOW / NEXT WINDOW / PLAN per rubric.md) each
> citing source links. If the web is unreachable, state that in
> `mitigation_note` and derive items from the failed security checks only.
>
> **Delta (if a baseline JSON was provided).** Match findings by id →
> Resolved / Still open / New. "Resolved" requires evidence; cite it.
>
> **Render.** Fill EVERY `{{slot}}` in template.html (inapplicable → "N/A").
> Duplicate REPEAT blocks per item; keep-and-fill or fully delete the
> BEGIN:DELTA / BEGIN:LINKAGE conditional blocks. Do not alter CSS or section
> structure. Before writing, verify zero occurrences of `{{` remain.
>
> **Write.** The filled HTML and the JSON sidecar (schema in rubric.md) to the
> given output paths. Disconnect the session only if you opened it.
>
> **Return** (≤ 10 lines): status word, finding count by severity, the 2–3
> headline findings, delta summary if applicable, and both output paths. Do not
> paste raw device output.

## Notes

- **Linkage section:** if the user asked to correlate against a peer device (or
  the prior report for this host contains peer-linked findings), include the
  BEGIN:LINKAGE block; otherwise delete it. Cross-device correlation requires
  data from both devices — only assert linkage you can evidence.
- **Unreachable device:** the agent returns the error and writes no files.
- **Platform gaps:** commands invalid on the OS → "not available on this
  platform" in that section. Never fabricate.
