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
