//! Unary model requests for Manager channels and proposals.
use anyhow::{Result, bail, ensure};
use rig_core::{
    completion::{
        CacheRetention, CompletionRequest, CompletionResponse, FinishReason, ToolDefinition,
    },
    error::{ErrorKind, ProviderError},
    message::{AssistantContent, Message},
    providers::{
        anthropic::AnthropicConfig,
        openai::{OpenAIConfig, Route, extension::OpenAiOptions},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Only host-controlled diagnostics cross into durable state or user notices.
/// Provider bodies and source chains can echo credentials and private input.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Failure {
    pub kind: String,
    pub http_status: Option<u16>,
    pub retryable: bool,
    pub elapsed_ms: u64,
    pub output_tokens: Option<u64>,
    pub finish_reason: Option<String>,
}
impl Failure {
    pub fn summary(&self) -> &'static str {
        match self.kind.as_str() {
            "timeout" => "The management request timed out",
            "transport" => "The management provider connection failed",
            "authorization" => "The management provider rejected access",
            "model_unavailable" => "The management model was not found",
            "rate_limited" => "The management provider is rate limited",
            "provider_unavailable" => "The management provider could not serve the request",
            "context_rejected" => "The management request exceeded the context limit",
            "request_rejected" => "The management provider rejected the request",
            "invalid_response" => "The management response was incomplete or invalid",
            "turn_budget_exhausted" => "The management turn reached its model-call budget",
            "context_preparation" => "The management context could not be prepared",
            _ => "The management run could not continue",
        }
    }
    pub fn timeout() -> Self {
        Self {
            kind: "timeout".into(),
            http_status: None,
            retryable: true,
            elapsed_ms: 0,
            output_tokens: None,
            finish_reason: None,
        }
    }
    pub fn internal() -> Self {
        Self {
            kind: "execution".into(),
            http_status: None,
            retryable: false,
            elapsed_ms: 0,
            output_tokens: None,
            finish_reason: None,
        }
    }
    fn invalid_response() -> Self {
        Self {
            kind: "invalid_response".into(),
            http_status: Some(200),
            retryable: false,
            elapsed_ms: 0,
            output_tokens: None,
            finish_reason: None,
        }
    }
    fn provider(error: ProviderError) -> Self {
        let report = error.report();
        let context = report.code.as_deref().is_some_and(|code| {
            matches!(
                code,
                "context_length_exceeded" | "context_window_exceeded" | "max_tokens_exceeded"
            )
        });
        let kind = match report.http_status {
            _ if context => "context_rejected",
            Some(401 | 403) => "authorization",
            Some(404) => "model_unavailable",
            Some(429) => "rate_limited",
            Some(408 | 425 | 500..=599) => "provider_unavailable",
            Some(200..=299) => "invalid_response",
            Some(_) => "request_rejected",
            None => match report.kind {
                ErrorKind::Http | ErrorKind::Timeout => "transport",
                ErrorKind::Request | ErrorKind::Url => "request_rejected",
                _ => "invalid_response",
            },
        };
        Self {
            kind: kind.into(),
            http_status: report.http_status,
            retryable: !context && report.retryable,
            elapsed_ms: 0,
            output_tokens: None,
            finish_reason: None,
        }
    }
    fn elapsed(mut self, started: Instant) -> Self {
        self.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        self
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.summary())?;
        if let Some(status) = self.http_status {
            write!(f, " (HTTP {status})")?;
        }
        Ok(())
    }
}
impl std::error::Error for Failure {}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub provider_kind: ProviderKind,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(skip_serializing)]
    pub api_key: String,
}
impl ProviderConfig {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.base_url)
            .map_err(|_| anyhow::anyhow!("Invalid manager endpoint"))?;
        ensure!(
            matches!(url.scheme(), "https" | "http")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && self.base_url.len() <= 2048,
            "Use an HTTP(S) manager endpoint without embedded credentials or query parameters"
        );
        ensure!(
            !self.model.trim().is_empty() && self.model.len() <= 200,
            "Enter a manager model"
        );
        ensure!(
            !self.api_key.trim().is_empty()
                && self.api_key.len() <= 65536
                && !self.api_key.contains(['\r', '\n']),
            "Enter a valid manager API key"
        );
        ensure!(
            self.context_window.is_none_or(|limit| limit >= 1024),
            "Manager context window must contain at least 1024 tokens"
        );
        Ok(())
    }
}
/// One proposal-only model turn. No code or generic tool dispatcher is reachable.
pub(crate) async fn propose(
    config: &ProviderConfig,
    system: &str,
    context: &Value,
    shutdown: CancellationToken,
) -> Result<Value> {
    config.validate()?;
    let request = CompletionRequest::new(context.to_string())
        .preamble(system)
        .tools(vec![proposal_definition()]);
    let response = tokio::select! {
        biased;
        _ = shutdown.cancelled() => bail!("Proposal generation cancelled"),
        response = call(config, request, None) => response?,
    };
    ensure!(
        response.finish_reason() != Some(FinishReason::Length),
        "Incomplete proposal response"
    );
    proposal_arguments(response.choice.iter())
}

fn proposal_definition() -> ToolDefinition {
    let fields = [
        "title",
        "why_now",
        "scope",
        "not_doing",
        "acceptance",
        "risk",
        "effort",
        "category",
    ];
    let properties = fields
        .iter()
        .map(|field| ((*field).to_owned(), json!({"type":"string"})))
        .collect::<serde_json::Map<_, _>>();
    ToolDefinition::new(
        "propose_changes".try_into().expect("static tool name"),
        "Submit up to three structured project proposals for owner review.",
        json!({
            "type":"object","additionalProperties":false,"required":["proposals"],
            "properties":{"proposals":{"type":"array","maxItems":3,"items":{
                "type":"object","additionalProperties":false,"required":fields,"properties":properties
            }}}
        }),
    )
}

fn proposal_arguments<'a>(parts: impl IntoIterator<Item = &'a AssistantContent>) -> Result<Value> {
    let mut calls = parts.into_iter().filter_map(|part| match part {
        AssistantContent::ToolCall(call) => Some(call),
        _ => None,
    });
    let call = calls.next().ok_or_else(Failure::invalid_response)?;
    ensure!(
        calls.next().is_none()
            && call.function.name.as_str() == "propose_changes"
            && call.function.invalid_arguments.is_none(),
        "Proposal generation allows only one propose_changes call"
    );
    let args = Value::Object(call.function.arguments.clone());
    ensure!(
        serde_json::to_vec(&args)?.len() <= 32_768,
        "Proposal response exceeds the request bound"
    );
    Ok(args)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    OpenaiCompletions,
    OpenaiResponses,
    Anthropic,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCompletions => "openai_completions",
            Self::OpenaiResponses => "openai_responses",
            Self::Anthropic => "anthropic",
        }
    }
}

pub async fn call(
    config: &ProviderConfig,
    request: CompletionRequest,
    cache_key: Option<&str>,
) -> Result<CompletionResponse, Failure> {
    let started = Instant::now();
    let host = reqwest::Url::parse(&config.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    let request = if host.as_deref() == Some("api.anthropic.com")
        && config.provider_kind == ProviderKind::Anthropic
    {
        request.cache(CacheRetention::Short)
    } else {
        request
    };
    let openai_options =
        if let Some(key) = cache_key.filter(|_| host.as_deref() == Some("api.openai.com")) {
            OpenAiOptions::new().prompt_cache_key(key)
        } else {
            OpenAiOptions::new()
        };
    let response = tokio::time::timeout(Duration::from_secs(120), async {
        match config.provider_kind {
            ProviderKind::OpenaiCompletions => {
                OpenAIConfig::new(config.api_key.clone())
                    .with_base_url(&config.base_url)
                    .client()
                    .chat(&config.model)
                    .call(request.provider_option(openai_options))
                    .await
            }
            ProviderKind::OpenaiResponses => {
                OpenAIConfig::new(config.api_key.clone())
                    .with_base_url(&config.base_url)
                    .with_route(Route::Responses)
                    .client()
                    .completion(&config.model)
                    .call(request.provider_option(openai_options.store(false)))
                    .await
            }
            ProviderKind::Anthropic => {
                let mut request = request;
                request.chat_history = coalesce_users(request.chat_history);
                if request.max_tokens.is_none() {
                    let max_output = rig_core::catalog::Catalog::builtin()
                        .resolve(&format!("anthropic/{}", config.model))
                        .and_then(|spec| spec.max_output_tokens)
                        .unwrap_or(16_384);
                    request.max_tokens =
                        Some(u64::from(max_output).min(config.context_limit() / 4));
                }
                AnthropicConfig::new(config.api_key.clone())
                    .with_base_url(&config.base_url)
                    .client()
                    .completion(&config.model)
                    .call(request)
                    .await
            }
        }
    })
    .await
    .map_err(|_| Failure::timeout().elapsed(started))?
    .map_err(|error| Failure::provider(error).elapsed(started))?;
    Ok(response)
}

fn coalesce_users(messages: Vec<Message>) -> Vec<Message> {
    let mut output = Vec::with_capacity(messages.len());
    for message in messages {
        match (output.last_mut(), message) {
            (Some(Message::User { content }), Message::User { content: next }) => {
                content.extend(next);
            }
            (_, message) => output.push(message),
        }
    }
    output
}

impl ProviderConfig {
    pub(super) fn context_limit(&self) -> u64 {
        let vendor = match self.provider_kind {
            ProviderKind::OpenaiCompletions | ProviderKind::OpenaiResponses => "openai",
            ProviderKind::Anthropic => "anthropic",
        };
        self.context_window
            .or_else(|| {
                rig_core::catalog::Catalog::builtin()
                    .resolve(&format!("{vendor}/{}", self.model))
                    .and_then(|spec| spec.context_window)
            })
            .unwrap_or(128_000)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};

    #[tokio::test]
    async fn responses_disables_storage_and_anthropic_coalesces_wire_users() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let routes = Router::new()
            .route("/v1/responses", post(|Json(request): Json<Value>| async move {
                assert_eq!(request["store"], false);
                assert!(request["previous_response_id"].is_null());
                Json(json!({"id":"resp_fixture","object":"response","status":"completed",
                    "model":"fixture","output":[{"type":"message","id":"m_fixture","role":"assistant",
                    "status":"completed","content":[{"type":"output_text","text":"Responses reply","annotations":[]}]}],
                    "usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":3,"total_tokens":15}}))
            }))
            .route("/v1/messages", post(|headers: HeaderMap, Json(request): Json<Value>| async move {
                assert_eq!(headers["x-api-key"], "fixture");
                assert!(headers.contains_key("anthropic-version"));
                assert_eq!(request["messages"].as_array().unwrap().len(), 1);
                let parts = request["messages"][0]["content"].as_array().unwrap();
                assert_eq!(parts[0]["text"], "first");
                assert_eq!(parts[1]["text"], "second");
                Json(json!({"type":"message","id":"msg_fixture","model":"fixture","role":"assistant",
                    "content":[{"type":"text","text":"Anthropic reply"}],"stop_reason":"end_turn",
                    "usage":{"input_tokens":4,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"output_tokens":3}}))
            }));
        let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
        let mut config = ProviderConfig {
            base_url,
            model: "fixture".into(),
            api_key: "fixture".into(),
            provider_kind: ProviderKind::OpenaiResponses,
            supports_images: false,
            context_window: None,
        };
        let request = CompletionRequest::new("second").message(Message::user("first"));
        let response = call(&config, request.clone(), None).await.unwrap();
        assert_eq!(
            rig_core::transcript::assistant_text_from_choice(&response.choice),
            "Responses reply"
        );
        assert_eq!(response.usage.cached_input_tokens, Some(8));
        config.provider_kind = ProviderKind::Anthropic;
        let response = call(&config, request.clone(), None).await.unwrap();
        assert_eq!(
            rig_core::transcript::assistant_text_from_choice(&response.choice),
            "Anthropic reply"
        );
        assert_eq!(request.chat_history.len(), 2);
        assert_eq!(response.usage.cache_creation_input_tokens, Some(3));
        server.abort();
    }
}
