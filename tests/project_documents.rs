use herdr_feishu_bridge::{
    App,
    config::Config,
    db::ProjectInput,
    project_documents::{self, Document},
};
use serde_json::json;
use std::os::unix::fs::symlink;

#[test]
fn fixed_documents_are_bounded_regular_utf8_files_without_symlink_escape() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        project_documents::read(root.path(), Document::Handoff).status,
        "missing"
    );
    std::fs::write(root.path().join("AGENTS.md"), "# 指令\n\n- Keep context.\n").unwrap();
    let read = project_documents::read(root.path(), Document::Agents);
    assert_eq!(read.status, "available");
    assert!(read.text.contains("Keep context"));
    let outside = tempfile::NamedTempFile::new().unwrap();
    symlink(outside.path(), root.path().join("HANDOFF.md")).unwrap();
    assert_eq!(
        project_documents::read(root.path(), Document::Handoff).status,
        "error"
    );
    std::fs::write(
        root.path().join("AGENTS.md"),
        vec![b'x'; project_documents::MAX_BYTES as usize + 1],
    )
    .unwrap();
    assert_eq!(
        project_documents::read(root.path(), Document::Agents).status,
        "error"
    );
    std::fs::write(root.path().join("AGENTS.md"), [255, 254]).unwrap();
    assert_eq!(
        project_documents::read(root.path(), Document::Agents).status,
        "error"
    );
    std::fs::remove_file(root.path().join("AGENTS.md")).unwrap();
    let path = std::ffi::CString::new(root.path().join("AGENTS.md").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert_eq!(
        project_documents::read(root.path(), Document::Agents).status,
        "error"
    );
    assert!(serde_json::from_value::<Document>(json!("../secrets")).is_err());
}

#[tokio::test]
async fn standby_switch_rechecks_after_waiting_for_a_racing_wake() {
    let root = tempfile::tempdir().unwrap();
    let app = App::new(root.path().into(), Config::default()).unwrap();
    let project = app
        .db
        .create(ProjectInput {
            name: "Project".into(),
            path: root.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: String::new(),
            native_session: "original-native".into(),
            extra_args: vec![],
        })
        .await
        .unwrap();
    let lock = app.project_lock(&project.id).await;
    let guard = lock.lock().await;
    let switching = app.clone();
    let key = project.id.clone();
    let attempt = tokio::spawn(async move { switching.switch_agent_mode(&key, "pi", true).await });
    tokio::task::yield_now().await;
    app.db
        .save_execution(
            &project.id,
            json!({"backend":"legacy","terminal_id":"newly-woken","shell_pid":1,"api_protocol":7}),
            "racing-generation",
        )
        .await
        .unwrap();
    drop(guard);
    let error = attempt.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("started working"));
    let unchanged = app.db.project(&project.id).await.unwrap();
    assert_eq!(unchanged.agent, "codex");
    assert_eq!(unchanged.terminal.as_deref(), Some("newly-woken"));
    assert_eq!(unchanged.native_session, "original-native");
    assert_eq!(unchanged.generation, "racing-generation");
}

#[tokio::test]
async fn standby_switch_preserves_workspace_and_clears_only_another_clis_identity() {
    let root = tempfile::tempdir().unwrap();
    let app = App::new(root.path().into(), Config::default()).unwrap();
    for name in ["AGENTS.md", "HANDOFF.md"] {
        std::fs::write(
            root.path().join(name),
            format!("# {name}\nExisting instructions"),
        )
        .unwrap();
    }
    let project = app
        .db
        .create(ProjectInput {
            name: "Project".into(),
            path: root.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: String::new(),
            native_session: "codex-native".into(),
            extra_args: vec!["--model".into(), "original".into()],
        })
        .await
        .unwrap();
    app.db.pause(&project.id, true).await.unwrap();
    let timer_id = app.db.call({ let id=project.id.clone(); move |c| {
        let timer=herdr_feishu_bridge::work::create_timer(c,&id,serde_json::from_value(json!({"name":"Existing reminder","prompt":"Keep this reminder","kind":"once","at":herdr_feishu_bridge::db::now()+60_000}))?,herdr_feishu_bridge::db::now())?;
        Ok(timer.id)
    }}).await.unwrap();
    let same = app
        .switch_agent_mode(&project.id, "codex", true)
        .await
        .unwrap();
    assert_eq!(same.native_session, "codex-native");
    assert_eq!(same.extra_args, project.extra_args);
    let changed = app
        .switch_agent_mode(&project.id, "pi", true)
        .await
        .unwrap();
    assert_eq!(changed.agent, "pi");
    assert!(changed.native_session.is_empty() && changed.extra_args.is_empty());
    assert_eq!(changed.path, project.path);
    assert!(changed.paused && changed.terminal.is_none());
    let timers = app
        .db
        .call({
            let id = project.id.clone();
            move |c| Ok(herdr_feishu_bridge::work::snapshot(c, &id)?.timers)
        })
        .await
        .unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].id, timer_id);
    for name in ["AGENTS.md", "HANDOFF.md"] {
        assert_eq!(
            std::fs::read_to_string(root.path().join(name)).unwrap(),
            format!("# {name}\nExisting instructions")
        );
    }
}

#[tokio::test]
async fn standby_http_route_is_readiness_safe_and_documents_require_admin() {
    let root = tempfile::tempdir().unwrap();
    let cfg = Config::default();
    let token = cfg.token.clone();
    let app = App::new(root.path().into(), cfg).unwrap();
    let project = app
        .db
        .create(ProjectInput {
            name: "Project".into(),
            path: root.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: String::new(),
            native_session: "native".into(),
            extra_args: vec![],
        })
        .await
        .unwrap();
    app.db
        .save_execution(
            &project.id,
            json!({"backend":"legacy","terminal_id":"active","shell_pid":1,"api_protocol":7}),
            "generation",
        )
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = herdr_feishu_bridge::api::router(app.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let http = reqwest::Client::new();
    let response = http
        .post(format!(
            "http://{address}/api/projects/{}/agent/standby",
            project.id
        ))
        .bearer_auth(&token)
        .json(&json!({"agent":"pi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("started working"));
    assert_eq!(app.db.project(&project.id).await.unwrap().agent, "codex");
    let response = http
        .get(format!(
            "http://{address}/api/projects/{}/documents/agents",
            project.id
        ))
        .bearer_auth(&project.mcp_token)
        .header("x-hfb-generation", "generation")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let response = http
        .get(format!(
            "http://{address}/api/projects/{}/documents/handoff",
            project.id
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["status"],
        "missing"
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
