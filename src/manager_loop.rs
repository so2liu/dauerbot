//! IO-free Rig checkpoints plus one explicitly configured provider call.
//! The host owns durable admission, revisions, tool effects and uncertainty.
use anyhow::{Result, bail, ensure};
use rig_agent::run::{AgentRun, AgentRunStep, InvalidToolCallAction, ModelTurn, ModelTurnOutcome};
use rig_core::{
    completion::{CompletionRequest, FinishReason, ToolDefinition},
    error::{ErrorKind, ProviderError},
    message::{AssistantContent, ToolResultContent, UserContent},
    providers::openai::OpenAIConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const MAX_CHECKPOINT: usize = 2 * 1024 * 1024;
const MAX_CONTENT: usize = 256 * 1024;
const TOOLS: [&str; 10] = [
    "read_file",
    "write_file",
    "delegate",
    "worker_status",
    "steer_worker",
    "send_md",
    "send_file",
    "close_agent",
    "cli_send_keys",
    "cli_view",
];

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
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub occurrence: String,
    pub name: String,
    pub arguments: Value,
}
pub enum LoopAction {
    Continue,
    Tools(Vec<ToolCall>),
    Done(String),
}
pub struct LoopStep {
    pub checkpoint: Value,
    pub action: LoopAction,
    pub model_responded: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    format: u32,
    system: String,
    run: AgentRun,
    pending: AgentRunStep,
    #[serde(default)]
    correction_used: bool,
    #[serde(default)]
    last_correction: Option<Failure>,
    #[serde(default)]
    images: Vec<crate::multimodal::ImageRef>,
    #[serde(default)]
    image_delivery: Vec<Value>,
    #[serde(default)]
    image_input_digest: Option<String>,
}
fn definitions() -> Vec<ToolDefinition> {
    let schemas = [
        (
            "Read a bounded UTF-8 project file; returns content and sha256.",
            json!({"path":{"type":"string"}}),
            vec!["path"],
        ),
        (
            "Write a project file. Existing files require their last-read expected_sha256.",
            json!({"path":{"type":"string"},"content":{"type":"string"},"expected_sha256":{"type":"string"}}),
            vec!["path", "content"],
        ),
        (
            "Delegate coding to this project's native CLI worker; returns a durable receipt. Optional target_cli selects a supported CLI through the ordinary cooperative handoff; held is not delivery and readiness failures remain visible in worker_status. Omit it to keep the current selection. Select source_event_id from trusted_sources when multiple discussions are present.",
            json!({"instructions":{"type":"string"},"source_event_id":{"type":"string"},"target_cli":{"type":"string","enum":["codex","claude","pi","agy"]}}),
            vec!["instructions"],
        ),
        (
            "Observe this project's native worker and delegated work receipts. For an owned continuing input, provide input_id to inspect the full durable effect ancestry, not only recent history. Page using after_effect and detail_offset; recorded confirmed or uncertain effects must not be replayed.",
            json!({"input_id":{"type":"string"},"after_effect":{"type":"integer","minimum":0},"detail_offset":{"type":"integer","minimum":0,"maximum":262144}}),
            vec![],
        ),
        (
            "Steer this project's native coding worker. Optional target_cli selects a supported CLI; if it differs, instructions wait behind the cooperative handoff for the selected successor instead of reaching the old worker. Omit it for current-worker steering. Select source_event_id from trusted_sources when multiple discussions are present.",
            json!({"instructions":{"type":"string"},"source_event_id":{"type":"string"},"target_cli":{"type":"string","enum":["codex","claude","pi","agy"]}}),
            vec!["instructions"],
        ),
        (
            "Send a Feishu topic reply. Choose a trusted discussion anchor; omit only for an independent initial message. Use intent=receipt only for a new task's brief acknowledgement; its original trusted source is sent at most once, including across model turns. Select source_event_id from trusted_sources when needed. Omit intent for ordinary replies.",
            json!({"to":{"type":"string"},"text":{"type":"string"},"reply_to_message_id":{"type":"string"},"intent":{"type":"string","enum":["receipt"]},"source_event_id":{"type":"string"}}),
            vec!["to", "text"],
        ),
        (
            "Send a project file through Feishu with an explicitly chosen discussion anchor.",
            json!({"to":{"type":"string"},"path":{"type":"string"},"reply_to_message_id":{"type":"string"}}),
            vec!["to", "path"],
        ),
        (
            "Explicitly close an obsolete owned execution selected from worker_status. Never close automatically: verify task completion, give a reason, and retain unknown/current/live/referenced targets. Returns an audited refusal, verified closure or uncertain receipt.",
            json!({"generation":{"type":"string"},"terminal_id":{"type":"string"},"reason":{"type":"string"},"task_finished":{"type":"boolean"}}),
            vec!["generation", "terminal_id", "reason", "task_finished"],
        ),
        (
            "Send an ordered, compact text/key sequence, with each segment exactly {text: string} or {key: string} to the current project's owned CLI. Use generation from worker_status. Text is UTF-8 with LF unchanged; keys are separate segments. Text never implies a key. Keys: enter, tab, esc, backspace, delete, up/down/left/right, home/end, pageup/pagedown, space; ctrl/alt/shift modifiers and modified letters, for example alt+m. Unknown keys fail before sending. Use cli_view to inspect results; a submitted sequence does not prove task completion. Do not repeat uncertain input.",
            json!({"generation":{"type":"string"},"segments":{"type":"array","minItems":1,"maxItems":32,"items":{"oneOf":[{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},{"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}]}}}),
            vec!["generation", "segments"],
        ),
        (
            "Read the current project's owned CLI visible rendered text. This does not send keys, resize, or take over its terminal. Known credential-like lines are redacted. Missing cursor/viewport metadata is explicit; internal execution identifiers are diagnostic, not user-facing task results.",
            json!({"generation":{"type":"string"}}),
            vec!["generation"],
        ),
    ];
    TOOLS.into_iter().zip(schemas).map(|(name,(description,properties,required))|ToolDefinition::new(name.try_into().expect("static tool name"),description,json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}))).collect()
}
/// Existing persisted context, without claiming hydration or historical wire identity.
pub(crate) fn normalized_context(checkpoint: Value) -> Option<CompletionRequest> {
    let state = decode(checkpoint).ok()?;
    let AgentRunStep::CallModel {
        prompt, history, ..
    } = state.pending
    else {
        return None;
    };
    Some(
        CompletionRequest::new(prompt)
            .messages(history)
            .preamble(state.system),
    )
}
fn encode(state: &Checkpoint) -> Result<Value> {
    let bytes = serde_json::to_vec(state)?;
    ensure!(
        bytes.len() <= MAX_CHECKPOINT,
        "Manager conversation reached its checkpoint limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn decode(mut value: Value) -> Result<Checkpoint> {
    ensure!(
        serde_json::to_vec(&value)?.len() <= MAX_CHECKPOINT,
        "Manager checkpoint is too large"
    );
    // Format 1 checkpoints from earlier releases may carry a retired local
    // output cap. Discard it rather than applying or re-encoding any limit.
    if let Some(fields) = value.as_object_mut() {
        fields.remove("output_budget");
    }
    let mut state: Checkpoint = serde_json::from_value(value)?;
    state.run = state.run.max_invalid_tool_call_retries(1);
    ensure!(state.format == 1, "Unsupported manager checkpoint version");
    Ok(state)
}
pub fn begin(input: String, system: String) -> Result<Value> {
    begin_with_images(input, system, vec![])
}
pub fn begin_with_images(
    input: String,
    system: String,
    images: Vec<crate::multimodal::ImageRef>,
) -> Result<Value> {
    ensure!(
        images.len() <= crate::multimodal::MAX_IMAGES,
        "Too many manager images"
    );
    ensure!(
        input.len() <= MAX_CONTENT && system.len() <= 65536,
        "Manager input is too large"
    );
    let image_input_digest = (!images.is_empty()).then(|| crate::multimodal::input_digest(&input));
    let mut run = AgentRun::new(input)
        .max_turns(32)
        .max_invalid_tool_call_retries(1);
    let pending = run
        .next_step()
        .map_err(|_| anyhow::anyhow!("Cannot initialize manager run"))?;
    encode(&Checkpoint {
        format: 1,
        system,
        run,
        pending,
        correction_used: false,
        last_correction: None,
        images,
        image_delivery: vec![],
        image_input_digest,
    })
}
fn calls(state: &Checkpoint) -> Result<Vec<ToolCall>> {
    let AgentRunStep::CallTools { calls } = &state.pending else {
        bail!("Manager is not awaiting tools")
    };
    ensure!(calls.len() <= 16, "Manager emitted too many tool calls");
    calls
        .iter()
        .enumerate()
        .map(|(ordinal, call)| {
            let name = call.tool_call.function.name.as_str();
            ensure!(
                TOOLS.contains(&name)
                    && call.preresolved_result.is_none()
                    && call.tool_call.function.invalid_arguments.is_none(),
                "Manager requested an unavailable tool"
            );
            let args = Value::Object(call.tool_call.function.arguments.clone());
            ensure!(
                serde_json::to_vec(&args)?.len() <= MAX_CONTENT,
                "Manager tool arguments are too large"
            );
            Ok(ToolCall {
                occurrence: format!("turn:{}:call:{ordinal}", state.run.turn()),
                name: name.to_owned(),
                arguments: args,
            })
        })
        .collect()
}
fn step(state: Checkpoint, model_responded: bool) -> Result<LoopStep> {
    let action = match &state.pending {
        AgentRunStep::CallTools { .. } => LoopAction::Tools(calls(&state)?),
        AgentRunStep::Done(response) => LoopAction::Done(
            response
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        AgentRunStep::CallModel { .. } => bail!("Manager still needs a model response"),
    };
    Ok(LoopStep {
        checkpoint: encode(&state)?,
        action,
        model_responded,
    })
}
/// The host must persist the supplied checkpoint and reserve its model attempt
/// before this call, then reject results from superseded revisions before saving.
pub async fn advance(
    config: &ProviderConfig,
    checkpoint: Value,
    cancellation: CancellationToken,
) -> Result<LoopStep> {
    advance_observed(config, checkpoint, cancellation, None).await
}
/// Diagnostics observe prepared logical content only; they cannot affect admission.
pub(crate) async fn advance_observed(
    config: &ProviderConfig,
    checkpoint: Value,
    cancellation: CancellationToken,
    observer: Option<crate::manager_context::Observer>,
) -> Result<LoopStep> {
    let mut state = decode(checkpoint)?;
    ensure!(!cancellation.is_cancelled(), "Manager turn cancelled");
    let AgentRunStep::CallModel {
        prompt,
        history,
        turn,
    } = state.pending.clone()
    else {
        return step(state, false);
    };
    config.validate()?;
    let definitions = definitions();
    state.run.advertise_tools(turn, definitions.clone());
    let names: BTreeSet<_> = TOOLS.into_iter().map(str::to_owned).collect();
    let system = if state.correction_used {
        format!(
            "{}\n<management_response_feedback>{{\"previous_response_executed_tools\":0,\"instruction\":\"Return only advertised tool names and complete valid JSON arguments. Keep output concise.\"}}</management_response_feedback>",
            state.system
        )
    } else {
        state.system.clone()
    };
    let mut prompt = prompt;
    let mut history = history;
    if !state.images.is_empty() {
        state.image_delivery = crate::multimodal::hydrate(
            &mut prompt,
            &mut history,
            &state.images,
            state.image_input_digest.as_deref(),
            config.supports_images,
        )
        .await;
    }
    let request = CompletionRequest::new(prompt)
        .messages(history)
        .preamble(&system)
        .tools(definitions);
    let mut observation = observer.map(|observer| {
        observer
            .store
            .capture(&observer.project, &observer.run, turn as u64, &request)
    });
    let client = OpenAIConfig::new(config.api_key.clone())
        .with_base_url(&config.base_url)
        .client();
    let model = client.chat(&config.model);
    let started = Instant::now();
    let response = tokio::select! {
        biased;
        _=cancellation.cancelled()=>bail!("Manager turn cancelled"),
        result=tokio::time::timeout(Duration::from_secs(120),model.call(request))=>match result {
            Ok(Ok(response))=>response,
            Ok(Err(error))=>{
                if let Some(observation) = &mut observation { observation.finish("failed"); }
                let error=Failure::provider(error).elapsed(started);
                if error.kind=="invalid_response" && !state.correction_used {
                    return corrected(&mut state,error);
                }
                return Err(error.into());
            },
            Err(_)=>{
                if let Some(observation) = &mut observation { observation.finish("failed"); }
                return Err(Failure::timeout().elapsed(started).into());
            },
        }
    };
    if let Some(observation) = &mut observation {
        observation.finish("provider_responded");
    }
    ensure!(!cancellation.is_cancelled(), "Manager turn cancelled");
    if response.finish_reason() == Some(FinishReason::Length) {
        let mut error = Failure::invalid_response().elapsed(started);
        error.output_tokens = response.usage.output_tokens;
        error.finish_reason = Some("length".into());
        // The provider ended this output early. Do not execute any sibling
        // tool or replay the same request; the host retains input and repairs.
        return Err(error.into());
    }
    if response.choice.iter().any(|part| matches!(part,AssistantContent::ToolCall(call) if call.function.invalid_arguments.is_some())) {
        let mut error=Failure::invalid_response().elapsed(started);
        error.output_tokens=response.usage.output_tokens;error.finish_reason=Some("invalid_tool_arguments".into());
        if state.correction_used { return Err(error.into()); }
        return corrected(&mut state,error);
    }
    let mut answer = ModelTurn::from_response_parts(&response, names.clone(), names);
    // Keep the normalized transcript, not an unbounded duplicate provider body.
    answer.raw = Value::Null;
    let outcome = state
        .run
        .model_response(answer)
        .map_err(|_| Failure::invalid_response().elapsed(started))?;
    if matches!(outcome, ModelTurnOutcome::NeedsResolution(_)) {
        let mut error = Failure::invalid_response().elapsed(started);
        error.output_tokens = response.usage.output_tokens;
        error.finish_reason = Some("invalid_tool_call".into());
        if state.correction_used {
            return Err(error.into());
        }
        state.run.resolve_invalid_tool_call(InvalidToolCallAction::retry("No tool from this response was executed. Return only advertised tool names with complete, valid JSON arguments; keep tool content concise."))
            .map_err(|_| Failure::invalid_response().elapsed(started))?;
        state.pending = state
            .run
            .next_step()
            .map_err(|_| Failure::invalid_response().elapsed(started))?;
        return corrected(&mut state, error);
    }
    state.correction_used = false;
    state.pending = state
        .run
        .next_step()
        .map_err(|_| anyhow::anyhow!("Manager run could not advance"))?;
    step(state, true)
}
fn corrected(state: &mut Checkpoint, error: Failure) -> Result<LoopStep> {
    state.correction_used = true;
    state.last_correction = Some(error);
    Ok(LoopStep {
        checkpoint: encode(state)?,
        action: LoopAction::Continue,
        model_responded: false,
    })
}
pub fn supply_tool_results(checkpoint: Value, results: Vec<(String, Value)>) -> Result<Value> {
    let mut state = decode(checkpoint)?;
    let pending = calls(&state)?;
    ensure!(
        results.len() == pending.len(),
        "Manager tool results do not match the pending calls"
    );
    let mut results = results
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    ensure!(
        results.len() == pending.len(),
        "Duplicate manager tool result"
    );
    let AgentRunStep::CallTools { calls } = &state.pending else {
        unreachable!()
    };
    let mut answers = Vec::new();
    for (call, slot) in calls.iter().zip(pending) {
        let value = results
            .remove(&slot.occurrence)
            .ok_or_else(|| anyhow::anyhow!("Unknown manager tool result occurrence"))?;
        ensure!(
            serde_json::to_vec(&value)?.len() <= MAX_CONTENT,
            "Manager tool result is too large"
        );
        answers.push(UserContent::tool_result(
            call.tool_call.id.clone(),
            call.tool_call.function.name.clone(),
            vec![ToolResultContent::json(value)],
        ));
    }
    state
        .run
        .tool_results(answers)
        .map_err(|_| anyhow::anyhow!("Manager rejected tool results"))?;
    state.pending = state
        .run
        .next_step()
        .map_err(|_| anyhow::anyhow!("Manager run could not continue"))?;
    encode(&state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Clone)]
    struct Fixture {
        requests: Arc<AtomicUsize>,
        corrective_requests: Arc<AtomicUsize>,
        script: Arc<Vec<Value>>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        hold: bool,
        bodies: Arc<std::sync::Mutex<Vec<Value>>>,
    }
    async fn fixture(
        script: Vec<Value>,
        hold: bool,
    ) -> (ProviderConfig, Fixture, tokio::task::JoinHandle<()>) {
        let fixture = Fixture {
            requests: Arc::new(AtomicUsize::new(0)),
            corrective_requests: Arc::new(AtomicUsize::new(0)),
            script: Arc::new(script),
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            hold,
            bodies: Arc::new(std::sync::Mutex::new(vec![])),
        };
        async fn handler(
            State(f): State<Fixture>,
            headers: axum::http::HeaderMap,
            Json(body): Json<Value>,
        ) -> (axum::http::StatusCode, Json<Value>) {
            assert_eq!(
                headers.get("authorization").unwrap(),
                "Bearer private-fixture-secret"
            );
            assert_eq!(body["model"], "fixture-model");
            let names = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["function"]["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(names, TOOLS);
            assert!(!body.to_string().contains("private-fixture-secret"));
            if body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .to_string()
                        .contains("<management_response_feedback>")
            }) {
                f.corrective_requests.fetch_add(1, Ordering::SeqCst);
            }
            f.bodies.lock().unwrap().push(body.clone());
            let n = f.requests.fetch_add(1, Ordering::SeqCst);
            f.started.notify_one();
            if f.hold {
                f.release.notified().await;
            }
            for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                assert!(
                    body.get(field).is_none(),
                    "Unexpected output-token parameter: {field}"
                );
            }
            let response = f.script[n].clone();
            if response["fixture_error"] == true {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error":{"message":"private-fixture-secret"}})),
                );
            }
            (axum::http::StatusCode::OK, Json(response))
        }
        let app = Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(fixture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            ProviderConfig {
                base_url: format!("http://{addr}/v1"),
                model: "fixture-model".into(),
                supports_images: false,
                api_key: "private-fixture-secret".into(),
            },
            fixture,
            server,
        )
    }
    fn answer(content: Value, calls: Value) -> Value {
        json!({"id":"fixture-response","object":"chat.completion","created":1,"model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":content,"tool_calls":calls},"finish_reason":if calls.is_array(){"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
    }
    #[tokio::test]
    async fn declared_images_reach_initial_and_followup_wire_without_checkpoint_pixels() {
        use crate::multimodal::{ImageRef, fixture_resource};
        use base64::Engine;
        let directory = tempfile::tempdir().unwrap();
        let resource = fixture_resource(directory.path(), "project", "message");
        let image = ImageRef::from_resource(
            "project",
            "message",
            &resource,
            std::path::Path::new(resource["cache_root"].as_str().unwrap()),
        )
        .unwrap();
        let mut other_resource = fixture_resource(directory.path(), "project", "message-b");
        let other_bytes = include_bytes!("../tests/fixtures/input-green.png");
        std::fs::write(other_resource["local_path"].as_str().unwrap(), other_bytes).unwrap();
        use sha2::Digest;
        other_resource["sha256"] = json!(format!("{:x}", sha2::Sha256::digest(other_bytes)));
        other_resource["bytes"] = json!(other_bytes.len());
        let other_image = ImageRef::from_resource(
            "project",
            "message-b",
            &other_resource,
            std::path::Path::new(other_resource["cache_root"].as_str().unwrap()),
        )
        .unwrap();
        let calls = json!([{"id":"read","type":"function","function":{"name":"worker_status","arguments":"{}"}}]);
        let (mut config, f, server) = fixture(
            vec![
                answer(Value::Null, calls),
                answer(json!("complete"), Value::Null),
                answer(json!("text-only"), Value::Null),
            ],
            false,
        )
        .await;
        config.supports_images = true;
        let initial = begin_with_images(
            "Inspect source pixels".into(),
            "Manager".into(),
            vec![other_image, image],
        )
        .unwrap();
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../tests/fixtures/input.png"));
        assert!(!initial.to_string().contains(&encoded));
        let first = advance(&config, initial.clone(), CancellationToken::new())
            .await
            .unwrap();
        let LoopAction::Tools(calls) = first.action else {
            panic!("tools")
        };
        let continuation = supply_tool_results(
            first.checkpoint,
            vec![(calls[0].occurrence.clone(), json!({"ready":true}))],
        )
        .unwrap();
        let restored = serde_json::from_str(&continuation.to_string()).unwrap();
        let last = advance(&config, restored, CancellationToken::new())
            .await
            .unwrap();
        assert!(!last.checkpoint.to_string().contains(&encoded));
        assert_eq!(last.checkpoint["image_delivery"][0]["mode"], "image_part");
        config.supports_images = false;
        let fallback = advance(&config, initial, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            fallback.checkpoint["image_delivery"][0]["reason"],
            "image_capability_not_declared"
        );
        let bodies = f.bodies.lock().unwrap();
        let mut counts = vec![];
        for body in bodies.iter() {
            let images = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .filter(|p| p["type"] == "image_url")
                .collect::<Vec<_>>();
            let green = base64::engine::general_purpose::STANDARD
                .encode(include_bytes!("../tests/fixtures/input-green.png"));
            for message in body["messages"].as_array().unwrap() {
                if let Some(parts) = message["content"].as_array() {
                    for (index, part) in parts
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| p["type"] == "image_url")
                    {
                        let caption = parts[index - 1]["text"].as_str().unwrap();
                        if part["image_url"]["url"] == format!("data:image/png;base64,{green}") {
                            assert!(
                                caption.contains("source_message_id=om_message-b")
                                    && caption.contains("cache_message_id=message-b")
                            );
                        } else {
                            assert_eq!(
                                part["image_url"]["url"],
                                format!("data:image/png;base64,{encoded}")
                            );
                            assert!(
                                caption.contains("source_message_id=om_message,")
                                    && caption.contains("cache_message_id=message,")
                            );
                        }
                    }
                }
            }
            counts.push(images.len());
        }
        assert_eq!(counts, vec![2, 2, 0]);
        server.abort();
    }
    #[tokio::test]
    async fn checkpointed_tool_occurrences_survive_restart_without_replaying_provider_or_tools() {
        let calls = json!([
            {"id":"duplicate","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"HANDOFF.md\"}"}},
            {"id":"duplicate","type":"function","function":{"name":"worker_status","arguments":"{}"}}
        ]);
        let (config, f, server) = fixture(
            vec![answer(Value::Null, calls), answer(json!(""), Value::Null)],
            false,
        )
        .await;
        let initial = begin("Read context".into(), "Restricted manager".into()).unwrap();
        let result = advance(&config, initial, CancellationToken::new())
            .await
            .unwrap();
        let LoopAction::Tools(calls) = result.action else {
            panic!("expected tools")
        };
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].occurrence, calls[1].occurrence);
        let restored =
            serde_json::from_str(&serde_json::to_string(&result.checkpoint).unwrap()).unwrap();
        let repeated = advance(&config, restored, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(repeated.action, LoopAction::Tools(_)));
        assert_eq!(f.requests.load(Ordering::SeqCst), 1);
        assert!(
            supply_tool_results(
                result.checkpoint.clone(),
                vec![(calls[0].occurrence.clone(), json!({}))]
            )
            .is_err()
        );
        assert!(
            supply_tool_results(
                result.checkpoint.clone(),
                vec![
                    (calls[0].occurrence.clone(), json!({})),
                    (calls[0].occurrence.clone(), json!({}))
                ]
            )
            .is_err()
        );
        let next = supply_tool_results(
            result.checkpoint,
            calls
                .into_iter()
                .map(|c| (c.occurrence, json!({"ok":true})))
                .collect(),
        )
        .unwrap();
        let result = advance(&config, next, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(result.action,LoopAction::Done(ref text) if text.is_empty()));
        let again = advance(&config, result.checkpoint, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(again.action, LoopAction::Done(_)));
        assert_eq!(f.requests.load(Ordering::SeqCst), 2);
        server.abort();
        let _ = server.await;
    }
    #[tokio::test]
    async fn cancellation_discards_inflight_answer_and_provider_errors_never_expose_credentials() {
        let (config, f, server) = fixture(vec![answer(json!("late"), Value::Null)], true).await;
        let initial = begin("input".into(), "system".into()).unwrap();
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let config = config.clone();
            let cancel = cancel.clone();
            async move { advance(&config, initial, cancel).await }
        });
        tokio::time::timeout(Duration::from_secs(2), f.started.notified())
            .await
            .unwrap();
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        f.release.notify_one();
        server.abort();
        let _ = server.await;
        let (config, _, server) = fixture(vec![json!({"fixture_error":true})], false).await;
        let result = advance(
            &config,
            begin("input".into(), "system".into()).unwrap(),
            CancellationToken::new(),
        )
        .await;
        let error = result.err().unwrap().to_string();
        assert!(!error.contains("private-fixture-secret"));
        assert!(
            !serde_json::to_string(&config)
                .unwrap()
                .contains("private-fixture-secret")
        );
        server.abort();
        let _ = server.await;
    }
    #[tokio::test]
    async fn prepared_context_observer_tracks_provider_response_failure_and_abandoned_future() {
        let store = Arc::new(crate::manager_context::Store::default());
        let observer = || {
            Some(crate::manager_context::Observer {
                store: store.clone(),
                project: "fixture-project".into(),
                run: "fixture-run".into(),
            })
        };
        let (config, f, server) = fixture(vec![answer(json!("late"), Value::Null)], true).await;
        let observation = observer();
        let task = tokio::spawn(async move {
            advance_observed(
                &config,
                begin(
                    "additional user message".into(),
                    "system instructions".into(),
                )
                .unwrap(),
                CancellationToken::new(),
                observation,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), f.started.notified())
            .await
            .unwrap();
        assert_eq!(
            store.view("fixture-project")["requests"][0]["status"],
            "prepared"
        );
        task.abort();
        let _ = task.await;
        assert_eq!(
            store.view("fixture-project")["requests"][0]["status"],
            "abandoned"
        );
        f.release.notify_one();
        server.abort();
        let _ = server.await;
        let (config, _, server) = fixture(vec![answer(json!("done"), Value::Null)], false).await;
        advance_observed(
            &config,
            begin("user".into(), "system".into()).unwrap(),
            CancellationToken::new(),
            observer(),
        )
        .await
        .unwrap();
        assert_eq!(
            store.view("fixture-project")["requests"][1]["status"],
            "provider_responded"
        );
        server.abort();
        let _ = server.await;
        let (config, _, server) = fixture(vec![json!({"fixture_error":true})], false).await;
        let _ = advance_observed(
            &config,
            begin("user".into(), "system".into()).unwrap(),
            CancellationToken::new(),
            observer(),
        )
        .await;
        let view = store.view("fixture-project");
        assert_eq!(view["requests"][2]["status"], "failed");
        assert!(!view.to_string().contains("private-fixture-secret"));
        server.abort();
        let _ = server.await;
    }
    #[tokio::test]
    async fn model_cannot_expand_registry_or_resume_unsupported_checkpoint() {
        let call = json!([{"id":"evil","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}]);
        let (config, _, server) = fixture(vec![answer(Value::Null, call)], false).await;
        let result = advance(
            &config,
            begin("input".into(), "system".into()).unwrap(),
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(result.unwrap().action, LoopAction::Continue));
        let mut checkpoint = begin("input".into(), "system".into()).unwrap();
        checkpoint["format"] = json!(99);
        assert!(
            advance(&config, checkpoint, CancellationToken::new())
                .await
                .is_err()
        );
        server.abort();
        let _ = server.await;
    }
    #[tokio::test]
    async fn provider_length_is_finite_without_executing_any_tool_or_replaying_the_request() {
        let mut truncated = answer(
            Value::Null,
            json!([
                {"id":"partial","type":"function","function":{"name":"write_file","arguments":"{\"path\":"}},
                {"id":"sibling","type":"function","function":{"name":"send_md","arguments":"{}"}}
            ]),
        );
        truncated["choices"][0]["finish_reason"] = json!("length");
        truncated["usage"]["completion_tokens"] = json!(17000);
        let (config, f, server) = fixture(vec![truncated], false).await;
        let error = advance(
            &config,
            begin("input".into(), "system".into()).unwrap(),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        let error = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(error.kind, "invalid_response");
        assert_eq!(error.http_status, Some(200));
        assert!(!error.retryable);
        assert_eq!(error.finish_reason.as_deref(), Some("length"));
        assert_eq!(error.output_tokens, Some(17000));
        assert_eq!(f.requests.load(Ordering::SeqCst), 1);
        server.abort();
        let _ = server.await;
    }
    #[tokio::test]
    async fn legacy_checkpoint_caps_are_ignored_for_initial_and_followup_requests() {
        for cap in [4096, 8192] {
            let call = json!([{"id":"read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"HANDOFF.md\"}"}}]);
            let (config, f, server) = fixture(
                vec![
                    answer(Value::Null, call),
                    answer(json!("finished"), Value::Null),
                ],
                false,
            )
            .await;
            let mut old = begin("input".into(), "system".into()).unwrap();
            old["output_budget"] = json!(cap);
            old["correction_used"] = json!(cap == 8192);
            let mut old_diagnostic = serde_json::to_value(Failure::invalid_response()).unwrap();
            old_diagnostic["output_budget"] = json!(cap);
            old["last_correction"] = old_diagnostic;
            let step = advance(&config, old, CancellationToken::new())
                .await
                .unwrap();
            assert!(step.checkpoint.get("output_budget").is_none());
            assert!(
                step.checkpoint["last_correction"]
                    .get("output_budget")
                    .is_none()
            );
            let LoopAction::Tools(calls) = step.action else {
                panic!("expected valid tool");
            };
            let next = supply_tool_results(
                step.checkpoint,
                vec![(calls[0].occurrence.clone(), json!({"content":"context"}))],
            )
            .unwrap();
            assert!(matches!(
                advance(&config, next, CancellationToken::new())
                    .await
                    .unwrap()
                    .action,
                LoopAction::Done(_)
            ));
            assert_eq!(f.requests.load(Ordering::SeqCst), 2);
            server.abort();
            let _ = server.await;
        }
    }
    #[tokio::test]
    async fn malformed_200_and_unknown_tool_receive_only_one_safe_correction() {
        let invalid = json!({"id":"bad","model":"fixture-model","choices":[]});
        let (config, f, server) = fixture(vec![invalid.clone(), invalid], false).await;
        let step = advance(
            &config,
            begin("input".into(), "system".into()).unwrap(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(step.action, LoopAction::Continue));
        let error = advance(&config, step.checkpoint, CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().kind,
            "invalid_response"
        );
        assert_eq!(f.requests.load(Ordering::SeqCst), 2);
        assert_eq!(f.corrective_requests.load(Ordering::SeqCst), 1);
        server.abort();
        let _ = server.await;
        let bad = answer(
            Value::Null,
            json!([
                {"id":"unknown","type":"function","function":{"name":"bash","arguments":"{}"}},
                {"id":"sibling","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"x\",\"content\":\"bad\"}"}}
            ]),
        );
        let (config, f, server) = fixture(vec![bad.clone(), bad], false).await;
        let step = advance(
            &config,
            begin("input".into(), "system".into()).unwrap(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(step.action, LoopAction::Continue));
        assert!(!step.model_responded);
        assert!(
            advance(&config, step.checkpoint, CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(f.requests.load(Ordering::SeqCst), 2);
        assert_eq!(f.corrective_requests.load(Ordering::SeqCst), 1);
        server.abort();
        let _ = server.await;
    }
    #[test]
    fn provider_classification_never_retains_secret_bodies_or_retries_permanent_errors() {
        for (status, kind, retryable) in [
            (400, "context_rejected", false),
            (403, "authorization", false),
            (429, "rate_limited", true),
            (503, "provider_unavailable", true),
            (200, "invalid_response", false),
        ] {
            let body = if status == 400 {
                r#"{"error":{"code":"context_length_exceeded","message":"private-fixture-secret"}}"#
            } else {
                r#"{"error":{"message":"private-fixture-secret"}}"#
            };
            let error = Failure::provider(ProviderError::from_http_response(
                axum::http::StatusCode::from_u16(status).unwrap(),
                body,
            ));
            assert_eq!(error.kind, kind);
            assert_eq!(error.retryable, retryable);
            assert_eq!(error.http_status, Some(status));
            assert!(
                !serde_json::to_string(&error)
                    .unwrap()
                    .contains("private-fixture-secret")
            );
            assert!(!error.to_string().contains("private-fixture-secret"));
        }
    }
    #[tokio::test]
    async fn invalid_known_tool_arguments_discard_valid_siblings_even_without_length_finish_reason()
    {
        let bad = answer(
            Value::Null,
            json!([
                {"id":"sibling","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"x\",\"content\":\"bad\"}"}},
                {"id":"partial","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}
            ]),
        );
        let good = answer(json!("recovered"), Value::Null);
        let (config, f, server) = fixture(vec![bad, good], false).await;
        let step = advance(
            &config,
            begin("input".into(), "system".into()).unwrap(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(step.action, LoopAction::Continue));
        assert!(!step.model_responded);
        assert_eq!(
            step.checkpoint["last_correction"]["finish_reason"],
            "invalid_tool_arguments"
        );
        assert!(matches!(
            advance(&config, step.checkpoint, CancellationToken::new())
                .await
                .unwrap()
                .action,
            LoopAction::Done(_)
        ));
        assert_eq!(f.requests.load(Ordering::SeqCst), 2);
        server.abort();
        let _ = server.await;
    }
}
