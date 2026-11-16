use herdr_feishu_bridge::{
    App, api,
    config::Config,
    db::{Db, ProjectInput},
    work,
};
use serde_json::json;
use std::future::IntoFuture;

fn input(path: &std::path::Path) -> ProjectInput {
    ProjectInput {
        name: "Authority fixture".into(),
        path: path.to_string_lossy().into(),
        agent: "codex".into(),
        chat_id: String::new(),
        native_session: "previous-native".into(),
        extra_args: vec![],
    }
}

#[tokio::test]
async fn upgrade_removes_the_stopped_project_gate_and_preserves_explicit_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite3");
    let db = Db::open(&path).unwrap();
    let stopped = db.create(input(dir.path())).await.unwrap();
    let mut paused_input = input(dir.path());
    paused_input.name = "Explicitly paused".into();
    let paused = db.create(paused_input).await.unwrap();
    db.pause(&paused.id, true).await.unwrap();
    db.sync(|c| {
        herdr_feishu_bridge::bot::save(c, &stopped.id, "cli_preserved", "saved-secret", false)?;
        c.execute_batch(
            "ALTER TABLE projects ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
             UPDATE projects SET enabled=0;
             PRAGMA user_version=5;",
        )?;
        Ok(())
    })
    .unwrap();
    drop(db);

    let upgraded = Db::open(&path).unwrap();
    assert!(!upgraded.project(&stopped.id).await.unwrap().paused);
    assert!(upgraded.project(&paused.id).await.unwrap().paused);
    assert_eq!(
        upgraded.project(&stopped.id).await.unwrap().native_session,
        "previous-native"
    );
    assert_eq!(
        upgraded.bot(&stopped.id).await.unwrap().app_secret,
        "saved-secret"
    );
    upgraded
        .sync(|c| {
            let obsolete: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name='enabled')",
                [],
                |row| row.get(0),
            )?;
            assert!(!obsolete);
            assert_eq!(
                c.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
                7
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn inherited_native_pointer_is_not_confirmation_for_a_new_execution() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
    let project = db.create(input(dir.path())).await.unwrap();
    db.save_execution(
        &project.id,
        json!({"terminal_id":"terminal","shell_pid":1}),
        "new-generation",
    )
    .await
    .unwrap();
    assert_ne!(
        db.project(&project.id).await.unwrap().execution["native_confirmed"],
        true
    );
    db.native_session(&project.id, "confirmed-new-history")
        .await
        .unwrap();
    let confirmed = db.project(&project.id).await.unwrap();
    assert_eq!(confirmed.native_session, "confirmed-new-history");
    assert_eq!(confirmed.execution["native_confirmed"], true);
}

#[tokio::test]
async fn pre_refactor_store_is_refused_without_mutating_its_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite3");
    let db = Db::open(&path).unwrap();
    let project = db.create(input(dir.path())).await.unwrap();
    db.set_session(
        &project.id,
        Some("old-core-terminal".into()),
        Some(10),
        "old-generation".into(),
    )
    .await
    .unwrap();
    db.sync(|c| {
        c.execute_batch(
            "ALTER TABLE projects DROP COLUMN execution;
             PRAGMA user_version=3;",
        )?;
        Ok(())
    })
    .unwrap();
    drop(db);
    assert!(Db::open(&path).is_err());
    let c = rusqlite::Connection::open(&path).unwrap();
    let columns: Vec<String> = c
        .prepare("PRAGMA table_info(projects)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!columns.iter().any(|column| column == "execution"));
    assert!(!columns.iter().any(|column| column == "enabled"));
    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 3);
    let terminal: String = c
        .query_row(
            "SELECT terminal FROM projects WHERE id=?",
            [&project.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(terminal, "old-core-terminal");
    let native: String = c
        .query_row(
            "SELECT native_session FROM projects WHERE id=?",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(native, "previous-native");
}

#[tokio::test]
async fn terminal_heartbeat_cannot_reactivate_a_released_control_pin() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::default();
    let token = cfg.token.clone();
    let app = App::new(dir.path().into(), cfg).unwrap();
    let project = app.db.create(input(dir.path())).await.unwrap();
    app.db
        .save_execution(&project.id, json!({"terminal_id":"t","shell_pid":1}), "g")
        .await
        .unwrap();
    let id = project.id.clone();
    app.db
        .call(move |c| {
            work::set_takeover(c, &id, "g", false, 0)?;
            Ok(())
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/projects/{}/takeover",
        listener.local_addr().unwrap(),
        project.id
    );
    let server = tokio::spawn(axum::serve(listener, api::router(app.clone())).into_future());
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .bearer_auth(token)
        .json(&json!({"generation":"g","active":true,"heartbeat":true}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["active"], false);
    let id = project.id;
    assert!(
        !app.db
            .call(move |c| work::has_takeover(c, &id, "g"))
            .await
            .unwrap()
    );
    server.abort();
}

#[tokio::test]
async fn three_tool_upgrade_preserves_messages_and_schedules_without_replaying_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite3");
    let db = Db::open(&path).unwrap();
    let project = db.create(input(dir.path())).await.unwrap();
    db.sync(|c| {
        herdr_feishu_bridge::bot::save(c, &project.id, "cli_preserved", "saved-secret", false)?;
        Ok(())
    })
    .unwrap();
    for (source, status) in [
        ("submitted", "submitted"),
        ("uncertain", "uncertain"),
        ("acknowledged", "acknowledged"),
    ] {
        let id = db
            .ingest_bot(
                &project.id,
                "cli_preserved",
                "oc_fixture",
                source,
                "Do not repeat",
                &json!({}),
            )
            .unwrap()
            .unwrap();
        db.state(&id, status, "").await.unwrap();
    }
    let id = project.id.clone();
    let timer = db.call(move |c| {
        work::create_timer(c,&id,serde_json::from_value(json!({"name":"Existing interval","prompt":"Check once recovered","kind":"interval","interval_seconds":60,"timezone":"Asia/Shanghai","request_id":"existing-schedule"}))?,0)
    }).await.unwrap();
    let saved = timer.clone();
    db.sync(|c| {
        work::fire_due(c, 60_000)?;
        // Reproduce the previous store's obsolete tables and a stale notice.
        c.execute_batch(
            "CREATE TABLE work_jobs(id TEXT PRIMARY KEY, status TEXT);
            INSERT INTO work_jobs VALUES('old-child','active');
            CREATE TABLE work_decisions(id TEXT PRIMARY KEY,status TEXT);
            INSERT INTO work_decisions VALUES('old-decision','pending');
            PRAGMA user_version=6;",
        )?;
        work::upsert_attention(
            c,
            &project.id,
            "g",
            "unfinished_run",
            "Old completion gate",
            "",
            61_000,
        )?;
        Ok(())
    })
    .unwrap();
    drop(db);

    let upgraded = Db::open(&path).unwrap();
    let id = project.id.clone();
    upgraded.call(move |c| {
        let state = work::snapshot(c,&id)?;
        assert_eq!(state.timers.len(),1);
        assert_eq!(state.timers[0].id,saved.id);
        assert_eq!(state.timers[0].next_fire,Some(120_000));
        assert_eq!(state.timers[0].timezone,saved.timezone);
        assert!(!state.attention.iter().any(|a| a.status == "open"));
        assert_eq!(c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('work_jobs','work_decisions')",[],|r|r.get::<_,i64>(0))?,0);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM messages WHERE project_id=? AND status='queued'",[&id],|r|r.get::<_,i64>(0))?,0);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM messages WHERE project_id=?",[&id],|r|r.get::<_,i64>(0))?,3);
        assert!(work::claim_steering(c,&id,"new",70_000)?.is_none());
        assert!(work::fire_due(c,70_000)?.is_empty());
        // Native timer controls still cancel pending occurrences, retain IDs,
        // resume after downtime, and never delete already submitted input.
        work::set_timer_enabled(c,&id,&saved.id,false,70_001)?;
        assert!(work::claim_next(c,&id,"new",70_002)?.is_none());
        work::set_timer_enabled(c,&id,&saved.id,true,200_000)?;
        assert_eq!(work::fire_due(c,200_001)?.len(),1);
        let run=work::claim_next(c,&id,"new",200_002)?.unwrap();
        work::delivery(c,&run.id,"submitted","",200_003)?;
        work::delete_timer(c,&id,&saved.id,200_004)?;
        assert!(work::fire_due(c,1_000_000)?.is_empty());
        assert_eq!(work::run(c,&id,&run.id)?.status,"submitted");
        assert!(c.prepare("PRAGMA foreign_key_check")?.query([])?.next()?.is_none());
        Ok(())
    }).await.unwrap();
    let messages = upgraded.messages(&project.id).await.unwrap();
    for status in ["submitted", "uncertain", "acknowledged"] {
        assert_eq!(
            messages
                .iter()
                .find(|m| m.source_id == status)
                .unwrap()
                .status,
            status
        );
    }
}
