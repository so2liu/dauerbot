use herdr_feishu_bridge::{App, api, config::Config, db::ProjectInput};
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
