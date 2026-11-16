use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    service::RequestContext,
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
pub struct Ready {
    #[serde(default)]
    pub native_session: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
pub struct Ack {
    pub message_id: String,
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
pub struct EventAck {
    pub event_id: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Finish {
    pub run_id: String,
    pub outcome: String,
    pub summary: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Checkpoint {
    pub task_id: String,
    pub summary: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct CreateTask {
    pub request_id: String,
    pub title: String,
    pub instructions: String,
    pub to: Option<String>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ScheduleTask {
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
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Decision {
    pub run_id: String,
    pub question: String,
    pub to: Option<String>,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Answer {
    pub decision_id: String,
    pub answer: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Job {
    pub run_id: String,
    pub name: String,
}
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct JobDone {
    pub job_id: String,
    pub summary: String,
    pub status: String,
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
    async fn ready(&self, native: &str) -> Result<Value> {
        let terminal = std::env::var("HERDR_PANE_ID")
            .context("Legacy HERDR pane identity is missing; launch through Dauerbot")?;
        self.post("ready",json!({"generation":self.credentials.generation,"terminal":terminal,"native_session":native})).await
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
        description = "Register the current main session as ready. Call this first after startup; provide the native session ID when known to support exact recovery."
    )]
    async fn session_ready(
        &self,
        Parameters(input): Parameters<Ready>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let native = input
            .native_session
            .or_else(|| {
                ctx.meta
                    .get("threadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        result(self.ready(&native).await, false)
    }
    #[tool(
        description = "Acknowledge receipt of a Feishu message_id. Call this first for every incoming <feishu_message> to preserve queue order."
    )]
    async fn ack_message(
        &self,
        Parameters(input): Parameters<Ack>,
    ) -> Result<CallToolResult, ErrorData> {
        result(
            self.post("ack", json!({"message_id":input.message_id}))
                .await,
            false,
        )
    }
    #[tool(
        description = "Confirm receipt of the current durable event. This is not task completion."
    )]
    async fn ack_event(
        &self,
        Parameters(input): Parameters<EventAck>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("event-ack", json!(input)).await, false)
    }
    #[tool(
        description = "Finish the current run with completed, noop, deferred, or failed and a useful progress summary. Settle native background work first; registered active jobs prevent completion and retirement."
    )]
    async fn finish_run(
        &self,
        Parameters(input): Parameters<Finish>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("finish", json!(input)).await, false)
    }
    #[tool(
        description = "Persist a task progress summary before waiting, sleeping, or switching Agent. Durable progress is independent of native conversation history."
    )]
    async fn checkpoint(
        &self,
        Parameters(input): Parameters<Checkpoint>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("checkpoint", json!(input)).await, false)
    }
    #[tool(
        description = "Inspect this project's durable tasks, runs, timers, decisions and background jobs."
    )]
    async fn list_work(&self) -> Result<CallToolResult, ErrorData> {
        result(self.post("work", json!({})).await, false)
    }
    #[tool(
        description = "Create a durable task for this project. It runs through the same main session queue and wakes the Agent when eligible."
    )]
    async fn create_task(
        &self,
        Parameters(input): Parameters<CreateTask>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("tasks", json!(input)).await, false)
    }
    #[tool(
        description = "Schedule future work: once with RFC3339 at, interval with interval_seconds, or cron with a five-field expression. Specify IANA timezone. The daemon persists the timer and wakes this Agent when due; no need to keep the process alive."
    )]
    async fn schedule_task(
        &self,
        Parameters(input): Parameters<ScheduleTask>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("timers", json!(input)).await, false)
    }
    #[tool(
        description = "Persist a user decision and defer the current task. Native login and permission prompts must remain in the interactive terminal instead. Choose a responsible Feishu recipient in to when known."
    )]
    async fn request_decision(
        &self,
        Parameters(input): Parameters<Decision>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("decision", json!(input)).await, false)
    }
    #[tool(
        description = "Record the user's answer to a pending decision. The answer creates a durable event for the original task."
    )]
    async fn answer_decision(
        &self,
        Parameters(input): Parameters<Answer>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("decision-answer", json!(input)).await, false)
    }
    #[tool(
        description = "Register a background command or subagent job before launching it. Active jobs prevent this execution from being retired."
    )]
    async fn register_job(
        &self,
        Parameters(input): Parameters<Job>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("jobs", json!(input)).await, false)
    }
    #[tool(
        description = "Report a registered background job completed or failed, with a useful result summary."
    )]
    async fn complete_job(
        &self,
        Parameters(input): Parameters<JobDone>,
    ) -> Result<CallToolResult, ErrorData> {
        result(self.post("job-complete", json!(input)).await, false)
    }
    #[tool(
        description = "Send Markdown to the Feishu chat or user selected in to. Use oc_ for chat IDs or ou_ for user Open IDs; replies may go to a different destination. Use a unique request_id for each message and reuse it only for retries with the same content and recipient. Only status=sent confirms success."
    )]
    async fn send_markdown(
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
            .with_instructions("You are the project's Feishu main session. Call session_ready first, ack_event for each durable event, and ack_message for nested Feishu messages. Persist progress and call finish_run when settled. Register background jobs and use schedule_task or request_decision for later work. Reply with send_markdown or send_file, choosing the mandatory to field (oc_ chat ID or ou_ user Open ID) according to user intent. It may differ from the source. Terminal output alone does not deliver a reply.")
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
             WHERE terminal IS NOT NULL AND enabled=1
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
    let server = BridgeMcp {
        credentials,
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(95))
            .build()?,
        tool_router: BridgeMcp::tool_router(),
    };
    // terminal.run may not have committed its returned address when the CLI launches MCP.
    let mut registered = false;
    for _ in 0..12 {
        if server.ready("").await.is_ok() {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !registered {
        bail!(
            "Cannot register the main terminal identity. Refusing to expose this project's tools to another terminal. Check daemon availability and the legacy HERDR_PANE_ID environment."
        );
    }
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod managed_tests {
    use super::*;

    #[test]
    fn global_entry_resolves_current_generation_without_running_database_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(dir.path().join("bridge.sqlite3")).unwrap();
        connection.execute_batch(
            "CREATE TABLE projects(id TEXT,generation TEXT,terminal TEXT,enabled INTEGER,execution TEXT);
             CREATE TABLE messages(status TEXT);
             INSERT INTO messages VALUES('sending');",
        ).unwrap();
        let execution = json!({"backend":"legacy","pane_id":"w1:p2","terminal_id":"terminal-1","generation":"generation-1"});
        connection
            .execute(
                "INSERT INTO projects VALUES('project-1','generation-1','terminal-1',1,?)",
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
