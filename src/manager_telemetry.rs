mod content;

use reqwest::{Client, Url, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_SPANS: usize = 256;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    enabled: bool,
    include_content: bool,
    host: String,
    public_key: String,
    secret_key: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            include_content: false,
            host: "https://cloud.langfuse.com".into(),
            public_key: String::new(),
            secret_key: String::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Exporter {
    client: Client,
    endpoint: Url,
    settings: Arc<Settings>,
    slots: Arc<Semaphore>,
}

impl std::fmt::Debug for Exporter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagerTelemetryExporter")
            .finish_non_exhaustive()
    }
}

impl Exporter {
    pub(crate) fn includes_content(&self) -> bool {
        self.settings.include_content
    }

    pub(crate) fn load(dir: &Path) -> Option<Self> {
        let path = dir.join("langfuse.json");
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let result = options.open(path).and_then(|file| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
                    return Ok(None);
                }
            }
            let mut bytes = Vec::new();
            file.take(8193).read_to_end(&mut bytes)?;
            Ok((bytes.len() <= 8192)
                .then(|| serde_json::from_slice(&bytes).ok())
                .flatten()
                .and_then(Self::configured))
        });
        match result {
            Ok(Some(exporter)) => exporter,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            _ => {
                tracing::warn!("Invalid Manager Langfuse configuration; export disabled");
                None
            }
        }
    }

    // An outer None denotes invalid configuration, Some(None) denotes disabled.
    fn configured(settings: Settings) -> Option<Option<Self>> {
        if !settings.enabled {
            return Some(None);
        }
        let mut endpoint = Url::parse(&settings.host).ok()?;
        let local = matches!(
            endpoint.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        );
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && local)
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/"
            || settings.public_key.contains(':')
            || [&settings.public_key, &settings.secret_key]
                .iter()
                .any(|key| key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return None;
        }
        endpoint.set_path("/api/public/otel/v1/traces");
        let client = Client::builder()
            .timeout(TIMEOUT)
            .redirect(Policy::none())
            .build()
            .ok()?;
        Some(Some(Self {
            client,
            endpoint,
            settings: Arc::new(settings),
            slots: Arc::new(Semaphore::new(4)),
        }))
    }

    async fn send(&self, payload: Value) {
        let result = self
            .client
            .post(self.endpoint.clone())
            .basic_auth(&self.settings.public_key, Some(&self.settings.secret_key))
            .json(&payload)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                // OTLP can return HTTP 200 while rejecting some spans. Read
                // only a bounded response and never log its contents.
                let mut response = response;
                let mut body = Vec::new();
                loop {
                    let chunk = match response.chunk().await {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(_) => {
                            tracing::warn!("Manager telemetry response could not be read");
                            return;
                        }
                    };
                    if body.len().saturating_add(chunk.len()) > 4096 {
                        tracing::warn!("Oversized Manager telemetry response");
                        return;
                    }
                    body.extend_from_slice(&chunk);
                }
                if !body.is_empty()
                    && !serde_json::from_slice::<Value>(&body).is_ok_and(|value| {
                        matches!(
                            value["partialSuccess"]["rejectedSpans"].as_str(),
                            None | Some("0")
                        ) && value["partialSuccess"]["rejectedSpans"]
                            .as_u64()
                            .unwrap_or(0)
                            == 0
                    })
                {
                    tracing::warn!("Manager telemetry ingestion response reported a problem");
                }
            }
            Ok(response) => {
                tracing::warn!(status = %response.status(), "Manager telemetry rejected")
            }
            Err(_) => tracing::warn!("Manager telemetry request failed"),
        }
    }
}

fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn span_id() -> String {
    Uuid::new_v4().simple().to_string()[..16].to_owned()
}

fn trace_id(input: &str) -> String {
    format!("{:x}", Sha256::digest(input.as_bytes()))[..32].to_owned()
}

pub(crate) struct Trace {
    exporter: Exporter,
    inputs: Vec<(String, Option<String>)>,
    remaining_content: usize,
    protected_tool_seen: bool,
    started: u128,
    ended: u128,
    root: String,
    spans: Vec<Value>,
    dropped: usize,
    outcome: &'static str,
    export_on_drop: bool,
}

impl Trace {
    pub(crate) fn new(exporter: Exporter, mut inputs: Vec<(String, Option<String>)>) -> Self {
        // Shared spans are replicated for every input. Budget before replication
        // so a large batch cannot multiply the aggregate content without bound.
        let share = content::BATCH_LIMIT / inputs.len().max(1);
        let mut reserved = 0;
        for (_, body) in &mut inputs {
            if let Some(raw) = body.take().filter(|_| exporter.includes_content()) {
                let clean = content::bounded(&raw, (share / 4).min(content::FIELD_LIMIT));
                reserved = reserved.max(clean.len() * 2);
                *body = Some(clean);
            }
        }
        Self {
            remaining_content: share.saturating_sub(reserved),
            protected_tool_seen: false,
            exporter,
            inputs,
            started: now(),
            ended: 0,
            root: span_id(),
            spans: Vec::new(),
            dropped: 0,
            outcome: "interrupted",
            export_on_drop: true,
        }
    }

    pub(crate) fn finish(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    pub(crate) fn span(&mut self, name: &str, kind: &'static str) -> Span<'_> {
        Span {
            trace: self,
            info: json!({"spanId":span_id(), "name":name, "kind":1,"startTimeUnixNano":now().to_string()}),
            attributes: vec![text("langfuse.observation.type", kind)],
            outcome: "interrupted",
        }
    }

    fn payload(&self) -> Value {
        let end = self.ended.max(self.started);
        let spans: Vec<_> = self
            .inputs
            .iter()
            .flat_map(|(id, body)| {
                let trace_id = trace_id(id);
                let mut root = json!({
                    "traceId":trace_id,"spanId":self.root,"name":"manager.interaction","kind":1,
                    "startTimeUnixNano":self.started.to_string(),"endTimeUnixNano":end.to_string(),
                    "status":status(self.outcome),"attributes":[
                        text("langfuse.observation.type","span"),
                        text("dauerbot.outcome",self.outcome),
                        number("dauerbot.batch_size",self.inputs.len() as u64),
                        number("dauerbot.dropped_spans",self.dropped as u64)
                    ]
                });
                let mut admitted = json!({
                "spanId":span_id(),"name":"manager.user_input","kind":1,
                "startTimeUnixNano":self.started.to_string(),"endTimeUnixNano":self.started.to_string(),
                "status":{"code":1},"attributes":[text("langfuse.observation.type","span")]
            });
            if let Some(body) = body {
                for span in [&mut root, &mut admitted] {
                    span["attributes"].as_array_mut().expect("span attributes")
                        .push(observation_content("langfuse.observation.input", body));
                }
            }
            std::iter::once(root).chain(std::iter::once(admitted).chain(self.spans.iter().cloned()).map(move |mut span| {
                    span["traceId"] = json!(trace_id);
                    span["parentSpanId"] = json!(self.root);
                    span
                }))
            })
            .collect();
        json!({"resourceSpans":[{
            "resource":{"attributes":[text("service.name","dauerbot")]},
            "scopeSpans":[{"scope":{"name":"dauerbot.manager"},"spans":spans}]
        }]})
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        if self.inputs.is_empty() || !self.export_on_drop {
            return;
        }
        let Ok(slot) = self.exporter.slots.clone().try_acquire_owned() else {
            tracing::warn!("Manager telemetry capacity reached; dropping trace");
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let trace = Self {
            exporter: self.exporter.clone(),
            remaining_content: 0,
            protected_tool_seen: false,
            inputs: std::mem::take(&mut self.inputs),
            root: std::mem::take(&mut self.root),
            spans: std::mem::take(&mut self.spans),
            started: self.started,
            ended: now(),
            dropped: self.dropped,
            outcome: self.outcome,
            export_on_drop: false,
        };
        runtime.spawn(async move {
            let _slot = slot;
            trace.exporter.send(trace.payload()).await;
        });
    }
}

pub(crate) struct Span<'a> {
    trace: &'a mut Trace,
    info: Value,
    attributes: Vec<Value>,
    outcome: &'static str,
}

impl Span<'_> {
    fn capture(&mut self, key: &str, value: &str) {
        if !self.trace.exporter.includes_content() || self.trace.remaining_content == 0 {
            return;
        }
        let budget = self.trace.remaining_content.min(content::FIELD_LIMIT);
        let clean = content::bounded(value, budget);
        if !clean.is_empty() {
            self.trace.remaining_content -= clean.len();
            self.attributes.push(observation_content(key, &clean));
        }
    }

    pub(crate) fn request(&mut self, request: &rig_core::completion::CompletionRequest) {
        if !self.trace.exporter.includes_content() || self.trace.remaining_content == 0 {
            return;
        }
        // A subsequent model call could quote an unmarked value from a
        // protected file even though we omit the tool's raw result.
        self.trace.protected_tool_seen |= content::has_protected_tool(request);
        if !self.trace.protected_tool_seen {
            self.capture(
                "langfuse.observation.input",
                &content::request_text(request),
            );
        }
    }

    pub(crate) fn assistant(&mut self, output: impl Iterator<Item = impl AsRef<str>>) {
        if !self.trace.exporter.includes_content()
            || self.trace.protected_tool_seen
            || self.trace.remaining_content == 0
        {
            return;
        }
        let mut projected = String::new();
        for part in output.take(32) {
            content::append(&mut projected, "assistant", Some(part.as_ref()));
        }
        self.capture("langfuse.observation.output", &projected);
    }

    pub(crate) fn tool_input(&mut self, name: &str, arguments: &Value) {
        if !self.trace.exporter.includes_content() || self.trace.protected_tool_seen {
            return;
        }
        match name {
            "read_file" | "write_file" | "send_file" => {
                if let Some(path) = arguments["path"].as_str() {
                    let display = if content::sensitive_path(path) {
                        self.trace.protected_tool_seen = true;
                        "[redacted path]"
                    } else {
                        path
                    };
                    self.capture("langfuse.observation.input", display);
                }
            }
            "send_md" | "delegate" | "steer_worker" => {
                let field = if name == "send_md" {
                    "text"
                } else {
                    "instructions"
                };
                if let Some(value) = arguments[field].as_str() {
                    self.capture("langfuse.observation.input", value);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn tool_outcome(&mut self, outcome: &'static str) {
        self.capture("langfuse.observation.output", outcome);
        self.finish(outcome);
    }

    pub(crate) fn model(&mut self, model: &str) {
        self.attributes
            .push(text("gen_ai.request.model", &content::bounded(model, 256)));
    }

    pub(crate) fn response(&mut self, usage: rig_core::completion::Usage) {
        if let Some(input) = usage.input_tokens {
            self.attributes
                .push(number("gen_ai.usage.input_tokens", input));
        }
        if let Some(output) = usage.output_tokens {
            self.attributes
                .push(number("gen_ai.usage.output_tokens", output));
        }
    }

    pub(crate) fn finish(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        if self.trace.spans.len() == MAX_SPANS {
            self.trace.dropped += 1;
            return;
        }
        let start = self.info["startTimeUnixNano"]
            .as_str()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        self.info["endTimeUnixNano"] = json!(now().max(start).to_string());
        self.info["status"] = status(self.outcome);
        self.attributes.push(text("dauerbot.outcome", self.outcome));
        self.info["attributes"] = json!(std::mem::take(&mut self.attributes));
        self.trace.spans.push(self.info.take());
    }
}

fn status(outcome: &str) -> Value {
    json!({"code":if matches!(outcome, "complete" | "ok" | "queued" | "held") {1} else {2}})
}

fn text(key: &str, value: &str) -> Value {
    json!({"key":key,"value":{"stringValue":value}})
}

fn observation_content(key: &str, redacted: &str) -> Value {
    text(key, &json!(redacted).to_string())
}

fn number(key: &str, value: u64) -> Value {
    json!({"key":key,"value":{"intValue":value.min(i64::MAX as u64).to_string()}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    #[test]
    fn content_budget_bounds_replicated_spans_and_keeps_limits_utf8_safe() {
        let exporter = Exporter::configured(Settings {
            enabled: true,
            include_content: true,
            public_key: "fixture-public".into(),
            secret_key: "fixture-secret".into(),
            ..Settings::default()
        })
        .unwrap()
        .unwrap();
        let long = "正文。".repeat(5000);
        let mut trace = Trace::new(
            exporter,
            (0..8)
                .map(|id| (id.to_string(), Some(long.clone())))
                .collect(),
        );
        trace.export_on_drop = false;
        for _ in 0..12 {
            let mut span = trace.span("manager.generation", "generation");
            span.capture("langfuse.observation.input", &long);
            span.capture("langfuse.observation.output", &long);
        }
        let payload = trace.payload();
        let attributes = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["attributes"].as_array().unwrap())
            .filter(|entry| {
                matches!(
                    entry["key"].as_str(),
                    Some("langfuse.observation.input" | "langfuse.observation.output")
                )
            })
            .map(|attribute| {
                serde_json::from_str::<String>(attribute["value"]["stringValue"].as_str().unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            attributes
                .iter()
                .any(|value| value.ends_with("[truncated]"))
        );
        assert!(
            attributes
                .iter()
                .all(|value| value.len() <= content::FIELD_LIMIT)
        );
        assert!(attributes.iter().map(String::len).sum::<usize>() <= content::BATCH_LIMIT);
        assert!(trace.remaining_content < 64);
    }

    #[test]
    fn protected_tool_path_suppresses_later_text_and_tool_arguments() {
        let exporter = Exporter::configured(Settings {
            enabled: true,
            include_content: true,
            public_key: "fixture-public".into(),
            secret_key: "fixture-secret".into(),
            ..Settings::default()
        })
        .unwrap()
        .unwrap();
        let mut trace = Trace::new(exporter, vec![("input".into(), None)]);
        trace.export_on_drop = false;
        trace
            .span("read_file", "tool")
            .tool_input("read_file", &json!({"path":"private/.env.production"}));
        trace
            .span("manager.generation", "generation")
            .assistant(std::iter::once("unmarked short value"));
        trace
            .span("send_md", "tool")
            .tool_input("send_md", &json!({"text":"unmarked short value"}));
        let exported = trace.payload().to_string();
        assert!(exported.contains("[redacted path]"));
        assert!(!exported.contains(".env"));
        assert!(!exported.contains("unmarked short value"));
    }

    struct Log(Arc<Mutex<Vec<u8>>>);

    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn export_deadline_and_local_warnings_do_not_disclose_keys_or_response() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let log = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || Log(log.clone()))
            .finish();
        let public = Uuid::new_v4().simple().to_string();
        let secret = Uuid::new_v4().simple().to_string();
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                use axum::{Router, http::StatusCode, routing::post};
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let host = format!("http://{}", listener.local_addr().unwrap());
                let reply = secret.clone();
                let router = Router::new().route(
                    "/api/public/otel/v1/traces",
                    post(async move || (StatusCode::FORBIDDEN, reply)),
                );
                let server =
                    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
                let exporter = Exporter::configured(Settings {
                    include_content: false,
                    enabled: true,
                    host,
                    public_key: public.clone(),
                    secret_key: secret.clone(),
                })
                .unwrap()
                .unwrap();
                exporter.send(json!({})).await;
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let mut stalled = exporter.clone();
                stalled
                    .endpoint
                    .set_port(Some(listener.local_addr().unwrap().port()))
                    .unwrap();
                let (accepted, mut received) = tokio::sync::oneshot::channel();
                let acceptor = tokio::spawn(async move {
                    let (_socket, _) = listener.accept().await.unwrap();
                    accepted.send(()).unwrap();
                    std::future::pending::<()>().await;
                });
                let started = std::time::Instant::now();
                tokio::time::timeout(TIMEOUT + Duration::from_secs(1), stalled.send(json!({})))
                    .await
                    .expect("Exporter must enforce its own deadline");
                assert!(
                    received.try_recv().is_ok(),
                    "Stalled collector must have received the connection"
                );
                assert!(
                    started.elapsed() >= TIMEOUT,
                    "Request must have waited for its own deadline"
                );
                acceptor.abort();
                server.abort();
            })
        });
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(output.contains("Manager telemetry rejected"));
        assert!(!output.contains(&public));
        assert!(!output.contains(&secret));
    }
}
