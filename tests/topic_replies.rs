use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use herdr_feishu_bridge::{App, bot, config::Config, db, envelope, feishu};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};

#[derive(Clone, Default)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    mode: Arc<AtomicU8>,
}
async fn create(State(fixture): State<Fixture>, Json(body): Json<Value>) -> Json<Value> {
    fixture
        .requests
        .lock()
        .unwrap()
        .push(json!({"create":body}));
    Json(json!({"code":0,"data":{"message_id":"om_new","chat_id":"oc_private"}}))
}
async fn reply(
    State(fixture): State<Fixture>,
    Path(anchor): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    fixture
        .requests
        .lock()
        .unwrap()
        .push(json!({"anchor":anchor,"body":body}));
    match fixture.mode.load(Ordering::SeqCst) {
        1 => Json(json!({"code":230071,"msg":"Chat does not support reply in thread"}))
            .into_response(),
        2 => (StatusCode::BAD_GATEWAY, "not JSON").into_response(),
        3 => Json(json!({"code":0,"data":{"message_id":"om_accepted","chat_id":"oc_private"}}))
            .into_response(),
        _ => Json(
            json!({"code":0,"data":{"message_id":format!("om_sent_{anchor}"),"chat_id":"oc_private",
            "thread_id":format!("omt_{anchor}"),"root_id":anchor,"parent_id":anchor}}),
        )
        .into_response(),
    }
}
async fn setup() -> (
    tempfile::TempDir,
    Arc<App>,
    db::Project,
    Fixture,
    tokio::task::JoinHandle<()>,
) {
    let fixture = Fixture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/open-apis/auth/v3/tenant_access_token/internal",
            post(|| async {
                Json(json!({"code":0,"tenant_access_token":"fixture","expire":3600}))
            }),
        )
        .route("/open-apis/im/v1/messages", post(create))
        .route("/open-apis/im/v1/messages/{anchor}/reply", post(reply))
        .route(
            "/open-apis/im/v1/files",
            post(|| async { Json(json!({"code":0,"data":{"file_key":"file_fixture"}})) }),
        )
        .with_state(fixture.clone());
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
            name: "Topics".into(),
            path: dir.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: "oc_private".into(),
            native_session: String::new(),
            extra_args: vec![],
        })
        .await
        .unwrap();
    app.db
        .sync(|c| bot::save(c, &project.id, "cli_topics", "fixture", false))
        .unwrap();
    (dir, app, project, fixture, server)
}
fn incoming(app: &App, project: &str, id: &str, chat: &str, kind: &str) {
    app.db
        .ingest_bot(
            project,
            "cli_topics",
            chat,
            id,
            "Reply here",
            &json!({
                "source":{"chat_id":chat,"chat_type":kind,"sender":{"open_id":"ou_person"}}
            }),
        )
        .unwrap();
}

#[tokio::test]
async fn explicit_anchors_keep_concurrent_text_and_file_replies_in_the_selected_discussion() {
    let (dir, app, project, fixture, server) = setup().await;
    incoming(&app, &project.id, "om_original", "oc_private", "p2p");
    incoming(&app, &project.id, "om_newer", "oc_elsewhere", "group");
    let path = dir.path().join("report.txt");
    std::fs::write(&path, "A real attachment").unwrap();
    let (text, file) = tokio::join!(
        app.outbox_to(
            &project.id,
            "text",
            "Earlier answer",
            json!({"reply_to_message_id":"om_original"}),
            Some("ou_person")
        ),
        app.outbox_to(
            &project.id,
            "file",
            "",
            json!({"reply_to_message_id":"om_newer","path":path}),
            Some("oc_elsewhere")
        ),
    );
    let text = text.unwrap();
    let file = file.unwrap();
    assert_eq!(text.status, "sent", "{}", text.error);
    assert_eq!(file.status, "sent", "{}", file.error);
    app.outbox_to(
        &project.id,
        "text",
        "Earlier answer",
        json!({"reply_to_message_id":"om_original"}),
        Some("ou_person"),
    )
    .await
    .unwrap();
    assert!(
        app.outbox_to(
            &project.id,
            "text",
            "Earlier answer",
            json!({"reply_to_message_id":"om_newer"}),
            Some("ou_person")
        )
        .await
        .is_err()
    );
    assert!(
        app.outbox_to(
            &project.id,
            "wrong-destination",
            "No",
            json!({"reply_to_message_id":"om_newer"}),
            Some("ou_person")
        )
        .await
        .is_err()
    );
    assert!(
        app.outbox_to(
            &project.id,
            "body-forgery",
            "No",
            json!({"reply_to_message_id":"om_forged"}),
            Some("oc_private")
        )
        .await
        .is_err()
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request["body"]["reply_in_thread"], true);
        assert!(request.get("create").is_none());
        match request["body"]["msg_type"].as_str().unwrap() {
            "post" => assert_eq!(request["anchor"], "om_original"),
            "file" => assert_eq!(request["anchor"], "om_newer"),
            other => panic!("Unexpected type: {other}"),
        }
    }
    server.abort();
}

#[tokio::test]
async fn source_thread_context_and_outbound_receipts_are_usable_without_a_discussion_map() {
    let (_dir, app, project, fixture, server) = setup().await;
    let event = json!({"header":{"event_type":"im.message.receive_v1"},"event":{
        "sender":{"sender_type":"user","sender_id":{"open_id":"ou_person"}},
        "message":{"message_id":"om_child","chat_id":"oc_private","chat_type":"p2p",
            "thread_id":"omt_existing","root_id":"om_root","parent_id":"om_root",
            "message_type":"text","content":"{\"text\":\"Follow-up\"}"}
    }});
    let (chat, id, content, attachment) = feishu::normalize(&event).unwrap().unwrap();
    let key = app
        .db
        .ingest_bot(&project.id, "cli_topics", &chat, &id, &content, &attachment)
        .unwrap()
        .unwrap();
    let message = app.db.message(&key).await.unwrap();
    assert_eq!(message.attachment["source"]["thread_id"], "omt_existing");
    let xml = envelope::render(&message, &json!({}), None);
    assert!(xml.contains("thread_id=\"omt_existing\""));
    app.outbox_to(
        &project.id,
        "root-reply",
        "Root reply",
        json!({"reply_to_message_id":"om_root"}),
        Some("oc_private"),
    )
    .await
    .unwrap();
    app.outbox_to(
        &project.id,
        "follow-up",
        "Next reply",
        json!({"reply_to_message_id":"om_sent_om_root"}),
        Some("oc_private"),
    )
    .await
    .unwrap();
    assert_eq!(
        fixture.requests.lock().unwrap()[1]["anchor"],
        "om_sent_om_root"
    );
    // A newer bot binding cannot reuse the former bot's source metadata.
    app.db
        .sync(|c| bot::save(c, &project.id, "cli_replacement", "fixture", false))
        .unwrap();
    assert!(
        app.outbox_to(
            &project.id,
            "wrong-bot",
            "No",
            json!({"reply_to_message_id":"om_root"}),
            Some("oc_private")
        )
        .await
        .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn independent_messages_and_pre_upgrade_request_retries_have_no_extra_root_or_null_mismatch()
{
    let (_dir, app, project, fixture, server) = setup().await;
    let legacy = app
        .db
        .enqueue_out(
            &project.id,
            "legacy",
            "Original",
            json!({
                "to":"oc_private","bot_app_id":"cli_topics"
            }),
        )
        .await
        .unwrap();
    let again = app
        .outbox_to(
            &project.id,
            "legacy",
            "Original",
            json!({"reply_to_message_id":null}),
            Some("oc_private"),
        )
        .await
        .unwrap();
    assert_eq!(legacy.id, again.id);
    assert_eq!(again.status, "sent");
    app.outbox_to(
        &project.id,
        "legacy",
        "Original",
        json!({}),
        Some("oc_private"),
    )
    .await
    .unwrap();
    app.outbox_to(
        &project.id,
        "independent",
        "Scheduled notice",
        json!({}),
        Some("oc_private"),
    )
    .await
    .unwrap();
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.get("create").is_some())
    );
    assert!(
        requests
            .iter()
            .all(|request| request["create"]["msg_type"] == "post")
    );
    server.abort();
}

#[tokio::test]
async fn unsupported_uncertain_and_accepted_without_thread_never_fall_back_or_automatically_repeat()
{
    let (_dir, app, project, fixture, server) = setup().await;
    incoming(&app, &project.id, "om_original", "oc_private", "p2p");
    for (mode, expected) in [(1, "failed"), (2, "uncertain"), (3, "uncertain")] {
        fixture.mode.store(mode, Ordering::SeqCst);
        let request = format!("case-{mode}");
        let result = app
            .outbox_to(
                &project.id,
                &request,
                "Reply",
                json!({"reply_to_message_id":"om_original"}),
                Some("oc_private"),
            )
            .await
            .unwrap();
        assert_eq!(result.status, expected, "{}", result.error);
        if mode == 3 {
            assert_eq!(result.attachment["sent_message_id"], "om_accepted");
        }
        app.outbox_to(
            &project.id,
            &request,
            "Reply",
            json!({"reply_to_message_id":"om_original"}),
            Some("oc_private"),
        )
        .await
        .unwrap();
    }
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request["anchor"] == "om_original")
    );
    server.abort();
}
