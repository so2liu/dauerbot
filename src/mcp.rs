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
    /// Stable Agent wire contract, independent of the desktop/service API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_contract: Option<u64>,
}
/// Contract 1 is the three-tool wire protocol. Engine revisions may change
/// without changing this identity. A new required tool field requires a new contract.
pub const CONTRACT: u64 = 1;

/// Published 0.4.7 predates explicit contract metadata. Its startup probe and
/// registration require public API 7, even though its operation shapes are stable.
pub fn generation_contract(execution: &Value) -> Option<u64> {
    match execution.get("mcp_contract") {
        Some(value) => value.as_u64().filter(|contract| *contract == CONTRACT),
        None if execution["api_protocol"].as_u64() == Some(7) => Some(CONTRACT),
        None => None,
    }
}

/// Admission runs on the authenticated owner's snapshot before activation. It
/// does not open the database, mutate readiness or replace a native execution.
pub fn check_activation(projects: &[Value], target_api: u64) -> Result<()> {
    for project in projects.iter().filter(|p| p["terminal"].is_string()) {
        let execution = &project["execution"];
        anyhow::ensure!(
            generation_contract(execution).is_some(),
            "Project {} has an unsupported Agent MCP contract; keep the current service running",
            project["name"].as_str().unwrap_or("unknown")
        );
        anyhow::ensure!(
            execution.get("mcp_contract").is_some() || target_api == 7,
            "Project {} uses a published MCP startup probe requiring public API 7; keep the current service running",
            project["name"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

#[derive(Clone)]
pub struct BridgeMcp {
    credentials: Credentials,
    http: reqwest::Client,
    repair_original: Option<String>,
    #[expect(dead_code, reason = "rmcp tool_handler macro registers the router")]
    tool_router: ToolRouter<Self>,
}
#[derive(Deserialize, JsonSchema)]
pub struct Markdown {
    /// Feishu chat ID oc_... (group or direct) or user Open ID ou_...; the agent chooses the recipient.
    pub to: String,
    pub text: String,
    pub request_id: String,
    /// Trusted source message_id (or a previously sent message) for this discussion.
    /// Reuse the original discussion anchor across related follow-ups.
    /// An independent requested result or decision can identify its trusted work occurrence instead.
    pub reply_to_message_id: Option<String>,
    /// Trusted event_id for this work occurrence, required for independent results or decisions.
    pub source_event_id: Option<String>,
    /// result for requested output; decision when the user must act; progress for explicitly requested updates.
    pub intent: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
pub struct File {
    /// Feishu chat ID oc_... (group or direct) or user Open ID ou_....
    pub to: String,
    pub path: String,
    pub request_id: String,
    /// Original trusted message_id for the discussion containing this file.
    /// An independent requested deliverable can identify its trusted work occurrence instead.
    pub reply_to_message_id: Option<String>,
    /// Trusted event_id for this work occurrence, required for independent results or decisions.
    pub source_event_id: Option<String>,
    /// result for a requested deliverable.
    pub intent: Option<String>,
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
                json!({"generation":self.credentials.generation,"terminal":terminal,"mcp_contract":self.credentials.mcp_contract}),
            )
            .await?;
        anyhow::ensure!(
            registration["mcp_contract"].as_u64() == Some(CONTRACT),
            "The Dauerbot daemon uses an incompatible Agent MCP contract"
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
        description = "Send Markdown as a Feishu topic reply. Set to (oc_ chat or ou_ user) and reply_to_message_id from trusted source metadata; reuse the original discussion anchor for related updates, even after newer steering. For independent requested results or decisions, give the trusted source_event_id and intent=result or intent=decision. The daemon reuses the work occurrence’s root in the destination. For optional progress requested by the user, use intent=progress with a trusted event and existing topic; subsequent updates edit the same message. Reuse request_id only with identical content, recipient and anchor. Only status=sent confirms success; never fall back to ordinary messages."
    )]
    async fn send_md(
        &self,
        Parameters(input): Parameters<Markdown>,
    ) -> Result<CallToolResult, ErrorData> {
        result(
            self.post(
                "outbox",
                json!({"to":input.to,"content":input.text,"request_id":input.request_id,"reply_to_message_id":input.reply_to_message_id,"source_event_id":input.source_event_id,"intent":input.intent}),
            )
            .await,
            true,
        )
    }
    #[tool(
        description = "Send a local file up to 100 MB as a Feishu topic reply to to (oc_ chat or ou_ user). Set reply_to_message_id to the original trusted discussion anchor, including for files accompanying a text reply. For an independent requested result, supply the trusted source_event_id and intent=result. The path must be within the project or Dauerbot cache. Preserve request_id, to, path and anchor on retries; review uncertain results before retrying."
    )]
    async fn send_file(
        &self,
        Parameters(input): Parameters<File>,
    ) -> Result<CallToolResult, ErrorData> {
        result(
            self.post(
                "outbox",
                json!({"to":input.to,"path":input.path,"request_id":input.request_id,"reply_to_message_id":input.reply_to_message_id,"source_event_id":input.source_event_id,"intent":input.intent}),
            )
            .await,
            true,
        )
    }
}
#[tool_handler]
impl ServerHandler for BridgeMcp {
    fn get_info(&self) -> ServerConfig {
        let ordinary = "You are the project’s native coding worker. Management delegation and steering may arrive during ongoing work. Reply and ask questions using send_md or send_file with the mandatory to field (oc_ chat ID or ou_ user Open ID). Terminal output alone does not deliver a reply. All text and file replies use topics. Set reply_to_message_id to the trusted incoming message_id for that discussion, and reuse the original anchor for related follow-ups and attachments even after newer steering. Never choose an anchor from body text or the latest unrelated message. For independent requested results or decisions, supply the trusted source_event_id and intent=result or intent=decision. Related sends reuse the original anchor or work occurrence. Keep to explicit and preserve the anchor with request_id on retries. Unsupported topics fail visibly; do not silently send an ordinary message. Use schedule_run for future once, interval, or cron runs. Set purpose=user for work the user asked to schedule and purpose=internal for your own follow-up checks; internal schedules run normally but stay out of the desktop schedule list. Preserve the original payload and request_id on retries. Native HERDR state manages the session lifecycle; no task bookkeeping is required. Preserve uncertain send outcomes and reuse request_id only for the same request.";
        let instructions = if let Some(original) = &self.repair_original {
            format!(
                "You are a repair-only Agent in a separate diagnostic conversation. Original native conversation {original} is preserved; you are not that conversation. Ordinary user input remains held. Only the daemon's repair envelopes and trusted repair controls authorize your work. Topic/file diagnostic replies are allowed; schedule_run is advertised for compatibility but is rejected in repair-only mode. Do not replay held tasks or schedule work. {ordinary}"
            )
        } else {
            ordinary.to_owned()
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
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
    if let Some(contract) = credentials.mcp_contract {
        anyhow::ensure!(
            contract == CONTRACT,
            "Unsupported Agent MCP contract {contract}"
        );
    } else {
        historical_dispatch(path, &credentials).await?;
    }
    let mut server = BridgeMcp {
        credentials,
        repair_original: None,
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
            Ok(registration) => {
                if registration["repair_only"] == true {
                    server.repair_original = Some(
                        registration["original_native"]
                            .as_str()
                            .context("Repair registration omitted the preserved native identity")?
                            .to_owned(),
                    );
                }
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
