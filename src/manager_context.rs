//! Bounded, process-local diagnostic views of prepared logical requests.
//! These are not provider wire traces or durable execution records.
use rig_core::{
    completion::CompletionRequest,
    message::{AssistantContent, Message, ToolResultContent, UserContent},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_REQUESTS: usize = 64;
const MAX_PROJECT_REQUESTS: usize = 4;
const MAX_TEXT_BYTES: usize = 48 * 1024;
// Manager input/tool-result serialization is separately bounded at 256 KiB.
// Parse that full structured input before applying the smaller display budget.
const MAX_STRUCTURED_BYTES: usize = 256 * 1024;
const MAX_BLOCKS: usize = 256;
const MAX_API_BYTES: usize = 1536 * 1024;

#[derive(Default)]
pub struct Store(Mutex<VecDeque<Record>>);
struct Record {
    project: String,
    value: Value,
}

pub(crate) struct Observer {
    pub store: Arc<Store>,
    pub project: String,
    pub run: String,
}
pub(crate) struct Guard {
    store: Arc<Store>,
    id: String,
    finished: bool,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
impl Store {
    pub(crate) fn capture(
        self: &Arc<Self>,
        project: &str,
        run: &str,
        turn: u64,
        request: &CompletionRequest,
    ) -> Guard {
        let id = crate::db::id();
        let value = prepared(&id, run, turn, request);
        if let Ok(mut records) = self.0.lock() {
            records.push_back(Record {
                project: project.into(),
                value,
            });
            while records.len() > MAX_REQUESTS {
                records.pop_front();
            }
        }
        Guard {
            store: self.clone(),
            id,
            finished: false,
        }
    }
    fn finish(&self, id: &str, status: &str) {
        if let Ok(mut records) = self.0.lock()
            && let Some(record) = records.iter_mut().find(|r| r.value["id"] == id)
        {
            record.value["status"] = json!(status);
            record.value["finished_at_ms"] = json!(now());
        }
    }
    /// Cloning diagnostic values neither consumes them nor changes execution state.
    pub fn view(&self, project: &str) -> Value {
        let requests: Vec<_> = self
            .0
            .lock()
            .map(|records| {
                records
                    .iter()
                    .rev()
                    .filter(|r| r.project == project)
                    .take(MAX_PROJECT_REQUESTS)
                    .map(|r| r.value.clone())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            })
            .unwrap_or_default();
        json!({"project_id":project,"retention":{"scope":"process_memory","max_requests_global":MAX_REQUESTS,"max_requests_returned":MAX_PROJECT_REQUESTS,"max_text_bytes_per_request":MAX_TEXT_BYTES,"lost_on_restart":true},"requests":requests,"limitations":[
            "Prepared logical content is captured before provider translation; it does not prove provider receipt or acceptance.",
            "Snapshots are bounded, redacted, process-local and lost on restart. Images are represented without pixel bytes; opaque provider items are omitted.",
            "Block order is retained. Per-message and per-tool historical timestamps are unavailable unless already present in captured content; structured JSON text is normalized for redaction.",
            "Known credential fields and patterns are redacted. Unknown secrets embedded in unmarked user prose cannot be reliably recognized."
        ]})
    }
}
impl Guard {
    pub(crate) fn finish(&mut self, status: &'static str) {
        self.store.finish(&self.id, status);
        self.finished = true;
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if !self.finished {
            self.store.finish(&self.id, "abandoned");
        }
    }
}

fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_', ' '], "");
    matches!(
        key.as_str(),
        "apikey"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "authorization"
            | "password"
            | "secret"
            | "clientsecret"
            | "appsecret"
            | "privatekey"
            | "mcptoken"
            | "credential"
            | "credentials"
            | "gatewaykey"
            | "providerkey"
            | "secretkey"
            | "headers"
            | "auth"
            | "base64"
            | "imageurl"
            | "imagedata"
            | "imagebytes"
    )
}
fn redact_json(value: &mut Value, changed: &mut bool) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if sensitive(key) {
                    *value = json!("[redacted sensitive field]");
                    *changed = true;
                } else {
                    redact_json(value, changed);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_json(value, changed);
            }
        }
        Value::String(text) => {
            let (next, redacted) = redact_text(text);
            *text = next;
            *changed |= redacted;
        }
        _ => {}
    }
}
fn redact_text(text: &str) -> (String, bool) {
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin") && lower.contains("private key-----") {
        return ("[redacted private key material]".into(), true);
    }
    let mut changed = false;
    let mut output = String::new();
    for line in text.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        if [
            "api_key",
            "api-key",
            "access_token",
            "refresh_token",
            "authorization:",
            "password=",
            "password:",
            "client_secret",
            "mcp_token",
            "token=",
            "token =",
            "sshpass ",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            output.push_str("[redacted credential-bearing line]\n");
            changed = true;
            continue;
        }
        let mut bearer = false;
        for token in line.split_inclusive(char::is_whitespace) {
            let trimmed = token.trim();
            let cleaned = trimmed.trim_start_matches(['"', '\'', '(', '[']);
            if bearer
                || cleaned.starts_with("sk-")
                || cleaned.starts_with("sk_")
                || cleaned.to_ascii_lowercase().starts_with("data:image/")
            {
                output.push_str("[redacted sensitive value]");
                output.extend(
                    token
                        .chars()
                        .rev()
                        .take_while(|c| c.is_whitespace())
                        .collect::<String>()
                        .chars()
                        .rev(),
                );
                changed = true;
            } else {
                output.push_str(token);
            }
            bearer = trimmed.eq_ignore_ascii_case("bearer");
        }
    }
    (output, changed)
}
fn clean(text: &str) -> (String, bool) {
    // Structured user/tool text frequently contains nested resource metadata.
    // The cap also bounds parsing and redaction work on the diagnostic path.
    if text.len() <= MAX_STRUCTURED_BYTES
        && let Ok(mut value) = serde_json::from_str::<Value>(text)
    {
        let mut changed = false;
        redact_json(&mut value, &mut changed);
        return (serde_json::to_string(&value).unwrap_or_default(), changed);
    }
    redact_text(text)
}
struct Builder {
    blocks: Vec<Value>,
    remaining: usize,
    truncated: bool,
    redacted: bool,
}
impl Builder {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            remaining: MAX_TEXT_BYTES,
            truncated: false,
            redacted: false,
        }
    }
    fn push(&mut self, role: &str, kind: &str, text: &str) {
        if self.blocks.len() >= MAX_BLOCKS || self.remaining == 0 {
            self.truncated = true;
            return;
        }
        let mut end = text.len().min(self.remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        // Redact before truncation so a credential prefix cannot be detached
        // from its field. Source text is already host-bounded manager context.
        let (text, changed) = clean(text);
        self.redacted |= changed;
        let mut clean_end = text.len().min(self.remaining);
        while !text.is_char_boundary(clean_end) {
            clean_end -= 1;
        }
        let truncated = clean_end < text.len() || end < text.len();
        self.truncated |= truncated;
        self.remaining -= clean_end;
        self.blocks.push(json!({"index":self.blocks.len(),"role":role,"kind":kind,"text":&text[..clean_end],"truncated":truncated}));
    }
    fn image(&mut self, role: &str) {
        self.push(role,"attachment","Image content part present in prepared logical request. Pixel bytes, image URLs and provider-native payload are omitted from this diagnostic view.");
    }
    fn messages(&mut self, messages: &[Message]) {
        for message in messages {
            match message {
                Message::System { content } => self.push("system", "text", content),
                Message::User { content } => {
                    for item in content {
                        match item {
                    UserContent::Text(text) => self.push("user","text",&text.text),
                    UserContent::Image(_) => self.image("user"),
                    UserContent::ToolResult(result) => {
                        self.push("tool","tool_result",&format!("Tool: {} · Call: {} · Error: {}",result.name,result.call,result.is_error));
                        for part in &result.content { match part {
                            ToolResultContent::Text(text) => self.push("tool","tool_result",&text.text),
                            ToolResultContent::Json{value} => self.push("tool","tool_result",&value.to_string()),
                            ToolResultContent::Image(_) => self.image("tool"),
                        }}
                    },
                    _ => self.push("user","attachment","Non-image media content part present; bytes and source URLs omitted."),
                }
                    }
                }
                Message::Assistant(message) => {
                    for item in &message.content {
                        match item {
                            AssistantContent::Text(text) => {
                                self.push("assistant", "text", &text.text)
                            }
                            AssistantContent::ToolCall(call) => {
                                self.push("assistant","tool_call",&json!({"id":call.id,"name":call.function.name,"arguments":call.function.arguments}).to_string());
                                if call.function.invalid_arguments.is_some() {
                                    self.push(
                                        "assistant",
                                        "tool_call",
                                        "Invalid tool argument text omitted.",
                                    );
                                    self.truncated = true;
                                }
                            }
                            AssistantContent::Image(_) => self.image("assistant"),
                            _ => {
                                self.push("assistant","omitted","Reasoning or opaque/native provider content omitted from diagnostic view.");
                                self.truncated = true;
                            }
                        }
                    }
                }
            }
        }
    }
}
fn prepared(id: &str, run: &str, turn: u64, request: &CompletionRequest) -> Value {
    let mut builder = Builder::new();
    builder.messages(&request.chat_history);
    let tools: Vec<_> = request.tools.iter().take(32).map(|tool| {
        builder.push("system","tool_definition",&json!({"name":tool.name,"description":tool.description,"parameters":tool.parameters}).to_string());
        json!({"name":tool.name})
    }).collect();
    json!({"id":id,"run_id":run,"turn":turn,"prepared_at_ms":now(),"finished_at_ms":null,"status":"prepared","source":"logical_request","blocks":builder.blocks,"tools":tools,"truncated":builder.truncated,"redactions":if builder.redacted {vec!["Known sensitive fields or values removed"]}else{vec![]}})
}

pub(crate) fn normalized(checkpoint: &Value, run: &str) -> Option<Value> {
    let request = crate::manager_loop::normalized_context(checkpoint.clone())?;
    let mut builder = Builder::new();
    // Persisted checkpoint structure is deliberately labelled as normalized;
    // hydration, correction feedback and historical wire timing are not rebuilt.
    builder.messages(&request.chat_history);
    Some(
        json!({"id":format!("checkpoint-{run}"),"run_id":run,"turn":0,"prepared_at_ms":null,"finished_at_ms":null,"status":"historical_normalized","source":"checkpoint_normalized","blocks":builder.blocks,"tools":[],"truncated":builder.truncated,"redactions":if builder.redacted {vec!["Known sensitive fields or values removed"]}else{vec![]}}),
    )
}
pub(crate) fn bounded(mut view: Value) -> Value {
    while serde_json::to_vec(&view).map_or(true, |bytes| bytes.len() > MAX_API_BYTES) {
        let Some(requests) = view["requests"].as_array_mut() else {
            break;
        };
        if requests.is_empty() {
            break;
        }
        requests.remove(0);
        view["response_truncated"] = json!(true);
        if let Some(limitations) = view["limitations"].as_array_mut() {
            let marker =
                json!("Older captures were omitted to keep the diagnostic response bounded.");
            if !limitations.contains(&marker) {
                limitations.push(marker);
            }
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_calls_results_and_followup_are_distinct_ordered_blocks() {
        use rig_core::message::{AssistantMessage, ToolCall, ToolFunction, ToolName};
        let call = ToolCall::from_wire(
            "fixture-call",
            ToolFunction::new(
                ToolName::new("read_file").unwrap(),
                json!({"path":"notes.md","password":"fixture-hidden"}),
            ),
        );
        let result = call.result(vec![ToolResultContent::json(
            json!({"content":"file text","token":"fixture-token"}),
        )]);
        let messages = vec![
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            Message::User {
                content: vec![UserContent::ToolResult(result)],
            },
            Message::user("new user correction"),
        ];
        let mut builder = Builder::new();
        builder.messages(&messages);
        assert_eq!(builder.blocks[0]["kind"], "tool_call");
        assert_eq!(builder.blocks[1]["kind"], "tool_result");
        assert_eq!(builder.blocks[2]["kind"], "tool_result");
        assert_eq!(builder.blocks[3]["text"], "new user correction");
        let output = json!(builder.blocks).to_string();
        assert!(output.contains("file text"));
        assert!(!output.contains("fixture-hidden"));
        assert!(!output.contains("fixture-token"));
    }
    #[test]
    fn redaction_is_structured_and_diagnostic_limits_are_explicit() {
        let request = CompletionRequest::new(
            json!({"task":"keep","api_key":"fixture-hidden","nested":{"token":"fixture-token"}})
                .to_string(),
        )
        .preamble("system instructions");
        let value = prepared("id", "run", 1, &request);
        let text = value.to_string();
        assert!(!text.contains("fixture-hidden"));
        assert!(!text.contains("fixture-token"));
        assert!(text.contains("keep"));
        assert_eq!(value["blocks"][0]["role"], "system");
        assert_eq!(value["blocks"][1]["role"], "user");
        assert_eq!(value["status"], "prepared");
        let mut builder = Builder::new();
        builder.push("user", "text", &"中".repeat(MAX_TEXT_BYTES));
        assert!(builder.truncated);
        assert!(builder.blocks[0]["text"].as_str().unwrap().len() <= MAX_TEXT_BYTES);
        assert!(builder.blocks[0]["truncated"].as_bool().unwrap());
        let (redacted, changed) = clean(
            "hello sk-fixture-secret\nBearer fake-secret\ndata:image/png;base64,ABCD\nauthorization: fake",
        );
        assert!(changed);
        assert!(!redacted.contains("fixture-secret"));
        assert!(!redacted.contains("fake-secret"));
        assert!(!redacted.contains("ABCD"));
        let (redacted, changed) =
            clean("sshpass -p 'fixture-password' ssh fixture-host\ntoken = 'fixture-token'");
        assert!(changed);
        assert!(!redacted.contains("fixture-password"));
        assert!(!redacted.contains("fixture-token"));
    }
    #[test]
    fn large_structured_context_is_redacted_by_field_before_display_truncation() {
        let input = json!({"has_api_key":true,"task":"safe task survives","api_key":"fixture-hidden","padding":"x".repeat(MAX_TEXT_BYTES+100)}).to_string();
        assert!(input.len() > MAX_TEXT_BYTES);
        let (cleaned, changed) = clean(&input);
        assert!(changed);
        let value: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(value["has_api_key"], true);
        assert_eq!(value["task"], "safe task survives");
        assert!(!cleaned.contains("fixture-hidden"));
        assert_ne!(cleaned, "[redacted credential-bearing line]\n");
        let mut builder = Builder::new();
        builder.push("user", "text", &input);
        assert!(builder.truncated);
        assert!(builder.redacted);
        assert!(
            builder.blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("has_api_key")
        );
        assert!(
            !builder.blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("fixture-hidden")
        );
    }
    #[test]
    fn store_is_read_only_scoped_bounded_and_drop_marks_abandoned() {
        let store = Arc::new(Store::default());
        let request = CompletionRequest::new("hello");
        let guard = store.capture("a", "run", 0, &request);
        let first = store.view("a");
        assert_eq!(first, store.view("a"));
        assert!(store.view("b")["requests"].as_array().unwrap().is_empty());
        drop(guard);
        assert_eq!(store.view("a")["requests"][0]["status"], "abandoned");
        let mut guard = store.capture("a", "run", 1, &request);
        guard.finish("provider_responded");
        drop(guard);
        assert_eq!(
            store.view("a")["requests"][1]["status"],
            "provider_responded"
        );
        for _ in 0..MAX_REQUESTS + 1 {
            drop(store.capture("b", "run", 0, &request));
        }
        assert!(store.view("a")["requests"].as_array().unwrap().is_empty());
        assert_eq!(store.0.lock().unwrap().len(), MAX_REQUESTS);
        assert_eq!(
            store.view("b")["requests"].as_array().unwrap().len(),
            MAX_PROJECT_REQUESTS
        );
    }
    #[test]
    fn image_payloads_are_represented_and_order_is_preserved() {
        let messages: Vec<Message> = serde_json::from_value(json!([
            {"role":"system","content":"system"},
                {"role":"user","content":[{"type":"text","text":"additional steer"},{"type":"image","data":{"type":"base64","value":"PIXELS_MUST_NOT_APPEAR"},"media_type":"png"}]}
        ])).unwrap();
        let mut builder = Builder::new();
        builder.messages(&messages);
        assert_eq!(builder.blocks[0]["role"], "system");
        assert_eq!(builder.blocks[1]["text"], "additional steer");
        assert_eq!(builder.blocks[2]["kind"], "attachment");
        assert!(
            !json!(builder.blocks)
                .to_string()
                .contains("PIXELS_MUST_NOT_APPEAR")
        );
        let checkpoint =
            crate::manager_loop::begin("saved user".into(), "saved system".into()).unwrap();
        let fallback = normalized(&checkpoint, "r").unwrap();
        assert_eq!(fallback["source"], "checkpoint_normalized");
        assert!(fallback["prepared_at_ms"].is_null());
    }
}
