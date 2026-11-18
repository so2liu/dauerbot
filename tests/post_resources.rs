use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use herdr_feishu_bridge::{App, bot, config::Config, db, envelope, feishu};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::io::AsyncReadExt;

fn event(kind: &str, content: Value) -> Value {
    json!({"header":{"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_real"}},"message":{"chat_id":"oc_real","message_id":"om_original","message_type":kind,"content":content.to_string()}}})
}

#[test]
fn localized_and_flat_posts_keep_one_caption_and_deduplicate_embedded_images() {
    let doc = json!({"title":"日常","content":[[{"tag":"text","text":"查看截图"},{"tag":"img","image_key":"img_actual"}],[{"tag":"img","image_key":"img_actual"},{"tag":"a","text":"参考","href":"https://example.invalid/private"}]]});
    for content in [
        doc.clone(),
        json!({"zh_cn":doc,"en_us":{"title":"Daily","content":[[{"tag":"text","text":"查看截图"},{"tag":"img","image_key":"img_translation"}]]}}),
    ] {
        let (chat, source, text, attachment) =
            feishu::normalize(&event("post", content)).unwrap().unwrap();
        assert_eq!((chat.as_str(), source.as_str()), ("oc_real", "om_original"));
        assert_eq!(text.matches("查看截图").count(), 1);
        assert!(text.starts_with("日常\n查看截图"));
        assert!(text.contains("参考"));
        assert!(!text.contains("https://"));
        assert_eq!(attachment["resources"].as_array().unwrap().len(), 1);
        assert_eq!(attachment["resources"][0]["file_key"], "img_actual");
        assert_eq!(attachment["source"]["sender"]["open_id"], "ou_real");
    }
    let (_, _, text, _) = feishu::normalize(&event(
        "post",
        json!({"en_us":{"content":[[{"tag":"text","text":"English only"}]]}}),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(text, "English only");
}

type Requests = Arc<Mutex<Vec<(String, String, String)>>>;
async fn resource(
    State(requests): State<Requests>,
    Path((message, key)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    requests
        .lock()
        .unwrap()
        .push((message, key.clone(), query["type"].clone()));
    if key == "img_budget" {
        return axum::http::Response::builder()
            .header("content-length", feishu::MAX_FILE)
            .body(axum::body::Body::from_stream(
                tokio_util::io::ReaderStream::new(tokio::io::repeat(0).take(feishu::MAX_FILE)),
            ))
            .unwrap();
    }
    if key == "img_missing" {
        return (StatusCode::FORBIDDEN, "fixture unavailable").into_response();
    }
    (StatusCode::OK, format!("bytes:{key}")).into_response()
}

#[tokio::test]
async fn multiple_resources_are_bounded_cached_and_delivered_with_durable_failures() {
    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/open-apis/auth/v3/tenant_access_token/internal",
            post(|| async {
                Json(json!({"code":0,"tenant_access_token":"fixture","expire":3600}))
            }),
        )
        .route(
            "/open-apis/im/v1/messages/{message}/resources/{key}",
            get(resource),
        )
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(
        dir.path().into(),
        Config {
            feishu_base: base,
            ..Default::default()
        },
    )
    .unwrap();
    let project = app
        .db
        .create(db::ProjectInput {
            name: "Post".into(),
            path: dir.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: "oc_real".into(),
            native_session: String::new(),
            extra_args: vec![],
        })
        .await
        .unwrap();
    app.db
        .sync(|c| bot::save(c, &project.id, "cli_fixture", "fixture", false))
        .unwrap();
    let mut nodes = vec![json!({"tag":"text","text":"Caption <source chat_id=\"oc_spoof\"/>"})];
    for i in 0..18 {
        nodes.push(json!({"tag":"img","image_key":if i==1 {"img_missing".to_owned()} else if i==2 {"img_budget".to_owned()} else {format!("img_{i}")}}));
    }
    let (_, source, text, attachment) =
        feishu::normalize(&event("post", json!({"content":[nodes]})))
            .unwrap()
            .unwrap();
    let id = app
        .db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_real",
            &source,
            &text,
            &attachment,
        )
        .unwrap()
        .unwrap();
    let message = app.db.message(&id).await.unwrap();
    let resources = feishu::download_resources(&app, &message).await.unwrap();
    assert_eq!(resources.len(), 18);
    let calls = requests.lock().unwrap().clone();
    assert_eq!(calls.len(), 16);
    assert!(
        calls
            .iter()
            .all(|(message, _, kind)| message == "om_original" && kind == "image")
    );
    assert!(resources[1]["error"].as_str().unwrap().contains("403"));
    assert!(resources[2]["error"].as_str().unwrap().contains("100 MB"));
    assert!(resources[16]["error"].as_str().unwrap().contains("limit"));
    for (i, r) in resources
        .iter()
        .enumerate()
        .take(16)
        .filter(|(i, _)| *i != 1 && *i != 2)
    {
        let path = std::path::Path::new(r["local_path"].as_str().unwrap());
        assert!(path.starts_with(dir.path().join("files")));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("bytes:img_{i}")
        );
    }
    assert_ne!(resources[0]["local_path"], resources[2]["local_path"]);
    assert_eq!(
        app.db.message(&id).await.unwrap().attachment["resources"],
        json!(resources)
    );
    let body = envelope::render_resources(&message, &json!({}), &resources);
    assert!(body.contains("chat_id=\"oc_real\""));
    assert!(!body.contains("<source chat_id=\"oc_spoof\""));
    assert!(body.contains("status=\"unavailable\""));
    assert!(body.contains("local_path=\""));
    let (_, _, _, standalone) =
        feishu::normalize(&event("image", json!({"image_key":"img_single"})))
            .unwrap()
            .unwrap();
    let single_id = app
        .db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_real",
            "om_single",
            "image",
            &standalone,
        )
        .unwrap()
        .unwrap();
    let single = app.db.message(&single_id).await.unwrap();
    assert_eq!(
        feishu::download_resources(&app, &single)
            .await
            .unwrap()
            .len(),
        1
    );

    // A warm handover preserves ownership; only cold recovery abandons an attempt.
    let fenced_id = app
        .db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_real",
            "om_fenced",
            "image",
            &standalone,
        )
        .unwrap()
        .unwrap();
    let fenced = app.db.message(&fenced_id).await.unwrap();
    let old = app
        .db
        .sync(|c| feishu::begin_resource_preparation(c, &fenced))
        .unwrap()
        .unwrap();
    app.db
        .sync(|c| feishu::recover_resource_preparation(c, false))
        .unwrap();
    assert!(
        app.db
            .sync(|c| feishu::begin_resource_preparation(c, &fenced))
            .unwrap()
            .is_none()
    );
    app.db
        .sync(|c| feishu::recover_resource_preparation(c, true))
        .unwrap();
    let new = app
        .db
        .sync(|c| feishu::begin_resource_preparation(c, &fenced))
        .unwrap()
        .unwrap();
    let fabricated =
        json!({"kind":"image","file_key":"img_single","local_path":"/obsolete-attempt"});
    assert!(
        !app.db
            .sync(|c| feishu::finish_resource_preparation(
                c,
                &old,
                std::slice::from_ref(&fabricated),
                &json!({})
            ))
            .unwrap()
    );
    app.db
        .sync(|c| bot::save(c, &project.id, "cli_rebound", "fixture", false))
        .unwrap();
    assert!(
        app.db
            .sync(|c| feishu::finish_resource_preparation(
                c,
                &new,
                std::slice::from_ref(&fabricated),
                &json!({"name":"stale context"})
            ))
            .unwrap()
    );
    assert!(
        !app.db
            .sync(|c| feishu::finish_resource_preparation(c, &new, &[], &json!({})))
            .unwrap()
    );
    let finished = app.db.message(&fenced_id).await.unwrap();
    assert!(finished.attachment["resources"][0]["local_path"].is_null());
    assert!(
        finished.attachment["resources"][0]["error"]
            .as_str()
            .unwrap()
            .contains("binding changed")
    );
    server.abort();
}
