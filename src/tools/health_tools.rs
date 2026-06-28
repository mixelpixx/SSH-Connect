//! `vps_health_check` — a one-call aggregate that runs the standard VPS health
//! battery over an existing SSH connection and returns structured JSON across
//! four categories (system, security, ssl_web, maintenance). The
//! `vps-health-report` skill / `vps-health-reporter` agent apply the scoring
//! rubric and render the HTML report from this output.
//!
//! Design note: the server-side shell does the messy extraction and emits clean
//! lines so Rust parsing stays trivial and robust:
//!   - `metric=VALUE`            → a scalar metric for the category
//!   - `svc|NAME=STATE`          → a systemd service state
//!   - `cert|DOMAIN=DAYS`        → days until a cert expires
//!   - `http|DOMAIN=CODE`        → HTTP status for a vhost
//! (`|` is used as the inner separator so domain names containing dots are safe.)

use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::CallToolResult,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::server::SshConnectServer;
use crate::state::{exec_command, SshConnection};
use crate::tools::util::json_result;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheckParams {
    /// Active connection ID from ssh_connect.
    connection_id: String,
    /// Domains to check cert-expiry and HTTP status for. If omitted, derived from
    /// the Let's Encrypt live certs on the host.
    #[serde(default)]
    domains: Vec<String>,
    /// Categories to include: "system", "security", "ssl_web", "maintenance".
    /// Default: all four.
    #[serde(default)]
    categories: Vec<String>,
}

fn invalid_err(msg: impl ToString) -> ErrorData {
    ErrorData::invalid_params(msg.to_string(), None)
}

/// Run a command, returning combined output or a short error string (never fails
/// the whole health check — a single broken probe shouldn't sink the report).
async fn run(conn: &mut SshConnection, cmd: &str, timeout_ms: u64) -> Result<String, String> {
    match exec_command(&mut conn.handle, cmd, timeout_ms).await {
        Ok((_code, out, err)) => Ok(if out.is_empty() { err } else { out }),
        Err(e) => Err(e.to_string()),
    }
}

/// Parse the line protocol described in the module docs into a category object.
fn parse_lines(raw: &str) -> Map<String, Value> {
    let mut metrics = Map::new();
    let mut services = Map::new();
    let mut certs = Map::new();
    let mut http = Map::new();

    for line in raw.lines() {
        let line = line.trim();
        let Some((lhs, val)) = line.split_once('=') else { continue };
        let val = val.trim();
        if let Some(name) = lhs.strip_prefix("svc|") {
            services.insert(name.to_string(), Value::String(val.to_string()));
        } else if let Some(name) = lhs.strip_prefix("cert|") {
            certs.insert(name.to_string(), num_or_str(val));
        } else if let Some(name) = lhs.strip_prefix("http|") {
            http.insert(name.to_string(), num_or_str(val));
        } else if !lhs.is_empty() {
            metrics.insert(lhs.to_string(), num_or_str(val));
        }
    }

    let mut out = metrics;
    if !services.is_empty() {
        out.insert("services".into(), Value::Object(services));
    }
    if !certs.is_empty() {
        out.insert("certs".into(), Value::Object(certs));
    }
    if !http.is_empty() {
        out.insert("http".into(), Value::Object(http));
    }
    out
}

fn num_or_str(v: &str) -> Value {
    if let Ok(i) = v.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = v.parse::<f64>() {
        return json!(f);
    }
    match v {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::String(v.to_string()),
    }
}

#[tool_router(router = health_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "Run the standard VPS health battery over an existing ssh_connect connection and return structured JSON across system, security, ssl_web, and maintenance. Pair with the vps-health-report skill to score and render an HTML report. Optional: domains[] (else derived from certbot), categories[] to limit scope.")]
    async fn vps_health_check(
        &self,
        Parameters(params): Parameters<HealthCheckParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self
            .pool
            .get(&params.connection_id)
            .await
            .ok_or_else(|| invalid_err(format!("No active connection: '{}'", params.connection_id)))?;

        let want = |c: &str| params.categories.is_empty() || params.categories.iter().any(|x| x == c);
        let mut conn = conn_arc.lock().await;
        let mut report = Map::new();

        // ── system ───────────────────────────────────────────────────────────
        if want("system") {
            let script = r#"
echo "uptime_seconds=$(cut -d. -f1 /proc/uptime 2>/dev/null)"
echo "load_1m=$(cut -d' ' -f1 /proc/loadavg 2>/dev/null)"
echo "cpu_count=$(nproc 2>/dev/null)"
m=$(free -m 2>/dev/null | awk '/^Mem:/{print $2" "$3}'); echo "mem_total_mb=${m% *}"; echo "mem_used_mb=${m#* }"
sw=$(free -m 2>/dev/null | awk '/^Swap:/{print $2}'); echo "swap_total_mb=${sw:-0}"
echo "disk_used_pct=$(df -P / | awk 'NR==2{gsub("%","",$5);print $5}')"
echo "inode_used_pct=$(df -Pi / | awk 'NR==2{gsub("%","",$5);print $5}')"
echo "failed_units=$(systemctl --failed --no-legend --plain 2>/dev/null | grep -c .)"
. /etc/os-release 2>/dev/null; echo "os=${PRETTY_NAME}"; echo "kernel=$(uname -r)"
for s in nginx mariadb mysql php8.3-fpm php8.2-fpm php-fpm docker redis-server fail2ban; do
  if systemctl list-unit-files "$s.service" --no-legend 2>/dev/null | grep -q .; then
    echo "svc|$s=$(systemctl is-active "$s" 2>/dev/null)"
  fi
done
"#;
            let mut sys = match run(&mut conn, script, 45_000).await {
                Ok(raw) => parse_lines(&raw),
                Err(e) => err_obj(&e),
            };
            sys.insert(
                "failed_units_detail".into(),
                Value::String(run(&mut conn, "systemctl --failed --no-legend --plain 2>/dev/null || true", 15_000).await.unwrap_or_default()),
            );
            sys.insert(
                "top_processes".into(),
                Value::String(run(&mut conn, "ps -eo pcpu,pmem,comm --sort=-pcpu 2>/dev/null | head -6", 15_000).await.unwrap_or_default()),
            );
            report.insert("system".into(), Value::Object(sys));
        }

        // ── security ─────────────────────────────────────────────────────────
        if want("security") {
            let script = r#"
echo "ssh_root_login=$(sshd -T 2>/dev/null | awk '/^permitrootlogin/{print $2}')"
echo "ssh_password_auth=$(sshd -T 2>/dev/null | awk '/^passwordauthentication/{print $2}')"
echo "ufw_status=$(ufw status 2>/dev/null | awk '/^Status:/{print $2}')"
echo "fail2ban=$(systemctl is-active fail2ban 2>/dev/null)"
b=$(grep -rhoE '^[[:space:]]*bind-address[[:space:]]*=.*' /etc/mysql/ 2>/dev/null | head -1 | awk -F= '{gsub(/ /,"",$2);print $2}'); echo "mysql_bind=${b:-unknown}"
echo "security_updates=$(apt-get -s upgrade 2>/dev/null | grep -i '^Inst' | grep -ci secur)"
"#;
            let sec = match run(&mut conn, script, 45_000).await {
                Ok(raw) => parse_lines(&raw),
                Err(e) => err_obj(&e),
            };
            report.insert("security".into(), Value::Object(sec));
        }

        // ── ssl_web ──────────────────────────────────────────────────────────
        if want("ssl_web") {
            let domain_list = if params.domains.is_empty() {
                "$(ls /etc/letsencrypt/live 2>/dev/null | grep -v README)".to_string()
            } else {
                params.domains.join(" ")
            };
            let script = format!(
                r#"
for dir in /etc/letsencrypt/live/*/; do
  d=$(basename "$dir"); [ "$d" = "*" ] && continue
  end=$(openssl x509 -enddate -noout -in "$dir/fullchain.pem" 2>/dev/null | cut -d= -f2)
  if [ -n "$end" ]; then echo "cert|$d=$(( ($(date -d "$end" +%s) - $(date +%s)) / 86400 ))"; fi
done
for h in {domain_list}; do
  echo "http|$h=$(curl -skS -o /dev/null -w '%{{http_code}}' --resolve "$h:443:127.0.0.1" --max-time 8 "https://$h/" 2>/dev/null)"
done
"#,
                domain_list = domain_list
            );
            let ssl = match run(&mut conn, &script, 60_000).await {
                Ok(raw) => parse_lines(&raw),
                Err(e) => err_obj(&e),
            };
            report.insert("ssl_web".into(), Value::Object(ssl));
        }

        // ── maintenance ──────────────────────────────────────────────────────
        if want("maintenance") {
            let script = r#"
echo "pending_updates=$(apt-get -s upgrade 2>/dev/null | grep -c '^Inst')"
echo "reboot_required=$(test -f /var/run/reboot-required && echo true || echo false)"
"#;
            let mut maint = match run(&mut conn, script, 45_000).await {
                Ok(raw) => parse_lines(&raw),
                Err(e) => err_obj(&e),
            };
            maint.insert(
                "reboot_pkgs".into(),
                Value::String(run(&mut conn, "cat /var/run/reboot-required.pkgs 2>/dev/null | tr '\\n' ' '", 10_000).await.unwrap_or_default()),
            );
            maint.insert(
                "recent_errors".into(),
                Value::String(run(&mut conn, "(tail -n 40 /var/log/nginx/error.log 2>/dev/null; journalctl -p err -n 30 --no-pager 2>/dev/null) | tail -n 40", 20_000).await.unwrap_or_default()),
            );
            maint.insert(
                "backups".into(),
                Value::String(run(&mut conn, "ls -lt /root/backups /var/backups /home/*/backups 2>/dev/null | head -12 || echo 'no standard backup dirs found'", 15_000).await.unwrap_or_default()),
            );
            report.insert("maintenance".into(), Value::Object(maint));
        }

        Ok(json_result(json!({
            "connection": params.connection_id,
            "categories": if params.categories.is_empty() {
                json!(["system","security","ssl_web","maintenance"])
            } else { json!(params.categories) },
            "report": Value::Object(report),
            "note": "Raw structured battery output. Apply the vps-health-report rubric to score severity and render the HTML report.",
        })))
    }
}

fn err_obj(msg: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("_error".into(), Value::String(msg.to_string()));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mixed_line_protocol() {
        let raw = "load_1m=0.12\ncpu_count=2\nsvc|nginx=active\nsvc|redis-server=inactive\ncert|orchis.ai=89\nhttp|orchis.ai=200\nmysql_bind=127.0.0.1\nreboot_required=true";
        let m = parse_lines(raw);
        assert_eq!(m.get("cpu_count").unwrap(), &json!(2));
        assert_eq!(m.get("load_1m").unwrap(), &json!(0.12));
        assert_eq!(m.get("reboot_required").unwrap(), &json!(true));
        assert_eq!(m.get("mysql_bind").unwrap(), &json!("127.0.0.1"));
        let svcs = m.get("services").unwrap().as_object().unwrap();
        assert_eq!(svcs.get("nginx").unwrap(), &json!("active"));
        let certs = m.get("certs").unwrap().as_object().unwrap();
        assert_eq!(certs.get("orchis.ai").unwrap(), &json!(89));
        let http = m.get("http").unwrap().as_object().unwrap();
        assert_eq!(http.get("orchis.ai").unwrap(), &json!(200));
    }
}
