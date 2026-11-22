//! Source-backed decisions belong to a bot discussion and an explicit work branch.
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub source_event_id: String,
    branch: String,
    key: String,
    evidence: String,
    kind: Kind,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Current,
    Reference,
    Obsolete,
}

struct Source {
    scope: String,
    message_id: String,
    body: String,
    order: i64,
}

fn source(c: &Connection, project: &str, origin: &str, include_body: bool) -> Result<Option<Source>> {
    let row = c.prepare_cached(
        "SELECT m.source_id,CASE WHEN ?3 THEN m.content ELSE '' END,m.attachment,m.rowid
         FROM work_events e JOIN messages m ON m.id=e.message_id AND m.project_id=e.project_id
         JOIN project_bots b ON b.project_id=e.project_id AND b.app_id=json_extract(m.attachment,'$.bot_app_id')
         WHERE e.project_id=?1 AND e.id=?2 AND e.source='feishu' AND m.direction='in'",
    )?.query_row(
        params![project, origin, include_body],
        |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, i64>(3)?)),
    ).optional()?;
    let Some((message_id, body, raw, order)) = row else {
        return Ok(None);
    };
    let a: Value = serde_json::from_str(&raw)?;
    let info = &a["source"];
    let Some(chat) = info["chat_id"].as_str().filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let thread = info["root_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .or_else(|| info["thread_id"].as_str().filter(|id| !id.is_empty()));
    let discussion = match info["chat_type"].as_str() {
        Some("group") => thread.unwrap_or(&message_id),
        Some("p2p") => thread.unwrap_or("main"),
        _ => return Ok(None),
    };
    let scope = json!([a["bot_app_id"], chat, discussion]).to_string();
    Ok(Some(Source {
        scope,
        message_id,
        body,
        order,
    }))
}

pub(super) fn discussion(c: &Connection, project: &str, origin: &str) -> Result<Option<String>> {
    Ok(source(c, project, origin, false)?.map(|source| source.scope))
}

pub(super) fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS manager_topic_decisions(
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        scope TEXT NOT NULL, branch TEXT NOT NULL, decision_key TEXT NOT NULL,
        kind TEXT NOT NULL, source_event TEXT NOT NULL, source_message TEXT NOT NULL,
        source_order INTEGER NOT NULL, evidence TEXT NOT NULL,
        PRIMARY KEY(project_id,scope,branch,decision_key,source_event)
    )",
    )?;
    Ok(())
}

fn label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 96 && !value.chars().any(char::is_control)
}

pub(super) fn record(c: &Connection, project: &str, input: Record) -> Result<Value> {
    ensure!(
        label(&input.branch) && label(&input.key),
        "Use a short branch and decision key"
    );
    let evidence = input.evidence.trim();
    ensure!(
        !evidence.is_empty() && evidence.len() <= 512,
        "Use a short exact user excerpt"
    );
    let source = source(c, project, &input.source_event_id, true)?
        .context("Decision requires an inbound message from the project's bound bot")?;
    ensure!(
        source.body.contains(evidence),
        "Decision evidence must be verbatim from the current user message, outside quote metadata"
    );
    let kind = match input.kind {
        Kind::Current => "current",
        Kind::Reference => "reference",
        Kind::Obsolete => "obsolete",
    };
    c.execute("INSERT OR IGNORE INTO manager_topic_decisions(project_id,scope,branch,decision_key,kind,source_event,source_message,source_order,evidence)
        VALUES(?,?,?,?,?,?,?,?,?)", params![project,source.scope,input.branch,input.key,kind,input.source_event_id,source.message_id,source.order,evidence])?;
    Ok(json!({"status":"recorded","source_event_id":input.source_event_id}))
}

pub(super) fn context(
    c: &Connection,
    project: &str,
    origin: &str,
    branch: Option<&str>,
) -> Result<Value> {
    let Some(source) = source(c, project, origin, false)? else {
        return Ok(Value::Null);
    };
    scoped_context(c, project, origin, &source.scope, branch)
}

pub(super) fn batch(
    c: &Connection,
    project: &str,
    origins: impl Iterator<Item = String>,
    budget: usize,
) -> Result<Value> {
    let mut scopes = std::collections::HashSet::new();
    let mut contexts = Vec::new();
    let mut remaining = budget.min(4096).saturating_sub(32);
    for origin in origins {
        let Some(source) = source(c, project, &origin, false)? else {
            continue;
        };
        if !scopes.insert(source.scope.clone()) {
            continue;
        }
        let mut context = scoped_context(c, project, &origin, &source.scope, None)?;
        while !context.is_null() && context.to_string().len() + 1 > remaining {
            if context["decisions"].as_array_mut().unwrap().pop().is_none() {
                context = Value::Null;
            }
        }
        if !context.is_null() && !context["decisions"].as_array().unwrap().is_empty() {
            remaining -= context.to_string().len() + 1;
            contexts.push(context);
        }
    }
    Ok(json!(contexts))
}

fn scoped_context(
    c: &Connection,
    project: &str,
    origin: &str,
    scope: &str,
    branch: Option<&str>,
) -> Result<Value> {
    // One explicit branch for a worker. The Manager may see separate branches,
    // with the branches kept distinct and no implicit winner picked by the host.
    // An old task anchor selects the discussion, not an older version of its
    // decisions. Already enqueued task payloads keep their frozen snapshot.
    let mut statement = c.prepare(
        "WITH decisions AS (
            SELECT d.branch,d.decision_key,
                CASE WHEN d.kind='current' AND EXISTS(SELECT 1 FROM manager_topic_decisions newer
                    WHERE newer.project_id=d.project_id AND newer.scope=d.scope AND newer.branch=d.branch
                    AND newer.decision_key=d.decision_key AND newer.source_order>d.source_order
                    AND newer.kind IN ('current','obsolete'))
                THEN 'obsolete' ELSE d.kind END AS category,
                d.source_event,d.source_message,d.source_order,d.evidence
            FROM manager_topic_decisions d
            WHERE d.project_id=?1 AND d.scope=?2 AND (?3 IS NULL OR d.branch=?3)
         )
         SELECT branch,decision_key,category,source_event,source_message,evidence
         FROM decisions ORDER BY CASE category WHEN 'current' THEN 0 WHEN 'reference' THEN 1 ELSE 2 END,
         source_order DESC,branch,decision_key LIMIT 10",
    )?;
    let entries = statement.query_map(params![project,scope,branch], |r| {
        let kind: String = r.get(2)?;
        Ok(json!({"branch":r.get::<_,String>(0)?,"key":r.get::<_,String>(1)?,
            "category":kind,"source_event_id":r.get::<_,String>(3)?,"source_message_id":r.get::<_,String>(4)?,
            "user_evidence":r.get::<_,String>(5)?}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    if entries.is_empty() {
        return Ok(Value::Null);
    }
    let mut context = json!({"origin_event_id":origin,"decisions":[]});
    for entry in entries {
        context["decisions"].as_array_mut().unwrap().push(entry);
        if context.to_string().len() > 4096 {
            context["decisions"].as_array_mut().unwrap().pop();
            break;
        }
    }
    Ok(context)
}
