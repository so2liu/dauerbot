use super::{enqueue, has_reply, route, uncertain_send};
use crate::work;
use anyhow::Result;
use rusqlite::{Connection, params};

const RESULT_GRACE_MS: i64 = 15_000;

struct Candidate {
    project: String,
    input: String,
    run: String,
    outstanding: bool,
    uncertain: bool,
    failed: bool,
    cancelled: bool,
    finished: bool,
}

impl Candidate {
    fn message(&self, chinese: bool) -> &'static str {
        let (english, chinese_text) = if self.outstanding {
            (
                "The delegated work did not produce a verified result within the wait period. Its outcome is still pending.",
                "委派任务在等待期限内没有返回可核实的结果，结果仍待确认。",
            )
        } else if self.uncertain {
            (
                "The delegated work's outcome is uncertain. Its attempt was preserved for review.",
                "委派任务的结果无法确认，执行记录已保留供核查。",
            )
        } else if self.failed {
            (
                "The delegated work failed before a verified result was available.",
                "委派任务执行失败，未取得可核实的结果。",
            )
        } else if self.cancelled {
            (
                "The delegated work was cancelled before a verified result was available.",
                "委派任务已取消，未取得可核实的结果。",
            )
        } else if self.finished {
            (
                "The worker finished its attempt. A user-facing result has not been verified yet.",
                "Worker 已结束本次执行，任务结果仍待核实。",
            )
        } else {
            (
                "The delegated work no longer has a verifiable execution record.",
                "委派任务已无可核实的执行记录。",
            )
        };
        if chinese { chinese_text } else { english }
    }
}

/// Reconcile only durable, origin-scoped delegations. The caller owns the same
/// transaction as the outbox insertion and outcome update.
pub(super) fn reconcile(c: &Connection, now: i64) -> Result<()> {
    let candidates = c
        .prepare(
            "SELECT i.project_id,o.input_id,o.run_id,
                COALESCE(MAX(e.status NOT IN ('completed','delivered','uncertain','failed','cancelled','dismissed')),0) AS outstanding,
                COALESCE(MAX(e.status='uncertain'),0),
                COALESCE(MAX(e.status='failed'),0),
                COALESCE(MAX(e.status IN ('cancelled','dismissed')),0),
                COALESCE(MAX(e.status IN ('completed','delivered')),0)
             FROM manager_input_outcomes o JOIN manager_inputs i ON i.id=o.input_id
             JOIN manager_runs r ON r.id=o.run_id AND r.project_id=i.project_id
             JOIN manager_config m ON m.project_id=i.project_id AND m.revision=r.revision
                AND m.input_revision=r.input_revision
             JOIN projects p ON p.id=i.project_id
             JOIN project_bots b ON b.project_id=i.project_id
             LEFT JOIN work_events e ON e.project_id=i.project_id
                AND e.source IN ('management','manager_worker') AND json_valid(e.payload)
                AND json_extract(e.payload,'$.origin_event_id')=i.origin_event_id
                AND json_extract(e.payload,'$.origin_channel_id')=i.channel_id
                AND EXISTS(
                    SELECT 1 FROM manager_effects f JOIN manager_runs owning ON owning.id=f.run_id
                    WHERE f.id=e.source_id AND f.name IN ('delegate','steer_worker')
                      AND owning.project_id=i.project_id
                )
             WHERE o.state='delegated' AND o.outbox_id IS NULL AND i.kind='user'
                AND i.status='consumed' AND r.status='complete' AND p.paused=0
                AND i.channel_id GLOB ('feishu:' || b.app_id || ':oc_*')
                AND NOT EXISTS(SELECT 1 FROM standalone_projects s WHERE s.project_id=p.id)
             GROUP BY o.input_id
             HAVING o.deadline<=?1 OR (COUNT(e.id)>0 AND outstanding=0 AND MAX(e.updated_at)<=?2)
             ORDER BY ROW_NUMBER() OVER (PARTITION BY i.project_id ORDER BY o.deadline,o.input_id),
                      o.deadline,o.input_id LIMIT 32",
        )?
        .query_map(params![now, now.saturating_sub(RESULT_GRACE_MS)], |row| {
            Ok(Candidate {
                project: row.get(0)?,
                input: row.get(1)?,
                run: row.get(2)?,
                outstanding: row.get(3)?,
                uncertain: row.get(4)?,
                failed: row.get(5)?,
                cancelled: row.get(6)?,
                finished: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for candidate in candidates {
        if work::has_project_takeover(c, &candidate.project)? {
            continue;
        }
        let Some(route) = route(c, &candidate.project, &candidate.input)? else {
            continue;
        };
        let (state, outbox) = match has_reply(c, &candidate.project, &candidate.input, &route)? {
            Some(existing) => ("replied", Some(existing)),
            None if uncertain_send(c, &candidate.input, &route)? => ("unreplied", None),
            None => (
                "replied",
                Some(enqueue(
                    c,
                    &candidate.project,
                    &candidate.input,
                    &candidate.run,
                    &route,
                    candidate.message(route.chinese),
                )?),
            ),
        };
        c.execute(
            "UPDATE manager_input_outcomes SET state=?,outbox_id=?
             WHERE input_id=? AND state='delegated' AND outbox_id IS NULL",
            params![state, outbox, candidate.input],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::{
        self,
        reply::tests::{claim, incoming, outbound_count, outcome},
    };
    use axum::{
        Json, Router,
        extract::{Path, State},
        routing::post,
    };
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    fn delegate(c: &Connection, project: &str, input: &str) -> Result<String> {
        let turn = claim(c, project, input)?;
        let effect = crate::db::id();
        c.execute(
            "INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status)
                VALUES(?,?,?,'delegate','{}','complete')",
            params![effect, turn.run, effect],
        )?;
        let event = manager::test_delegate(c, project, &effect, Some(input))?;
        c.execute(
            "UPDATE work_events SET payload=json_set(payload,'$.origin_channel_id',
                    'feishu:cli_fixture:oc_fixture') WHERE id=?",
            [&event],
        )?;
        super::super::finish(c, &turn, "The worker is investigating.")?;
        manager::close_turn(c, &turn)?;
        assert_eq!(outcome(c, input)?, ("delegated".into(), None));
        Ok(event)
    }

    #[tokio::test]
    async fn terminal_and_timeout_receipts_send_once_to_each_original_topic() -> Result<()> {
        let (_dir, app, project) = manager::tests::fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        app.config.write().await.feishu_base = format!("http://{}", listener.local_addr()?);
        let (capture, mut deliveries) = mpsc::unbounded_channel();
        let fixture = Router::new()
            .route(
                "/open-apis/auth/v3/tenant_access_token/internal",
                post(|| async {
                    Json(json!({"code":0,"tenant_access_token":"fixture-token","expire":3600}))
                }),
            )
            .route(
                "/open-apis/im/v1/messages/{anchor}/reply",
                post(
                    |State(capture): State<mpsc::UnboundedSender<(String, Value)>>,
                     Path(anchor): Path<String>,
                     Json(body): Json<Value>| async move {
                        capture.send((anchor.clone(), body)).unwrap();
                        Json(
                            json!({"code":0,"data":{"message_id":format!("{anchor}_response"),
                            "chat_id":"oc_fixture","thread_id":format!("omt_{anchor}"),
                            "root_id":anchor,"parent_id":anchor}}),
                        )
                    },
                ),
            )
            .with_state(capture);
        let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
        let finished = incoming(&app, &project.id, "om_finished", "om_finished_root")?;
        let waiting = incoming(&app, &project.id, "om_waiting", "om_waiting_root")?;
        let result = async {
            let receipts = app.db.sync(|c| {
                let tx = c.unchecked_transaction()?;
                let terminal = delegate(&tx, &project.id, &finished)?;
                let outstanding = delegate(&tx, &project.id, &waiting)?;
                let before_timeout = RESULT_GRACE_MS + 1;
                let deadline = before_timeout + 1;
                tx.execute(
                    "UPDATE manager_input_outcomes SET deadline=? WHERE input_id IN (?,?)",
                    params![deadline, finished, waiting],
                )?;
                reconcile(&tx, before_timeout)?;
                assert_eq!(outbound_count(&tx, &project.id)?, 0);
                tx.execute(
                    "UPDATE work_events SET status='delivered',updated_at=1 WHERE id=?",
                    [&terminal],
                )?;
                reconcile(&tx, before_timeout - 1)?;
                assert_eq!(outbound_count(&tx, &project.id)?, 0);
                reconcile(&tx, before_timeout)?;
                assert_eq!(outbound_count(&tx, &project.id)?, 1);
                assert_eq!(outcome(&tx, &waiting)?, ("delegated".into(), None));
                reconcile(&tx, deadline)?;
                reconcile(&tx, deadline + 1)?;
                assert_eq!(outbound_count(&tx, &project.id)?, 2);
                let status: String = tx.query_row(
                    "SELECT status FROM work_events WHERE id=?",
                    [&outstanding],
                    |r| r.get(0),
                )?;
                assert_eq!(status, "queued");
                let receipts = [&finished, &waiting]
                    .into_iter()
                    .map(|input| {
                        let (state, id) = outcome(&tx, input)?;
                        assert_eq!(state, "replied");
                        Ok(id.expect("receipt ID"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                tx.commit()?;
                Ok(receipts)
            })?;
            for (id, anchor, expected) in [
                (
                    &receipts[0],
                    "om_finished_root",
                    "worker finished its attempt",
                ),
                (&receipts[1], "om_waiting_root", "within the wait period"),
            ] {
                app.send_out(id).await?;
                app.send_out(id).await?;
                assert_eq!(app.db.message(id).await?.status, "sent");
                let (actual, body) = deliveries.try_recv()?;
                assert_eq!(actual, anchor);
                assert_eq!(body["reply_in_thread"], true);
                assert!(body["content"].as_str().unwrap().contains(expected));
            }
            assert!(deliveries.try_recv().is_err());
            Ok(())
        }
        .await;
        server.abort();
        result
    }

    #[tokio::test]
    async fn delegated_receipt_remains_held_during_pause_or_takeover_and_after_stop() -> Result<()>
    {
        let (_dir, app, project) = manager::tests::fixture().await;
        let input = incoming(&app, &project.id, "om_input", "om_root")?;
        app.db.sync(|c| {
            let tx = c.unchecked_transaction()?;
            let event = delegate(&tx, &project.id, &input)?;
            tx.execute(
                "UPDATE work_events SET status='failed',updated_at=0 WHERE id=?",
                [&event],
            )?;
            tx.execute("UPDATE projects SET paused=1 WHERE id=?", [&project.id])?;
            reconcile(&tx, RESULT_GRACE_MS)?;
            assert_eq!(outbound_count(&tx, &project.id)?, 0);
            tx.execute(
                "UPDATE projects SET paused=0,generation='held-generation',terminal='held-terminal'
                    WHERE id=?",
                [&project.id],
            )?;
            work::set_takeover(&tx, &project.id, "held-generation", true, 0)?;
            reconcile(&tx, RESULT_GRACE_MS)?;
            assert_eq!(outbound_count(&tx, &project.id)?, 0);
            work::set_takeover(&tx, &project.id, "held-generation", false, 0)?;
            manager::stop(&tx, &project.id)?;
            reconcile(&tx, RESULT_GRACE_MS)?;
            assert_eq!(outbound_count(&tx, &project.id)?, 0);
            assert_eq!(outcome(&tx, &input)?, ("unreplied".into(), None));
            tx.commit()?;
            Ok(())
        })
    }
}
