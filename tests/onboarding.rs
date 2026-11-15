//! Real HTTP boundaries for bot group discovery: pagination, auth and permission errors.
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use herdr_feishu_bridge::{App, api, config::Config};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Clone, Default)]
struct Fixture {
    calls: Arc<AtomicUsize>,
    denied: Arc<AtomicBool>,
}
async fn chats(
    State(state): State<Fixture>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    state.calls.fetch_add(1, Ordering::Relaxed);
    if state.denied.load(Ordering::Relaxed) {
        return Json(json!({"code":99991672,"msg":"missing scope im:chat:readonly"}));
    }
    if query.contains_key("page_token") {
        assert_eq!(query["page_token"], "next-page");
        Json(
            json!({"code":0,"data":{"has_more":false,"items":[{"chat_id":"oc_two","name":"第二群","secret_field":"must-not-leak"}]}}),
        )
    } else {
        Json(
            json!({"code":0,"data":{"has_more":true,"page_token":"next-page","items":[{"chat_id":"oc_one","name":"第一群"}]}}),
        )
    }
}
#[tokio::test]
async fn bot_groups_require_local_auth_follow_pages_and_preserve_scope_errors() {
    let fixture = Fixture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/open-apis/auth/v3/tenant_access_token/internal",
            post(|| async {
                Json(json!({"code":0,"tenant_access_token":"fixture-token","expire":3600}))
            }),
        )
        .route("/open-apis/im/v1/chats", get(chats))
        .with_state(fixture.clone());
    let upstream_task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg = Config {
        port: listener.local_addr().unwrap().port(),
        feishu_base: upstream,
        app_id: "cli_fixture".into(),
        app_secret: "fixture-secret".into(),
        ..Default::default()
    };
    let url = format!("{}/api/onboarding/chats", cfg.url());
    let token = cfg.token.clone();
    let app = App::new(dir.path().to_owned(), cfg).unwrap();
    let router = api::router(app);
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let http = reqwest::Client::new();
    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
    let response: Value = http
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["items"].as_array().unwrap().len(), 2);
    assert_eq!(response["items"][1]["chat_id"], "oc_two");
    assert!(response["items"][1].get("secret_field").is_none());
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
    fixture.denied.store(true, Ordering::Relaxed);
    let response = http.get(&url).bearer_auth(token).send().await.unwrap();
    assert_eq!(response.status(), 400);
    let response: Value = response.json().await.unwrap();
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("im:chat:readonly")
    );
    task.abort();
    upstream_task.abort();
}
