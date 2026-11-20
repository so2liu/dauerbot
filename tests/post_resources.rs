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
    if key == "img_not_image" {
        return (StatusCode::OK, "not image pixels").into_response();
    }
    if query["type"] == "file" {
        return (StatusCode::OK, "document bytes").into_response();
    }
    let mut bytes = include_bytes!("fixtures/input.png").to_vec();
    bytes.extend_from_slice(key.as_bytes());
    (StatusCode::OK, bytes).into_response()
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
        assert!(path.starts_with(dir.path().canonicalize().unwrap().join("files")));
        assert_eq!(
            std::fs::read(path).unwrap(),
            [
                include_bytes!("fixtures/input.png").as_slice(),
                format!("img_{i}").as_bytes()
            ]
            .concat()
        );
        assert!(
            path.starts_with(
                dir.path()
                    .canonicalize()
                    .unwrap()
                    .join("files")
                    .join(&project.id)
            )
        );
        assert_eq!(r["cache_project_id"], project.id);
        assert_eq!(r["cache_message_id"], message.id);
        assert_eq!(r["media_type"], "image/png");
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

    let (_, _, _, bad_attachment) =
        feishu::normalize(&event("image", json!({"image_key":"img_not_image"})))
            .unwrap()
            .unwrap();
    let bad_id = app
        .db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_real",
            "om_bad_type",
            "bad image",
            &bad_attachment,
        )
        .unwrap()
        .unwrap();
    let bad = app.db.message(&bad_id).await.unwrap();
    let rejected = feishu::download_resources(&app, &bad).await.unwrap();
    assert!(rejected[0]["local_path"].is_null());
    assert!(
        rejected[0]["error"]
            .as_str()
            .unwrap()
            .contains("unsupported media type")
    );
    assert!(
        std::fs::read_dir(dir.path().join("files").join(&project.id))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part"))
    );
    let (_, _, _, file_attachment) = feishu::normalize(&event(
        "file",
        json!({"file_key":"doc_fixture","file_name":"note.txt"}),
    ))
    .unwrap()
    .unwrap();
    let file_id = app
        .db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_real",
            "om_document",
            "document",
            &file_attachment,
        )
        .unwrap()
        .unwrap();
    let document = app.db.message(&file_id).await.unwrap();
    let documents = feishu::download_resources(&app, &document).await.unwrap();
    assert_eq!(documents[0]["kind"], "file");
    assert!(documents[0]["media_type"].is_null());
    assert_eq!(
        std::fs::read(documents[0]["local_path"].as_str().unwrap()).unwrap(),
        b"document bytes"
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|(message, key, kind)| message == "om_document"
                && key == "doc_fixture"
                && kind == "file")
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

#[derive(Clone, Default)]
struct PreparationRace {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    downloads: Arc<Mutex<Vec<String>>>,
    model_requests: Arc<Mutex<Vec<Value>>>,
}

#[tokio::test]
async fn manager_routing_before_or_during_real_download_does_not_strand_images() {
    use herdr_feishu_bridge::{manager, work};
    use std::time::Duration;
    async fn image(
        State(state): State<PreparationRace>,
        Path((message, _)): Path<(String, String)>,
    ) -> Response {
        state.downloads.lock().unwrap().push(message);
        state.started.notify_one();
        state.release.notified().await;
        (
            StatusCode::OK,
            include_bytes!("fixtures/input.png").as_slice(),
        )
            .into_response()
    }
    async fn model(State(state): State<PreparationRace>, Json(body): Json<Value>) -> Json<Value> {
        let image_count = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|part| part["type"] == "image_url")
            .count();
        assert_eq!(
            image_count, 1,
            "The real manager request must contain downloaded pixels"
        );
        state.model_requests.lock().unwrap().push(body);
        Json(
            json!({"id":"fixture","object":"chat.completion","created":1,"model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"Image inspected in the fixture."},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
        )
    }
    let state = PreparationRace::default();
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
            get(image),
        )
        .route("/v1/chat/completions", post(model))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let app = App::new(
        directory.path().into(),
        Config {
            feishu_base: base.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    let project = app
        .db
        .create(db::ProjectInput {
            name: "Manager image race".into(),
            path: directory.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: "oc_real".into(),
            native_session: String::new(),
            extra_args: vec![],
        })
        .await
        .unwrap();
    app.db
        .sync(|c| {
            bot::save(c, &project.id, "cli_fixture", "fixture", false)?;
            manager::save_provider(
                c,
                manager::ConfigInput {
                    base_url: format!("{base}/v1"),
                    model: "fixture".into(),
                    api_key: Some("fixture".into()),
                    clear_api_key: false,
                    expected_revision: 0,
                    supports_images: Some(true),
                },
            )?;
            Ok(())
        })
        .unwrap();
    let (_, _, caption, attachment) =
        feishu::normalize(&event("image", json!({"image_key":"img_race"})))
            .unwrap()
            .unwrap();
    let first = app
        .db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_real",
            "om_before_begin",
            &caption,
            &attachment,
        )
        .unwrap()
        .unwrap();
    app.db.sync(|c| work::sync_inbound(c).map(|_| ())).unwrap();
    assert_eq!(app.db.message(&first).await.unwrap().status, "acknowledged");
    let resources = tokio::spawn(feishu::resource_worker(app.clone()));
    let management = tokio::spawn(manager::run(app.clone()));
    for (index, id) in [first.clone(), String::new()].into_iter().enumerate() {
        let id = if index == 0 {
            id
        } else {
            app.db
                .ingest_bot(
                    &project.id,
                    "cli_fixture",
                    "oc_real",
                    "om_during_download",
                    &caption,
                    &attachment,
                )
                .unwrap()
                .unwrap()
        };
        tokio::time::timeout(Duration::from_secs(5), state.started.notified())
            .await
            .unwrap();
        if index == 1 {
            // Preparation owns a real blocked HTTP download before routing takes ownership.
            assert!(
                app.db.message(&id).await.unwrap().attachment["resource_preparation"].is_string()
            );
            app.db.sync(|c| work::sync_inbound(c).map(|_| ())).unwrap();
        }
        assert_eq!(app.db.message(&id).await.unwrap().status, "acknowledged");
        assert_ne!(
            app.db.message(&id).await.unwrap().attachment["resources_ready"],
            true
        );
        state.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5),async {
            loop {
                let done=app.db.sync(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM manager_inputs i JOIN work_events e ON e.id=i.origin_event_id WHERE e.message_id=? AND i.status='consumed')",[&id],|r|r.get::<_,bool>(0))?)).unwrap();
                if done {break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let message = app.db.message(&id).await.unwrap();
        assert_eq!(message.attachment["resources_ready"], true);
        assert_eq!(
            std::fs::read(
                message.attachment["resources"][0]["local_path"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
            include_bytes!("fixtures/input.png")
        );
        app.db.sync(|c|{
            assert_eq!(c.query_row("SELECT count(*) FROM work_events WHERE message_id=?",[&id],|r|r.get::<_,i64>(0))?,1);
            assert_eq!(c.query_row("SELECT count(*) FROM work_runs WHERE event_id IN(SELECT id FROM work_events WHERE message_id=?)",[&id],|r|r.get::<_,i64>(0))?,0);
            Ok(())
        }).unwrap();
    }
    app.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), resources)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), management)
        .await
        .unwrap()
        .unwrap();
    // Stop cancels the authoritative owner before the worker can discover it.
    let stopped = app
        .db
        .ingest_bot(
            &project.id,
            "cli_fixture",
            "oc_real",
            "om_stopped_image",
            &caption,
            &attachment,
        )
        .unwrap()
        .unwrap();
    app.db.sync(|c| manager::stop(c, &project.id)).unwrap();
    let stopped = app.db.message(&stopped).await.unwrap();
    assert!(
        app.db
            .sync(|c| feishu::begin_resource_preparation(c, &stopped))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        *state.downloads.lock().unwrap(),
        vec!["om_before_begin", "om_during_download"]
    );
    assert_eq!(state.model_requests.lock().unwrap().len(), 2);
    server.abort();
}
