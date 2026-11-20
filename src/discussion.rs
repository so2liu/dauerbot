//! Scope anchorless results and decisions to one work occurrence and recipient.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) fn origin(
    c: &Connection,
    project: &str,
    event: &str,
    app_id: &str,
    to: &str,
    attachment: &mut Value,
) -> Result<()> {
    let event = crate::work::event(c, project, event)?;
    let id = if event.source == "management" {
        event.payload["origin_event_id"]
            .as_str()
            .unwrap_or(&event.id)
    } else {
        &event.id
    };
    let origin = if id == event.id {
        &event
    } else {
        &crate::work::event(c, project, id)?
    };
    if origin.source == "timer" {
        if let Some(target) = origin.payload["to"].as_str().filter(|to| !to.is_empty()) {
            ensure!(
                target == to,
                "The scheduled work specifies a different recipient"
            );
        }
        ensure!(
            origin.payload["purpose"] != "internal" || attachment["discussion_intent"] != "result",
            "An internal follow-up cannot start a user result discussion"
        );
    }
    let input: Option<(String, String)> = c
        .query_row(
            "SELECT m.source_id,m.attachment FROM work_events e JOIN messages m ON m.id=e.message_id
             WHERE e.id=? AND e.project_id=? AND m.direction='in'",
            params![id, project],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let inbound = input
        .map(|(anchor, raw)| -> Result<_> { Ok((anchor, serde_json::from_str::<Value>(&raw)?)) })
        .transpose()?;
    attachment["discussion_scope"] = json!(id);
    if let Some((anchor, input)) =
        inbound.filter(|(_, input)| input["source"]["chat_id"].is_string())
    {
        ensure!(
            input["bot_app_id"].as_str() == Some(app_id),
            "The source belongs to another bot"
        );

        let source = &input["source"];
        let requester = source["sender"]["open_id"].as_str();
        ensure!(
            source["chat_id"].as_str() == Some(to)
                || (source["chat_type"] == "p2p" && requester == Some(to)),
            "The source discussion belongs to another recipient"
        );
        if attachment["discussion_intent"] == "progress" {
            let supplied = attachment["reply_to_message_id"].as_str();
            ensure!(
                supplied.is_none()
                    || supplied == Some(anchor.as_str())
                    || supplied == source["root_id"].as_str()
                    || supplied == source["parent_id"].as_str(),
                "Progress must reply to the selected source topic"
            );
        }
        if attachment["reply_to_message_id"].is_null() {
            attachment["reply_to_message_id"] = json!(anchor);
        }
        if attachment["discussion_intent"] == "decision" {
            attachment["discussion_mention"] = json!(requester);
        }
    } else if attachment["discussion_intent"] == "decision" && to.starts_with("ou_") {
        attachment["discussion_mention"] = json!(to);
    }
    Ok(())
}

pub(crate) fn root(
    c: &Connection,
    project: &str,
    app_id: &str,
    to: &str,
    scope: &str,
) -> Result<Option<String>> {
    let previous: Option<(String, Option<String>)> = c
        .query_row(
            "SELECT status,json_extract(attachment,'$.sent_message_id')
             FROM messages WHERE project_id=? AND direction='out'
              AND json_extract(attachment,'$.bot_app_id')=?
              AND json_extract(attachment,'$.to')=?
              AND json_extract(attachment,'$.discussion_scope')=?
              AND json_extract(attachment,'$.discussion_root')=1
              AND status IN ('sending','sent','uncertain')
              ORDER BY rowid LIMIT 1",
            params![project, app_id, to, scope],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match previous {
        Some((status, root)) if status == "sent" => Ok(Some(
            root.context("The saved discussion root has no message ID")?,
        )),
        Some(_) => anyhow::bail!(
            "The discussion root delivery is uncertain; inspect it before sending another"
        ),
        None => Ok(None),
    }
}

pub(crate) fn migrate(c: &Connection, recover: bool) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS discussion_progress (
          project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          app_id TEXT NOT NULL, recipient TEXT NOT NULL, scope TEXT NOT NULL,
          anchor TEXT NOT NULL, message_id TEXT NOT NULL DEFAULT '', fallback INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY(project_id,app_id,recipient,scope));
         CREATE TABLE IF NOT EXISTS discussion_progress_requests (
          project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          request_id TEXT NOT NULL, app_id TEXT NOT NULL, recipient TEXT NOT NULL, scope TEXT NOT NULL,
          anchor TEXT NOT NULL, content TEXT NOT NULL, status TEXT NOT NULL,
          message_id TEXT NOT NULL DEFAULT '',
          PRIMARY KEY(project_id,request_id));",
    )?;
    if recover {
        c.execute(
            "UPDATE discussion_progress_requests SET status='uncertain' WHERE status='sending'",
            [],
        )?;
    }
    Ok(())
}

#[derive(Clone)]
struct ProgressRequest {
    project: String,
    app_id: String,
    request: String,
    content: String,
    to: String,
    scope: String,
    anchor: String,
}

pub(crate) async fn progress(
    app: &crate::App,
    project: String,
    app_id: String,
    request: String,
    content: String,
    attachment: Value,
) -> Result<Value> {
    let _guard = app.progress_lock.lock().await;
    let to = attachment["to"].as_str().context("Provide a recipient")?;
    let scope = attachment["discussion_scope"]
        .as_str()
        .context("Progress requires a trusted work occurrence")?;
    let anchor = attachment["reply_to_message_id"]
        .as_str()
        .context("Progress requires an existing trusted topic")?;
    ensure!(
        !content.trim().is_empty() && content.len() <= 24_000,
        "Progress must contain 1 to 24000 bytes"
    );
    ensure!(attachment["path"].is_null(), "Progress supports text only");
    let data = ProgressRequest {
        project,
        app_id,
        request,
        content,
        to: to.into(),
        scope: scope.into(),
        anchor: anchor.into(),
    };
    let read = data.clone();
    let existing = app.db.call(move |c| {
        let ProgressRequest {project,app_id,request,content,to,scope,anchor} = &read;
        crate::db::validate_out_anchor(c, project, app_id, to, anchor)?;
        let has_topic: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE project_id=? AND json_extract(attachment,'$.bot_app_id')=?
              AND ((direction='in' AND (source_id=? OR json_extract(attachment,'$.source.root_id')=?
                 OR json_extract(attachment,'$.source.parent_id')=?) AND COALESCE(json_extract(attachment,'$.source.thread_id'),'')<>'')
              OR (direction='out' AND status='sent' AND json_extract(attachment,'$.sent_message_id')=?
                 AND COALESCE(json_extract(attachment,'$.sent_thread_id'),'')<>'')))",
            params![project,app_id,anchor,anchor,anchor,anchor], |row| row.get(0),
        )?;
        ensure!(has_topic, "Progress requires a confirmed existing topic");
        let old = c.query_row(
            "SELECT app_id,recipient,scope,anchor,content,status,message_id FROM discussion_progress_requests WHERE project_id=? AND request_id=?",
            params![project,request], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?,
                row.get::<_,String>(3)?, row.get::<_,String>(4)?, row.get::<_,String>(5)?, row.get::<_,String>(6)?)),
        ).optional()?;
        if let Some((saved_app,saved_to,saved_scope,saved_anchor,saved_content,status,mid)) = old {
            ensure!(saved_app == *app_id && saved_to == *to && saved_scope == *scope && saved_anchor == *anchor && saved_content == *content,
                "request_id was used with different progress");
            return Ok(Some(json!({"status":status,"message_id":mid})));
        }
        Ok(None)
    }).await?;
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let binding = app.db.bot(&data.project).await?;
    ensure!(binding.app_id == data.app_id, "The project bot changed");
    let token = crate::feishu::token(app, &binding).await?;
    let submit = data.clone();
    let (saved_anchor, message_id, fallback) = app.db.call(move |c| {
        let ProgressRequest {project,app_id,request,content,to,scope,anchor} = &submit;
        let tx = c.unchecked_transaction()?;
        crate::manager::validate_outbox(&tx, project, &attachment)?;
        let uncertain: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM discussion_progress_requests WHERE project_id=? AND app_id=?
             AND recipient=? AND scope=? AND status IN ('sending','uncertain'))",
            params![project,app_id,to,scope], |row| row.get(0),
        )?;
        ensure!(!uncertain, "The previous progress delivery is uncertain");
        tx.execute(
            "INSERT OR IGNORE INTO discussion_progress(project_id,app_id,recipient,scope,anchor) VALUES(?,?,?,?,?)",
            params![project,app_id,to,scope,anchor],
        )?;
        let saved = tx.query_row(
            "SELECT anchor,message_id,fallback FROM discussion_progress WHERE project_id=? AND app_id=? AND recipient=? AND scope=?",
            params![project,app_id,to,scope], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,bool>(2)?)),
        )?;
        tx.execute(
            "INSERT INTO discussion_progress_requests(project_id,request_id,app_id,recipient,scope,anchor,content,status)
             VALUES(?,?,?,?,?,?,?,'sending')",
            params![project,request,app_id,to,scope,anchor,content],
        )?;
        tx.commit()?;
        Ok(saved)
    }).await?;
    let mut effect =
        crate::feishu::progress_message(app, &token, &saved_anchor, &message_id, &data.content)
            .await;
    let mut did_fallback = false;
    if !message_id.is_empty()
        && matches!(effect, Ok(crate::feishu::ProgressDelivery::Uneditable))
        && !fallback
    {
        tracing::warn!(
            "Feishu no longer permits editing the progress message; replying in its existing topic"
        );
        did_fallback = true;
        effect =
            crate::feishu::progress_message(app, &token, &saved_anchor, "", &data.content).await;
    }
    let (status, receipt, error) = match effect {
        Ok(crate::feishu::ProgressDelivery::Sent(id)) => ("sent", id, None),
        Ok(crate::feishu::ProgressDelivery::Uneditable) => (
            "failed",
            String::new(),
            Some("Feishu cannot edit the progress message".into()),
        ),
        Ok(crate::feishu::ProgressDelivery::Rejected(error)) => {
            ("failed", String::new(), Some(error))
        }
        Err(error) => ("uncertain", String::new(), Some(error.to_string())),
    };
    if let Some(error) = &error {
        tracing::warn!(%error, %status, "Feishu progress update failed");
    }
    let delivered_id = receipt.clone();
    app.db.call(move |c| {
        let ProgressRequest {project,app_id,request,to,scope,..} = data;
        let tx = c.unchecked_transaction()?;
        if status == "sent" || did_fallback {
            tx.execute(
                "UPDATE discussion_progress SET message_id=CASE WHEN ?='' THEN message_id ELSE ? END,fallback=MAX(fallback,?)
                 WHERE project_id=? AND app_id=? AND recipient=? AND scope=?",
                params![delivered_id,delivered_id,did_fallback,project,app_id,to,scope],
            )?;
        }
        tx.execute(
            "UPDATE discussion_progress_requests SET status=?,message_id=? WHERE project_id=? AND request_id=?",
            params![status,delivered_id,project,request],
        )?;
        tx.commit()?;
        Ok(())
    }).await?;
    Ok(json!({"status":status,"message_id":receipt,"error":error}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, config::Config, db};
    use axum::{Json, Router, extract::Request, routing::any};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn per_occurrence_roots_and_in_place_progress_share_no_outbox_state() {
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = http.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let router = Router::new().fallback(any(move |request: Request| {
            let recorded = recorded.clone();
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let bytes = axum::body::to_bytes(request.into_body(), 128 * 1024).await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                if path.contains("/auth/") {
                    return Json(json!({"code":0,"tenant_access_token":"fixture-token"}));
                }
                let mut requests = recorded.lock().unwrap();
                let index = requests.len() + 1;
                requests.push((method.to_string(), path.clone(), body));
                if index == 5 {
                    return Json(json!({"code":230075}));
                }
                Json(json!({"code":0,"data":{"message_id":format!("om_{index}"),"chat_id":"oc_fixture","thread_id":"omt_fixture"}}))
            }
        }));
        let server = tokio::spawn(async { axum::serve(http, router).await.unwrap() });
        let temp = tempfile::tempdir().unwrap();
        let app = App::new(
            temp.path().join("store"),
            Config {
                feishu_base: format!("http://{addr}"),
                ..Config::default()
            },
        )
        .unwrap();
        let p = app
            .db
            .create(db::ProjectInput {
                name: "Discussion fixture".into(),
                path: temp.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: "oc_fixture".into(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        app.db
            .sync(|c| crate::bot::save(c, &p.id, "cli_fixture", "fixture-secret", false))
            .unwrap();
        app.db.ingest_bot(&p.id, "cli_fixture", "oc_fixture", "om_request", "Please complete the work",
            &json!({"source":{"chat_id":"oc_fixture","chat_type":"group","thread_id":"omt_user","sender":{"open_id":"ou_requester"}}})).unwrap();
        let (mut attachment, origin_id) = app
            .db
            .sync(|c| {
                crate::work::sync_inbound(c)?;
                let event: String = c.query_row(
                    "SELECT id FROM work_events WHERE project_id=? AND source_id='om_request'",
                    [&p.id],
                    |r| r.get(0),
                )?;
                let mut attachment = json!({"to":"oc_fixture","discussion_intent":"progress"});
                origin(
                    c,
                    &p.id,
                    &event,
                    "cli_fixture",
                    "oc_fixture",
                    &mut attachment,
                )?;
                assert_eq!(attachment["reply_to_message_id"], "om_request");
                Ok((attachment, event))
            })
            .unwrap();
        for (scope, request) in [
            ("schedule-one", "first"),
            ("schedule-one", "follow-up"),
            ("schedule-two", "independent"),
        ] {
            let out = app.db.sync(|c| db::enqueue_out_request(c,&p.id,"cli_fixture",request,"Requested result",
                json!({"to":"oc_fixture","discussion_intent":"result","discussion_scope":scope}))).unwrap();
            app.send_out(&out).await.unwrap();
            assert_eq!(app.db.message(&out).await.unwrap().status, "sent");
        }
        for (request, content) in [
            ("progress-one", "Started meaningful phase"),
            ("progress-two", "Blocked phase resolved"),
            ("progress-three", "Evidence gathered"),
        ] {
            assert_eq!(
                progress(
                    &app,
                    p.id.clone(),
                    "cli_fixture".into(),
                    request.into(),
                    content.into(),
                    attachment.clone()
                )
                .await
                .unwrap()["status"],
                "sent"
            );
        }
        assert_eq!(
            progress(
                &app,
                p.id.clone(),
                "cli_fixture".into(),
                "progress-two".into(),
                "Blocked phase resolved".into(),
                attachment.clone()
            )
            .await
            .unwrap()["message_id"],
            "om_6"
        );
        attachment["discussion_scope"] = json!(origin_id);
        app.db
            .sync(|c| {
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE direction='out'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    3
                );
                assert_eq!(
                    root(c, &p.id, "cli_fixture", "oc_fixture", "schedule-one")?.as_deref(),
                    Some("om_1")
                );
                Ok(())
            })
            .unwrap();
        let requests = calls.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|(method, path, _)| (method.as_str(), path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("POST", "/open-apis/im/v1/messages"),
                ("POST", "/open-apis/im/v1/messages/om_1/reply"),
                ("POST", "/open-apis/im/v1/messages"),
                ("POST", "/open-apis/im/v1/messages/om_request/reply"),
                ("PUT", "/open-apis/im/v1/messages/om_4"),
                ("POST", "/open-apis/im/v1/messages/om_request/reply"),
                ("PUT", "/open-apis/im/v1/messages/om_6"),
            ]
        );
        assert_eq!(requests[1].2["reply_in_thread"], true);
        server.abort();
    }
}
