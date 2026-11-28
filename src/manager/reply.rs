//! Durable disposition of claimed Manager inputs and their scoped outbox effects.
use super::*;

mod delegated;
pub(super) fn reconcile(c: &Connection, now: i64) -> Result<()> {
    delegated::reconcile(c, now)
}
#[cfg(test)]
mod tests;

const DELEGATION_WAIT_MS: i64 = 30 * 60 * 1000;

pub(super) fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS manager_input_outcomes (
            input_id TEXT PRIMARY KEY REFERENCES manager_inputs(id) ON DELETE CASCADE,
            run_id TEXT NOT NULL REFERENCES manager_runs(id) ON DELETE CASCADE,
            state TEXT NOT NULL CHECK(state IN ('replied','delegated','unreplied')),
            outbox_id TEXT,
            created_at INTEGER NOT NULL,
            closed_at INTEGER,
            deadline INTEGER
        );
        CREATE INDEX IF NOT EXISTS manager_input_outcomes_due
            ON manager_input_outcomes(deadline) WHERE state='delegated';",
    )?;
    Ok(())
}

pub(super) fn claim(c: &Connection, input: &str, run: &str) -> Result<()> {
    c.execute(
        "INSERT INTO manager_input_outcomes(input_id,run_id,state,created_at)
         VALUES(?,?,'unreplied',?)
         ON CONFLICT(input_id) DO UPDATE SET run_id=excluded.run_id
         WHERE manager_input_outcomes.closed_at IS NULL",
        params![input, run, db::now()],
    )?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Route {
    app: String,
    chat: String,
    anchor: String,
    message: String,
    thread: Option<String>,
    direct: Option<String>,
    origin: String,
    chinese: bool,
}

fn route(c: &Connection, project: &str, input: &str) -> Result<Option<Route>> {
    Ok(c.query_row(
        "SELECT b.app_id,json_extract(m.attachment,'$.source.chat_id'),
           COALESCE(NULLIF(json_extract(m.attachment,'$.source.root_id'),''),m.source_id),
           m.source_id,e.id,m.content,
           NULLIF(json_extract(m.attachment,'$.source.thread_id'),''),
           CASE WHEN json_extract(m.attachment,'$.source.chat_type')='p2p'
                THEN json_extract(m.attachment,'$.source.sender.open_id') END
         FROM manager_inputs i JOIN work_events e ON e.id=i.origin_event_id AND e.project_id=i.project_id
         JOIN messages m ON m.id=e.message_id AND m.project_id=e.project_id
         JOIN project_bots b ON b.project_id=i.project_id
         WHERE i.id=? AND i.project_id=? AND e.source='feishu'
           AND m.direction='in' AND m.source_id GLOB 'om_*'
           AND json_extract(m.attachment,'$.bot_app_id')=b.app_id
           AND json_extract(m.attachment,'$.source.chat_id') GLOB 'oc_*'",
        params![input,project],
        |row| {
            let content: String = row.get(5)?;
            Ok(Route {
                app: row.get(0)?,
                chat: row.get(1)?,
                anchor: row.get(2)?,
                message: row.get(3)?,
                origin: row.get(4)?,
                thread: row.get(6)?,
                direct: row.get(7)?,
                chinese: content.chars().any(|ch| ('\u{3400}'..='\u{9fff}').contains(&ch)),
            })
        },
    ).optional()?)
}

fn enqueue(
    c: &Connection,
    project: &str,
    input: &str,
    run: &str,
    route: &Route,
    text: &str,
) -> Result<String> {
    let revision: i64 = c.query_row(
        "SELECT revision FROM manager_runs WHERE id=? AND project_id=?",
        params![run, project],
        |row| row.get(0),
    )?;
    let key = format!("manager-outcome-{input}");
    db::enqueue_out_request(
        c,
        project,
        &route.app,
        &key,
        text,
        json!({
            "to":route.chat, "reply_to_message_id":route.anchor, "manager_effect":key,
            "manager_receipt_origin":route.origin, "manager_revision":revision,
            "manager_outcome_input":input,
        }),
    )
}

pub(super) fn validate_outbox(c: &Connection, project: &str, attachment: &Value) -> Result<()> {
    let Some(input) = attachment["manager_outcome_input"].as_str() else {
        return Ok(());
    };
    let route = route(c, project, input)?.context("Manager reply lost its original topic")?;
    ensure!(
        attachment["bot_app_id"] == route.app
            && attachment["to"] == route.chat
            && attachment["reply_to_message_id"] == route.anchor,
        "Manager reply destination changed"
    );
    let live: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM manager_input_outcomes o
         JOIN manager_runs r ON r.id=o.run_id JOIN manager_config m ON m.project_id=r.project_id
         WHERE o.input_id=? AND r.project_id=? AND r.status='complete'
           AND m.revision=r.revision AND m.input_revision=r.input_revision AND o.state='replied')",
        params![input, project],
        |row| row.get(0),
    )?;
    ensure!(
        live && uses_manager(c, project)?,
        "Manager reply was stopped or superseded"
    );
    ensure!(
        has_reply(c, project, input, &route)?.is_none() && !uncertain_send(c, input, &route)?,
        "A corresponding reply was already accepted or needs verification"
    );
    Ok(())
}

// A shared thread anchor is only evidence when the send belongs to this
// input's turn. Independent workers can also attach the exact origin event.
fn has_reply(c: &Connection, project: &str, input: &str, route: &Route) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT m.id FROM messages m
         LEFT JOIN manager_effects f ON f.id=m.source_id AND f.name IN ('send_md','send_file')
         WHERE m.project_id=?1 AND m.direction='out'
           AND m.status IN ('pending','sending','sent','uncertain')
           AND json_extract(m.attachment,'$.bot_app_id')=?2
           AND (json_extract(m.attachment,'$.to') IN (?3,?8) OR json_extract(m.attachment,'$.sent_chat_id')=?3)
           AND (json_extract(m.attachment,'$.reply_to_message_id') IN (?4,?5)
                OR json_extract(m.attachment,'$.sent_thread_id')=?9
                OR json_extract(m.attachment,'$.sent_root_id')=?4
                OR (?8 IS NOT NULL AND json_extract(m.attachment,'$.reply_to_message_id') IS NULL)
                OR EXISTS(SELECT 1 FROM messages a WHERE a.project_id=m.project_id
                     AND a.direction='out' AND json_extract(a.attachment,'$.bot_app_id')=?2
                     AND json_extract(a.attachment,'$.sent_chat_id')=?3
                     AND json_extract(a.attachment,'$.sent_thread_id')=?9
                     AND json_extract(a.attachment,'$.sent_message_id')=json_extract(m.attachment,'$.reply_to_message_id')))
           AND (m.status IN ('sending','uncertain')
                OR COALESCE(json_extract(m.attachment,'$.discussion_intent'),'result')!='progress')
           AND (
             (f.id IS NOT NULL AND EXISTS(SELECT 1 FROM manager_run_inputs b
                JOIN manager_inputs i ON i.id=b.input_id
                WHERE b.run_id=f.run_id AND (b.input_id=?6 OR
                   (i.origin_event_id=?7 AND i.kind='worker'
                    AND json_extract(i.payload,'$.kind') IN ('worker_run','worker_intent')))))
             OR json_extract(m.attachment,'$.discussion_scope')=?7
             OR (json_extract(m.attachment,'$.task_worker_generation') IS NOT NULL AND
                 json_extract(m.attachment,'$.reply_to_message_id')=?5)
           )
         ORDER BY m.rowid DESC LIMIT 1",
        params![project,route.app,route.chat,route.anchor,route.message,input,route.origin,route.direct,route.thread],
        |row| row.get(0),
    ).optional()?)
}

fn uncertain_send(c: &Connection, input: &str, route: &Route) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM manager_run_inputs b JOIN manager_effects f ON f.run_id=b.run_id
          WHERE b.input_id=? AND f.name IN ('send_md','send_file')
          AND (f.status IN ('submitting','uncertain')
               OR json_extract(f.result,'$.status') IN ('sending','uncertain')
               OR (json_extract(f.result,'$.status')='sent'
                   AND COALESCE(json_extract(f.arguments,'$.intent'),'result')!='progress'))
          AND json_extract(f.arguments,'$.to') IN (?,?)
          AND COALESCE(json_extract(f.arguments,'$.reply_to_message_id'),?) IN (?,?))",
        params![input,route.chat,route.direct,route.anchor,route.anchor,route.message], |row| row.get(0),
    )?)
}

fn outstanding(c: &Connection, project: &str, origin: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_events e JOIN manager_effects f ON f.id=e.source_id
         WHERE e.project_id=? AND e.source IN ('management','manager_worker') AND json_valid(e.payload)
           AND f.name IN ('delegate','steer_worker') AND json_extract(e.payload,'$.origin_event_id')=?
           AND e.status IN ('queued','worker_waiting','submitting','submitted','acknowledged'))",
        params![project,origin], |row| row.get(0),
    )?)
}

pub(super) fn finish(c: &Connection, context: &Context, final_text: &str) -> Result<()> {
    valid(c, context)?;
    let inputs = c
        .prepare(
            "SELECT i.id,i.kind,EXISTS(SELECT 1 FROM manager_input_outcomes o
              WHERE o.input_id=i.id AND o.closed_at IS NOT NULL)
         FROM manager_input_claims b JOIN manager_inputs i ON i.id=b.input_id
         WHERE b.run_id=? AND i.status='processing'
         ORDER BY i.rowid",
        )?
        .query_map([&context.run], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let routes = inputs
        .iter()
        .map(|(id, _, _)| route(c, &context.project, id))
        .collect::<Result<Vec<_>>>()?;
    // The Manager transcript is shared by the chat. Only reuse final text if
    // every Feishu user input being finalized belongs to the same discussion.
    let scope = routes.iter().flatten().next();
    let one_topic = scope.is_some_and(|first| {
        routes.iter().all(|route| {
            route.as_ref().is_some_and(|route| {
                route.app == first.app && route.chat == first.chat && route.anchor == first.anchor
            })
        })
    });
    let now = db::now();
    let mut finalized = None;
    for ((input, kind, closed), route) in inputs.iter().zip(routes) {
        if *closed {
            continue;
        }
        let (mut state, mut outbox, mut deadline) = ("unreplied", None, None);
        if let Some(route) = route
            && kind == "user"
        {
            if let Some(id) = has_reply(c, &context.project, input, &route)? {
                state = "replied";
                outbox = Some(id);
            } else if outstanding(c, &context.project, &route.origin)? {
                state = "delegated";
                deadline = Some(now + DELEGATION_WAIT_MS);
            } else if !uncertain_send(c, input, &route)? {
                let text = final_text.trim();
                let text = if text.is_empty()
                    || text.len() > crate::feishu::MAX_MARKDOWN_BYTES
                    || !one_topic
                {
                    if route.chinese {
                        "这次没有生成可发送的结果。"
                    } else {
                        "No result was produced for this request."
                    }
                } else {
                    text
                };
                // One fallback can cover several inputs in the same discussion.
                let id = if one_topic && let Some(id) = &finalized {
                    String::clone(id)
                } else {
                    enqueue(c, &context.project, input, &context.run, &route, text)?
                };
                if one_topic {
                    finalized = Some(id.clone());
                }
                state = "replied";
                outbox = Some(id);
            }
        }
        c.execute(
            "INSERT INTO manager_input_outcomes(input_id,run_id,state,outbox_id,created_at,closed_at,deadline)
             VALUES(?,?,?,?,?,?,?)
             ON CONFLICT(input_id) DO UPDATE SET run_id=excluded.run_id,state=excluded.state,
                outbox_id=excluded.outbox_id,closed_at=excluded.closed_at,deadline=excluded.deadline
             WHERE manager_input_outcomes.closed_at IS NULL",
            params![input, context.run, state, outbox, now, now, deadline],
        )?;
    }
    Ok(())
}
