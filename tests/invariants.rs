use herdr_feishu_bridge::{
    App, api,
    config::Config,
    db::{Db, ProjectInput},
    feishu, herdr,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;

fn input(dir: &TempDir, name: &str, chat: &str) -> ProjectInput {
    ProjectInput {
        name: name.into(),
        path: dir.path().to_string_lossy().into(),
        agent: "codex".into(),
        chat_id: chat.into(),
        native_session: String::new(),
        extra_args: vec![],
    }
}

#[tokio::test]
async fn durable_dedup_and_uncertain_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db.sqlite");
    let db = Db::open(&path).unwrap();
    let p = db.create(input(&dir, "一", "oc_test")).await.unwrap();
    let key = db
        .ingest("oc_test", "om_one", "hello", &json!({}))
        .unwrap()
        .unwrap();
    assert!(
        db.ingest("oc_test", "om_one", "hello", &json!({}))
            .unwrap()
            .is_none()
    );
    assert!(
        db.ingest("oc_unbound", "om_two", "hello", &json!({}))
            .unwrap()
            .is_none()
    );
    assert!(
        db.ack(&p.id, "om_one").await.is_err(),
        "queued is not acknowledged before delivery"
    );
    db.state(&key, "submitting", "").await.unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.message(&key).await.unwrap().status, "uncertain");
    assert!(db.ack("another-project", "om_one").await.is_err());
    db.ack(&p.id, "om_one").await.unwrap();
    assert_eq!(db.message(&key).await.unwrap().status, "acknowledged");
}

#[tokio::test]
async fn outbox_request_ids_do_not_create_duplicate_messages() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let p = db.create(input(&dir, "一", "oc_test")).await.unwrap();
    let first = db
        .enqueue_out(&p.id, "stable-id", "hello", json!({}))
        .await
        .unwrap();
    let again = db
        .enqueue_out(&p.id, "stable-id", "hello", json!({}))
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(db.messages(&p.id).await.unwrap().len(), 1);
    assert!(
        db.create(input(&dir, "二", "oc_test")).await.is_err(),
        "chat routing must be unambiguous"
    );
}

#[tokio::test]
async fn one_projects_backlog_cannot_starve_another_project() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let a = db.create(input(&dir, "积压项目", "oc_a")).await.unwrap();
    let b = db.create(input(&dir, "其他项目", "oc_b")).await.unwrap();
    db.pause(&a.id, true).await.unwrap();
    for n in 0..40 {
        db.ingest("oc_a", &format!("om_a_{n}"), "backlog", &json!({}))
            .unwrap();
    }
    db.ingest("oc_b", "om_b", "new message", &json!({}))
        .unwrap();
    let heads = db.pending("in").await.unwrap();
    assert_eq!(heads.len(), 2);
    assert!(heads.iter().any(|m| m.project_id == b.id));
    assert_eq!(
        heads
            .iter()
            .find(|m| m.project_id == a.id)
            .unwrap()
            .source_id,
        "om_a_0"
    );
}

#[tokio::test]
async fn process_and_generation_are_required_before_typing() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "一", "")).await.unwrap();
    p.terminal_pid = Some(100);
    p.generation = "generation-1".into();
    let t = json!({"state":"live","pid":100,"argv":["codex","--no-daemon"],"labels":{"hfb.project":p.id,"hfb.generation":p.generation}});
    assert!(herdr::check_identity(&p, &t).is_ok());
    let mut changed = t.clone();
    changed["pid"] = json!(101);
    assert!(herdr::check_identity(&p, &changed).is_err());
    changed = t.clone();
    changed["argv"] = json!(["/bin/zsh"]);
    assert!(herdr::check_identity(&p, &changed).is_err());
    changed = t.clone();
    changed["labels"]["hfb.generation"] = json!("older");
    assert!(herdr::check_identity(&p, &changed).is_err());
    changed = t;
    changed["state"] = json!("finished");
    assert!(herdr::check_identity(&p, &changed).is_err());
}

#[tokio::test]
async fn attachments_cannot_escape_project_via_path_or_symlink() {
    let root = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let db = Db::open(&root.path().join("db")).unwrap();
    let p = db.create(input(&project, "一", "")).await.unwrap();
    std::fs::create_dir(root.path().join("files")).unwrap();
    std::fs::write(project.path().join("good.txt"), "ok").unwrap();
    std::fs::write(root.path().join("secret"), "private").unwrap();
    assert!(feishu::allowed_file(&p, &project.path().join("good.txt"), root.path()).is_ok());
    assert!(feishu::allowed_file(&p, &root.path().join("secret"), root.path()).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path().join("secret"), project.path().join("link"))
            .unwrap();
        assert!(feishu::allowed_file(&p, &project.path().join("link"), root.path()).is_err());
    }
}

#[test]
fn inbound_filters_bots_and_normalizes_messages() {
    let mut v = json!({"header":{"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_me"}},"message":{"chat_id":"oc_test","message_id":"om_1","message_type":"text","content":"{\"text\":\"hello\\n世界\"}"}}});
    let (_, _, text, _) = feishu::normalize(&v).unwrap().unwrap();
    assert!(text.contains("hello\n世界"));
    v["event"]["sender"]["sender_type"] = json!("app");
    assert!(feishu::normalize(&v).unwrap().is_none());
    v["event"]["sender"]["sender_type"] = json!("user");
    v["event"]["message"]["content"] = json!("not json");
    assert!(feishu::normalize(&v).is_err());
}

#[test]
fn startup_settle_failure_retries_only_when_core_confirms_no_input() {
    let safe: anyhow::Error = herdr::RpcFailure {
        kind: "unavailable".into(),
        message: "still starting".into(),
        hint: "nothing was typed; check the screen".into(),
    }
    .into();
    assert!(herdr::definitely_not_sent(&safe));
    let ambiguous: anyhow::Error = herdr::RpcFailure {
        kind: "unavailable".into(),
        message: "connection lost".into(),
        hint: "retry".into(),
    }
    .into();
    assert!(!herdr::definitely_not_sent(&ambiguous));
    assert!(!herdr::definitely_not_sent(&anyhow::anyhow!(
        "transport timed out after writing"
    )));
}

async fn test_server() -> (TempDir, Arc<App>, String, tokio::task::JoinHandle<()>) {
    let dir = TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg = Config {
        port: listener.local_addr().unwrap().port(),
        ..Config::default()
    };
    let url = cfg.url();
    let app = App::new(dir.path().to_owned(), cfg).unwrap();
    let router = api::router(app.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, app, url, task)
}

#[tokio::test]
async fn local_api_auth_origin_and_project_scope() {
    let (dir, app, url, server) = test_server().await;
    let cfg = app.config.read().await.clone();
    let http = reqwest::Client::new();
    let p = app.db.create(input(&dir, "一", "oc_1")).await.unwrap();
    let q = app.db.create(input(&dir, "二", "oc_2")).await.unwrap();
    let result = http.get(format!("{url}/api/state")).send().await.unwrap();
    assert_eq!(result.status(), 401);
    let result = http
        .get(format!("{url}/api/state"))
        .bearer_auth(&cfg.token)
        .header("Origin", "https://untrusted.example")
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 403);
    let result: Value = http
        .get(format!("{url}/api/state"))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(result["projects"][0].get("mcp_token").is_none());
    assert!(result["settings"].get("app_secret").is_none());
    app.db
        .set_session(&p.id, Some("t1".into()), Some(1), "current".into())
        .await
        .unwrap();
    for (path, generation) in [
        (format!("/projects/{}/stop", p.id), "current"),
        (format!("/projects/{}/outbox", q.id), "current"),
        (format!("/projects/{}/ready", p.id), "stale"),
    ] {
        let result = http
            .post(format!("{url}/api{path}"))
            .bearer_auth(&p.mcp_token)
            .header("x-hfb-generation", generation)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 401);
    }
    // A valid scoped credential is authenticated, but validation still rejects a bad acknowledgement.
    let result = http
        .post(format!("{url}/api/projects/{}/ack", p.id))
        .bearer_auth(&p.mcp_token)
        .header("x-hfb-generation", "current")
        .json(&json!({"message_id":"om_unrelated"}))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 400);
    // A custom Core session's t1 must never be passed to an unrelated default App.
    app.config.write().await.core_session = "isolated-test".into();
    let result = http
        .post(format!("{url}/api/projects/{}/show", p.id))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 400);
    server.abort();
}

#[tokio::test]
async fn antigravity_uses_interactive_prompt_flag_instead_of_a_positional_prompt() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut input = input(&dir, "交互式 AGY", "");
    input.agent = "agy".into();
    let mut p = db.create(input).await.unwrap();
    let args = herdr::launch_args(
        &p,
        &dir.path().join("mcp.json"),
        &json!({}),
        "等待飞书任务",
        false,
    )
    .unwrap();
    assert_eq!(&args[args.len() - 2..], &["-i", "等待飞书任务"]);
    assert!(!args.iter().any(|v| v == "-p" || v == "--print"));
    p.native_session = "conversation-id".into();
    let args = herdr::launch_args(
        &p,
        &dir.path().join("mcp.json"),
        &json!({}),
        "继续待命",
        true,
    )
    .unwrap();
    assert!(
        args.windows(2)
            .any(|v| v == ["--conversation", "conversation-id"])
    );
    assert_eq!(&args[args.len() - 2..], &["-i", "继续待命"]);
}

#[tokio::test]
async fn workspace_trust_requires_exact_project_identity_and_selected_yes() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "信任边界", "")).await.unwrap();
    p.agent = "agy".into();
    p.generation = "generation".into();
    p.terminal_pid = Some(123);
    let t = json!({
        "state":"live","pid":123,"cwd":p.path,"argv":["agy","-i","prompt"],
        "labels":{"hfb.project":p.id,"hfb.generation":p.generation},
        "status":{"app":"antigravity","state":"blocked","kind":"permission","msg":"Trust this folder?"}
    });
    let screen = format!(
        "Accessing workspace:\n\n{}\n\nDo you trust the contents of this project?\n\n> Yes, I trust this folder\n  No, exit\n",
        p.path
    );
    assert!(herdr::is_project_trust(&p, &t, &screen));
    for invalid in [
        screen.replace(&p.path, "/another/project"),
        screen.replace("> Yes, I trust this folder", "  Yes, I trust this folder"),
        "Allow shell command execution?".into(),
    ] {
        assert!(!herdr::is_project_trust(&p, &t, &invalid));
    }
    for (field, replacement) in [
        ("/status/msg", json!("Allow this tool?")),
        ("/labels/hfb.generation", json!("old")),
        ("/pid", json!(999)),
        ("/state", json!("finished")),
        ("/cwd", json!("/another/project")),
    ] {
        let mut invalid = t.clone();
        *invalid.pointer_mut(field).unwrap() = replacement;
        assert!(!herdr::is_project_trust(&p, &invalid, &screen));
    }
    p.ready = true;
    assert!(!herdr::is_project_trust(&p, &t, &screen));
}

#[tokio::test]
async fn native_launch_keeps_cli_model_defaults_and_user_overrides() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "原生模型", "")).await.unwrap();
    for (agent, approval) in [
        ("codex", "--dangerously-bypass-approvals-and-sandbox"),
        ("claude", "--dangerously-skip-permissions"),
        ("pi", "--approve"),
        ("agy", "--dangerously-skip-permissions"),
    ] {
        p.agent = agent.into();
        p.extra_args.clear();
        let args = herdr::launch_args(
            &p,
            &dir.path().join("mcp.json"),
            &json!({"command":"hfb","args":["mcp"]}),
            "登记 MCP 并待命",
            false,
        )
        .unwrap();
        assert!(args.iter().any(|v| v == approval), "{agent}");
        assert!(
            !args.iter().any(|v| [
                "--model",
                "--provider",
                "--settings",
                "--mode",
                "--effort",
                "--thinking",
                "-m"
            ]
            .contains(&v.as_str())),
            "{agent}"
        );
        assert!(
            !args
                .iter()
                .any(|v| v.starts_with("model=") || v.starts_with("model_provider=")),
            "{agent}"
        );
        p.extra_args = vec!["--model".into(), "user-selected-model".into()];
        let args = herdr::launch_args(
            &p,
            &dir.path().join("mcp.json"),
            &json!({}),
            "登记 MCP 并待命",
            false,
        )
        .unwrap();
        assert_eq!(args.iter().filter(|v| v.as_str() == "--model").count(), 1);
        assert!(
            args.windows(2)
                .any(|v| v == ["--model", "user-selected-model"])
        );
    }
}

#[test]
fn launch_forwards_existing_network_settings_without_overriding_identity_or_models() {
    let vars = [
        ("HTTPS_PROXY", "http://127.0.0.1:7897"),
        ("no_proxy", "localhost,127.0.0.1"),
        ("HC_TERMINAL_ID", "unrelated"),
        ("HC_SESSION", "unrelated"),
        ("ANTHROPIC_MODEL", "unrelated-model"),
        ("GEMINI_API_KEY", "unrelated-key"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()));
    let forwarded = herdr::launch_network_env(vars);
    assert_eq!(forwarded.len(), 2);
    assert_eq!(forwarded["HTTPS_PROXY"], "http://127.0.0.1:7897");
    assert_eq!(forwarded["no_proxy"], "localhost,127.0.0.1");
    assert!(herdr::launch_network_env([]).is_empty());
}
