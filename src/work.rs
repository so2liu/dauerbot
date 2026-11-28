//! Message delivery journal and schedules, independent of an Agent CLI or process.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, TimeZone};
use chrono_tz::Tz;
use cron::Schedule;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::str::FromStr;

pub const WARM_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TimerInput {
    pub name: String,
    pub prompt: String,
    pub kind: String,
    #[serde(default)]
    pub at: Option<Value>,
    #[serde(default)]
    pub interval_seconds: Option<i64>,
    #[serde(default)]
    pub cron: Option<String>,
    #[serde(default = "utc")]
    pub timezone: String,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    /// user for user-requested work, internal for Agent follow-ups; empty is unclassified.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub purpose: String,
}
fn utc() -> String {
    "UTC".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub id: String,
    pub project_id: String,
    #[serde(skip_serializing)]
    pub task_id: String,
    pub source: String,
    pub source_id: String,
    pub message_id: Option<String>,
    pub payload: Value,
    pub status: String,
    pub received_at: Option<i64>,
    pub error: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Run {
    pub id: String,
    pub project_id: String,
    #[serde(skip_serializing)]
    pub task_id: String,
    pub event_id: String,
    pub generation: String,
    pub status: String,
    pub error: String,
    pub started_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Execution {
    pub generation: String,
    pub project_id: String,
    pub agent: String,
    pub terminal: String,
    pub native_session: String,
    pub status: String,
    pub last_activity: i64,
    pub idle_since: Option<i64>,
    pub warm_until: Option<i64>,
    pub error: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Timer {
    pub id: String,
    pub project_id: String,
    #[serde(skip_serializing)]
    pub task_id: String,
    pub name: String,
    pub prompt: String,
    pub kind: String,
    pub at: Option<i64>,
    pub interval_seconds: Option<i64>,
    pub cron: String,
    pub timezone: String,
    pub to: String,
    pub next_fire: Option<i64>,
    pub enabled: bool,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub purpose: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Attention {
    pub id: String,
    pub project_id: String,
    pub generation: String,
    pub reason: String,
    pub message: String,
    pub to: String,
    pub status: String,
    pub actions: Value,
    pub occurrences: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub resolved_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Lease {
    pub project_id: String,
    pub generation: String,
    pub active: bool,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkState {
    pub events: Vec<Event>,
    pub runs: Vec<Run>,
    pub execution: Option<Execution>,
    pub timers: Vec<Timer>,
    pub attention: Vec<Attention>,
    pub lease: Option<Lease>,
}

/// Prefixes preserve the existing meaning of Feishu `conversations` and `messages`.
pub fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS work_tasks(
          id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          title TEXT NOT NULL, instructions TEXT NOT NULL, to_id TEXT NOT NULL DEFAULT '',
          status TEXT NOT NULL, checkpoint TEXT NOT NULL DEFAULT '',
          created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS work_events(
          id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          task_id TEXT NOT NULL REFERENCES work_tasks(id) ON DELETE CASCADE,
          source TEXT NOT NULL, source_id TEXT NOT NULL,
          message_id TEXT UNIQUE REFERENCES messages(id) ON DELETE SET NULL,
          payload TEXT NOT NULL, status TEXT NOT NULL, received_at INTEGER,
          error TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
          UNIQUE(project_id,source,source_id));
        CREATE INDEX IF NOT EXISTS work_event_queue ON work_events(project_id,status,created_at);
        CREATE INDEX IF NOT EXISTS manager_outcome_delegations
            ON work_events(project_id,json_extract(payload,'$.origin_event_id'))
            WHERE source IN ('management','manager_worker') AND json_valid(payload);
        CREATE TABLE IF NOT EXISTS work_runs(
          id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          task_id TEXT NOT NULL REFERENCES work_tasks(id) ON DELETE CASCADE,
          event_id TEXT NOT NULL REFERENCES work_events(id) ON DELETE CASCADE,
          generation TEXT NOT NULL, status TEXT NOT NULL, outcome TEXT NOT NULL DEFAULT '',
          summary TEXT NOT NULL DEFAULT '', error TEXT NOT NULL DEFAULT '',
          started_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER);
        CREATE INDEX IF NOT EXISTS work_run_active ON work_runs(project_id,status);
        CREATE TABLE IF NOT EXISTS work_executions(
          generation TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          agent TEXT NOT NULL, terminal TEXT NOT NULL, native_session TEXT NOT NULL DEFAULT '',
          status TEXT NOT NULL, last_activity INTEGER NOT NULL, idle_since INTEGER, warm_until INTEGER,
          error TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS work_execution_project ON work_executions(project_id,created_at);
        CREATE TABLE IF NOT EXISTS work_timers(
          id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          task_id TEXT NOT NULL REFERENCES work_tasks(id) ON DELETE CASCADE,
          name TEXT NOT NULL, prompt TEXT NOT NULL, kind TEXT NOT NULL, at_ms INTEGER,
          interval_seconds INTEGER, cron TEXT NOT NULL DEFAULT '', timezone TEXT NOT NULL,
          to_id TEXT NOT NULL DEFAULT '', next_fire INTEGER, enabled INTEGER NOT NULL DEFAULT 1,
          revision INTEGER NOT NULL DEFAULT 1, request_id TEXT, request_payload TEXT NOT NULL DEFAULT '',
          created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, purpose TEXT NOT NULL DEFAULT '');
        CREATE UNIQUE INDEX IF NOT EXISTS work_timer_request ON work_timers(project_id,request_id) WHERE request_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS work_timer_due ON work_timers(enabled,next_fire);
        CREATE TABLE IF NOT EXISTS work_occurrences(
          timer_id TEXT NOT NULL REFERENCES work_timers(id) ON DELETE CASCADE,
          revision INTEGER NOT NULL, scheduled_at INTEGER NOT NULL,
          event_id TEXT REFERENCES work_events(id) ON DELETE SET NULL, status TEXT NOT NULL,
          PRIMARY KEY(timer_id,revision,scheduled_at));
        CREATE TABLE IF NOT EXISTS work_attention(
          id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          generation TEXT NOT NULL, reason TEXT NOT NULL, message TEXT NOT NULL, to_id TEXT NOT NULL DEFAULT '',
          status TEXT NOT NULL DEFAULT 'open', actions TEXT NOT NULL DEFAULT '[]',
          occurrences INTEGER NOT NULL DEFAULT 1, first_seen INTEGER NOT NULL,
          last_seen INTEGER NOT NULL, resolved_at INTEGER);
        CREATE UNIQUE INDEX IF NOT EXISTS work_attention_open
          ON work_attention(project_id,generation,reason) WHERE status='open';
        CREATE TABLE IF NOT EXISTS work_leases(
          project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          generation TEXT NOT NULL, active INTEGER NOT NULL, updated_at INTEGER NOT NULL,
          PRIMARY KEY(project_id,generation));
        ",
    )?;
    let has_purpose: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_timers') WHERE name='purpose')",
        [],
        |r| r.get(0),
    )?;
    if !has_purpose {
        // Legacy reminders stay unclassified and operational. Do not infer
        // purpose from names, languages, recurrence kinds or occurrence history.
        c.execute(
            "ALTER TABLE work_timers ADD COLUMN purpose TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    let has_sequence: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_runs') WHERE name='native_seq')",
        [],
        |r| r.get(0),
    )?;
    if !has_sequence {
        c.execute("ALTER TABLE work_runs ADD COLUMN native_seq INTEGER", [])?;
    }
    let has_idle_sequence: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_executions') WHERE name='idle_seq')",
        [],
        |r| r.get(0),
    )?;
    if !has_idle_sequence {
        c.execute(
            "ALTER TABLE work_executions ADD COLUMN idle_seq INTEGER",
            [],
        )?;
    }
    let has_child_revision: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_executions') WHERE name='child_revision')", [], |r| r.get(0))?;
    if !has_child_revision {
        c.execute(
            "ALTER TABLE work_executions ADD COLUMN child_revision TEXT",
            [],
        )?;
    }
    // Removed bookkeeping must never gate an upgraded session. Keep the journal,
    // messages, schedule IDs and occurrences; do not requeue submitted input.
    c.execute_batch("DROP TABLE IF EXISTS work_jobs; DROP TABLE IF EXISTS work_decisions;
        UPDATE work_attention SET status='resolved' WHERE reason IN ('unfinished_run','not_ready');")?;
    crate::cli_control::classify(c)?;
    sync_inbound(c)?;
    Ok(())
}

fn json_column(r: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    let text: String = r.get(index)?;
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}
fn event_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get(0)?,
        project_id: r.get(1)?,
        task_id: r.get(2)?,
        source: r.get(3)?,
        source_id: r.get(4)?,
        message_id: r.get(5)?,
        payload: json_column(r, 6)?,
        status: r.get(7)?,
        received_at: r.get(8)?,
        error: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}
const EVENT: &str = "id,project_id,task_id,source,source_id,message_id,payload,status,received_at,error,created_at,updated_at";
fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        project_id: r.get(1)?,
        task_id: r.get(2)?,
        event_id: r.get(3)?,
        generation: r.get(4)?,
        status: r.get(5)?,
        error: r.get(6)?,
        started_at: r.get(7)?,
        updated_at: r.get(8)?,
        finished_at: r.get(9)?,
    })
}
const RUN: &str =
    "id,project_id,task_id,event_id,generation,status,error,started_at,updated_at,finished_at";
fn execution_row(r: &Row<'_>) -> rusqlite::Result<Execution> {
    Ok(Execution {
        generation: r.get(0)?,
        project_id: r.get(1)?,
        agent: r.get(2)?,
        terminal: r.get(3)?,
        native_session: r.get(4)?,
        status: r.get(5)?,
        last_activity: r.get(6)?,
        idle_since: r.get(7)?,
        warm_until: r.get(8)?,
        error: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}
const EXECUTION: &str = "generation,project_id,agent,terminal,native_session,status,last_activity,idle_since,warm_until,error,created_at,updated_at";
fn timer_row(r: &Row<'_>) -> rusqlite::Result<Timer> {
    Ok(Timer {
        id: r.get(0)?,
        project_id: r.get(1)?,
        task_id: r.get(2)?,
        name: r.get(3)?,
        prompt: r.get(4)?,
        kind: r.get(5)?,
        at: r.get(6)?,
        interval_seconds: r.get(7)?,
        cron: r.get(8)?,
        timezone: r.get(9)?,
        to: r.get(10)?,
        next_fire: r.get(11)?,
        enabled: r.get(12)?,
        revision: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        purpose: r.get(16)?,
    })
}
const TIMER: &str = "id,project_id,task_id,name,prompt,kind,at_ms,interval_seconds,cron,timezone,to_id,next_fire,enabled,revision,created_at,updated_at,purpose";
fn attention_row(r: &Row<'_>) -> rusqlite::Result<Attention> {
    Ok(Attention {
        id: r.get(0)?,
        project_id: r.get(1)?,
        generation: r.get(2)?,
        reason: r.get(3)?,
        message: r.get(4)?,
        to: r.get(5)?,
        status: r.get(6)?,
        actions: json_column(r, 7)?,
        occurrences: r.get(8)?,
        first_seen: r.get(9)?,
        last_seen: r.get(10)?,
        resolved_at: r.get(11)?,
    })
}
const ATTENTION: &str = "id,project_id,generation,reason,message,to_id,status,actions,occurrences,first_seen,last_seen,resolved_at";

fn transaction<T>(c: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    // Savepoints also work inside the atomic database migration transaction.
    let name = format!("dauerbot_work_{}", crate::db::id().replace('-', ""));
    c.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f(c) {
        Ok(result) => {
            c.execute_batch(&format!("RELEASE SAVEPOINT {name}"))?;
            Ok(result)
        }
        Err(error) => {
            let _ = c.execute_batch(&format!(
                "ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"
            ));
            Err(error)
        }
    }
}

/// Import only missing records. Existing receipts never imply successful task completion.
pub fn sync_inbound(c: &Connection) -> Result<usize> {
    transaction(c, |c| {
        // Legacy retry/dismiss/receipt endpoints remain authoritative for message delivery.
        let changes=c.prepare(
          "SELECT e.id,m.status,m.error,m.updated_at FROM work_events e JOIN messages m ON m.id=e.message_id
           WHERE e.status NOT IN ('delivered','completed','cancelled','dismissed','managed','consumed','transferred') AND m.status!=e.status
           AND NOT (m.status='submitting' AND e.status='uncertain')
           AND NOT EXISTS(SELECT 1 FROM work_runs r WHERE r.event_id=e.id AND r.finished_at IS NOT NULL AND r.outcome!='')")?
          .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?
          .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, status, error, updated) in changes {
            if ![
                "queued",
                "submitting",
                "submitted",
                "acknowledged",
                "uncertain",
                "failed",
                "dismissed",
            ]
            .contains(&status.as_str())
            {
                continue;
            }
            c.execute("UPDATE work_events SET status=?,error=?,received_at=CASE WHEN ?='acknowledged' THEN COALESCE(received_at,?) ELSE received_at END,updated_at=? WHERE id=?",
              params![status,error,status,updated,updated,id])?;
            match status.as_str() {
                "queued" | "dismissed" => {
                    c.execute("UPDATE work_runs SET status='interrupted',error=?,finished_at=?,updated_at=? WHERE event_id=? AND finished_at IS NULL",
                      params![if status=="dismissed"{"Dismissed by the user"}else{"Explicit delivery retry"},updated,updated,id])?;
                    c.execute("UPDATE work_tasks SET status=?,updated_at=? WHERE id=(SELECT task_id FROM work_events WHERE id=?)",
                      params![if status=="queued"{"queued"}else{"cancelled"},updated,id])?;
                    if status == "queued" { crate::manager::route_event(c, &id)?; }
                }
                "acknowledged" => {
                    c.execute("UPDATE work_runs SET status='running',error='',updated_at=? WHERE event_id=? AND status IN ('submitting','submitted','uncertain') AND finished_at IS NULL",params![updated,id])?;
                }
                "uncertain" => {
                    c.execute("UPDATE work_runs SET status='uncertain',error=?,updated_at=? WHERE event_id=? AND finished_at IS NULL",params![error,updated,id])?;
                }
                _ => {}
            }
        }
        let mut query = c.prepare(
            "SELECT id,project_id,source_id,content,status,attachment,error,created_at,updated_at
           FROM messages WHERE direction='in' AND json_extract(attachment,'$.daemon_control') IS NULL AND NOT EXISTS(
             SELECT 1 FROM work_events WHERE message_id=messages.id) ORDER BY rowid",
        )?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let count = rows.len();
        for (id, project, source, content, status, attachment, error, created, updated) in rows {
            let task_state = match status.as_str() {
                "queued" => "queued",
                "dismissed" => "cancelled",
                "acknowledged" => "imported",
                "failed" => "failed",
                _ => "uncertain",
            };
            let event_state = if status == "submitting" {
                "uncertain"
            } else {
                &status
            };
            c.execute("INSERT INTO work_tasks(id,project_id,title,instructions,status,created_at,updated_at)
              VALUES(?,?,?,?,?,?,?)",params![id,project,content.chars().take(100).collect::<String>(),content,task_state,created,updated])?;
            let mut context: Value = serde_json::from_str(&attachment).unwrap_or(Value::Null);
            if let Some(fields) = context.as_object_mut() {
                // Quotes and preceding history are read from the original message at claim or render time.
                fields.remove("quoted_message");
                fields.remove("quote_pending");
                fields.remove("preceding_messages");
                fields.remove("preceding_pending");
            }
            let payload = json!({"content":content,"attachment":context,"legacy_status":status});
            let kind = if source.starts_with("local-") {
                "local"
            } else {
                "feishu"
            };
            c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,message_id,payload,status,received_at,error,created_at,updated_at)
              VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",params![id,project,id,kind,source,id,payload.to_string(),event_state,
                (status=="acknowledged").then_some(updated),error,created,updated])?;
            crate::manager::route_event(c, &id)?;
        }
        Ok(count)
    })
}

fn valid_request(request: Option<&str>) -> Result<()> {
    if let Some(request) = request {
        ensure!(
            !request.trim().is_empty() && request.len() <= 240,
            "Request ID must contain 1 to 240 bytes"
        );
    }
    Ok(())
}
fn valid_to(to: Option<&str>) -> Result<()> {
    if let Some(to) = to.filter(|s| !s.is_empty()) {
        ensure!(
            to.starts_with("oc_") || to.starts_with("ou_"),
            "Recipient must be a chat ID (oc_) or user Open ID (ou_)"
        );
    }
    Ok(())
}
fn insert_event(
    c: &Connection,
    project: &str,
    task: &str,
    source: &str,
    source_id: &str,
    payload: Value,
    now: i64,
) -> Result<Event> {
    ensure!(
        !source.is_empty() && !source_id.is_empty(),
        "Event source and identity are required"
    );
    c.execute("INSERT OR IGNORE INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at)
      VALUES(?,?,?,?,?,?,'queued',?,?)",params![crate::db::id(),project,task,source,source_id,payload.to_string(),now,now])?;
    Ok(c.query_row(
        &format!("SELECT {EVENT} FROM work_events WHERE project_id=? AND source=? AND source_id=?"),
        params![project, source, source_id],
        event_row,
    )?)
}
pub fn event(c: &Connection, project: &str, id: &str) -> Result<Event> {
    c.query_row(
        &format!("SELECT {EVENT} FROM work_events WHERE project_id=? AND id=?"),
        params![project, id],
        event_row,
    )
    .context("Event not found")
}
pub fn events(c: &Connection, project: &str) -> Result<Vec<Event>> {
    Ok(c.prepare(&format!(
        "SELECT {EVENT} FROM work_events WHERE project_id=?
         ORDER BY CASE WHEN status IN ('uncertain','failed') THEN 0 WHEN status IN ('submitting','submitted') THEN 1 WHEN status='acknowledged' THEN 2 ELSE 3 END, created_at DESC LIMIT 200"
    ))?
    .query_map([project], event_row)?
    .collect::<rusqlite::Result<_>>()?)
}
pub fn run(c: &Connection, project: &str, id: &str) -> Result<Run> {
    c.query_row(
        &format!("SELECT {RUN} FROM work_runs WHERE project_id=? AND id=?"),
        params![project, id],
        run_row,
    )
    .context("Run not found")
}
pub fn active_run(c: &Connection, project: &str) -> Result<Option<Run>> {
    Ok(c.query_row(&format!("SELECT {RUN} FROM work_runs WHERE project_id=? AND status IN ('submitting','submitted','running','uncertain') ORDER BY started_at LIMIT 1"),
      [project],run_row).optional()?)
}

pub fn active_execution_run(
    c: &Connection,
    project: &str,
    generation: &str,
) -> Result<Option<Run>> {
    Ok(c.query_row(
        &format!("SELECT {RUN} FROM work_runs WHERE project_id=? AND generation=? AND status IN ('submitting','submitted','running','uncertain') ORDER BY started_at LIMIT 1"),
        params![project,generation],run_row,
    ).optional()?)
}

/// Claim and state changes share the same transaction; receipt remains a separate operation.
pub fn claim_next(
    c: &Connection,
    project: &str,
    generation: &str,
    now: i64,
) -> Result<Option<Run>> {
    claim(c, project, generation, now, false, false)
}

/// Conversation input steers the live Agent even while earlier work is unfinished.
/// The inbox is a durable delivery journal, not a turn-completion barrier.
pub fn claim_steering(
    c: &Connection,
    project: &str,
    generation: &str,
    now: i64,
) -> Result<Option<Run>> {
    claim(c, project, generation, now, true, false)
}

pub(crate) fn claim_worker(c: &Connection, project: &str, generation: &str, now: i64) -> Result<Option<Run>> {
    claim(c,project,generation,now,false,true)
}

fn claim(
    c: &Connection,
    project: &str,
    generation: &str,
    now: i64,
    steering_only: bool,
    independent: bool,
) -> Result<Option<Run>> {
    transaction(c, |c| {
        if independent {
            let parent = crate::db::project_by_key(c,project)?;
            if crate::manager_workers::authorize(c,&parent,generation,true).is_err() {
                return Ok(None);
            }
            let in_flight: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM work_runs WHERE project_id=? AND generation=? AND status IN ('submitting','submitted','running','uncertain'))",
                params![project,generation],|r|r.get(0))?;
            if in_flight {
                return Ok(None);
            }
        }
        if has_takeover(c, project, generation)?
            || crate::cli_switch::latest(c, project)?
                .as_ref().is_some_and(|request| {
                    request.holds_input || (!independent && crate::cli_switch::blocks_worker(request))
                })
        {
            return Ok(None);
        }
        // Resource preparation runs independently of project reconciliation.
        // Skip only those messages; later steering must not wait for a download.
        let prepared = format!("NOT EXISTS(SELECT 1 FROM messages m WHERE m.id=COALESCE(work_events.message_id,
             (SELECT origin.message_id FROM work_events origin WHERE work_events.source IN ('management','manager_worker')
             AND origin.id=json_extract(work_events.payload,'$.origin_event_id'))) AND {})", crate::feishu::preparation_pending());
        // A minimal delivery-journal fixture can omit the manager schema.
        // Production stores always install it before reconciling work.
        let source = if independent {
            "status='worker_waiting' AND source='manager_worker' AND json_extract(payload,'$.target_generation')=?2"
        } else if crate::manager::uses_manager(c, project)? {
            "status='queued' AND source='management'"
        } else {
            "status='queued' AND source!='manager_worker'"
        };
        let parameters = [project,generation];
        let parameters = rusqlite::params_from_iter(&parameters[..if independent {2} else {1}]);
        let message = c.query_row(
            &format!("SELECT {EVENT} FROM work_events WHERE project_id=? AND (message_id IS NOT NULL OR source IN ('management','manager_worker')) AND {source} AND {prepared} ORDER BY created_at,rowid LIMIT 1"),
            parameters.clone(), event_row,
        ).optional()?;
        let next = if message.is_some() {
            message
        } else {
            if steering_only {
                return Ok(None);
            }
            c.query_row(&format!("SELECT {EVENT} FROM work_events WHERE project_id=? AND {source} AND {prepared} ORDER BY created_at,rowid LIMIT 1"),
              parameters,event_row).optional()?
        };
        let Some(event) = next else { return Ok(None) };
        if !independent && !crate::manager::bind_event(c, &event, generation)? {
            return Ok(None);
        }
        let id = crate::db::id();
        c.execute(
            "UPDATE work_events SET status='submitting',updated_at=? WHERE id=?",
            params![now, event.id],
        )?;
        c.execute(
            "UPDATE messages SET status='submitting',updated_at=?,attempts=attempts+1 WHERE id=?",
            params![now, event.message_id],
        )?;
        c.execute(
            "UPDATE work_tasks SET status='running',updated_at=? WHERE id=?",
            params![now, event.task_id],
        )?;
        c.execute("INSERT INTO work_runs(id,project_id,task_id,event_id,generation,status,started_at,updated_at)
          VALUES(?,?,?,?,?,'submitting',?,?)",params![id,project,event.task_id,event.id,generation,now,now])?;
        activity(c, project, generation, now)?;
        Ok(Some(run(c, project, &id)?))
    })
}

pub fn delivery(c: &Connection, run_id: &str, status: &str, error: &str, now: i64) -> Result<()> {
    ensure!(
        ["queued", "submitted", "uncertain", "failed"].contains(&status),
        "Invalid delivery state"
    );
    transaction(c, |c| {
        let (event, task, current, project): (String, String, String, String) = c.query_row(
            "SELECT event_id,task_id,status,project_id FROM work_runs WHERE id=?",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        // A fast MCP receipt or finish can arrive before the send call returns.
        if !["submitting", "submitted", "uncertain"].contains(&current.as_str()) {
            return Ok(());
        }
        let run_status = if status == "queued" {
            "interrupted"
        } else {
            status
        };
        c.execute("UPDATE work_runs SET status=?,error=?,updated_at=?,finished_at=CASE WHEN ? IN ('interrupted','failed') THEN ? ELSE finished_at END WHERE id=?",
          params![run_status,error,now,run_status,now,run_id])?;
        c.execute("UPDATE work_events SET status=CASE WHEN ?1='queued' AND source='manager_worker' THEN 'worker_waiting' ELSE ?1 END,error=?2,updated_at=?3 WHERE id=?4 AND status!='acknowledged'",params![status,error,now,event])?;
        c.execute("UPDATE messages SET status=?,error=?,updated_at=? WHERE id=(SELECT message_id FROM work_events WHERE id=?) AND status!='acknowledged'",
          params![status,error,now,event])?;
        c.execute(
            "UPDATE work_tasks SET status=?,updated_at=? WHERE id=?",
            params![
                if status == "queued" {
                    "queued"
                } else if status == "failed" {
                    "failed"
                } else if status == "uncertain" {
                    "uncertain"
                } else {
                    "running"
                },
                now,
                task
            ],
        )?;
        if matches!(status, "failed" | "uncertain") {
            crate::manager::worker_changed(c, &project)?;
        }
        Ok(())
    })
}
fn cron_weekday(field: &str) -> Result<String> {
    const NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    if field == "*" {
        return Ok("*".into());
    }
    let ordinal = |text: &str| -> Result<usize> {
        let value = text
            .parse::<usize>()
            .ok()
            .or_else(|| {
                NAMES
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(text))
            })
            .context("Invalid cron weekday")?;
        ensure!(
            value <= 7,
            "Cron weekdays use 0–7, with 0 and 7 both meaning Sunday"
        );
        Ok(value)
    };
    let mut names = Vec::new();
    for item in field.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => (
                base,
                step.parse::<usize>().context("Invalid cron weekday step")?,
            ),
            None => (item, 1),
        };
        ensure!(
            step > 0 && step <= 7,
            "Cron weekday step must be between 1 and 7"
        );
        let (start, end) = if base == "*" {
            (0, 6)
        } else if let Some((start, end)) = base.split_once('-') {
            (ordinal(start)?, ordinal(end)?)
        } else {
            let start = ordinal(base)?;
            (
                start,
                if item.contains('/') {
                    6.max(start)
                } else {
                    start
                },
            )
        };
        ensure!(
            start <= end,
            "Cron weekday range must be in ascending order"
        );
        for value in (start..=end).step_by(step) {
            let name = NAMES[value % 7];
            if !names.contains(&name) {
                names.push(name)
            }
        }
    }
    ensure!(!names.is_empty(), "Cron weekday set cannot be empty");
    Ok(names.join(","))
}
fn cron_next(expression: &str, timezone: Tz, now: i64) -> Result<Option<i64>> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    ensure!(
        fields.len() == 5,
        "Use five cron fields: minute hour day month weekday"
    );
    let weekday = cron_weekday(fields[4])?;
    let current = timezone
        .timestamp_millis_opt(now)
        .single()
        .context("Schedule time is out of range")?;
    // The crate uses Sunday=1 and intersects day-of-month/week. UNIX cron uses
    // Sunday=0/7 and unions the two fields when neither begins with a wildcard.
    let expressions = if !fields[2].starts_with('*') && !fields[4].starts_with('*') {
        vec![
            format!(
                "0 {} {} {} {} *",
                fields[0], fields[1], fields[2], fields[3]
            ),
            format!("0 {} {} * {} {}", fields[0], fields[1], fields[3], weekday),
        ]
    } else {
        vec![format!(
            "0 {} {} {} {} {}",
            fields[0], fields[1], fields[2], fields[3], weekday
        )]
    };
    let mut next = None;
    for expression in expressions {
        let schedule = Schedule::from_str(&expression).context("Invalid cron expression")?;
        if let Some(value) = schedule
            .after(&current)
            .next()
            .map(|d| d.timestamp_millis())
        {
            next = Some(next.map_or(value, |prior: i64| prior.min(value)));
        }
    }
    Ok(next)
}
fn parse_at(value: &Value) -> Result<i64> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .context("Start time must be milliseconds since Unix epoch"),
        Value::String(s) => Ok(DateTime::parse_from_rfc3339(s)
            .context("Start time must include its timezone offset (RFC3339)")?
            .timestamp_millis()),
        _ => bail!("Start time must be RFC3339 text or milliseconds since Unix epoch"),
    }
}
type TimerFields = (Option<i64>, Option<i64>, String, Option<i64>);
fn timer_fields(input: &TimerInput, now: i64) -> Result<TimerFields> {
    ensure!(
        matches!(input.purpose.as_str(), "" | "user" | "internal"),
        "Schedule purpose must be user or internal"
    );
    ensure!(
        !input.name.trim().is_empty() && input.name.len() <= 240,
        "Schedule name must contain 1 to 240 bytes"
    );
    ensure!(
        !input.prompt.trim().is_empty(),
        "Scheduled instructions cannot be empty"
    );
    valid_to(input.to.as_deref())?;
    valid_request(input.request_id.as_deref())?;
    let timezone: Tz = input
        .timezone
        .parse()
        .context("Use a valid IANA timezone, such as Europe/London")?;
    match input.kind.as_str() {
        "once" => {
            let at = parse_at(
                input
                    .at
                    .as_ref()
                    .context("One-shot schedules require a start time")?,
            )?;
            ensure!(at >= 0, "Start time cannot be before Unix epoch");
            Ok((Some(at), None, String::new(), Some(at)))
        }
        "interval" => {
            let seconds = input
                .interval_seconds
                .context("Interval schedules require interval_seconds")?;
            ensure!(
                seconds > 0 && seconds <= 365 * 24 * 60 * 60,
                "Interval must be between one second and one year"
            );
            let at = input.at.as_ref().map(parse_at).transpose()?;
            let next = at.unwrap_or(
                now.checked_add(seconds * 1000)
                    .context("Schedule time overflow")?,
            );
            Ok((at, Some(seconds), String::new(), Some(next)))
        }
        "cron" => {
            let expression = input
                .cron
                .as_deref()
                .context("Cron schedules require a cron expression")?;
            let next =
                cron_next(expression, timezone, now)?.context("Cron has no future occurrence")?;
            Ok((None, None, expression.trim().to_owned(), Some(next)))
        }
        _ => bail!("Schedule kind must be once, interval, or cron"),
    }
}
pub fn timer(c: &Connection, project: &str, id: &str) -> Result<Timer> {
    c.query_row(
        &format!("SELECT {TIMER} FROM work_timers WHERE project_id=? AND id=?"),
        params![project, id],
        timer_row,
    )
    .context("Schedule not found")
}
pub fn create_timer(c: &Connection, project: &str, input: TimerInput, now: i64) -> Result<Timer> {
    let (at, interval, cron, next) = timer_fields(&input, now)?;
    let request_payload = serde_json::to_string(&input)?;
    transaction(c, |c| {
        if let Some(request_id)=input.request_id.as_deref()
          && let Some((id,payload))=c.query_row("SELECT id,request_payload FROM work_timers WHERE project_id=? AND request_id=?",
            params![project,request_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?{
            ensure!(payload==request_payload,"Request ID was already used for a different schedule");
            return timer(c,project,&id)
        }
        let task = crate::db::id();
        c.execute("INSERT INTO work_tasks(id,project_id,title,instructions,to_id,status,created_at,updated_at)
            VALUES(?,?,?,?,?,'scheduled',?,?)", params![task,project,input.name.trim(),input.prompt,input.to.as_deref().unwrap_or(""),now,now])?;
        let id = crate::db::id();
        c.execute("INSERT INTO work_timers(id,project_id,task_id,name,prompt,kind,at_ms,interval_seconds,cron,timezone,to_id,next_fire,request_id,request_payload,created_at,updated_at,purpose)
          VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![id,project,task,input.name.trim(),input.prompt,input.kind,at,interval,cron,input.timezone,
          input.to.as_deref().unwrap_or(""),next,input.request_id,request_payload,now,now,input.purpose])?;
        timer(c, project, &id)
    })
}
fn cancel_timer_queue(c: &Connection, id: &str, revision: i64, now: i64) -> Result<()> {
    let managed = if crate::manager::installed(c)? {
        " OR (e.status='managed' AND EXISTS(SELECT 1 FROM manager_inputs
            WHERE origin_event_id=e.id AND status='queued'))"
    } else {
        ""
    };
    let ids = c
        .prepare(&format!(
            "SELECT e.id FROM work_events e JOIN work_occurrences o ON o.event_id=e.id
             WHERE o.timer_id=? AND o.revision<=? AND (e.status='queued'{managed})"
        ))?
        .query_map(params![id, revision], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for event in ids {
        if !managed.is_empty() {
            c.execute("UPDATE manager_inputs SET status='cancelled' WHERE origin_event_id=? AND status='queued'", [&event])?;
        }
        c.execute("UPDATE work_events SET status='cancelled',error='Schedule changed before dispatch',updated_at=? WHERE id=? AND status IN ('queued','managed')",params![now,event])?;
        c.execute(
            "UPDATE work_occurrences SET status='cancelled' WHERE event_id=?",
            [event],
        )?;
    }
    Ok(())
}
pub fn update_timer(
    c: &Connection,
    project: &str,
    id: &str,
    input: TimerInput,
    now: i64,
) -> Result<Timer> {
    let (at, interval, cron, next) = timer_fields(&input, now)?;
    transaction(c, |c| {
        let old = timer(c, project, id)?;
        cancel_timer_queue(c, id, old.revision, now)?;
        let purpose = if input.purpose.is_empty() {
            &old.purpose
        } else {
            &input.purpose
        };
        c.execute("UPDATE work_timers SET name=?,prompt=?,kind=?,at_ms=?,interval_seconds=?,cron=?,timezone=?,to_id=?,next_fire=?,enabled=1,revision=revision+1,updated_at=?,purpose=? WHERE id=?",
          params![input.name.trim(),input.prompt,input.kind,at,interval,cron,input.timezone,input.to.as_deref().unwrap_or(""),next,now,purpose,id])?;
        c.execute(
            "UPDATE work_tasks SET title=?,instructions=?,to_id=?,updated_at=? WHERE id=?",
            params![
                input.name.trim(),
                input.prompt,
                input.to.as_deref().unwrap_or(""),
                now,
                old.task_id
            ],
        )?;
        timer(c, project, id)
    })
}
/// Soft-delete schedules so already dispatched occurrences retain their provenance.
pub fn delete_timer(c: &Connection, project: &str, id: &str, now: i64) -> Result<()> {
    transaction(c, |c| {
        let old = timer(c, project, id)?;
        cancel_timer_queue(c, id, old.revision, now)?;
        c.execute("UPDATE work_timers SET enabled=0,next_fire=NULL,revision=revision+1,updated_at=? WHERE id=?",params![now,id])?;
        Ok(())
    })
}
pub fn set_timer_enabled(
    c: &Connection,
    project: &str,
    id: &str,
    enabled: bool,
    now: i64,
) -> Result<Timer> {
    transaction(c, |c| {
        let old = timer(c, project, id)?;
        if !enabled {
            cancel_timer_queue(c, id, old.revision, now)?;
            c.execute(
                "UPDATE work_timers SET enabled=0,revision=revision+1,updated_at=? WHERE id=?",
                params![now, id],
            )?;
        } else {
            ensure!(
                old.next_fire.is_some(),
                "This completed schedule has no next occurrence; edit its start time"
            );
            c.execute(
                "UPDATE work_timers SET enabled=1,updated_at=? WHERE id=?",
                params![now, id],
            )?;
        }
        timer(c, project, id)
    })
}
fn next_after(timer: &Timer, now: i64) -> Result<Option<i64>> {
    match timer.kind.as_str() {
        "once" => Ok(None),
        "interval" => {
            let step = timer
                .interval_seconds
                .context("Schedule interval is missing")?
                .checked_mul(1000)
                .context("Schedule interval overflow")?;
            ensure!(step > 0, "Schedule interval must be positive");
            let due = timer
                .next_fire
                .context("Schedule next occurrence is missing")?;
            let elapsed = now
                .checked_sub(due)
                .context("Schedule time overflow")?
                .max(0);
            let advance = (elapsed / step + 1)
                .checked_mul(step)
                .context("Schedule time overflow")?;
            Ok(Some(
                due.checked_add(advance).context("Schedule time overflow")?,
            ))
        }
        "cron" => {
            let timezone: Tz = timer
                .timezone
                .parse()
                .context("Stored schedule timezone is invalid")?;
            cron_next(&timer.cron, timezone, now)
        }
        _ => bail!("Stored schedule kind is invalid"),
    }
}
/// One transaction records each occurrence, inserts its event, and advances the timer.
pub fn fire_due(c: &Connection, now: i64) -> Result<Vec<Event>> {
    transaction(c, |c| {
        crate::manager::settle_sources(c)?;
        let timers=c.prepare(&format!("SELECT {TIMER} FROM work_timers WHERE enabled=1 AND next_fire<=? ORDER BY next_fire,id LIMIT 100"))?
          .query_map([now],timer_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut events = Vec::new();
        for timer in timers {
            let due = timer.next_fire.context("Due schedule has no occurrence")?;
            let inserted=c.execute("INSERT OR IGNORE INTO work_occurrences(timer_id,revision,scheduled_at,status) VALUES(?,?,?,'checking')",
              params![timer.id,timer.revision,due])?;
            if inserted == 0 {
                continue;
            }
            let pending:bool=c.query_row(
              "SELECT EXISTS(SELECT 1 FROM work_events WHERE task_id=? AND status IN ('queued','submitting','submitted','uncertain','managed'))
               OR EXISTS(SELECT 1 FROM work_runs WHERE task_id=? AND status IN ('submitting','submitted','running','uncertain'))
",
              params![timer.task_id,timer.task_id],|r|r.get(0))?;
            if pending {
                c.execute("UPDATE work_occurrences SET status='skipped_pending' WHERE timer_id=? AND revision=? AND scheduled_at=?",
                  params![timer.id,timer.revision,due])?;
            } else {
                let source_id = format!("{}:{}:{}", timer.id, timer.revision, due);
                let event = insert_event(
                    c,
                    &timer.project_id,
                    &timer.task_id,
                    "timer",
                    &source_id,
                    json!({"timer_id":timer.id,"revision":timer.revision,"scheduled_at":due,"fired_at":now,
                    "instructions":timer.prompt,"to":timer.to,"timezone":timer.timezone,"purpose":timer.purpose,"catch_up":"coalesce_one"}),
                    now,
                )?;
                c.execute("UPDATE work_occurrences SET status='queued',event_id=? WHERE timer_id=? AND revision=? AND scheduled_at=?",
                  params![event.id,timer.id,timer.revision,due])?;
                c.execute(
                    "UPDATE work_tasks SET status='queued',updated_at=? WHERE id=?",
                    params![now, timer.task_id],
                )?;
                crate::manager::route_event(c, &event.id)?;
                events.push(event);
            }
            let next = next_after(&timer, now)?;
            c.execute("UPDATE work_timers SET next_fire=?,enabled=?,updated_at=? WHERE id=? AND revision=?",
              params![next,next.is_some(),now,timer.id,timer.revision])?;
        }
        Ok(events)
    })
}

pub fn execution(c: &Connection, project: &str, generation: &str) -> Result<Option<Execution>> {
    Ok(c.query_row(
        &format!("SELECT {EXECUTION} FROM work_executions WHERE project_id=? AND generation=?"),
        params![project, generation],
        execution_row,
    )
    .optional()?)
}
pub fn begin_execution(
    c: &Connection,
    project: &str,
    generation: &str,
    now: i64,
) -> Result<Execution> {
    ensure!(!generation.is_empty(), "Execution generation is required");
    let (agent, terminal, native): (String, String, String) = c.query_row(
        "SELECT agent,COALESCE(terminal,''),native_session FROM projects WHERE id=?",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    c.execute("INSERT INTO work_executions(generation,project_id,agent,terminal,native_session,status,last_activity,created_at,updated_at)
      VALUES(?,?,?,?,?,'starting',?,?,?) ON CONFLICT(generation) DO UPDATE SET terminal=excluded.terminal,native_session=excluded.native_session,updated_at=excluded.updated_at",
      params![generation,project,agent,terminal,native,now,now,now])?;
    execution(c, project, generation)?.context("Execution record is missing")
}
fn activity(c: &Connection, project: &str, generation: &str, now: i64) -> Result<()> {
    c.execute("UPDATE work_executions SET status='running',last_activity=MAX(last_activity,?),idle_since=NULL,warm_until=NULL,updated_at=? WHERE project_id=? AND generation=? AND status NOT IN ('sleeping','stopped','exited','error')",
      params![now,now,project,generation])?;
    Ok(())
}
/// Coverage changes and child activity invalidate the whole warm interval and handoff.
pub fn observe_children(
    c: &Connection,
    project: &str,
    generation: &str,
    coverage: &Value,
    now: i64,
) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    let revision = coverage["revision"].as_str();
    let previous: Option<String> = tx
        .query_row(
            "SELECT child_revision FROM work_executions WHERE project_id=? AND generation=?",
            params![project, generation],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if coverage["state"] != "quiescent" || revision.is_none() || previous.as_deref() != revision {
        activity(&tx, project, generation, now)?;
        tx.execute("UPDATE idle_handoffs SET status='cancelled',error='Native child activity or coverage changed' WHERE project_id=? AND generation=? AND status='requested'", params![project,generation])?;
    }
    tx.execute(
        "UPDATE work_executions SET child_revision=? WHERE project_id=? AND generation=?",
        params![revision, project, generation],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn touch_execution(c: &Connection, project: &str, generation: &str, now: i64) -> Result<()> {
    activity(c, project, generation, now)
}
pub fn set_native_session(
    c: &Connection,
    project: &str,
    generation: &str,
    native: &str,
    now: i64,
) -> Result<()> {
    ensure!(
        !native.is_empty(),
        "Native session identity cannot be empty"
    );
    ensure!(c.execute("UPDATE work_executions SET native_session=?,updated_at=? WHERE project_id=? AND generation=?",
      params![native,now,project,generation])?==1,"Execution not found");
    Ok(())
}

fn lease_row(r: &Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        project_id: r.get(0)?,
        generation: r.get(1)?,
        active: r.get(2)?,
        updated_at: r.get(3)?,
    })
}
pub fn set_takeover(
    c: &Connection,
    project: &str,
    generation: &str,
    active: bool,
    now: i64,
) -> Result<Lease> {
    ensure!(
        !generation.is_empty(),
        "Execution generation is required for terminal control"
    );
    c.execute("INSERT INTO work_leases(project_id,generation,active,updated_at) VALUES(?,?,?,?)
      ON CONFLICT(project_id,generation) DO UPDATE SET active=excluded.active,updated_at=excluded.updated_at",
      params![project,generation,active,now])?;
    activity(c, project, generation, now)?;
    Ok(Lease {
        project_id: project.into(),
        generation: generation.into(),
        active,
        updated_at: now,
    })
}
pub fn has_takeover(c: &Connection, project: &str, generation: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_leases WHERE project_id=? AND generation=? AND active=1)",
        params![project, generation],
        |r| r.get(0),
    )?)
}

pub fn has_project_takeover(c: &Connection, project: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM work_leases l
            LEFT JOIN work_executions e ON e.project_id=l.project_id AND e.generation=l.generation
            WHERE l.project_id=? AND l.active=1
              AND (e.status IS NULL OR e.status NOT IN ('sleeping','stopped','exited','error'))
              AND (EXISTS(SELECT 1 FROM projects p WHERE p.id=l.project_id
                          AND p.generation=l.generation AND p.terminal IS NOT NULL)
                   OR EXISTS(SELECT 1 FROM manager_workers w WHERE w.project_id=l.project_id
                             AND w.generation=l.generation AND w.state IN ('starting','ready')))
        )",
        [project],
        |row| row.get(0),
    )?)
}

/// Call only after the execution adapter has independently established native idle.
/// A clean observation starts the warm period. The journal never proves business success.
pub fn execution_idle_observed(
    c: &Connection,
    project: &str,
    generation: &str,
    sequence: i64,
    now: i64,
) -> Result<bool> {
    let tx = c.unchecked_transaction()?;
    let previous: Option<i64> = tx
        .query_row(
            "SELECT idle_seq FROM work_executions WHERE project_id=? AND generation=?",
            params![project, generation],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if previous != Some(sequence) {
        tx.execute("UPDATE work_executions SET idle_seq=?,idle_since=NULL,warm_until=NULL WHERE project_id=? AND generation=?",params![sequence,project,generation])?;
    }
    let ready = execution_idle(&tx, project, generation, now)?;
    tx.commit()?;
    Ok(ready)
}

pub fn execution_idle(c: &Connection, project: &str, generation: &str, now: i64) -> Result<bool> {
    transaction(c, |c| {
        let Some(record) = execution(c, project, generation)? else {
            return Ok(false);
        };
        if ["sleeping", "stopped", "exited", "error"].contains(&record.status.as_str()) {
            return Ok(false);
        }
        let pending: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_events WHERE project_id=? AND status='queued')",
            [project],
            |r| r.get(0),
        )?;
        if pending || has_takeover(c, project, generation)? {
            c.execute("UPDATE work_executions SET idle_since=NULL,warm_until=NULL,updated_at=? WHERE project_id=? AND generation=?",
              params![now,project,generation])?;
            return Ok(false);
        }
        let idle_since = record.idle_since.unwrap_or(now.max(record.last_activity));
        let warm_until = idle_since
            .checked_add(WARM_MS)
            .context("Warm period time overflow")?;
        c.execute("UPDATE work_executions SET status='idle',idle_since=?,warm_until=?,updated_at=? WHERE project_id=? AND generation=?",
          params![idle_since,warm_until,now,project,generation])?;
        Ok(now >= warm_until)
    })
}
pub fn finish_execution(
    c: &Connection,
    project: &str,
    generation: &str,
    status: &str,
    error: &str,
    now: i64,
) -> Result<()> {
    ensure!(
        ["sleeping", "stopped", "exited", "error"].contains(&status),
        "Invalid finished execution status"
    );
    transaction(c, |c| {
        c.execute("UPDATE work_executions SET status=?,error=?,idle_since=NULL,warm_until=NULL,updated_at=? WHERE project_id=? AND generation=?",
          params![status,error,now,project,generation])?;
        c.execute(
            "UPDATE work_leases SET active=0,updated_at=? WHERE project_id=? AND generation=?",
            params![now, project, generation],
        )?;
        // An interrupted execution does not establish whether its submitted effects happened.
        c.execute("UPDATE work_runs SET status='uncertain',error=?,updated_at=? WHERE project_id=? AND generation=? AND status IN ('submitting','submitted','running')",
          params![error,now,project,generation])?;
        c.execute("UPDATE work_events SET status='uncertain',error=?,updated_at=? WHERE status!='uncertain' AND id IN(
          SELECT event_id FROM work_runs WHERE project_id=? AND generation=? AND status='uncertain')",params![error,now,project,generation])?;
        c.execute(
            "UPDATE messages SET status='uncertain',error=?,updated_at=? WHERE status!='uncertain' AND id IN(
          SELECT e.message_id FROM work_events e JOIN work_runs r ON r.event_id=e.id
          WHERE r.project_id=? AND r.generation=? AND r.status='uncertain')",
            params![error, now, project, generation],
        )?;
        c.execute("UPDATE work_tasks SET status='uncertain',updated_at=? WHERE status!='uncertain' AND id IN(
          SELECT task_id FROM work_runs WHERE project_id=? AND generation=? AND status='uncertain')",params![now,project,generation])?;
        crate::manager::worker_changed(c, project)?;
        Ok(())
    })
}

/// Repeated observations update one open item; acknowledging a notification does not resolve it.
#[allow(clippy::too_many_arguments)]
pub fn upsert_attention(
    c: &Connection,
    project: &str,
    generation: &str,
    reason: &str,
    message: &str,
    to: &str,
    now: i64,
) -> Result<Value> {
    ensure!(
        !reason.is_empty() && !message.trim().is_empty(),
        "Attention reason and explanation are required"
    );
    valid_to(Some(to))?;
    transaction(c, |c| {
        let existing=c.query_row("SELECT id FROM work_attention WHERE project_id=? AND generation=? AND reason=? AND status='open'",
          params![project,generation,reason],|r|r.get::<_,String>(0)).optional()?;
        let created = existing.is_none();
        let id = existing.unwrap_or_else(crate::db::id);
        if created {
            c.execute("INSERT INTO work_attention(id,project_id,generation,reason,message,to_id,first_seen,last_seen)
              VALUES(?,?,?,?,?,?,?,?)",params![id,project,generation,reason,message,to,now,now])?;
        } else {
            c.execute("UPDATE work_attention SET message=?,to_id=CASE WHEN ?='' THEN to_id ELSE ? END,occurrences=occurrences+1,last_seen=? WHERE id=?",
              params![message,to,to,now,id])?;
        }
        let item = c.query_row(
            &format!("SELECT {ATTENTION} FROM work_attention WHERE id=?"),
            [id],
            attention_row,
        )?;
        let mut value = serde_json::to_value(item)?;
        value["created"] = Value::Bool(created);
        Ok(value)
    })
}
pub fn resolve_attention(
    c: &Connection,
    project: &str,
    generation: &str,
    now: i64,
) -> Result<usize> {
    Ok(c.execute("UPDATE work_attention SET status='resolved',resolved_at=?,last_seen=? WHERE project_id=? AND generation=? AND status='open'",
      params![now,now,project,generation])?)
}
pub fn resolve_attention_reason(
    c: &Connection,
    project: &str,
    generation: &str,
    reason: &str,
    now: i64,
) -> Result<usize> {
    Ok(c.execute("UPDATE work_attention SET status='resolved',resolved_at=?,last_seen=? WHERE project_id=? AND generation=? AND reason=? AND status='open'",
      params![now,now,project,generation,reason])?)
}

/// Result recovery clears warnings only after every unresolved effect was reviewed.
pub fn resolve_recovery_attention(c: &Connection, project: &str, now: i64) -> Result<usize> {
    let unresolved:bool=c.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_events WHERE project_id=?1 AND status IN ('submitting','submitted','uncertain'))
         OR EXISTS(SELECT 1 FROM work_runs WHERE project_id=?1 AND finished_at IS NULL AND status IN ('submitting','submitted','running','uncertain'))",
        [project],|r|r.get(0))?;
    if unresolved {
        return Ok(0);
    }
    Ok(c.execute(
        "UPDATE work_attention SET status='resolved',resolved_at=?1,last_seen=?1
         WHERE project_id=?2 AND status='open' AND reason IN ('uncertain_input','execution_lost','unfinished_run')",
        params![now,project])?)
}

/// Explicit user recovery applies to one event; uncertain effects are never replayed automatically.
pub fn resolve_event(
    c: &Connection,
    project: &str,
    id: &str,
    retry: bool,
    now: i64,
) -> Result<Event> {
    transaction(c, |c| {
        let current = event(c, project, id)?;
        ensure!(
            ["uncertain", "failed", "queued", "worker_waiting", "submitted", "acknowledged"]
                .contains(&current.status.as_str()),
            "This event cannot be resolved"
        );
                if retry && current.source=="manager_worker" {
            let parent = crate::db::project_by_key(c,project)?;
            let generation = current.payload["target_generation"].as_str().context("Missing task worker generation")?;
            crate::manager_workers::authorize(c,&parent,generation,true)?;
        }
        let status = if !retry { "dismissed" } else if current.source=="manager_worker" { "worker_waiting" } else { "queued" };
        c.execute(
            "UPDATE work_events SET status=?,error='',updated_at=? WHERE id=?",
            params![status, now, id],
        )?;
        c.execute("UPDATE work_runs SET status='interrupted',error='Resolved by the user',finished_at=?,updated_at=? WHERE event_id=? AND finished_at IS NULL",params![now,now,id])?;
        c.execute(
            "UPDATE work_tasks SET status=?,updated_at=? WHERE id=?",
            params![
                if retry { "queued" } else { "cancelled" },
                now,
                current.task_id
            ],
        )?;
        if let Some(message) = current.message_id {
            c.execute(
                "UPDATE messages SET status=?,error='',updated_at=? WHERE id=?",
                params![status, now, message],
            )?;
        }
        if retry {
            crate::manager::route_event(c, id)?;
        }
        resolve_recovery_attention(c, project, now)?;
        event(c, project, id)
    })
}
pub fn snapshot(c: &Connection, project: &str) -> Result<WorkState> {
    let runs = c
        .prepare(&format!(
            "SELECT {RUN} FROM work_runs WHERE project_id=? ORDER BY started_at DESC LIMIT 100"
        ))?
        .query_map([project], run_row)?
        .collect::<rusqlite::Result<_>>()?;
    let seats: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='manager_workers')",
        [], |row| row.get(0)
    )?;
    let primary_scope = if seats {
        "AND generation NOT IN (SELECT generation FROM manager_workers WHERE project_id=?1)"
    } else {
        ""
    };
    let execution=c.query_row(&format!("SELECT {EXECUTION} FROM work_executions WHERE project_id=?1 {primary_scope}
        AND generation=COALESCE((SELECT NULLIF(generation,'') FROM projects WHERE id=?1),generation)
        ORDER BY created_at DESC,rowid DESC LIMIT 1"), [project],execution_row).optional()?;
    let lease = if let Some(execution) = &execution {
        c.query_row("SELECT project_id,generation,active,updated_at FROM work_leases WHERE project_id=? AND generation=?",
          params![project,execution.generation],lease_row).optional()?
    } else {
        None
    };
    Ok(WorkState{
        events:events(c,project)?,runs,execution,lease,
        timers:c.prepare(&format!("SELECT {TIMER} FROM work_timers WHERE project_id=? ORDER BY created_at DESC"))?
          .query_map([project],timer_row)?.collect::<rusqlite::Result<_>>()?,
        attention:c.prepare(&format!("SELECT {ATTENTION} FROM work_attention WHERE project_id=? ORDER BY (status='open') DESC,last_seen DESC LIMIT 100"))?
          .query_map([project],attention_row)?.collect::<rusqlite::Result<_>>()?,
    })
}

/// Record the native transition after prompt submission, without an Agent receipt.
/// A stale idle snapshot cannot settle a newly submitted prompt. Uncertain input
/// remains uncertain even if the CLI later becomes idle; it is never replayed.
pub fn observe_idle(
    c: &Connection,
    project: &str,
    generation: &str,
    sequence: Option<i64>,
    now: i64,
) -> Result<()> {
    transaction(c, |c| {
        c.execute(
            "UPDATE work_runs SET status='settled',finished_at=?,updated_at=?
            WHERE project_id=? AND generation=? AND status IN ('submitted','running')
            AND (native_seq IS NULL OR native_seq<?
                 OR (? IS NULL AND updated_at<=?-2000))",
            params![now, now, project, generation, sequence, sequence, now],
        )?;
        c.execute("UPDATE work_events SET status='delivered',updated_at=? WHERE id IN
            (SELECT event_id FROM work_runs WHERE project_id=? AND generation=? AND status='settled')
            AND status IN ('submitted','acknowledged')",params![now,project,generation])?;
        crate::manager::worker_changed(c, project)?;
        Ok(())
    })
}

pub fn submission_sequence(c: &Connection, run: &str, sequence: Option<i64>) -> Result<()> {
    c.execute(
        "UPDATE work_runs SET native_seq=? WHERE id=?",
        params![sequence, run],
    )?;
    Ok(())
}
