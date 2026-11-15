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
    pub text: String,
    pub request_id: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct File {
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
            .context("无法连接 Bridge daemon")?;
        let status = response.status();
        let result: Value = response.json().await?;
        if !status.is_success() {
            bail!("{}", result["error"].as_str().unwrap_or("Bridge 操作失败"));
        }
        Ok(result)
    }
    async fn ready(&self, native: &str) -> Result<Value> {
        let terminal = std::env::var("HC_TERMINAL_ID")
            .context("未继承 HERDR 终端身份；Codex 请使用 --no-daemon")?;
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
        description = "登记当前主会话已就绪。启动后先调用；如知道原生会话 ID 请提供，以便精确恢复。"
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
        description = "确认收到指定飞书 message_id。每条 [FEISHU_MESSAGE] 到达后先调用，防止重复或乱序。"
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
        description = "向当前项目绑定的飞书聊天发送 Markdown。request_id 每次新消息唯一，重试同一消息复用；只有 status=sent 才表示发送成功。"
    )]
    async fn send_markdown(
        &self,
        Parameters(input): Parameters<Markdown>,
    ) -> Result<CallToolResult, ErrorData> {
        result(
            self.post(
                "outbox",
                json!({"content":input.text,"request_id":input.request_id}),
            )
            .await,
            true,
        )
    }
    #[tool(
        description = "向当前项目飞书聊天发送本地附件（最大100MB）。路径须在项目目录或Bridge缓存内。新文件使用新的request_id。"
    )]
    async fn send_file(
        &self,
        Parameters(input): Parameters<File>,
    ) -> Result<CallToolResult, ErrorData> {
        result(
            self.post(
                "outbox",
                json!({"path":input.path,"request_id":input.request_id}),
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
            .with_instructions("你是飞书项目主会话。先 session_ready，收到用户消息先 ack_message；请使用 send_markdown/send_file 向用户回复，不将仅在终端显示结果当作已送达。")
    }
}
pub async fn serve(path: &Path) -> Result<()> {
    let credentials: Credentials = serde_json::from_slice(&std::fs::read(path)?)?;
    let url = reqwest::Url::parse(&credentials.url)?;
    if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
        bail!("MCP 凭证只能连接本机 daemon");
    }
    let server = BridgeMcp {
        credentials,
        http: reqwest::Client::builder()
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
            "无法登记主终端身份，拒绝向其他终端提供此项目的飞书工具。请确认 daemon 可用、HC_TERMINAL_ID 正确，且 Codex 使用 --no-daemon"
        );
    }
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
