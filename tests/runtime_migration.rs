use herdr_feishu_bridge::{
    App, api,
    config::Config,
    db::{Db, ProjectInput},
};
use serde_json::json;
use std::future::IntoFuture;

fn project(dir: &std::path::Path, name: &str) -> ProjectInput {
    ProjectInput {
        name: name.into(),
        path: dir.to_string_lossy().into(),
        agent: "codex".into(),
        chat_id: String::new(),
        native_session: "native-history".into(),
        extra_args: vec![],
    }
}

#[tokio::test]
async fn backend_migration_preserves_bot_messages_and_native_history_without_adopting_core_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite3");
    let db = Db::open(&path).unwrap();
    let p = db
        .create(project(dir.path(), "Existing project"))
        .await
        .unwrap();
    db.ingest_bot(
        "oc_private",
        "om_saved",
        "Saved before migration",
        &json!({"source":{"chat_id":"oc_private","chat_type":"p2p"}}),
    )
    .unwrap();
    db.set_session(
        &p.id,
        Some("t-old-core".into()),
        Some(123),
        "old-generation".into(),
    )
    .await
    .unwrap();
    db.sync(|c|{c.execute_batch("ALTER TABLE projects DROP COLUMN execution; ALTER TABLE projects DROP COLUMN enabled; DROP TABLE archived_core_executions; PRAGMA user_version=3;")?;Ok(())}).unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    let migrated = db.project(&p.id).await.unwrap();
    assert_eq!(migrated.native_session, "native-history");
    assert!(migrated.terminal.is_none());
    assert!(migrated.execution.is_null());
    assert_eq!(db.bot_project().await.unwrap(), Some(p.id.clone()));
    assert_eq!(db.messages(&p.id).await.unwrap().len(), 1);
    let old = db
        .sync(|c| {
            Ok(c.query_row(
                "SELECT terminal FROM archived_core_executions WHERE project_id=?",
                [p.id],
                |r| r.get::<_, String>(0),
            )?)
        })
        .unwrap();
    assert_eq!(old, "t-old-core");
    drop(db);
    let again = Db::open(&path).unwrap();
    assert_eq!(again.projects().await.unwrap().len(), 1);
}

#[tokio::test]
async fn project_tools_are_generation_scoped_and_cannot_manage_other_projects() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::default();
    let app = App::new(dir.path().into(), cfg.clone()).unwrap();
    let a = app.db.create(project(dir.path(), "A")).await.unwrap();
    let b = app.db.create(project(dir.path(), "B")).await.unwrap();
    app.db
        .set_session(
            &a.id,
            Some("legacy-terminal".into()),
            Some(1),
            "active-generation".into(),
        )
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(axum::serve(listener, api::router(app.clone())).into_future());
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let payload = json!({"title":"Work","instructions":"Do the work","request_id":"same-task"});
    let path = format!("{url}/api/projects/{}/tasks", a.id);
    let stale = http
        .post(&path)
        .bearer_auth(&a.mcp_token)
        .header("x-hfb-generation", "old-generation")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 401);
    let request = || {
        http.post(&path)
            .bearer_auth(&a.mcp_token)
            .header("x-hfb-generation", "active-generation")
            .json(&payload)
    };
    let first = request().send().await.unwrap();
    assert!(first.status().is_success());
    let first: serde_json::Value = first.json().await.unwrap();
    let again: serde_json::Value = request().send().await.unwrap().json().await.unwrap();
    assert_eq!(first["id"], again["id"]);
    let other = http
        .post(format!("{url}/api/projects/{}/tasks", b.id))
        .bearer_auth(&a.mcp_token)
        .header("x-hfb-generation", "active-generation")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 401);
    let state = http
        .get(format!("{url}/api/state"))
        .bearer_auth(&a.mcp_token)
        .header("x-hfb-generation", "active-generation")
        .send()
        .await
        .unwrap();
    assert_eq!(state.status(), 401);
    server.abort();
}
