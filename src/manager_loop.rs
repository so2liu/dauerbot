//! IO-free Rig checkpoints plus one explicitly configured provider call.
//! The host owns durable admission, revisions, tool effects and uncertainty.
use anyhow::{Result, bail, ensure};
use rig_agent::run::{AgentRun, AgentRunStep, ModelTurn, ModelTurnOutcome};
use rig_core::{
    completion::{CompletionRequest, ToolDefinition},
    message::{AssistantContent, ToolResultContent, UserContent},
    providers::openai::OpenAIConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use tokio_util::sync::CancellationToken;

const MAX_CHECKPOINT: usize = 2 * 1024 * 1024;
const MAX_CONTENT: usize = 256 * 1024;
const TOOLS: [&str; 7] = [
    "read_file",
    "write_file",
    "delegate",
    "worker_status",
    "steer_worker",
    "send_md",
    "send_file",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
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
    Tools(Vec<ToolCall>),
    Done(String),
}
pub struct LoopStep {
    pub checkpoint: Value,
    pub action: LoopAction,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    format: u32,
    system: String,
    run: AgentRun,
    pending: AgentRunStep,
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
            "Delegate coding to this project's native CLI worker; returns a durable receipt.",
            json!({"instructions":{"type":"string"}}),
            vec!["instructions"],
        ),
        (
            "Observe this project's native worker and delegated work receipts.",
            json!({}),
            vec![],
        ),
        (
            "Steer this project's current native coding worker.",
            json!({"instructions":{"type":"string"}}),
            vec!["instructions"],
        ),
        (
            "Send a Feishu topic reply. Choose a trusted discussion anchor; omit only for an independent initial message.",
            json!({"to":{"type":"string"},"text":{"type":"string"},"reply_to_message_id":{"type":"string"}}),
            vec!["to", "text"],
        ),
        (
            "Send a project file through Feishu with an explicitly chosen discussion anchor.",
            json!({"to":{"type":"string"},"path":{"type":"string"},"reply_to_message_id":{"type":"string"}}),
            vec!["to", "path"],
        ),
    ];
    TOOLS.into_iter().zip(schemas).map(|(name,(description,properties,required))|ToolDefinition::new(name.try_into().expect("static tool name"),description,json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}))).collect()
}
fn encode(state: &Checkpoint) -> Result<Value> {
    let bytes = serde_json::to_vec(state)?;
    ensure!(
        bytes.len() <= MAX_CHECKPOINT,
        "Manager conversation reached its checkpoint limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn decode(value: Value) -> Result<Checkpoint> {
    ensure!(
        serde_json::to_vec(&value)?.len() <= MAX_CHECKPOINT,
        "Manager checkpoint is too large"
    );
    let state: Checkpoint = serde_json::from_value(value)?;
    ensure!(state.format == 1, "Unsupported manager checkpoint version");
    Ok(state)
}
pub fn begin(input: String, system: String) -> Result<Value> {
    ensure!(
        input.len() <= MAX_CONTENT && system.len() <= 65536,
        "Manager input is too large"
    );
    let mut run = AgentRun::new(input).max_turns(32);
    let pending = run
        .next_step()
        .map_err(|_| anyhow::anyhow!("Cannot initialize manager run"))?;
    encode(&Checkpoint {
        format: 1,
        system,
        run,
        pending,
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
fn step(state: Checkpoint) -> Result<LoopStep> {
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
    })
}
/// The host must persist the supplied checkpoint and reserve its model attempt
/// before this call, then reject results from superseded revisions before saving.
pub async fn advance(
    config: &ProviderConfig,
    checkpoint: Value,
    cancellation: CancellationToken,
) -> Result<LoopStep> {
    let mut state = decode(checkpoint)?;
    ensure!(!cancellation.is_cancelled(), "Manager turn cancelled");
    let AgentRunStep::CallModel {
        prompt,
        history,
        turn,
    } = state.pending.clone()
    else {
        return step(state);
    };
    config.validate()?;
    let definitions = definitions();
    state.run.advertise_tools(turn, definitions.clone());
    let names: BTreeSet<_> = TOOLS.into_iter().map(str::to_owned).collect();
    let request = CompletionRequest::new(prompt)
        .messages(history)
        .preamble(&state.system)
        .tools(definitions)
        .max_tokens(4096);
    let client = OpenAIConfig::new(config.api_key.clone())
        .with_base_url(&config.base_url)
        .client();
    let model = client.chat(&config.model);
    let response = tokio::select! {
        biased;
        _=cancellation.cancelled()=>bail!("Manager turn cancelled"),
        result=tokio::time::timeout(Duration::from_secs(45),model.call(request))=>match result {
            Ok(Ok(response))=>response,
            Ok(Err(_))=>bail!("Manager provider request failed; check its endpoint, model and credentials"),
            Err(_)=>bail!("Manager provider request timed out"),
        }
    };
    ensure!(!cancellation.is_cancelled(), "Manager turn cancelled");
    let mut answer = ModelTurn::from_response_parts(&response, names.clone(), names);
    // Keep the normalized transcript, not an unbounded duplicate provider body.
    answer.raw = Value::Null;
    let outcome = state
        .run
        .model_response(answer)
        .map_err(|_| anyhow::anyhow!("Manager provider returned an invalid response"))?;
    ensure!(
        !matches!(outcome, ModelTurnOutcome::NeedsResolution(_)),
        "Manager requested an unavailable tool"
    );
    state.pending = state
        .run
        .next_step()
        .map_err(|_| anyhow::anyhow!("Manager run could not advance"))?;
    step(state)
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
        script: Arc<Vec<Value>>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        hold: bool,
    }
    async fn fixture(
        script: Vec<Value>,
        hold: bool,
    ) -> (ProviderConfig, Fixture, tokio::task::JoinHandle<()>) {
        let fixture = Fixture {
            requests: Arc::new(AtomicUsize::new(0)),
            script: Arc::new(script),
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            hold,
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
            let n = f.requests.fetch_add(1, Ordering::SeqCst);
            f.started.notify_one();
            if f.hold {
                f.release.notified().await;
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
    async fn model_cannot_expand_registry_or_resume_unsupported_checkpoint() {
        let call = json!([{"id":"evil","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}]);
        let (config, _, server) = fixture(vec![answer(Value::Null, call)], false).await;
        let result = advance(
            &config,
            begin("input".into(), "system".into()).unwrap(),
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
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
}
