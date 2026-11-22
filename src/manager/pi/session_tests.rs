use super::*;
use crate::{config::Config, db::Project};
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const FIRST_INPUT: &str = "PI-HISTORY-A: the complete opening instruction with details 1324.";
const FIRST_REPLY: &str = "PI-HISTORY-A-REPLY: the complete answer with its second sentence.";
const FOREIGN_INPUT: &str = "PI-FOREIGN-CONTENT: outside the first discussion.";
const FOREIGN_REPLY: &str = "PI-FOREIGN-REPLY: outside the first discussion.";
const RECOVERY: &str =
    "Pi session could not be restored; started a new discussion session and reloaded host context.";

const SUMMARY: &str = "PI-NATIVE-COMPACTION-CHECKPOINT: retain the established goal.";
const SUMMARY_REQUEST: &str = "Create a structured context checkpoint summary";

fn is_compaction_request(request: &Value) -> bool {
    if request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
    {
        return false;
    }
    let messages = request["messages"].to_string();
    messages.contains(SUMMARY_REQUEST)
        || messages.contains(
            "Create a concise checkpoint of the user's request and the progress shown above",
        )
}

enum ModelReply {
    Text(&'static str),
    LongText(&'static str, bool),
    Hang,
    Tool(&'static str, Value),
}

#[derive(Default)]
struct ServerState {
    script: Mutex<VecDeque<ModelReply>>,
    requests: Mutex<Vec<Value>>,
    attempted_sends: AtomicUsize,
    in_flight: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct ProviderFixture {
    state: Arc<ServerState>,
    endpoint: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for ProviderFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl ProviderFixture {
    async fn start() -> Result<Self> {
        let state = Arc::new(ServerState::default());
        let model = state.clone();
        let feishu = state.clone();
        let router = Router::new()
            .route(
                "/open-apis/auth/v3/tenant_access_token/internal",
                post(|| async {
                    Json(json!({"code":0,"tenant_access_token":"fixture-tenant","expire":7200}))
                }),
            )
            .route(
                "/open-apis/im/v1/messages/{message}/reply",
                post(move || {
                    let feishu = feishu.clone();
                    async move {
                        feishu.attempted_sends.fetch_add(1, Ordering::SeqCst);
                        StatusCode::BAD_GATEWAY
                    }
                }),
            )
            .route(
                "/v1/chat/completions",
                post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                    let model = model.clone();
                    async move {
                        assert_eq!(headers["authorization"], "Bearer private-manager-key");
                        let summarizes = is_compaction_request(&body);
                        let reply = if summarizes {
                            ModelReply::Text(SUMMARY)
                        } else {
                            model.script.lock().unwrap().pop_front().expect("unscripted Pi request")
                        };
                        model.requests.lock().unwrap().push(body);
                        let mut prompt_tokens = 10;
                        let (delta, finish) = match reply {
                            ModelReply::Text(text) => (json!({"role":"assistant","content":text}), "stop"),
                            ModelReply::Hang => {
                                model.in_flight.notify_one();
                                model.release.notified().await;
                                return StatusCode::BAD_GATEWAY.into_response();
                            }
                            ModelReply::LongText(marker, trigger) => {
                                if trigger { prompt_tokens = 125_000; }
                                (json!({"role":"assistant","content":format!("{marker} {}", "fixture ".repeat(3750))}), "stop")
                            }
                            ModelReply::Tool(name, arguments) => (json!({
                                "role":"assistant","tool_calls":[{"index":0,"id":format!("call_{name}"),
                                    "type":"function","function":{"name":name,"arguments":arguments.to_string()}}]
                            }), "tool_calls"),
                        };
                        let chunk = |delta: Value, finish: Value| json!({
                            "id":"fixture","object":"chat.completion.chunk","created":1,"model":"fixture-manager",
                            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
                        });
                        ([("content-type","text/event-stream")], format!(
                            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            chunk(delta, Value::Null), chunk(json!({}), json!(finish)),
                            json!({
                                "id":"fixture","object":"chat.completion.chunk","created":1,"model":"fixture-manager",
                                "choices":[],"usage":{"prompt_tokens":prompt_tokens,"completion_tokens":1,"total_tokens":prompt_tokens+1}
                            }),
                        )).into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Ok(Self {
            state,
            endpoint,
            server,
        })
    }

    async fn claim_and_drive(
        &self,
        app: Arc<App>,
        project: &Project,
        steps: impl IntoIterator<Item = ModelReply>,
    ) -> Result<(String, Vec<Value>)> {
        {
            let mut script = self.state.script.lock().unwrap();
            assert!(script.is_empty(), "previous Pi response was unused");
            script.extend(steps);
        }
        let turn = app.db.sync(|c| {
            crate::work::sync_inbound(c)?;
            super::super::claim(c, &project.id)?.context("fixture turn")
        })?;
        assert!(turn.checkpoint["pi"].is_object());
        let run_id = turn.context.run.clone();
        tokio::time::timeout(Duration::from_secs(35), super::super::drive(app, turn)).await??;
        assert!(self.state.script.lock().unwrap().is_empty());
        let requests = std::mem::take(&mut *self.state.requests.lock().unwrap());
        for request in &requests {
            if is_compaction_request(request) {
                continue;
            }
            let tools = request["tools"]
                .as_array()
                .context("Pi completion tool definitions")?;
            assert_eq!(tools.len(), TOOLS.len());
            let names: BTreeSet<_> = tools
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, TOOLS.into_iter().collect());
        }
        Ok((run_id, requests))
    }
}

fn insert(
    app: &App,
    project: &Project,
    message: &str,
    root: &str,
    thread: &str,
    body: &str,
) -> Result<String> {
    app.db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_fixture",
            message,
            body,
            &json!({"source":{"chat_id":"oc_fixture","chat_type":"group","message_type":"text",
            "thread_id":thread,"root_id":root,"sender":{"open_id":"ou_fixture"}}}),
        )?
        .context("unique fixture message")
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    key: String,
    file: String,
    session_id: String,
}

fn snapshots(app: &App, project: &Project) -> Result<Vec<Snapshot>> {
    app.db.sync(|c| {
        let mut statement = c.prepare(
            "SELECT scope,snapshot,session_id FROM manager_pi_sessions WHERE project_id=? ORDER BY scope",
        )?;
        Ok(statement.query_map([&project.id], |row| Ok(Snapshot {
            key: row.get(0)?,
            file: row.get(1)?,
            session_id: row.get(2)?,
        }))?.collect::<rusqlite::Result<_>>()?)
    })
}

fn history(request: &Value, role: &str) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == role)
        .map(|message| match message["content"].as_str() {
            Some(text) => text.to_owned(),
            None => message["content"].to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn configured(
    prov: &ProviderFixture,
) -> Result<(tempfile::TempDir, Arc<App>, Project, Config)> {
    let executable =
        PathBuf::from(std::env::var_os("DAUERBOT_TEST_PI").context("Set DAUERBOT_TEST_PI")?);
    let (dir, app, project) = super::super::tests::fixture().await;
    let selector = app.dir.join(".dauerbot").join("manager-pi");
    std::fs::create_dir_all(&selector)?;
    std::fs::write(
        selector.join(&project.id),
        executable.as_os_str().as_encoded_bytes(),
    )?;
    let config = Config {
        feishu_base: prov.endpoint.clone(),
        ..Config::default()
    };
    *app.config.write().await = config.clone();
    app.db.sync(|c| {
        let settings = super::super::public(c, &project.id)?;
        super::super::save_provider(
            c,
            serde_json::from_value(json!({
                "base_url":format!("{}/v1", prov.endpoint), "model":settings["model"],
                "expected_revision":settings["revision"],
            }))?,
        )?;
        Ok(())
    })?;

    Ok((dir, app, project, config))
}

#[tokio::test]
#[ignore = "requires an explicitly selected installed Pi executable"]
async fn pi_persistent_session_preserves_discussion_boundaries_and_effects() -> Result<()> {
    let provider = ProviderFixture::start().await?;
    let (_dir, mut app, project, config) = configured(&provider).await?;
    let store = app.dir.clone();
    let origin = insert(
        &app,
        &project,
        "om_first",
        "om_discussion_a",
        "omt_first",
        FIRST_INPUT,
    )?;
    let (first_run, first_requests) = provider
        .claim_and_drive(
            app.clone(),
            &project,
            [
                ModelReply::Tool(
                    "write_file",
                    json!({"path":"persisted.txt","content":"One confirmed write"}),
                ),
                ModelReply::Tool(
                    "send_md",
                    json!({
                        "to":"oc_fixture","text":"One uncertain delivery","source_event_id":origin,
                        "reply_to_message_id":"om_first","intent":"result",
                    }),
                ),
                ModelReply::Text(FIRST_REPLY),
            ],
        )
        .await?;
    assert_eq!(first_requests.len(), 3);
    assert_eq!(provider.state.attempted_sends.load(Ordering::SeqCst), 1);
    let first_snapshot = snapshots(&app, &project)?
        .pop()
        .context("first discussion identity")?;

    insert(
        &app,
        &project,
        "om_foreign",
        "om_discussion_b",
        "omt_first",
        FOREIGN_INPUT,
    )?;
    let (_, foreign) = provider
        .claim_and_drive(app.clone(), &project, [ModelReply::Text(FOREIGN_REPLY)])
        .await?;
    assert_eq!(foreign.len(), 1);
    assert!(!foreign[0]["messages"].to_string().contains("PI-HISTORY-A"));
    let identities = snapshots(&app, &project)?;
    assert_eq!(identities.len(), 2);
    let foreign_snapshot = identities
        .iter()
        .find(|snapshot| snapshot.key != first_snapshot.key)
        .context("another discussion identity")?;
    assert_ne!(foreign_snapshot.session_id, first_snapshot.session_id);

    let older = "PI-QUEUED-OLDER: do this first.";
    let later = "PI-QUEUED-LATER: do this second.";
    insert(
        &app,
        &project,
        "om_second",
        "om_discussion_a",
        "omt_second",
        older,
    )?;
    insert(
        &app,
        &project,
        "om_third",
        "om_discussion_a",
        "omt_second",
        later,
    )?;
    app.db.sync(|c| {
        crate::work::sync_inbound(c)?;
        assert_eq!(c.query_row(
            "SELECT count(*) FROM manager_inputs WHERE project_id=? AND status='queued' AND kind='user'",
            [&project.id], |row| row.get::<_, i64>(0)
        )?, 2);
        Ok(())
    })?;

    drop(app);
    app = App::new(store, config)?;
    let (_, resumed) = provider
        .claim_and_drive(
            app.clone(),
            &project,
            [
                ModelReply::Tool("bash", json!({"command":"touch must-not-exist"})),
                ModelReply::Text("PI-RESUMED-REPLY: attached to the original discussion."),
            ],
        )
        .await?;
    assert_eq!(resumed.len(), 2);
    let user = history(&resumed[0], "user");
    let assistant = history(&resumed[0], "assistant");
    assert_eq!(user.matches(FIRST_INPUT).count(), 1, "{user}");
    assert_eq!(assistant.matches(FIRST_REPLY).count(), 1, "{assistant}");
    assert!(
        user.find(older).context("older queued input")?
            < user.find(later).context("later queued input")?
    );
    assert!(!resumed[0]["messages"].to_string().contains("PI-FOREIGN"));
    let tool_result = resumed[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "call_bash")
        .context("Pi denial of forged Bash call")?;
    assert_eq!(tool_result["content"], "Tool bash not found");
    let resumed_snapshot = snapshots(&app, &project)?
        .into_iter()
        .find(|snapshot| snapshot.key == first_snapshot.key)
        .context("resumed discussion identity")?;
    assert_eq!(resumed_snapshot.session_id, first_snapshot.session_id);
    assert_eq!(
        snapshots(&app, &project)?
            .iter()
            .find(|snapshot| snapshot.key == foreign_snapshot.key),
        Some(foreign_snapshot)
    );

    // Corrupt the selected topic's stored bytes while leaving its pointer intact.
    let session_root = app.dir.join(".dauerbot").join("manager-pi-sessions");
    assert_eq!(session_root.metadata()?.permissions().mode() & 0o777, 0o700);
    std::fs::write(
        session_root.join(format!("{}.jsonl", resumed_snapshot.file)),
        b"bad session bytes",
    )?;
    insert(
        &app,
        &project,
        "om_recover",
        "om_discussion_a",
        "omt_second",
        "PI-RECOVERY: keep going.",
    )?;
    let (recovery_run, recovered) = provider
        .claim_and_drive(
            app.clone(),
            &project,
            [ModelReply::Text(
                "PI-RECOVERY-REPLY: a new conversation recovered the host context.",
            )],
        )
        .await?;
    assert_eq!(recovered.len(), 1);
    let recovered_messages = recovered[0]["messages"].to_string();
    assert!(recovered_messages.contains(RECOVERY));
    assert!(recovered_messages.contains("PI-RECOVERY: keep going."));
    assert!(
        recovered_messages.contains(FIRST_INPUT),
        "host context missing"
    );
    assert!(!recovered_messages.contains("PI-FOREIGN"));
    let fallback_snapshot = snapshots(&app, &project)?
        .into_iter()
        .find(|snapshot| snapshot.key == first_snapshot.key)
        .context("replacement discussion identity")?;
    assert_ne!(fallback_snapshot.session_id, first_snapshot.session_id);
    assert_eq!(
        snapshots(&app, &project)?
            .iter()
            .find(|snapshot| snapshot.key == foreign_snapshot.key),
        Some(foreign_snapshot)
    );

    app.db.sync(|c| {
        let note: String = c.query_row(
            "SELECT note FROM manager_turn_notes WHERE run_id=?",
            [&recovery_run], |row| row.get(0),
        )?;
        assert!(note.starts_with(RECOVERY), "{note}");
        let effects: Vec<(String, String, String)> = c.prepare(
            "SELECT run_id,name,status FROM manager_effects ORDER BY rowid",
        )?.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(effects, vec![
            (first_run.clone(), "write_file".into(), "complete".into()),
            (first_run.clone(), "send_md".into(), "uncertain".into()),
        ]);
        assert_eq!(c.query_row(
            "SELECT status FROM messages WHERE direction='out' AND content='One uncertain delivery'",
            [], |row| row.get::<_, String>(0),
        )?, "uncertain");
        assert_eq!(c.query_row(
            "SELECT count(*) FROM manager_inputs WHERE project_id=? AND status='queued' AND kind='user'",
            [&project.id], |row| row.get::<_, i64>(0)
        )?, 0);
        Ok(())
    })?;
    assert_eq!(provider.state.attempted_sends.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(Path::new(&project.path).join("persisted.txt"))?,
        "One confirmed write"
    );
    assert!(!Path::new(&project.path).join("must-not-exist").exists());
    eprintln!(
        "Pi restored one same-root transcript after database reopen, isolated a second root sharing the same thread ID, retained FIFO, rejected forged Bash, and rotated a corrupt snapshot without changing either effect receipt."
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicitly selected installed Pi executable"]
async fn pi_persistent_session_keeps_native_compaction() -> Result<()> {
    let provider = ProviderFixture::start().await?;
    let (_dir, app, project, _) = configured(&provider).await?;
    for (index, marker) in [
        "PI-PRECOMPACTION-FIRST",
        "PI-PRECOMPACTION-SECOND",
        "PI-PRECOMPACTION-THIRD",
        "PI-PRECOMPACTION-FOURTH",
    ]
    .into_iter()
    .enumerate()
    {
        insert(
            &app,
            &project,
            &format!("om_compact_{index}"),
            "om_compact_root",
            "omt_compact_thread",
            &format!("Actual long conversation, turn {index}"),
        )?;
        let (_, requests) = provider
            .claim_and_drive(
                app.clone(),
                &project,
                [ModelReply::LongText(marker, index == 3)],
            )
            .await?;
        if index == 3 {
            assert!(
                requests
                    .iter()
                    .any(|request| request["messages"].to_string().contains(SUMMARY_REQUEST)),
                "Pi did not make a native summarization request after the fixture reported threshold usage"
            );
        } else {
            assert_eq!(requests.len(), 1);
        }
    }
    let snapshot = snapshots(&app, &project)?
        .pop()
        .context("compacted discussion identity")?;
    let native_transcript = std::fs::read_to_string(
        app.dir
            .join(".dauerbot")
            .join("manager-pi-sessions")
            .join(format!("{}.jsonl", snapshot.file)),
    )?;
    assert!(
        native_transcript.lines().any(|line| {
            let entry: Value = serde_json::from_str(line).unwrap();
            entry["type"] == "compaction"
                && entry["summary"]
                    .as_str()
                    .is_some_and(|text| text.contains(SUMMARY))
        }),
        "Pi did not persist its compaction entry"
    );
    insert(
        &app,
        &project,
        "om_after_compact",
        "om_compact_root",
        "omt_compact_thread",
        "PI-POSTCOMPACTION: continue from the retained context.",
    )?;
    let (_, requests) = provider
        .claim_and_drive(
            app.clone(),
            &project,
            [ModelReply::Text(
                "PI-POSTCOMPACTION-REPLY: continuing correctly.",
            )],
        )
        .await?;
    assert_eq!(requests.len(), 1);
    let wire = requests[0]["messages"].to_string();
    assert!(wire.contains(SUMMARY));
    assert!(wire.contains("PI-POSTCOMPACTION: continue from the retained context."));
    assert!(!wire.contains("PI-PRECOMPACTION-FIRST"));
    assert_eq!(
        snapshots(&app, &project)?.pop().unwrap().session_id,
        snapshot.session_id
    );
    eprintln!(
        "Pi made its own threshold summarization call after four long turns and resumed the same session from its durable compaction entry."
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicitly selected installed Pi executable"]
async fn pi_persistent_session_does_not_replay_active_interrupted_turn() -> Result<()> {
    let provider = ProviderFixture::start().await?;
    let (_dir, app, project, config) = configured(&provider).await?;
    insert(
        &app,
        &project,
        "om_settled",
        "om_crash_root",
        "omt_crash_thread",
        "PI-SETTLED: establish a checkpoint.",
    )?;
    provider
        .claim_and_drive(
            app.clone(),
            &project,
            [ModelReply::Text(
                "PI-SETTLED-ANSWER: the last committed transcript.",
            )],
        )
        .await?;
    let settled = snapshots(&app, &project)?;
    let origin = insert(
        &app,
        &project,
        "om_interrupted",
        "om_crash_root",
        "omt_crash_thread",
        "PI-INTERRUPTED: preserve me without replay.",
    )?;
    provider.state.script.lock().unwrap().extend([
        ModelReply::Tool(
            "write_file",
            json!({"path":"before-crash.txt","content":"Committed before interrupted response"}),
        ),
        ModelReply::Hang,
    ]);
    let turn = app.db.sync(|c| {
        crate::work::sync_inbound(c)?;
        super::super::claim(c, &project.id)?.context("active fixture turn")
    })?;
    let run = turn.context.run.clone();
    let driver = tokio::spawn(super::super::drive(app.clone(), turn));
    tokio::time::timeout(Duration::from_secs(20), provider.state.in_flight.notified()).await?;
    driver.abort();
    assert!(driver.await.unwrap_err().is_cancelled());
    provider.state.release.notify_one();
    assert_eq!(provider.state.requests.lock().unwrap().len(), 2);

    let store = app.dir.clone();
    drop(app);
    // The production owner token changes with the daemon process. Recreate that
    // transition explicitly because this fixture reopens App in the same test process.
    {
        let db = rusqlite::Connection::open(store.join("bridge.sqlite3"))?;
        assert_eq!(
            db.execute(
                "UPDATE manager_runs SET owner='prior-fixture-daemon' WHERE id=?",
                [&run]
            )?,
            1
        );
    }
    let app = App::new(store, config)?;
    let resumed = app
        .db
        .sync(|c| super::super::claim(c, &project.id)?.context("interrupted fixture claim"))?;
    assert_eq!(resumed.context.run, run);
    assert_eq!(resumed.checkpoint["pi"]["started"], true);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(20),
            super::super::drive(app.clone(), resumed)
        )
        .await?
        .is_err()
    );
    assert_eq!(
        provider.state.requests.lock().unwrap().len(),
        2,
        "resubmitted the interrupted provider request"
    );
    assert_eq!(
        snapshots(&app, &project)?,
        settled,
        "published an interrupted trajectory"
    );
    app.db.sync(|c| {
        assert_eq!(
            c.query_row(
                "SELECT status FROM manager_inputs WHERE origin_event_id=?",
                [&origin],
                |row| row.get::<_, String>(0)
            )?,
            "processing"
        );
        let effects = c
            .prepare("SELECT name,status FROM manager_effects WHERE run_id=? ORDER BY rowid")?
            .query_map([&run], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(effects, vec![("write_file".into(), "complete".into())]);
        Ok(())
    })?;
    assert_eq!(
        std::fs::read_to_string(Path::new(&project.path).join("before-crash.txt"))?,
        "Committed before interrupted response"
    );
    eprintln!(
        "An interrupted live Pi request kept the input and confirmed effect. Restoring its working claim made no provider request and left the last settled snapshot intact."
    );
    Ok(())
}
