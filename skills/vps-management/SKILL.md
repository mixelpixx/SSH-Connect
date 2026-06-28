---
name: vps-management
description: Use when administering a Linux/Ubuntu VPS or server over the SSH-Connect server — running shell commands with exit codes, deploying websites, controlling nginx/systemd services, managing SSL certificates and the UFW firewall, updating packages, and operating MySQL/MariaDB, Redis, and Docker. Emphasizes safe-change discipline (back up, change, verify) for production systems.
---

# VPS / Server Management

The playbook for driving the **SSH-Connect** server's server-ops tools to
administer Linux/Ubuntu hosts. These tools use a separate SSH connection model
from the interactive switch tools: connect with **`ssh_connect`** (returns a
`connectionId`) and run commands with **`ssh_exec`**, which captures **exit code,
stdout, and stderr** — use the exit code to decide success, never guess from text.

> For network switches and serial consoles use `connect` + `run_command` instead
> (see the `switch-management` skill). `ssh_connect`/`ssh_exec` are for servers.

## The cardinal rule: back up before you change

Before any change to a production service or config:

1. Capture current state (config file, DB dump, service status, package list).
2. Make the change.
3. Verify it took effect (re-read config, `is-active`, HTTP check, query).
4. Only then treat it as done. If verification fails, roll back from step 1.

## Connecting & running commands

```
ssh_connect { host, username, password | privateKeyPath }      -> { connectionId }
ssh_exec    { connectionId, command, cwd?, timeout? }            -> exit_code / stdout / stderr
```

- Check `exit_code == 0` for success. A non-zero code with empty stderr still failed.
- Long operations (package upgrades, backups, `docker pull`) need a larger `timeout`.
- Prefer the dedicated tools below over raw `ssh_exec` when one fits — they encode
  the safe form of the operation and parse results for you.

## Services & web stack

| Task | Tool |
|------|------|
| Start/stop/reload **nginx**, test config | `ubuntu_nginx_control` (always `test` before `reload`) |
| Any systemd unit (status/start/stop/enable/logs) | `ubuntu_service_control` |
| Enable/disable nginx vhosts, view/write/test site configs | `vps_nginx_config` |
| Issue/renew/list Let's Encrypt certs | `ubuntu_ssl_certificate` |
| Deploy site files (with backup/restore) | `ubuntu_website_deployment` |
| apt update/upgrade/autoremove (security-only mode) | `ubuntu_update_packages` |
| UFW firewall rules | `ubuntu_ufw_firewall` |

**nginx discipline:** edit config → `ubuntu_nginx_control { action: "test" }` →
only `reload` if the test passes. A failed reload on a bad config can drop the site.

**Firewall discipline:** before enabling UFW or tightening rules, make sure the SSH
port is allowed — locking out port 22 ends your session. Add the allow rule first,
verify, then enable.

## Data services

| Task | Tool |
|------|------|
| List DBs/tables, query, backup/restore, create/drop DB | `ubuntu_mysql` (password via env, not echoed) |
| Redis ping/info/get/del/flush-cache/dbsize | `vps_redis` |
| Docker ps/start/stop/restart/logs/exec/pull/inspect | `vps_docker` |

**Database discipline:** always `backup` before `restore`, `drop-db`, or a
schema-changing `query`. Treat `drop-db` and `flush-cache` as destructive — confirm
the target name first.

## Inspecting a host

- `vps_system_stats` — disk (df), memory, CPU count, load average, OS info. Start here.
- `vps_logs` — tail nginx-access/nginx-error/syslog/auth/mysql/php/journalctl, with
  an optional grep filter. Filter by recency/keyword rather than dumping everything.
- `vps_process` — list (top by CPU) / find / kill (choose the signal deliberately).
- `vps_file_read` / `vps_file_ops` — read files; delete/mkdir/chmod/chown/find/copy/
  move (recursive + sudo where needed). `vps_cron` — list/add/remove cron jobs.

## Standard health check

For a full read-only assessment + HTML report, use the **`vps-health-report`** skill
(or the `vps-health-reporter` agent). The fast path is the **`vps_health_check`** tool —
one call returns structured JSON across system, security, SSL/web, and maintenance:

```
vps_health_check { connectionId, domains?: ["orchis.ai", ...] }
```

## Standard workflows

Run these the same way every time so results are predictable and safe.

### Patch + reboot
1. **Snapshot:** record state to compare against — `vps_system_stats`, `ubuntu_service_control status` for key units, and the running kernel (`ssh_exec uname -r`).
2. **Update:** `ubuntu_update_packages { upgrade: true, autoremove: true }` (pass a larger `timeoutMs` for slow mirrors). For very long upgrades use the long-op pattern below instead.
3. **Reboot:** `ssh_exec` a detached reboot so the call returns cleanly — `nohup bash -c 'sleep 2; systemctl reboot' >/dev/null 2>&1 &`.
4. **Wait + reconnect:** the SSH connection drops. Wait ~60–90s, then `ssh_connect` again, retrying with backoff until it answers.
5. **Verify:** new kernel (`uname -r`), `reboot-required` cleared, all services `is-active`, sites returning expected HTTP codes. A `vps_health_check` here is the cleanest confirmation.

### SSL / certbot renewal troubleshooting
1. Check expiry: `ubuntu_ssl_certificate { action: "list" }` (or `vps_health_check` ssl_web).
2. Read the failure: `ubuntu_service_control { service: "certbot", action: "status" }` and `vps_logs { service: "journalctl", unit: "certbot" }`.
3. Common cause: an authenticator that needs port 80 (`standalone`) while nginx holds it → switch the failing certs to the nginx/webroot authenticator and reissue.
4. Confirm auto-renewal with a dry run, then clear the failed unit: `systemctl reset-failed certbot.service`.

### Service recovery
`ubuntu_service_control status` → `vps_logs` (the unit's journal) → fix the root cause → restart → verify `is-active`. Escalate to reboot only if a unit can't be recovered in place.

### Long-running operations (the reliable pattern)
`ssh_exec`'s timeout now **terminates** the remote command and returns an error (best-effort — OpenSSH may ignore the signal on a non-PTY exec). For multi-minute work (big upgrades, image pulls, DB restores), don't just raise the timeout — run it **detached and poll a log file**:

```
ssh_exec: nohup bash -c '<long cmd>; echo "[[DONE rc=$?]]" >> /root/op.log' >/root/op.log 2>&1 & echo launched
# then poll every ~15–20s:
ssh_exec: grep -c "\[\[DONE" /root/op.log; tail -3 /root/op.log
```

### Usage notes
- **Sequence dependent calls.** Tool calls execute concurrently; let `ssh_connect` (or any prerequisite) fully return before issuing calls that depend on it, or they'll race the not-yet-ready connection.
- **Secrets discipline.** Prefer key auth; never put a password in `ssh_exec` output; pass DB passwords via env (`MYSQL_PWD`) — `ubuntu_mysql` already does. Don't echo secrets into reports.

## Quick troubleshooting

| Symptom | First checks |
|---------|--------------|
| Site down | `ubuntu_service_control { service: "nginx", action: "status" }`, `vps_logs nginx-error`, `vps_system_stats` (disk full?) |
| 502 / upstream error | Is the app/container up? `vps_docker ps` / `ubuntu_service_control status`; app logs via `vps_logs` |
| Cert expired | `ubuntu_ssl_certificate` list/renew |
| Can't connect after firewall change | UFW likely blocked SSH — recover via console/serial and re-allow the port |
| High load | `vps_process` (top CPU), `vps_system_stats` (load avg / memory pressure) |
