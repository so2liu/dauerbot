//! Pi owns the model/tool loop. The daemon retains all tool and delivery authority.
use super::{
    App, Context, Failure, ProviderConfig, ToolCall, Value, checkpoint, current, finish_turn, json,
    manager_loop, tool,
};
use crate::multimodal::ImageRef;
use anyhow::{Context as _, Result, bail, ensure};
use rig_core::message::{DocumentSourceKind, UserContent};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
};

const TOOLS: [&str; 9] = [
    "read_file",
    "write_file",
    "delegate",
    "steer_worker",
    "worker_status",
    "send_md",
    "send_file",
    "schedule_run",
    "record_topic_decision",
];

#[derive(Deserialize, Serialize)]
pub(super) struct Turn {
    executable: PathBuf,
    prompt: String,
    images: Vec<ImageRef>,
    started: bool,
}

impl Turn {
    pub(super) fn select(
        database: Option<&str>,
        project: &crate::db::Project,
        prompt: &str,
        images: &[ImageRef],
    ) -> Result<Option<Value>> {
        let Some(root) = database
            .filter(|path| !path.is_empty())
            .and_then(|path| Path::new(path).parent())
        else {
            return Ok(None);
        };
        ensure!(
            !project.id.is_empty()
                && project
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "Invalid Manager project identity"
        );
        let path = root.join(".dauerbot").join("manager-pi").join(&project.id);
        let executable = match std::fs::read_to_string(path) {
            Ok(raw) => PathBuf::from(raw.trim()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            executable.is_absolute(),
            "Select an absolute Pi executable path"
        );
        let executable = executable.canonicalize()?;
        let workspace = Path::new(&project.path).canonicalize()?;
        ensure!(
            executable.is_file() && !executable.starts_with(workspace),
            "Select a trusted Pi executable outside the project workspace"
        );
        Ok(Some(
            json!({"pi": Self { executable, prompt: prompt.into(), images: images.to_vec(), started: false }}),
        ))
    }
}

async fn tool_request(mut stream: UnixStream, app: &Arc<App>, ctx: &Context) -> Result<()> {
    let mut request = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        BufReader::new(&mut stream)
            .take(1_100_000)
            .read_until(b'\n', &mut request),
    )
    .await??;
    ensure!(request.last() == Some(&b'\n'), "Incomplete Pi tool request");
    let call: ToolCall = serde_json::from_slice(&request)?;
    let result = if TOOLS.contains(&call.name.as_str())
        && !call.occurrence.is_empty()
        && call.occurrence.len() <= 512
        && !app.service.manager_gate.is_closed()
    {
        tool(app, ctx, call).await?
    } else {
        json!({"error":"Tool is outside the active Manager allowlist"})
    };
    stream.write_all(&serde_json::to_vec(&result)?).await?;
    stream.shutdown().await?;
    Ok(())
}

fn start(turn: &Turn, config: &ProviderConfig, root: &Path) -> Result<tokio::process::Child> {
    config.validate()?;
    let agent = root.join("agent");
    std::fs::create_dir(&agent)?;
    let mut definitions = manager_loop::definitions();
    definitions.retain(|tool| TOOLS.contains(&tool.name.as_str()));
    ensure!(
        definitions.len() == TOOLS.len(),
        "Pi Manager tool registry is incomplete"
    );
    let models = json!({"providers":{"dauerbot-manager":{
        "baseUrl":config.base_url, "api":"openai-completions", "apiKey":"$DAUERBOT_PI_KEY",
        "compat":{"supportsDeveloperRole":false},
        "models":[{"id":config.model,"input":if config.supports_images { vec!["text","image"] } else { vec!["text"] }}]
    }}});
    std::fs::write(agent.join("models.json"), models.to_string())?;
    std::fs::write(
        agent.join("settings.json"),
        br#"{"retry":{"enabled":false}}"#,
    )?;
    std::fs::write(root.join("tools.json"), serde_json::to_vec(&definitions)?)?;
    let extension = root.join("manager.ts");
    std::fs::write(&extension, include_str!("pi_extension.ts"))?;
    let child = Command::new(&turn.executable)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("XDG_CACHE_HOME", root)
        .env("XDG_DATA_HOME", root)
        .env("XDG_STATE_HOME", root)
        .env("TMPDIR", root)
        .env("PI_CODING_AGENT_DIR", agent)
        .env("DAUERBOT_PI_KEY", &config.api_key)
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--offline",
            "--no-approve",
            "--no-builtin-tools",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--provider",
            "dauerbot-manager",
            "--model",
        ])
        .arg(&config.model)
        .arg("--tools")
        .arg(TOOLS.join(","))
        .arg("--extension")
        .arg(extension)
        .arg("--system-prompt")
        .arg(super::SYSTEM)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    Ok(child)
}

async fn images(turn: &Turn) -> Vec<Value> {
    let mut images = Vec::new();
    for source in &turn.images {
        if let Ok(UserContent::Image(image)) = source.content().await
            && let DocumentSourceKind::Base64(data) = image.data
        {
            images.push(json!({"type":"image","data":data,"mimeType":source.media_type}));
        }
    }
    images
}

pub(super) async fn drive(
    app: Arc<App>,
    ctx: &Context,
    config: &ProviderConfig,
    data: Value,
) -> Result<&'static str> {
    let mut turn: Turn = serde_json::from_value(data)?;
    let result = run(&app, ctx, config, &mut turn).await;
    result.map_err(|error| {
        #[cfg(test)]
        eprintln!("Pi fixture runner: {error:#}");
        #[cfg(not(test))]
        let _ = error;
        let mut failure = Failure::internal();
        failure.kind = "pi_runtime_interrupted".into();
        failure.retryable = false;
        failure.into()
    })
}

async fn run(
    app: &Arc<App>,
    ctx: &Context,
    config: &ProviderConfig,
    turn: &mut Turn,
) -> Result<&'static str> {
    ensure!(!turn.started, "Interrupted Pi run requires recovery");
    let temporary = tempfile::Builder::new().prefix("dbpi").tempdir_in("/tmp")?;
    let root = temporary.path();
    let listener = UnixListener::bind(root.join("tools.sock"))?;
    let images = images(turn).await;
    turn.started = true;
    checkpoint(app, ctx, &json!({"pi":turn}), false).await?;
    let mut child = start(turn, config, root)?;
    let result = rpc(app, ctx, config, turn, &listener, &mut child, images).await;
    // Reap Pi before discarding its private socket and configuration.
    let _ = child.kill().await;
    result
}

async fn rpc(
    app: &Arc<App>,
    ctx: &Context,
    config: &ProviderConfig,
    turn: &Turn,
    listener: &UnixListener,
    child: &mut tokio::process::Child,
    images: Vec<Value>,
) -> Result<&'static str> {
    let mut stdin = child.stdin.take().context("Missing Pi input")?;
    let mut stdout = BufReader::new(child.stdout.take().context("Missing Pi output")?).lines();
    stdin.write_all(b"{\"type\":\"get_state\"}\n").await?;
    let reply = tokio::time::timeout(Duration::from_secs(10), stdout.next_line())
        .await??
        .context("Pi did not report its configured model")?;
    let state: Value = serde_json::from_str(&reply)?;
    let model = &state["data"]["model"];
    ensure!(
        state["type"] == "response"
            && state["command"] == "get_state"
            && state["success"] == true
            && model["provider"] == "dauerbot-manager"
            && model["id"] == config.model
            && model["baseUrl"] == config.base_url
            && model["api"] == "openai-completions",
        "Pi did not select the configured Manager provider"
    );
    ensure!(
        current(app, ctx).await && !app.service.manager_gate.is_closed(),
        "Management turn superseded"
    );
    let request = json!({"type":"prompt","message":turn.prompt,"images":images});
    stdin.write_all(format!("{request}\n").as_bytes()).await?;
    let deadline = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut note = None;
    let mut turns = 0;
    loop {
        tokio::select! {
            _ = &mut deadline => bail!("Pi Manager deadline exceeded"),
            _ = tick.tick() => {
                if app.service.manager_gate.is_closed() { return Ok("yielded"); }
                ensure!(!app.shutdown.is_cancelled() && current(app, ctx).await, "Management turn superseded");
            }
            socket = listener.accept() => {
                let (stream, _) = socket?;
                tool_request(stream, app, ctx).await?;
            }
            line = stdout.next_line() => {
                let line = line?.context("Pi exited before settling")?;
                let event: Value = serde_json::from_str(&line)?;
                match event["type"].as_str() {
                    Some("response") if event["success"] == false => bail!("Pi rejected its prompt"),
                    Some("turn_start") => {
                        turns += 1;
                        ensure!(turns <= 32, "Pi Manager turn limit reached");
                    }
                    Some("agent_end") => {
                        let messages = event["messages"].as_array().context("Pi returned no messages")?;
                        let output = messages.iter().rev().find(|message| message["role"] == "assistant").context("Pi returned no answer")?;
                        ensure!(output["stopReason"] == "stop", "Pi did not complete its answer");
                        note = Some(output["content"].as_array().into_iter().flatten()
                            .filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n"));
                    }
                    Some("agent_settled") => {
                        ensure!(event["aborted"] != true, "Pi aborted");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    let note = note.context("Pi settled without a completed answer")?;
    let Some(_permit) = app.service.gate.enter() else {
        return Ok("yielded");
    };
    let context = ctx.clone();
    app.db
        .call(move |c| finish_turn(c, &context, &note))
        .await?;
    Ok("complete")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, response::IntoResponse, routing::post};
    use std::sync::Mutex;

    #[tokio::test]
    #[ignore = "requires an explicitly selected installed Pi executable"]
    async fn pi_owns_the_loop_with_exact_tools_and_host_owned_effects() -> Result<()> {
        let executable =
            PathBuf::from(std::env::var_os("DAUERBOT_TEST_PI").context("Set DAUERBOT_TEST_PI")?);
        let (_dir, app, project) = super::super::tests::fixture().await;
        let insert = |message: &str, root: &str, body: &str| -> Result<String> {
            app.db.ingest_bot(&project.id, "cli_fixture", "oc_fixture", message, body,
                &json!({"source":{"chat_id":"oc_fixture","chat_type":"group","message_type":"text",
                    "thread_id":root,"root_id":root,"sender":{"open_id":"ou_fixture"}}}))?
                .context("unique fixture message")
        };
        let claim = || {
            app.db.sync(|c| {
                crate::work::sync_inbound(c)?;
                super::super::claim(c, &project.id)?.context("fixture turn")
            })
        };
        let decision = insert("om_historic", "om_topic", "Confirmed scope 1324")?;
        let old = claim()?;
        let receipt = tool(&app, &old.context, ToolCall {
            occurrence:"record".into(), name:"record_topic_decision".into(),
            arguments:json!({"source_event_id":decision,"branch":"main","key":"year","kind":"current","evidence":"Confirmed scope 1324"}),
        }).await?;
        assert_eq!(receipt["status"], "recorded");
        app.db.sync(|c| finish_turn(c, &old.context, ""))?;
        insert("om_same", "om_topic", "Old same-thread evidence")?;
        let same = claim()?;
        app.db.sync(|c| finish_turn(c, &same.context, ""))?;
        for i in 0..10 {
            insert(
                &format!("om_noise_{i}"),
                "om_unrelated",
                "DO NOT IMPORT OTHER TOPIC",
            )?;
            let noise = claim()?;
            app.db.sync(|c| finish_turn(c, &noise.context, ""))?;
        }
        let active = insert("om_current", "om_topic", "Current fixture instruction")?;
        let marker_dir = app.dir.join(".dauerbot").join("manager-pi");
        std::fs::create_dir_all(&marker_dir)?;
        std::fs::write(
            marker_dir.join(&project.id),
            executable.as_os_str().as_encoded_bytes(),
        )?;
        let steps = [
            (
                "write_file",
                json!({"path":"pi-fixture.txt","content":"Written by Pi"}),
            ),
            ("read_file", json!({"path":"pi-fixture.txt"})),
            ("bash", json!({"command":"touch should-not-exist"})),
            (
                "delegate",
                json!({"source_event_id":active,"decision_branch":"main","instructions":"Implement the fixture task"}),
            ),
            (
                "schedule_run",
                json!({"name":"fixture reminder","prompt":"Review result","kind":"interval","interval_seconds":3600,"purpose":"internal"}),
            ),
            (
                "send_md",
                json!({"to":"oc_fixture","text":"Pi fixture reply","source_event_id":active,"reply_to_message_id":"om_current","intent":"result"}),
            ),
        ];
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = requests.clone();
        let router = Router::new()
            .route("/open-apis/auth/v3/tenant_access_token/internal", post(|| async {
                Json(json!({"code":0,"tenant_access_token":"fixture-tenant","expire":7200}))
            }))
            .route("/open-apis/im/v1/messages/{message}/reply", post(|| async {
                Json(json!({"code":0,"data":{"message_id":"om_fake_sent","thread_id":"omt_fixture","root_id":"om_topic","parent_id":"om_current","chat_id":"oc_fixture"}}))
            }))
            .route("/v1/chat/completions", post(move |headers:HeaderMap, Json(body):Json<Value>| {
                assert_eq!(headers["authorization"], "Bearer private-manager-key");
                let mut seen = captured.lock().unwrap();
                let step = steps.get(seen.len()).cloned();
                seen.push(body);
                async move {
                    let (delta, finish) = if let Some((name, args)) = step {
                        (json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("call_{name}"),
                            "type":"function","function":{"name":name,"arguments":args.to_string()}}]}), "tool_calls")
                    } else {
                        (json!({"role":"assistant","content":"Pi fixture finished"}), "stop")
                    };
                    let chunk = |delta: Value, finish: Value| json!({
                        "id":"fixture","object":"chat.completion.chunk","created":1,"model":"fixture-manager",
                        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
                    });
                    ([("content-type","text/event-stream")], format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        chunk(delta, Value::Null), chunk(json!({}), json!(finish))
                    )).into_response()
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        app.config.write().await.feishu_base = endpoint.clone();
        app.db.sync(|c| {
            let settings = super::super::public(c, &project.id)?;
            super::super::save_provider(
                c,
                serde_json::from_value(json!({
                    "base_url":format!("{endpoint}/v1"),
                    "model":settings["model"], "expected_revision":settings["revision"],
                }))?,
            )?;
            Ok(())
        })?;
        let turn = claim()?;
        assert!(turn.checkpoint["pi"].is_object());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let run = turn.context.run.clone();
        let outcome = tokio::time::timeout(
            Duration::from_secs(35),
            super::super::drive(app.clone(), turn),
        )
        .await?;
        server.abort();
        outcome?;
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 7);
            for request in requests.iter() {
                let actual: std::collections::BTreeSet<_> = request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["function"]["name"].as_str().unwrap())
                    .collect();
                assert_eq!(actual, TOOLS.into_iter().collect());
                assert_eq!(request["tools"].as_array().unwrap().len(), TOOLS.len());
                assert!(
                    request["max_tokens"].is_null() && request["max_completion_tokens"].is_null()
                );
            }
            let messages = requests[0]["messages"].to_string();
            assert!(
                messages.contains("Old same-thread evidence")
                    && messages.contains("Current fixture instruction")
            );
            assert!(messages.contains("Confirmed scope 1324") && messages.contains(&decision));
            assert!(!messages.contains("DO NOT IMPORT OTHER TOPIC"));
            let forbidden_result = requests[3]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|part| part["role"] == "tool" && part["tool_call_id"] == "call_bash")
                .context("Missing rejection from Pi")?;
            assert!(forbidden_result["content"].to_string().contains("bash"));
            assert!(
                requests[2]["messages"]
                    .to_string()
                    .contains("Written by Pi")
            );
            eprintln!(
                "Exactly nine tools on all seven requests. Forbidden call rejection: {}",
                forbidden_result["content"]
            );
        }
        assert_eq!(
            std::fs::read_to_string(Path::new(&project.path).join("pi-fixture.txt"))?,
            "Written by Pi"
        );
        app.db.sync(|c| {
            let effects = c.prepare("SELECT name,status FROM manager_effects WHERE run_id=? ORDER BY rowid")?
                .query_map([&run], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(effects.len(),5);
            assert!(effects.iter().all(|(_,status)| status == "complete"), "{effects:?}");
            let timer:u32 = c.query_row("SELECT count(*) FROM work_timers WHERE project_id=?", [&project.id], |row| row.get(0))?;
            assert_eq!(timer, 1);
            let sent:u32 = c.query_row("SELECT count(*) FROM messages WHERE project_id=? AND direction='out' AND status='sent'", [&project.id], |row| row.get(0))?;
            assert_eq!(sent, 1);
            Ok(())
        })?;
        let event = app.db.sync(|c| {
            let id: String = c.query_row(
                "SELECT id FROM work_events WHERE source='management' AND project_id=?",
                [&project.id],
                |row| row.get(0),
            )?;
            crate::work::event(c, &project.id, &id)
        })?;
        let assignment = crate::worker_task::fixture_assignment(&app, &project, &event).await?;
        let task = std::fs::read_to_string(assignment)?;
        assert!(
            task.contains("Confirmed scope 1324")
                && task.contains(&decision)
                && task.contains("om_historic")
        );
        assert!(task.contains(&active) && task.contains("Implement the fixture task"));
        eprintln!(
            "Pi completed read/write/delegate/send/schedule. The generated TASK.md contains both origin IDs and the confirmed decision."
        );
        Ok(())
    }
}
