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
        let terminal = std::env::var("HC_TERMINAL_ID")
            .context("HERDR terminal identity is missing; use --no-daemon with Codex")?;
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
            .with_instructions("You are the project's Feishu main session. Call session_ready first and ack_message for incoming messages. Reply with send_markdown or send_file, choosing the mandatory to field (oc_ chat ID or ou_ user Open ID) according to user intent. It may differ from the source. Terminal output alone does not deliver a reply.")
    }
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
            "Cannot register the main terminal identity. Refusing to expose this project's tools to another terminal. Check daemon availability, HC_TERMINAL_ID, and Codex --no-daemon."
        );
    }
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
