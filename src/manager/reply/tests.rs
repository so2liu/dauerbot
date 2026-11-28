use super::*;
use crate::manager::{self, Context};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

pub(super) fn incoming(
    app: &crate::App,
    project: &str,
    message: &str,
    root: &str,
) -> Result<String> {
    let input = app
        .db
        .ingest_bot(
            project,
            "cli_fixture",
            "oc_fixture",
            message,
            "Please check this request.",
            &json!({"source":{"chat_id":"oc_fixture","chat_type":"p2p",
                "message_type":"text","root_id":root,"parent_id":root,
                "thread_id":format!("omt_{root}")}}),
        )?
        .expect("fixture must be admitted");
    app.db.sync(crate::work::sync_inbound)?;
    Ok(input)
}

pub(super) fn claim(c: &Connection, project: &str, input: &str) -> Result<Context> {
    let (revision, input_revision) = c.query_row(
        "SELECT revision,input_revision FROM manager_config WHERE project_id=?",
        [project],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let run = crate::db::id();
    c.execute(
        "INSERT INTO manager_runs(id,project_id,revision,input_revision,origin_event_id,
          checkpoint,status,owner,created_at) VALUES(?,?,?,?,?,?,'working',?,?)",
        params![
            run,
            project,
            revision,
            input_revision,
            input,
            json!({"channel_id":"feishu:cli_fixture:oc_fixture"}).to_string(),
            manager::owner(),
            crate::db::now()
        ],
    )?;
    manager::own_input(c, &run, input)?;
    c.execute(
        "INSERT INTO manager_run_inputs(run_id,input_id) VALUES(?,?)",
        params![run, input],
    )?;
    Ok(Context {
        project: project.into(),
        run,
        revision,
        input_revision,
    })
}

pub(super) fn outcome(c: &Connection, input: &str) -> Result<(String, Option<String>)> {
    Ok(c.query_row(
        "SELECT state,outbox_id FROM manager_input_outcomes WHERE input_id=?",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

fn send_effect(
    c: &Connection,
    turn: &Context,
    anchor: &str,
    status: &str,
    intent: &str,
) -> Result<String> {
    let effect = crate::db::id();
    let attachment = json!({"to":"oc_fixture","reply_to_message_id":anchor,
        "manager_effect":effect,"discussion_intent":intent});
    let message = crate::db::enqueue_out_request(
        c,
        &turn.project,
        "cli_fixture",
        &effect,
        "Original tool reply",
        attachment,
    )?;
    c.execute(
        "INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status,result)
         VALUES(?,?,?,'send_md',?,?,?)",
        params![
            effect,
            turn.run,
            effect,
            json!({"to":"oc_fixture","reply_to_message_id":anchor,"intent":intent,
                "text":"Original tool reply"})
            .to_string(),
            if status == "uncertain" {
                "uncertain"
            } else {
                "complete"
            },
            json!({"status":status,"outbox_id":message}).to_string()
        ],
    )?;
    c.execute(
        "UPDATE messages SET status=? WHERE id=?",
        params![status, message],
    )?;
    Ok(message)
}

pub(super) fn outbound_count(c: &Connection, project: &str) -> Result<i64> {
    Ok(c.query_row(
        "SELECT count(*) FROM messages WHERE project_id=? AND direction='out'",
        [project],
        |row| row.get(0),
    )?)
}

#[tokio::test]
async fn accepted_effects_suppress_finalize_even_after_a_reclaimed_turn() -> Result<()> {
    for status in ["sent", "uncertain"] {
        let (_dir, app, project) = manager::tests::fixture().await;
        let input = incoming(&app, &project.id, "om_input", "om_root")?;
        app.db.sync(|c| {
            let tx = c.unchecked_transaction()?;
            let first = claim(&tx, &project.id, &input)?;
            let prior_outbox = send_effect(&tx, &first, "om_root", status, "result")?;
            tx.execute(
                "UPDATE manager_runs SET status='superseded' WHERE id=?",
                [&first.run],
            )?;
            let reclaimed = claim(&tx, &project.id, &input)?;
            finish(&tx, &reclaimed, "Do not send another answer.")?;
            assert_eq!(
                outcome(&tx, &input)?,
                ("replied".into(), Some(prior_outbox))
            );
            assert_eq!(outbound_count(&tx, &project.id)?, 1);
            tx.commit()?;
            Ok(())
        })?;
    }
    Ok(())
}

#[tokio::test]
async fn progress_and_replies_in_another_topic_do_not_satisfy_input() -> Result<()> {
    let (_dir, app, project) = manager::tests::fixture().await;
    let other_input = incoming(&app, &project.id, "om_other", "om_other_root")?;
    let input = incoming(&app, &project.id, "om_current", "om_current_root")?;
    app.db.sync(|c| {
        let tx = c.unchecked_transaction()?;
        let turn = claim(&tx, &project.id, &input)?;
        let progress = send_effect(&tx, &turn, "om_current_root", "sent", "progress")?;
        let other_reply = send_effect(&tx, &turn, "om_other_root", "sent", "result")?;
        finish(&tx, &turn, "Final answer for the current topic.")?;
        let (state, reply) = outcome(&tx, &input)?;
        assert_eq!(state, "replied");
        let reply = reply.expect("finalize must journal a reply");
        assert_ne!(reply, progress);
        assert_ne!(reply, other_reply);
        let (text, route) = tx.query_row(
            "SELECT content,attachment FROM messages WHERE id=?",
            [&reply],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        let route: Value = serde_json::from_str(&route)?;
        assert_eq!(text, "Final answer for the current topic.");
        assert_eq!(route["reply_to_message_id"], "om_current_root");
        assert_eq!(route["to"], "oc_fixture");
        assert!(outcome(&tx, &other_input).is_err());
        assert_eq!(outbound_count(&tx, &project.id)?, 3);
        tx.commit()?;
        Ok(())
    })?;
    Ok(())
}

#[tokio::test]
async fn mixed_topic_finalization_does_not_forward_shared_model_text() -> Result<()> {
    let (_dir, app, project) = manager::tests::fixture().await;
    let first = incoming(&app, &project.id, "om_first", "om_first_root")?;
    let second = incoming(&app, &project.id, "om_second", "om_second_root")?;
    app.db.sync(|c| {
        let tx = c.unchecked_transaction()?;
        let turn = claim(&tx, &project.id, &first)?;
        manager::own_input(&tx, &turn.run, &second)?;
        tx.execute(
            "INSERT INTO manager_run_inputs(run_id,input_id) VALUES(?,?)",
            params![turn.run, second],
        )?;
        finish(&tx, &turn, "A result whose intended topic is ambiguous.")?;
        finish(&tx, &turn, "A repeated finalize is a no-op.")?;
        for (input, anchor) in [(&first, "om_first_root"), (&second, "om_second_root")] {
            let (state, outbox) = outcome(&tx, input)?;
            assert_eq!(state, "replied");
            let (content, actual_anchor): (String, String) = tx.query_row(
                "SELECT content,json_extract(attachment,'$.reply_to_message_id')
                 FROM messages WHERE id=?",
                [outbox.expect("safe topic notice must be queued")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(content, "No result was produced for this request.");
            assert_eq!(actual_anchor, anchor);
        }
        assert_eq!(outbound_count(&tx, &project.id)?, 2);
        tx.commit()?;
        Ok(())
    })?;
    Ok(())
}

#[tokio::test]
async fn run_authority_loss_preserves_input_and_prevents_finalize() -> Result<()> {
    for control in ["pause", "stop", "takeover"] {
        let (_dir, app, project) = manager::tests::fixture().await;
        let input = incoming(&app, &project.id, "om_input", "om_root")?;
        app.db.sync(|c| {
            let tx = c.unchecked_transaction()?;
            let turn = claim(&tx, &project.id, &input)?;
            match control {
                "pause" => {
                    tx.execute("UPDATE projects SET paused=1 WHERE id=?", [&project.id])?;
                }
                "stop" => manager::stop(&tx, &project.id)?,
                "takeover" => {
                    tx.execute(
                        "UPDATE projects SET generation='fixture-generation',terminal='fixture-terminal' WHERE id=?",
                        [&project.id],
                    )?;
                    crate::work::set_takeover(
                        &tx, &project.id, "fixture-generation", true, crate::db::now(),
                    )?;
                }
                _ => unreachable!(),
            }
            assert!(finish(&tx, &turn, "Must not send.").is_err());
            assert_eq!(outbound_count(&tx, &project.id)?, 0, "{control}");
            let status: String = tx.query_row(
                "SELECT status FROM manager_inputs WHERE id=?",
                [&input],
                |row| row.get(0),
            )?;
            assert_eq!(status, "processing", "{control}");
            tx.commit()?;
            Ok(())
        })?;
    }
    Ok(())
}

async fn bookkeeping_response(Json(request): Json<Value>) -> Json<Value> {
    let has_tool_result = request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
    let choice = if has_tool_result {
        json!({"role":"assistant","content":"The result is available.", "tool_calls":null})
    } else {
        json!({"role":"assistant","content":null,"tool_calls":[{"id":"status-call",
            "type":"function","function":{"name":"worker_status","arguments":"{}"}}]})
    };
    Json(
        json!({"id":"fixture","object":"chat.completion","created":1,"model":"fixture-manager",
        "choices":[{"index":0,"message":choice,
            "finish_reason":if has_tool_result { "stop" } else { "tool_calls" }}],
        "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    )
}

#[tokio::test]
async fn bookkeeping_turn_delivers_ordinary_final_text_once() -> Result<()> {
    let (_dir, app, project) = manager::tests::fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    app.config.write().await.feishu_base = base.clone();
    app.db.sync(|c| {
        c.execute(
            "UPDATE manager_provider_config SET base_url=? WHERE id=1",
            [format!("{base}/v1")],
        )?;
        Ok(())
    })?;
    let (send, mut replies) = mpsc::unbounded_channel();
    let router = Router::new()
        .route("/v1/chat/completions", post(bookkeeping_response))
        .route(
            "/open-apis/auth/v3/tenant_access_token/internal",
            post(|| async {
                Json(json!({"code":0,"tenant_access_token":"fixture-token","expire":3600}))
            }),
        )
        .route(
            "/open-apis/im/v1/messages/{anchor}/reply",
            post(
                |State(send): State<mpsc::UnboundedSender<(String, Value)>>,
                 Path(anchor): Path<String>,
                 Json(body): Json<Value>| async move {
                    send.send((anchor, body)).unwrap();
                    Json(json!({"code":0,"data":{"message_id":"om_fixture_reply",
                    "chat_id":"oc_fixture","thread_id":"omt_om_root",
                    "root_id":"om_root","parent_id":"om_root"}}))
                },
            ),
        )
        .with_state(send);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let input = incoming(&app, &project.id, "om_input", "om_root")?;
    let manager_task = tokio::spawn(manager::run(app.clone()));

    let verification = async {
        let outbox = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Some(id) = app.db.sync(|c| {
                    use rusqlite::OptionalExtension;
                    Ok(c.query_row(
                        "SELECT outbox_id FROM manager_input_outcomes WHERE input_id=? AND state='replied'",
                        [&input],
                        |row| row.get::<_, String>(0),
                    ).optional()?)
                })? {
                    return Ok::<_, anyhow::Error>(id);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        app.send_out(&outbox).await?;
        app.send_out(&outbox).await?;
        assert_eq!(app.db.message(&outbox).await?.status, "sent");
        let (anchor, body) = tokio::time::timeout(Duration::from_secs(3), replies.recv())
            .await?
            .expect("fixture reply request");
        assert_eq!(anchor, "om_root");
        assert_eq!(body["reply_in_thread"], true);
        let content: Value = serde_json::from_str(body["content"].as_str().unwrap())?;
        assert_eq!(content["zh_cn"]["content"][0][0]["text"], "The result is available.");
        assert!(replies.try_recv().is_err());
        app.db.sync(|c| {
            assert_eq!(outbound_count(c, &project.id)?, 1);
            let (input_status, tool): (String, String) = c.query_row(
                "SELECT i.status,e.name FROM manager_inputs i
                 JOIN manager_input_outcomes o ON o.input_id=i.id
                 JOIN manager_effects e ON e.run_id=o.run_id AND e.name='worker_status'
                 WHERE i.id=?",
                [&input],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(input_status, "consumed");
            assert_eq!(tool, "worker_status");
            Ok(())
        })
    }.await;
    app.shutdown.cancel();
    manager_task.await?;
    server.abort();
    verification
}
