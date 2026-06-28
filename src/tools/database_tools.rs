use rmcp::{
    ErrorData,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::server::SshConnectServer;
use crate::state::exec_command;

fn internal_err(msg: impl ToString) -> ErrorData {
    ErrorData::internal_error(msg.to_string(), None)
}

fn invalid_err(msg: impl ToString) -> ErrorData {
    ErrorData::invalid_params(msg.to_string(), None)
}

fn default_username() -> String {
    "root".to_string()
}

// ── param structs ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct MysqlParams {
    /// Active connection ID from ssh_connect
    connection_id: String,
    /// Action: list-databases | list-tables | query | backup | restore | create-db | drop-db
    action: String,
    /// Target database name (required for list-tables, query, backup, restore, create-db, drop-db)
    database: Option<String>,
    /// SQL statement (required for query action)
    query: Option<String>,
    /// Remote file path for backup (output) or restore (input)
    file_path: Option<String>,
    /// MySQL username (default: "root")
    #[serde(default = "default_username")]
    username: String,
    /// MySQL password (passed via MYSQL_PWD env var, not -p flag, to avoid exposure in ps)
    password: Option<String>,
}

// ── tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = database_tool_router, vis = "pub(crate)")]
impl SshConnectServer {
    #[tool(description = "MySQL/MariaDB management. action: list-databases | list-tables | query | backup | restore | create-db | drop-db. Password is passed securely via MYSQL_PWD env var (not exposed in ps).")]
    async fn ubuntu_mysql(
        &self,
        Parameters(params): Parameters<MysqlParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let conn_arc = self.pool.get(&params.connection_id).await.ok_or_else(|| {
            invalid_err(format!("No active connection: '{}'", params.connection_id))
        })?;

        // Build password prefix: MYSQL_PWD='...' if password supplied
        let pwd_prefix = match &params.password {
            Some(pw) => format!("MYSQL_PWD={} ", shell_quote(pw)),
            None => String::new(),
        };
        let user = shell_quote(&params.username);

        // Helper closures
        let require_db = || {
            params.database.as_deref().ok_or_else(|| invalid_err("'database' required for this action"))
        };
        let require_file = || {
            params.file_path.as_deref().ok_or_else(|| invalid_err("'filePath' required for this action"))
        };

        let (cmd, timeout_ms) = match params.action.as_str() {
            "list-databases" => (
                format!("{pwd_prefix}mysql -u{user} -e 'SHOW DATABASES;' 2>&1"),
                60_000u64,
            ),
            "list-tables" => {
                let db = require_db()?;
                (format!("{pwd_prefix}mysql -u{user} {} -e 'SHOW TABLES;' 2>&1", shell_quote(db)), 60_000)
            }
            "query" => {
                let db = require_db()?;
                let sql = params.query.as_deref().ok_or_else(|| invalid_err("'query' required for query action"))?;
                (format!(
                    "{pwd_prefix}mysql -u{user} {} -e {} 2>&1",
                    shell_quote(db),
                    shell_quote(sql)
                ), 60_000)
            }
            "backup" => {
                let db = require_db()?;
                let fp = require_file()?;
                (format!(
                    "{pwd_prefix}mysqldump -u{user} {} > {} 2>&1",
                    shell_quote(db),
                    shell_quote(fp)
                ), 600_000)
            }
            "restore" => {
                let db = require_db()?;
                let fp = require_file()?;
                (format!(
                    "{pwd_prefix}mysql -u{user} {} < {} 2>&1",
                    shell_quote(db),
                    shell_quote(fp)
                ), 600_000)
            }
            "create-db" => {
                let db = require_db()?;
                (format!(
                    "{pwd_prefix}mysql -u{user} -e 'CREATE DATABASE `{}`;' 2>&1",
                    db.replace('`', "\\`")
                ), 60_000)
            }
            "drop-db" => {
                let db = require_db()?;
                (format!(
                    "{pwd_prefix}mysql -u{user} -e 'DROP DATABASE `{}`;' 2>&1",
                    db.replace('`', "\\`")
                ), 60_000)
            }
            other => return Err(invalid_err(format!(
                "Unknown action '{}'. Use: list-databases | list-tables | query | backup | restore | create-db | drop-db",
                other
            ))),
        };

        let mut conn = conn_arc.lock().await;
        let (exit_code, stdout, stderr) = exec_command(&mut conn.handle, &cmd, timeout_ms)
            .await
            .map_err(|e| internal_err(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "exit_code: {}\n{}{}",
            exit_code,
            stdout,
            if stderr.is_empty() { String::new() } else { format!("\n[stderr]\n{}", stderr) }
        ))]))
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
