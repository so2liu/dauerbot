use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub url: String,
    pub project: String,
    pub token: String,
    pub generation: String,
}
#[derive(Clone)]
pub struct BridgeMcp {
    credentials: Credentials,
    http: reqwest::Client,
    #[expect(dead_code, reason = "rmcp tool_handler macro registers the router")]
    tool_router: ToolRouter<Self>,
}
#[derive(Deserialize, JsonSchema)]
pub struct Markdown {
    /// Feishu chat ID oc_... (group or direct) or user Open ID ou_...; the agent chooses the recipient.
    pub to: String,
    pub text: String,
    pub request_id: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct File {
    /// Feishu chat ID oc_... (group or direct) or user Open ID ou_....
    pub to: String,
    pub path: String,
    pub request_id: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ScheduleRun {
    /// Stable ID for this schedule creation; reuse on retries.
    pub request_id: String,
    pub name: String,
    pub prompt: String,
    /// once, interval, or cron
    pub kind: String,
    /// RFC3339 timestamp for one-shot schedules.
    pub at: Option<String>,
    pub interval_seconds: Option<u64>,
    pub cron: Option<String>,
    /// IANA timezone such as Asia/Shanghai or America/New_York.
    pub timezone: String,
    pub to: Option<String>,
    /// user for work the user asked to schedule; internal for your own follow-up checks.
    /// Set this explicitly for new schedules. Missing/empty is accepted for legacy retries.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub purpose: String,
}
impl BridgeMcp {
    async fn post(&self, operation: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!(
                "{}/api/projects/{}/{}",
                self.credentials.url, self.credentials.project, operation
            ))
            .bearer_auth(&self.credentials.token)
            .header("x-hfb-generation", &self.credentials.generation)
            .json(&body)
            .send()
            .await
            .context("Cannot connect to the Dauerbot daemon")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!(
                "The Dauerbot daemon does not support this MCP contract. Use a matching daemon build (API protocol {}).",
                crate::api::PROTOCOL
            );
        }
        let result: Value = response.json().await?;
        if !status.is_success() {
            bail!(
                "{}",
                result["error"]
                    .as_str()
                    .unwrap_or("Dauerbot operation failed")
            );
        }
        Ok(result)
    }
    async fn register(&self) -> Result<Value> {
        let terminal = std::env::var("HERDR_PANE_ID")
            .context("Legacy HERDR pane identity is missing; launch through Dauerbot")?;
        let registration = self
            .post(
                "register",
                json!({"generation":self.credentials.generation,"terminal":terminal}),
            )
            .await?;
        anyhow::ensure!(
            registration["api_protocol"].as_u64() == Some(crate::api::PROTOCOL),
            "The Dauerbot daemon uses an incompatible API protocol"
        );
        Ok(registration)
    }
}
fn result(value: Result<Value>, must_sent: bool) -> Result<CallToolResult, ErrorData> {
    match value {
        Ok(v) => {
            let failed = must_sent && v["status"] != "sent";
            let text = serde_json::to_string(&v).unwrap_or_default();
            Ok(if failed {
                CallToolResult::error(vec![ContentBlock::text(text)])
            } else {
                CallToolResult::success(vec![ContentBlock::text(text)])
            })
        }
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
            e.to_string(),
        )])),
    }
}

#[tool_router]
impl BridgeMcp {
    #[tool(
        description = "Schedule future work: once with RFC3339 at, interval with interval_seconds, or cron with a five-field expression. Specify IANA timezone and purpose: user for work the user asked to schedule, internal for your own follow-up checks. Internal schedules execute normally but stay out of the desktop schedule list. Set purpose explicitly for new schedules; preserve the original payload and request_id on retries. The daemon persists the timer and wakes this Agent when due; no need to keep the process alive."
    )]
    async fn schedule_run(
        &self,
        Parameters(input): Parameters<ScheduleRun>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("timers", json!(input)).await, false)
    }
    #[tool(
        description = "Send Markdown to the Feishu chat or user selected in to. Use oc_ for chat IDs or ou_ for user Open IDs; replies may go to a different destination. Use a unique request_id for each message and reuse it only for retries with the same content and recipient. Only status=sent confirms success."
    )]
    async fn send_md(
        &self,
        Parameters(input): Parameters<Markdown>,
    ) -> Result<CallToolResult, ErrorData> {
        result(
            self.post(
                "outbox",
                json!({"to":input.to,"content":input.text,"request_id":input.request_id}),
            )
            .await,
            true,
        )
    }
    #[tool(
        description = "Send a local file up to 100 MB to the recipient selected in to (oc_ chat ID or ou_ user Open ID). The file must be inside the project directory or Dauerbot cache. Use a new request_id for each new file request."
    )]
    async fn send_file(
        &self,
        Parameters(input): Parameters<File>,
    ) -> Result<CallToolResult, ErrorData> {
        result(
            self.post(
                "outbox",
                json!({"to":input.to,"path":input.path,"request_id":input.request_id}),
            )
            .await,
            true,
        )
    }
}
#[tool_handler]
impl ServerHandler for BridgeMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("You are the project’s Feishu session. Incoming messages steer your current work immediately. Reply and ask questions using send_md or send_file with the mandatory to field (oc_ chat ID or ou_ user Open ID). Terminal output alone does not deliver a reply. Use schedule_run for future once, interval, or cron runs. Set purpose=user for work the user asked to schedule and purpose=internal for your own follow-up checks; internal schedules run normally but stay out of the desktop schedule list. Preserve the original payload and request_id on retries. Native HERDR state manages the session lifecycle; no task bookkeeping is required. Preserve uncertain send outcomes and reuse request_id only for the same request.")
    }
}

/// A global agy entry must resolve each invocation from its inherited HERDR identity,
/// rather than retaining whichever project's credentials were written most recently.
pub async fn serve_managed(dir: &Path) -> Result<()> {
    let (Ok(pane), Ok(socket)) = (
        std::env::var("HERDR_PANE_ID"),
        std::env::var("HERDR_SOCKET_PATH"),
    ) else {
        return serve_unmanaged().await;
    };
    let cfg: crate::config::Config =
        serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
    let expected = crate::herdr::socket_path(&cfg)?;
    let owned = std::fs::canonicalize(&socket)
        .ok()
        .zip(std::fs::canonicalize(expected).ok())
        .is_some_and(|(actual, expected)| actual == expected);
    if !owned {
        return serve_unmanaged().await;
    }
    let Some(path) = managed_credentials_path(dir, &pane)? else {
        return serve_unmanaged().await;
    };
    serve(&path).await
}

struct Unmanaged;
impl ServerHandler for Unmanaged {}

async fn serve_unmanaged() -> Result<()> {
    // Ordinary agy sessions share the global configuration but receive no project tools.
    Unmanaged
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

fn managed_credentials_path(dir: &Path, pane: &str) -> Result<Option<std::path::PathBuf>> {
    use rusqlite::OptionalExtension;
    // Opening the application Db would run startup recovery against live work.
    let connection = rusqlite::Connection::open_with_flags(
        dir.join("bridge.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let identity: Option<(String, String)> = connection
        .query_row(
            "SELECT id,generation FROM projects
             WHERE terminal IS NOT NULL
               AND json_extract(execution,'$.backend')='legacy'
               AND json_extract(execution,'$.pane_id')=?1
               AND json_extract(execution,'$.generation')=generation
               AND json_extract(execution,'$.terminal_id')=terminal",
            [&pane],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((project, generation)) = identity else {
        return Ok(None);
    };
    Ok(Some(
        dir.join("sessions")
            .join(project)
            .join(generation)
            .join("credentials.json"),
    ))
}

pub async fn serve(path: &Path) -> Result<()> {
    let credentials: Credentials = serde_json::from_slice(&std::fs::read(path)?)?;
    let url = reqwest::Url::parse(&credentials.url)?;
    if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
        bail!("MCP credentials may connect only to a local daemon");
    }
    historical_dispatch(path, &credentials).await?;
    let server = BridgeMcp {
        credentials,
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(95))
            .build()?,
        tool_router: BridgeMcp::tool_router(),
    };
    // terminal.run may not have committed its returned address when the CLI launches MCP.
    let mut registration_error = None;
    for _ in 0..12 {
        match server.register().await {
            Ok(_) => {
                registration_error = None;
                break;
            }
            Err(error) => registration_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if let Some(error) = registration_error {
        bail!(
            "Cannot register the main terminal identity: {error}. Check daemon compatibility, availability and the legacy HERDR_PANE_ID environment."
        );
    }
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

/// An already-running old daemon still writes the mutable bundle path. Dispatch
/// to its original signed executable; do not emulate retired MCP tool contracts.
async fn historical_dispatch(path: &Path, credentials: &Credentials) -> Result<()> {
    let Some(data) = path.ancestors().nth(4) else {
        return Ok(());
    };
    if path
        .ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .is_none_or(|n| n != "sessions")
    {
        return Ok(());
    }
    // Never Config::load or App::new here: those can change config and recover a live DB.
    let cfg: crate::config::Config =
        serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
    anyhow::ensure!(
        credentials.url.trim_end_matches('/') == cfg.url(),
        "MCP daemon address differs from its private configuration"
    );
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()?;
    let state: Value = http
        .get(format!("{}/api/state", cfg.url()))
        .bearer_auth(&cfg.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let protocol = state["api_protocol"]
        .as_u64()
        .context("Daemon protocol is missing")?;
    if protocol == crate::api::PROTOCOL {
        return Ok(());
    }
    let version = state["version"]
        .as_str()
        .context("Daemon version is missing")?;
    anyhow::ensure!(
        !crate::update::is_newer(version, crate::update::CURRENT)?,
        "update_required: The daemon is newer than this MCP; open the matching newer app"
    );
    let project = state["projects"]
        .as_array()
        .and_then(|ps| {
            ps.iter().find(|p| {
                p["id"] == credentials.project && p["generation"] == credentials.generation
            })
        })
        .context("The authenticated daemon no longer owns this generation")?;
    anyhow::ensure!(
        project["terminal"].is_string(),
        "The historical execution is no longer live"
    );
    crate::update::record_service(data, &state)?;
    let retained = match crate::update::historical_companion(data, version, protocol) {
        Ok(path) => path,
        Err(error) => {
            let message = format!(
                "update_required: Cannot reconnect this Agent to Dauerbot {version} (API {protocol}): {error:#}. Open the installed Dauerbot app, update its background service, then choose Recover Agent after update. Saved messages and uncertain results are preserved."
            );
            let mut status = crate::update::Status::new(crate::update::Preferences::load(data)?);
            status.phase = "update_required".into();
            status.message = message.clone();
            status.save(data)?;
            bail!("{message}");
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        retained.inherit_across_exec()?;
        let error = std::process::Command::new(retained.path())
            .args(["mcp", "--credentials"])
            .arg(path)
            .exec();
        Err(error).context("Cannot execute the retained historical MCP")
    }
    #[cfg(not(unix))]
    {
        let _ = retained;
        bail!("Historical MCP recovery requires the native macOS app")
    }
}

#[cfg(test)]
mod managed_tests {
    use super::*;

    #[test]
    fn global_entry_resolves_current_generation_without_running_database_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(dir.path().join("bridge.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE projects(id TEXT,generation TEXT,terminal TEXT,execution TEXT);
             CREATE TABLE messages(status TEXT);
             INSERT INTO messages VALUES('sending');",
            )
            .unwrap();
        let execution = json!({"backend":"legacy","pane_id":"w1:p2","terminal_id":"terminal-1","generation":"generation-1"});
        connection
            .execute(
                "INSERT INTO projects VALUES('project-1','generation-1','terminal-1',?)",
                [execution.to_string()],
            )
            .unwrap();
        assert_eq!(
            managed_credentials_path(dir.path(), "w1:p2")
                .unwrap()
                .unwrap(),
            dir.path()
                .join("sessions/project-1/generation-1/credentials.json")
        );
        assert!(
            managed_credentials_path(dir.path(), "w9:p9")
                .unwrap()
                .is_none()
        );
        connection
            .execute("UPDATE projects SET generation='generation-2'", [])
            .unwrap();
        assert!(
            managed_credentials_path(dir.path(), "w1:p2")
                .unwrap()
                .is_none()
        );
        let state: String = connection
            .query_row("SELECT status FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "sending");
    }
}
