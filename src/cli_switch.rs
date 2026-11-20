//! Explicit, cooperative CLI handoff. Automatic idle retirement is stricter.
use crate::{
    App,
    config::Config,
    db::{self, Project},
    handoff, herdr, native_children, work,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

const BUDGET_MS: i64 = 180_000;
const RECOVERY_BUDGET_MS: i64 = 3 * BUDGET_MS;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub target: String,
    pub phase: String,
    pub error: String,
    pub generation: String,
    pub nonce: String,
    pub baseline: String,
    pub sequence: i64,
    pub revision: String,
    pub created_at: i64,
    pub attempts: i64,
    pub holds_input: bool,
    pub phase_started_at: i64,
    pub next_attempt_at: i64,
    pub recovery_attempts: i64,
    pub recovery_generation: String,
    pub intent: String,
    pub requested_model: Option<crate::models::Selection>,
    #[serde(skip_serializing)]
    pub source: Value,
}
pub fn migrate(c: &Connection, recover: bool) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS cli_switch_requests (
      id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
      target TEXT NOT NULL,phase TEXT NOT NULL,error TEXT NOT NULL DEFAULT '',generation TEXT NOT NULL,
      nonce TEXT NOT NULL DEFAULT '',baseline TEXT NOT NULL DEFAULT '',sequence INTEGER NOT NULL DEFAULT 0,
      revision TEXT NOT NULL DEFAULT '',created_at INTEGER NOT NULL,attempts INTEGER NOT NULL DEFAULT 0);
      CREATE INDEX IF NOT EXISTS cli_switch_project ON cli_switch_requests(project_id,created_at);")?;
    let has_hold = c
        .prepare("PRAGMA table_info(cli_switch_requests)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "holds_input");
    if !has_hold {
        // Pre-hold failed requests were settled and may have resumed the old CLI.
        // Only outstanding transitions gain a hold during this schema upgrade.
        c.execute_batch("ALTER TABLE cli_switch_requests ADD COLUMN holds_input INTEGER NOT NULL DEFAULT 0;
          UPDATE cli_switch_requests SET holds_input=1 WHERE phase IN ('preparing','requested','switching','starting','startup_failed','uncertain');")?;
    }
    for (name, sql) in [
        ("source", "TEXT NOT NULL DEFAULT '{}'"),
        ("phase_started_at", "INTEGER NOT NULL DEFAULT 0"),
        ("next_attempt_at", "INTEGER NOT NULL DEFAULT 0"),
        ("recovery_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("recovery_generation", "TEXT NOT NULL DEFAULT ''"),
    ] {
        let exists: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('cli_switch_requests') WHERE name=?)",
            [name],
            |r| r.get(0),
        )?;
        if !exists {
            c.execute_batch(&format!(
                "ALTER TABLE cli_switch_requests ADD COLUMN {name} {sql}"
            ))?;
        }
    }
    c.execute(
        "UPDATE cli_switch_requests SET phase_started_at=created_at WHERE phase_started_at=0",
        [],
    )?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS cli_repair_prompts (
        id TEXT PRIMARY KEY,request_id TEXT NOT NULL REFERENCES cli_switch_requests(id) ON DELETE CASCADE,
        generation TEXT NOT NULL,payload TEXT NOT NULL,status TEXT NOT NULL,sequence INTEGER NOT NULL,
        created_at INTEGER NOT NULL,error TEXT NOT NULL DEFAULT '');
      CREATE TABLE IF NOT EXISTS cli_repair_instructions (
        request_id TEXT NOT NULL REFERENCES cli_switch_requests(id) ON DELETE CASCADE,
        instruction_id TEXT NOT NULL,payload TEXT NOT NULL,prompt_id TEXT NOT NULL DEFAULT '',
        PRIMARY KEY(request_id,instruction_id));")?;
    if recover {
        // An interrupted send may already have reached the native Agent. Observe
        // its nonce/sequence; never resubmit that prompt after a service restart.
        c.execute(
            "UPDATE cli_repair_prompts SET status='uncertain' WHERE status='submitting'",
            [],
        )?;
        c.execute("UPDATE cli_switch_requests SET phase=CASE WHEN phase='switching' THEN 'uncertain' ELSE 'recovery_pending' END,phase_started_at=?,next_attempt_at=0,error='The service restarted during the CLI handoff; automatic recovery preserves the native conversation and queued input.' WHERE phase IN ('preparing','requested','switching')", [db::now()])?;
    }
    Ok(())
}
pub fn latest(c: &Connection, project: &str) -> Result<Option<Request>> {
    Ok(c.query_row("SELECT id,target,phase,error,generation,nonce,baseline,sequence,revision,created_at,attempts,holds_input,phase_started_at,next_attempt_at,recovery_attempts,recovery_generation,source FROM cli_switch_requests WHERE project_id=? ORDER BY rowid DESC LIMIT 1",[project],|r| {
        let source:Value=serde_json::from_str(&r.get::<_,String>(16)?).unwrap_or(Value::Null);
        let intent=source["intent"].as_str().unwrap_or("cli").to_owned();
        let requested_model=if intent=="model" {serde_json::from_value(source["target_model"].clone()).ok().flatten()} else {None};
        Ok(Request{id:r.get(0)?,target:r.get(1)?,phase:r.get(2)?,error:r.get(3)?,generation:r.get(4)?,nonce:r.get(5)?,baseline:r.get(6)?,sequence:r.get(7)?,revision:r.get(8)?,created_at:r.get(9)?,attempts:r.get(10)?,holds_input:r.get(11)?,phase_started_at:r.get(12)?,next_attempt_at:r.get(13)?,recovery_attempts:r.get(14)?,recovery_generation:r.get(15)?,intent,requested_model,source})
    }).optional()?)
}

pub(crate) fn blocks_worker(r: &Request) -> bool {
    r.holds_input || r.phase == "recovery_exhausted"
}

pub fn active(r: &Request) -> bool {
    r.holds_input && !matches!(r.phase.as_str(), "ready" | "cancelled")
}

fn model_repair_can_be_corrected(r: &Request) -> bool {
    r.source["intent"] == "model"
        && active(r)
        && (matches!(
            r.phase.as_str(),
            "failed"
                | "startup_failed"
                | "uncertain"
                | "recovery_pending"
                | "recovery_stopping"
                | "recovery_starting"
                | "repairing"
        ) || r.phase == "requested" && !r.recovery_generation.is_empty())
}
pub(crate) fn retiring_candidate(r: &Request, execution: &Value, generation: &str) -> bool {
    (execution["cli_switch_request"] == r.id && execution["cli_recovery_request"] != r.id)
        || r.source["retiring_candidates"]
            .as_array()
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate["generation"] == generation
                        && candidate["request_id"] == execution["cli_switch_request"]
                        && execution["cli_switch_request"].is_string()
                })
            })
}
fn remember_retiring_candidate(source: &mut Value, execution: &Value, generation: &str) {
    if generation.is_empty() {
        return;
    }
    let Some(request) = execution["cli_switch_request"].as_str() else {
        return;
    };
    let mut candidates = source["retiring_candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !candidates.iter().any(|candidate| {
        candidate["generation"] == generation && candidate["request_id"] == request
    }) {
        candidates.push(json!({"generation":generation,"request_id":request}));
    }
    source["retiring_candidates"] = json!(candidates);
}

/// A repair remains available after a completed switch or a runtime failure.
/// Reuse its saved original CLI when possible; never invent a source session.
pub fn ensure_repair_request(c: &Connection, project: &str) -> Result<Request> {
    let previous = latest(c, project)?;
    if let Some(r) = previous
        .as_ref()
        .filter(|r| active(r) || r.phase == "recovery_exhausted")
    {
        return Ok(r.clone());
    }
    let (agent, native, args, generation, execution): (String, String, String, String, String) = c
        .query_row(
            "SELECT agent,native_session,extra_args,generation,execution FROM projects WHERE id=?",
            [project],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
    let mut source = if let Some(previous) =
        previous.filter(|r| r.phase == "ready" && r.target == agent)
    {
        if previous.source["agent"]
            .as_str()
            .is_some_and(|a| !a.is_empty())
        {
            previous.source
        } else {
            let historical:Option<(String,String)> = c.query_row(
                "SELECT agent,native_session FROM work_executions WHERE project_id=? AND generation=?",params![project,previous.generation],
                |row|Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            if let Some((source_agent, source_native)) = historical {
                json!({"agent":source_agent,"native_session":source_native,"extra_args":[],"generation":previous.generation,"historical_arguments_unavailable":true})
            } else {
                json!({"agent":agent,"native_session":native,"extra_args":serde_json::from_str::<Value>(&args).unwrap_or(json!([])),"generation":generation})
            }
        }
    } else {
        json!({"agent":agent,"native_session":native,"extra_args":serde_json::from_str::<Value>(&args).unwrap_or(json!([])),"generation":generation})
    };
    // Runtime repair preserves both sides of a previous cross-CLI switch:
    // the original repair owner and the current target's native conversation.
    if source.get("selection").is_none() {
        let original = source["agent"].as_str().unwrap_or(&agent);
        source["selection"] = json!(crate::models::load(c, project, original)?);
    }
    let target_model = crate::models::load(c, project, &agent)?;
    if source["intent"] == "model" {
        source["derived_runtime_repair"] = json!(true);
        source["target_model"] = json!(target_model);
        source["model_payload"] = json!({"selection":target_model,"expected":null});
        let execution: Value = serde_json::from_str(&execution)?;
        remember_retiring_candidate(&mut source, &execution, &generation);
    } else {
        source["intent"] = json!("runtime");
    }
    source["runtime_target"] = json!({"agent":agent,"native_session":native,"extra_args":serde_json::from_str::<Value>(&args).unwrap_or(json!([])),"selection":target_model});
    c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input,source,phase_started_at,error) VALUES(?,?,?,'recovery_pending',?,?,1,?,?,?)",params![db::id(),project,agent,generation,db::now(),source.to_string(),db::now(),"Repairing this project's CLI runtime while preserving pending input and native continuity."])?;
    latest(c, project)?.ok_or_else(|| anyhow::anyhow!("Repair request was not journaled"))
}

/// Recovery control is journaled independently of ordinary held messages. Only
/// the trusted ingress layer chooses whether a message is a repair instruction.
pub fn journal_repair_instruction(
    c: &Connection,
    project: &str,
    instruction_id: &str,
    payload: &Value,
) -> Result<()> {
    let r = ensure_repair_request(c, project)?;
    ensure!(
        !instruction_id.is_empty(),
        "Provide a repair instruction ID"
    );
    let encoded = serde_json::to_string(payload)?;
    let previous: Option<String> = c
        .query_row(
            "SELECT payload FROM cli_repair_instructions WHERE request_id=? AND instruction_id=?",
            params![r.id, instruction_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(previous) = previous {
        ensure!(
            previous == encoded,
            "This repair instruction ID already has different content"
        );
    } else {
        c.execute(
            "INSERT INTO cli_repair_instructions(request_id,instruction_id,payload) VALUES(?,?,?)",
            params![r.id, instruction_id, encoded],
        )?;
    }
    Ok(())
}

pub fn revision(c: &Connection, project: &str) -> Result<String> {
    Ok(c.query_row("SELECT COALESCE((SELECT MAX(rowid) FROM messages WHERE project_id=?1 AND direction='in'),0)||':'||COALESCE((SELECT MAX(rowid) FROM work_events WHERE project_id=?1),0)",[project],|r|r.get(0))?)
}
/// Called inside each delivery claim transaction, not just by reconciliation.
pub fn holds_input(c: &Connection, project: &str) -> Result<bool> {
    Ok(latest(c, project)?.is_some_and(|r| r.holds_input))
}
pub fn cancel(c: &Connection, project: &str, error: &str) -> Result<()> {
    c.execute("UPDATE cli_switch_requests SET phase='cancelled',holds_input=0,error=? WHERE project_id=? AND (holds_input=1 OR phase IN ('preparing','requested','switching','starting','startup_failed','uncertain','recovery_exhausted'))",params![error,project])?;
    Ok(())
}
/// Only after a verified stop: abandon the transition and pause atomically, so
/// releasing its delivery hold cannot automatically wake the old CLI.
pub fn stopped(c: &Connection, project: &str, generation: &str) -> Result<()> {
    stopped_mode(c, project, generation, true)
}
pub(crate) fn stopped_mode(
    c: &Connection,
    project: &str,
    generation: &str,
    cancel_intake: bool,
) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    let held = holds_input(&tx, project)?;
    let managed = cancel_intake && crate::manager::installed(&tx)?;
    if managed {
        crate::manager::stop(&tx, project)?;
    }
    if cancel_intake {
        tx.execute("UPDATE feishu_group_intake SET status='cancelled' WHERE project_id=? AND status='pending'",[project])?;
    }
    work::finish_execution(
        &tx,
        project,
        generation,
        "stopped",
        "Stopped by the user",
        db::now(),
    )?;
    cancel(
        &tx,
        project,
        "Stopped explicitly by the user; pending input is paused",
    )?;
    ensure!(tx.execute("UPDATE projects SET terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null',paused=CASE WHEN ? THEN 1 ELSE paused END WHERE id=? AND generation=?",params![held || managed,project,generation])?==1,"Execution changed before stop completion");
    tx.commit()?;
    Ok(())
}
fn retire_vanished(c: &Connection, p: &Project, r: &Request) -> Result<bool> {
    let tx = c.unchecked_transaction()?;
    let saved: (String, Option<String>, Option<i64>, String, bool) = tx.query_row(
        "SELECT generation,terminal,terminal_pid,execution,paused FROM projects WHERE id=?",
        [&p.id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    if saved.0 != p.generation
        || saved.1 != p.terminal
        || saved.2 != p.terminal_pid
        || serde_json::from_str::<Value>(&saved.3)? != p.execution
        || saved.4
        || work::has_takeover(&tx, &p.id, &p.generation)?
        || !latest(&tx, &p.id)?.is_some_and(|current| current.id == r.id && current.holds_input)
    {
        return Ok(false);
    }
    work::finish_execution(
        &tx,
        &p.id,
        &p.generation,
        "exited",
        "The immutable HERDR terminal and its root process disappeared; submitted effects remain uncertain",
        db::now(),
    )?;
    ensure!(tx.execute("UPDATE projects SET terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null' WHERE id=? AND generation=? AND paused=0",params![p.id,p.generation])? == 1,"Execution changed during disappearance recovery");
    phase(
        &tx,
        &r.id,
        "recovery_pending",
        "The old native execution is verified gone; restoring its saved repair conversation without touching the reused pane",
    )?;
    tx.commit()?;
    Ok(true)
}

fn phase(c: &Connection, id: &str, value: &str, error: &str) -> Result<()> {
    c.execute(
        "UPDATE cli_switch_requests SET phase_started_at=CASE WHEN phase!=? THEN ? ELSE phase_started_at END,phase=?,error=?,next_attempt_at=0 WHERE id=?",
        params![value, db::now(), value, error, id],
    )?;
    Ok(())
}
fn record_failure(c: &Connection, id: &str, error: &str) -> Result<()> {
    c.execute(
        "UPDATE cli_switch_requests SET source=json_set(source,'$.last_failure',?) WHERE id=?",
        params![error, id],
    )?;
    phase(c, id, "recovery_pending", error)?;
    notify_manager(c, id, "recovery_pending", error)
}

fn notify_manager(c: &Connection, id: &str, state: &str, error: &str) -> Result<()> {
    let project: String = c.query_row(
        "SELECT project_id FROM cli_switch_requests WHERE id=?",
        [id],
        |row| row.get(0),
    )?;
    crate::manager::observe_cli_transition(c, &project, id, state, error)
}

fn recovery_budget_spent(request: &Request) -> bool {
    let started = request.source["recovery_budget_started_at"]
        .as_i64().unwrap_or(request.created_at);
    db::now().saturating_sub(started) >= RECOVERY_BUDGET_MS
}

fn suspend_recovery(c: &Connection, id: &str) -> Result<bool> {
    let reason = "CLI recovery timed out. Pending work is preserved. Retry recovery or stop the current execution before choosing a different CLI.";
    let changed = c.execute(
        "UPDATE cli_switch_requests SET phase='recovery_exhausted',holds_input=0,error=?,next_attempt_at=0 WHERE id=? AND holds_input=1",
        params![reason, id],
    )?;
    if changed == 1 {
        notify_manager(c, id, "recovery_exhausted", reason)?;
    }
    Ok(changed == 1)
}

pub fn proof(r: &Request, text: Option<&str>, sequence: Option<i64>) -> bool {
    text.is_some_and(|t| {
        t.len() > 100
            && t.contains(&format!("<!-- dauerbot-switch-ready:{} -->", r.nonce))
            && handoff::fingerprint(Some(t)) != r.baseline
    }) && sequence.is_some_and(|s| s > r.sequence)
}
fn stop_retry_safe(p: &Project, r: &Request, record: &Value, text: Option<&str>) -> bool {
    crate::repair_lane::foreground_quiescent(p, record)
        && record["children"]["state"] != "active"
        && record["state_change_seq"]
            .as_i64()
            .is_some_and(|seq| seq > r.sequence)
        && text.is_some_and(|text| {
            text.contains(&format!("<!-- dauerbot-switch-ready:{} -->", r.nonce))
        })
        && handoff::fingerprint(text) == r.baseline
}

pub fn claim(c: &Connection, project: &str, r: &Request, accepted_hash: &str) -> Result<bool> {
    let tx = c.unchecked_transaction()?;
    if work::has_takeover(&tx, project, &r.generation)? {
        return Ok(false);
    }
    let valid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=? AND generation=? AND paused=0)",
        params![project, r.generation],
        |v| v.get(0),
    )?;
    if !valid {
        return Ok(false);
    }
    let changed=tx.execute("UPDATE cli_switch_requests SET phase='switching',phase_started_at=?,baseline=? WHERE id=? AND phase='requested' AND nonce=? AND holds_input=1 AND rowid=(SELECT MAX(rowid) FROM cli_switch_requests WHERE project_id=?)",params![db::now(),accepted_hash,r.id,r.nonce,project])?;
    tx.commit()?;
    Ok(changed == 1)
}
fn target_selection(r: &Request) -> Result<Option<Option<crate::models::Selection>>> {
    let value = if r.source["intent"] == "model" {
        Some(&r.source["target_model"])
    } else if r.source["runtime_target"]["agent"] == r.target {
        r.source["runtime_target"].get("selection")
    } else {
        None
    };
    value
        .map(|value| serde_json::from_value(value.clone()).map_err(Into::into))
        .transpose()
}
fn target_selection_matches(r: &Request, execution: &Value) -> bool {
    match target_selection(r) {
        Ok(Some(expected)) => execution.get("model_selection") == Some(&json!(expected)),
        Ok(None) => true,
        Err(_) => false,
    }
}
#[derive(Debug, PartialEq, Eq)]
enum NativeResume {
    Fresh,
    Unconfirmed,
    Matching,
    Forked,
}
fn native_resume(agent: &str, execution: &Value, observed: &str) -> NativeResume {
    let Some(expected) = execution["intended_native_session"]
        .as_str()
        .filter(|s| !s.is_empty())
    else {
        return NativeResume::Fresh;
    };
    // A saved project pointer alone is not evidence from this generation.
    if execution["native_confirmed"] != true || observed.is_empty() {
        return NativeResume::Unconfirmed;
    }
    let same = herdr::same_native_session(agent, expected, observed);
    if same {
        NativeResume::Matching
    } else {
        NativeResume::Forked
    }
}
fn target_native_matches(r: &Request, execution: &Value, observed: &str) -> bool {
    execution.get("repair_lane").is_none()
        && matches!(
            native_resume(&r.target, execution, observed),
            NativeResume::Fresh | NativeResume::Matching
        )
}
fn target_launch_state(c: &Connection, project: &str, r: &Request) -> Result<(String, String)> {
    let runtime = &r.source["runtime_target"];
    let resume = runtime["agent"] == r.target;
    let mut native = if resume {
        runtime["native_session"].as_str().unwrap_or("").to_owned()
    } else {
        String::new()
    };
    let mut args: Vec<String> = if resume {
        serde_json::from_value(runtime["extra_args"].clone())?
    } else {
        vec![]
    };
    if r.source["intent"] == "model" {
        let current_native: String = c.query_row(
            "SELECT native_session FROM projects WHERE id=?",
            [project],
            |row| row.get(0),
        )?;
        if !current_native.is_empty() {
            native = current_native;
        }
        if r.source["target_model"].is_null() {
            args = crate::models::clear_model_args(&r.target, &args)?;
        }
    }
    if let Some(selection) = target_selection(r)? {
        crate::models::save(c, project, &r.target, selection.as_ref())?;
    }
    Ok((native, serde_json::to_string(&args)?))
}

fn restore_source_state(c: &Connection, project: &str, source: &Value) -> Result<()> {
    if let Some(selection) = source.get("selection") {
        let selection: Option<crate::models::Selection> =
            serde_json::from_value(selection.clone())?;
        crate::models::save(
            c,
            project,
            source["agent"].as_str().unwrap_or(""),
            selection.as_ref(),
        )?;
    }
    ensure!(c.execute("UPDATE projects SET agent=?,native_session=?,extra_args=?,generation='',ready=0,execution='null' WHERE id=? AND terminal IS NULL",params![source["agent"].as_str().unwrap_or(""),source["native_session"].as_str().unwrap_or(""),source["extra_args"].to_string(),project])?==1,"Project woke during recovery");
    Ok(())
}

fn ready(c: &Connection, project: &str, generation: &str, r: &Request) -> Result<bool> {
    let tx = c.unchecked_transaction()?;
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE id=? AND generation=? AND agent=? AND terminal IS NOT NULL AND ready=1 AND paused=0 AND json_extract(execution,'$.cli_switch_request')=? AND json_extract(execution,'$.mcp_registered')=1)",params![project,generation,r.target,r.id],|v|v.get(0))?;
    let (execution, native): (String, String) = tx.query_row(
        "SELECT execution,native_session FROM projects WHERE id=?",
        [project],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let execution: Value = serde_json::from_str(&execution)?;
    if !valid
        || !target_selection_matches(r, &execution)
        || !target_native_matches(r, &execution, &native)
        || work::has_takeover(&tx, project, generation)?
    {
        return Ok(false);
    }
    let changed=tx.execute("UPDATE cli_switch_requests SET phase='ready',error='',holds_input=0 WHERE id=? AND phase IN ('starting','startup_failed','failed','recovery_pending') AND rowid=(SELECT MAX(rowid) FROM cli_switch_requests WHERE project_id=?)",params![r.id,project])?;
    if changed == 1 {
        notify_manager(&tx, &r.id, "ready", "")?;
    }
    tx.commit()?;
    Ok(changed == 1)
}
fn complete(c: &Connection, project: &str, r: &Request) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    ensure!(tx.execute("UPDATE cli_switch_requests SET phase='starting',phase_started_at=?,next_attempt_at=0 WHERE id=? AND phase IN ('switching','uncertain')",params![db::now(),r.id])?==1,"CLI transition changed");
    tx.execute("UPDATE cli_switch_requests SET source=json_set(source,'$.native_session',(SELECT native_session FROM projects WHERE id=?)) WHERE id=?", params![project,r.id])?;
    work::finish_execution(
        &tx,
        project,
        &r.generation,
        "stopped",
        "CLI handoff completed",
        db::now(),
    )?;
    let (native, args) = target_launch_state(&tx, project, r)?;
    ensure!(tx.execute("UPDATE projects SET agent=?,native_session=?,extra_args=?,terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null' WHERE id=? AND generation=?",params![r.target,native,args,project,r.generation])?==1,"Execution changed during CLI handoff");
    tx.commit()?;
    Ok(())
}
fn prompt(r: &Request) -> String {
    let target = if r.source["intent"] == "model" {
        format!(
            "a model change within {} (pending selection: {}; preserve this same native conversation and unrelated launch arguments)",
            r.target, r.source["target_model"]
        )
    } else {
        r.target.clone()
    };
    format!(
        "<dauerbot_cli_switch request_id=\"{}\" handoff_nonce=\"{}\">\nThe user requested switching this project to {} now. Finish or safely settle already-delivered work and JOIN/SETTLE every native subagent and external worker before acknowledging. New messages and scheduled input are held for the selected successor; they will not be delivered to you and do not invalidate this handoff. Update HANDOFF.md with the current goal, completed work, decisions, files, pending issues and next actions for the new CLI. Preserve user content and AGENTS.md. ONLY when no child or external worker or ongoing side effect remains, write this exact line INSIDE the project-root HANDOFF.md file, not merely in terminal output or your final response: <!-- dauerbot-switch-ready:{} -->. This line is your explicit ready-to-switch acknowledgement. If you cannot settle outstanding work, omit the marker and explain it in HANDOFF.md. Return to idle after writing. Do not close your own terminal or send an automatic completion message.\n</dauerbot_cli_switch>",
        r.id,
        r.nonce,
        crate::xml(&target),
        r.nonce
    )
}
async fn artifact(p: &Project) -> Result<Option<String>> {
    let path = PathBuf::from(&p.path);
    let content = tokio::task::spawn_blocking(move || {
        crate::project_documents::read(&path, crate::project_documents::Document::Handoff)
    })
    .await?;
    match content.status.as_str() {
        "available" => Ok(Some(content.text)),
        "missing" => Ok(None),
        _ => anyhow::bail!("{}", content.message),
    }
}
/// Accept a native CLI selection while the caller owns the project lock and
/// transaction. This shares the ordinary UI handoff and delivery-lock path.
pub(crate) fn request(c: &Connection, p: &Project, target: &str) -> Result<Option<Request>> {
    ensure!(
        ["codex", "claude", "pi", "agy"].contains(&target),
        "Unsupported target CLI; choose codex, claude, pi, or agy"
    );
    ensure!(!p.paused, "Resume this project before changing its CLI");
    ensure!(
        !App::update_required(p),
        "Recover this project after its service update before changing the CLI"
    );
    ensure!(
        !work::has_takeover(c, &p.id, &p.generation)?,
        "Release terminal input before changing the CLI"
    );
    let previous = latest(c, &p.id)?;
    ensure!(
        !previous
            .as_ref()
            .is_some_and(|r| r.phase == "recovery_exhausted"),
        "Retry recovery or stop the unresolved execution before changing the target CLI"
    );
    if let Some(r) = previous.as_ref().filter(|r| active(r)) {
        ensure!(
            r.target == target,
            "A CLI handoff is already in progress; review its result before starting another"
        );
        c.execute(
            "UPDATE cli_switch_requests SET next_attempt_at=0 WHERE id=?",
            [&r.id],
        )?;
        return latest(c, &p.id);
    }
    let retry = p.agent == target
        && p.terminal.is_none()
        && previous
            .as_ref()
            .is_some_and(|r| r.phase == "startup_failed" && r.target == target);
    if p.agent == target && !retry {
        return Ok(None);
    }
    let initial_phase = if retry { "starting" } else { "preparing" };
    let mut source = json!({"agent":p.agent,"native_session":p.native_session,
        "extra_args":p.extra_args,"generation":p.generation});
    source["selection"] = json!(crate::models::load(c, &p.id, &p.agent)?);
    c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input,source,phase_started_at) VALUES(?,?,?,?,?,?,1,?,?)",
        params![db::id(),p.id,target,initial_phase,p.generation,db::now(),source.to_string(),db::now()])?;
    latest(c, &p.id)
}

impl App {
    pub(crate) async fn cli_switch_bootstrap(&self, project: &str) -> Result<String> {
        let id = project.to_owned();
        let r = self.db.call(move |c| latest(c, &id)).await?;
        Ok(
            if r.as_ref()
                .is_some_and(|r| r.holds_input && matches!(r.phase.as_str(), "recovery_starting"))
            {
                " This is a daemon-managed CLI-switch recovery execution. Preserve the native conversation and completed effects. Do not resume historical tasks or process ordinary queued input. Read AGENTS.md and HANDOFF.md, connect MCP, then await the daemon's narrowly scoped repair instruction.".into()
            } else if r.as_ref().is_some_and(|r| {
                r.source["intent"] == "model"
                    && matches!(r.phase.as_str(), "starting" | "startup_failed")
            }) {
                " The user changed this project's native model selection. Continue the same native conversation using the selected launch settings; preserve completed effects, user content and request IDs. Read AGENTS.md and HANDOFF.md. Do not replay historical work or held input; the daemon delivers pending instructions after native and MCP readiness. Launch selection is not proof of an effective model observation.".into()
            } else if r.is_some_and(|r| matches!(r.phase.as_str(), "starting" | "startup_failed")) {
                " The user explicitly switched this project's CLI now. Read this project's AGENTS.md and HANDOFF.md before continuing the current goal. If HANDOFF.md is absent, this is a fresh workspace: use the project instructions and await user input. Preserve completed effects and request IDs; do not replay already delivered messages or work merely because this is a fresh native conversation.".into()
            } else {
                String::new()
            },
        )
    }
    /// Input belongs to the selected successor from acceptance through readiness,
    /// including while the old Agent prepares its handoff or the request fails.
    pub(crate) async fn cli_switch_holds_input(&self, p: &Project) -> Result<bool> {
        let id = p.id.clone();
        self.db.call(move |c| holds_input(c, &id)).await
    }
    pub(crate) async fn cli_switch_launch_context(
        &self,
        p: &Project,
        resume: bool,
    ) -> Result<Option<(String, Option<String>)>> {
        let id = p.id.clone();
        let r = self.db.call(move |c| latest(c, &id)).await?;
        Ok(r.filter(|r| {
            matches!(r.phase.as_str(), "starting" | "startup_failed") && r.target == p.agent
        })
        .map(|r| {
            let intended =
                (r.source["intent"] == "model" && resume && !p.native_session.is_empty())
                    .then(|| p.native_session.clone());
            (r.id, intended)
        }))
    }
    /// Accept a project-scoped native model transition. Selection remains
    /// pending until verified source retirement, and idempotency precedes stale
    /// catalogue checks so a retried accepted action never becomes a new effect.
    pub async fn request_model_switch_with_id(
        self: &Arc<Self>,
        key: &str,
        selection: Option<crate::models::Selection>,
        request_id: &str,
        expected: Option<crate::models::Scope>,
    ) -> Result<Project> {
        ensure!(
            !request_id.is_empty() && request_id.len() <= 200,
            "request_id must contain 1 to 200 characters"
        );
        let p = self.db.project(key).await?;
        let lock = self.project_lock(&p.id).await;
        let _guard = lock.lock().await;
        let current = self.db.project(&p.id).await?;
        let id = current.id.clone();
        let request_id = request_id.to_owned();
        self.db.call(move|c|{
            let tx=c.unchecked_transaction()?;
            let prior:Option<(String,String,String)>=tx.query_row(
                "SELECT project_id,target,source FROM cli_switch_requests WHERE id=?",[&request_id],
                |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional()?;
            if let Some((project,agent,source))=prior {
                let normalized=selection.as_ref().map(|s|crate::models::validate(&agent,s)).transpose()?;
                let payload=json!({"selection":normalized,"expected":expected});
                let source:Value=serde_json::from_str(&source)?;
                ensure!(project==id && source["intent"]=="model" && source["model_payload"]==payload,
                    "This request_id already refers to a different action, model, scope or project");
                return Ok(());
            }
            // Compare the scope at acceptance, not before acquiring the project
            // lock or while fetching a catalogue asynchronously.
            let (agent,generation,paused,native,args,terminal,execution):(String,String,bool,String,String,Option<String>,String)=tx.query_row(
                "SELECT agent,generation,paused,native_session,extra_args,terminal,execution FROM projects WHERE id=?",[&id],
                |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
            )?;
            let args:Vec<String>=serde_json::from_str(&args)?;
            let execution:Value=serde_json::from_str(&execution)?;
            let direct_resume=terminal.is_none();
            if direct_resume {
                let unclosed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM work_executions WHERE project_id=? AND status NOT IN ('sleeping','stopped','exited','error'))",[&id],|row|row.get(0))?;
                ensure!(!unclosed,"A saved native execution has not been verified closed; repair its ownership before starting a different model");
            }
            ensure!(["codex","claude","pi","agy"].contains(&agent.as_str()),"Model selection is unavailable for this CLI");
            if let Some(scope)=expected.as_ref() {
                ensure!(scope.agent==agent && scope.generation==generation,
                    "This model catalogue is stale: the project CLI or native execution changed. Open the model selector again");
            }
            ensure!(!paused,"Resume this project before changing its model");
            ensure!(terminal.is_none() || crate::mcp::generation_contract(&execution).is_some(),"Recover this project's native runtime before changing its model");
            ensure!(!work::has_takeover(&tx,&id,&generation)?,"Release terminal input before changing the model");
            let previous=latest(&tx,&id)?.filter(active);
            if let Some(previous)=previous.as_ref() {
                ensure!(previous.target==agent && model_repair_can_be_corrected(previous),
                    "A CLI or model transition is already in progress; only its model-repair target can be corrected");
            }
            let selection=selection.as_ref().map(|s|crate::models::validate(&agent,s)).transpose()?;
            let source_selection=crate::models::load(&tx,&id,&agent)?;
            let (mut source,initial_phase,recovery_generation)=if let Some(previous)=previous.as_ref() {
                let mut source=previous.source.clone();
                // The original repair owner descriptor is immutable. A failed
                // candidate's current preference/session must not replace it.
                if retiring_candidate(previous,&execution,&generation) {
                    remember_retiring_candidate(&mut source,&execution,&generation);
                }
                let mut history=source["superseded_requests"].as_array().cloned().unwrap_or_default();
                history.push(json!({"id":previous.id,"phase":previous.phase,"generation":previous.generation,"nonce":previous.nonce,"accepted_handoff":previous.baseline,"sequence":previous.sequence,"error":previous.error}));
                source["superseded_requests"]=json!(history);
                source["target_model"]=json!(selection);
                source["runtime_target"]["selection"]=json!(selection);
                source["model_payload"]=json!({"selection":selection,"expected":expected});
                source["supersedes"]=json!(previous.id);
                let repair_owner=previous.recovery_generation==generation && execution["cli_recovery_request"]==previous.id
                    && !retiring_candidate(previous,&execution,&generation);
                (source,"recovery_pending",if repair_owner {generation.clone()} else {String::new()})
            } else {
                (json!({"intent":"model","agent":agent,"native_session":native,"extra_args":args,
                    "generation":generation,"selection":source_selection,"target_model":selection,
                    "model_payload":{"selection":selection,"expected":expected},
                    "runtime_target":{"agent":agent,"native_session":native,"extra_args":args,"selection":selection}}),"preparing",String::new())
            };
            source["intent"]=json!("model");
            let initial_phase=if direct_resume {
                source["direct_model_resume"]=json!(true);
                "starting"
            } else {initial_phase};
            // One transaction replaces the held owner: no observer can see an
            // unlocked gap, and old ACKs no longer pass the latest-request guard.
            if let Some(previous)=previous.as_ref() {
                ensure!(tx.execute("UPDATE cli_switch_requests SET phase='superseded',holds_input=0,error='Superseded by a new explicitly selected repair model' WHERE id=? AND holds_input=1 AND rowid=(SELECT MAX(rowid) FROM cli_switch_requests WHERE project_id=?)",params![previous.id,id])?==1,"The pending model repair changed during correction");
            }
            tx.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input,source,phase_started_at,recovery_generation) VALUES(?,?,?,?,?,?,1,?,?,?)",
                params![request_id,id,agent,initial_phase,generation,db::now(),source.to_string(),db::now(),recovery_generation])?;
            if let Some(previous)=previous.as_ref() {
                tx.execute("UPDATE cli_repair_instructions SET request_id=? WHERE request_id=? AND prompt_id=''",params![request_id,previous.id])?;
                if !recovery_generation.is_empty() {
                    tx.execute("UPDATE projects SET execution=json_set(execution,'$.cli_recovery_request',?) WHERE id=? AND generation=?",params![request_id,id,generation])?;
                    tx.execute("UPDATE projects SET execution=json_set(execution,'$.repair_lane.request_id',?) WHERE id=? AND generation=? AND json_type(execution,'$.repair_lane')='object'",params![request_id,id,generation])?;
                }
            }
            if direct_resume {
                // A stopped/sleeping same-CLI conversation needs no old-model
                // bootstrap just to acknowledge a handoff. Apply the pending
                // override and retain its native ID in this acceptance commit.
                let accepted=latest(&tx,&id)?.ok_or_else(||anyhow::anyhow!("Missing accepted model transition"))?;
                if previous.is_some() {
                    // A closed failed candidate can leave its forked native
                    // pointer behind. Restore the genuine settled source before
                    // applying the corrected target; never adopt the candidate.
                    restore_source_state(&tx,&id,&accepted.source)?;
                }
                let (native,args)=target_launch_state(&tx,&id,&accepted)?;
                ensure!(tx.execute("UPDATE projects SET native_session=?,extra_args=?,generation='',ready=0,execution='null',terminal_pid=NULL WHERE id=? AND terminal IS NULL",params![native,args,id])?==1,"Native execution woke during model selection");
            }
            tx.commit()?;
            Ok(())
        }).await?;
        self.notify.notify_one();
        self.db.project(&p.id).await
    }

    pub async fn request_cli_switch(self: &Arc<Self>, key: &str, target: &str) -> Result<Project> {
        let p = self.db.project(key).await?;
        let lock = self.project_lock(&p.id).await;
        let _guard = lock.lock().await;
        let p = self.db.project(&p.id).await?;
        let current = p.clone();
        let target = target.to_owned();
        self.db.call(move |c| {
            let tx = c.unchecked_transaction()?;
            request(&tx, &current, &target)?;
            tx.commit()?;
            Ok(())
        }).await?;
        self.notify.notify_one();
        Ok(p)
    }
    /// Called with the project lock. None handles terminal-free startup only;
    /// a live observation is processed after normal input steering.
    pub(crate) async fn advance_cli_switch(
        &self,
        p: &Project,
        cfg: Option<&Config>,
        record: Option<&Value>,
    ) -> Result<bool> {
        let id = p.id.clone();
        let Some(mut r) = self.db.call(move |c| latest(c, &id)).await? else {
            return Ok(false);
        };
        if r.phase == "recovery_exhausted" {
            self.set_phase(
                p,
                if p.paused {
                    "paused"
                } else {
                    "needs_attention"
                },
                record.cloned().unwrap_or(Value::Null),
            )
            .await;
            return Ok(true);
        }
        if !active(&r) {
            return Ok(false);
        }
        // Live execution readiness and timeout decisions require an observation.
        // The top reconciliation pass must not fail a newly ready candidate.
        if p.terminal.is_some() && record.is_none() {
            return Ok(false);
        }
        let cfg_owned;
        let cfg = if let Some(cfg) = cfg {
            cfg
        } else {
            cfg_owned = self.config.read().await.clone();
            &cfg_owned
        };
        let id = p.id.clone();
        let generation = p.generation.clone();
        let takeover = self
            .db
            .call(move |c| work::has_takeover(c, &id, &generation))
            .await?;
        if p.paused || takeover {
            self.set_phase(
                p,
                if p.paused { "paused" } else { "taken_over" },
                record.cloned().unwrap_or(Value::Null),
            )
            .await;
            return Ok(true);
        }
        if recovery_budget_spent(&r)
            && !matches!(r.phase.as_str(), "starting" | "switching")
            && !(p.agent == r.target
                && p.ready
                && p.execution["mcp_registered"] == true
                && p.execution["cli_switch_request"] == r.id
                && record.is_some_and(|v| {
                    crate::repair_lane::check_identity(p, v).is_ok()
                        && matches!(
                            v["status"]["state"].as_str(),
                            Some("idle" | "done" | "working")
                        )
                })
                && target_selection_matches(&r, &p.execution)
                && target_native_matches(&r, &p.execution, &p.native_session)
                && r.phase != "recovery_stopping")
        {
            let id = r.id.clone();
            if self.db.call(move |c| {
                    let tx = c.unchecked_transaction()?;
                    let changed = suspend_recovery(&tx, &id)?;
                    tx.commit()?;
                    Ok(changed)
                }).await? {
                self.set_phase(p, "needs_attention", record.cloned().unwrap_or(Value::Null))
                    .await;
            }
            return Ok(true);
        }
        if crate::repair_lane::descriptor(p).is_some() {
            let current = p.clone();
            self.db
                .call(move |c| crate::repair_lane::authorize(c, &current, false).map(|_| ()))
                .await?;
        }
        if crate::repair_lane::descriptor(p).is_some() && r.phase == "requested" {
            let current = p.clone();
            let rid = r.id.clone();
            let generation_id = p.generation.clone();
            let retry=self.db.call(move|c|{
                let lane=crate::repair_lane::authorize(c,&current,false)?.context("Repair role retired")?;
                crate::repair_lane::effect_phase(c,&lane)?;
                let last:Option<String>=c.query_row("SELECT status FROM cli_repair_prompts WHERE request_id=? AND generation=? ORDER BY rowid DESC LIMIT 1",params![rid,generation_id],|row|row.get(0)).optional()?;
                if last.as_deref()==Some("not_sent") {c.execute("UPDATE cli_switch_requests SET phase='recovery_pending',next_attempt_at=0 WHERE id=? AND phase='requested' AND holds_input=1",[rid])?;return Ok(true);}
                Ok(false)
            }).await?;
            if retry {
                self.notify.notify_one();
                return Ok(true);
            }
        }
        if matches!(
            r.phase.as_str(),
            "failed"
                | "startup_failed"
                | "recovery_pending"
                | "recovery_stopping"
                | "recovery_starting"
                | "repairing"
        ) {
            return self.advance_cli_recovery(p, &mut r, cfg, record).await;
        }
        if r.phase == "uncertain" {
            // A retry is permitted only after a fresh identity check of the same
            // immutable pane. No successor can start before verified closure.
            if p.generation != r.generation || p.terminal.is_none() {
                self.fail_cli_switch(&r, "The stop boundary needs native continuity recovery.")
                    .await?;
                return Ok(true);
            }
            let observed = record.expect("live execution observed");
            if observed["state"] != "finished" {
                if let Err(error) = herdr::check_pane_ownership(p, observed) {
                    self.defer_cli_recovery(
                        &r,
                        &format!("Rechecking the original execution before recovery: {error}"),
                    )
                    .await?;
                    return Ok(true);
                }
                if db::now() < r.next_attempt_at {
                    return Ok(true);
                }
                let accepted = artifact(p).await?;
                let rid = r.id.clone();
                let controls=self.db.call(move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM cli_repair_instructions WHERE request_id=? AND prompt_id='')",[rid],|row|row.get::<_,bool>(0))?)).await?;
                if controls || !stop_retry_safe(p, &r, observed, accepted.as_deref()) {
                    if self.recover_uncertain_source(p,&mut r,observed,"The uncertain stop was not retried: source activity, its accepted handoff, or new repair controls require a fresh diagnostic handoff.").await? {
                        return Ok(true);
                    }
                    self.defer_cli_recovery(&r,"Waiting for renewed source quiescence, settled children and the unchanged accepted nonce handoff before retrying native stop.").await?;
                    return Ok(true);
                }
                let fresh = herdr::observe(cfg, p).await?;
                let final_text = artifact(p).await?;
                if fresh["state"] != "finished"
                    && (!stop_retry_safe(p, &r, &fresh, final_text.as_deref())
                        || fresh["children"] != observed["children"]
                        || handoff::fingerprint(final_text.as_deref())
                            != handoff::fingerprint(accepted.as_deref()))
                {
                    if self.recover_uncertain_source(p,&mut r,&fresh,"Source activity or its accepted handoff changed during stop retry verification; requesting a fresh diagnostic handoff on the same verified execution.").await? {
                        return Ok(true);
                    }
                    self.defer_cli_recovery(&r,"Source activity or the accepted handoff changed during stop retry verification; continuing recovery observations.").await?;
                    return Ok(true);
                }
                match herdr::stop(cfg, p).await {
                    Ok(()) => {}
                    Err(error) => {
                        self.defer_cli_recovery(
                            &r,
                            &format!("Verifying the original stop before recovery: {error}"),
                        )
                        .await?;
                        return Ok(true);
                    }
                }
            }
            if !matches!(herdr::observe(cfg,p).await,Ok(closed) if closed["state"] == "finished") {
                self.defer_cli_recovery(
                    &r,
                    "Verifying the original execution has closed; no second Agent is started.",
                )
                .await?;
                return Ok(true);
            }
            let text = artifact(p).await?;
            if handoff::fingerprint(text.as_deref()) != r.baseline {
                self.fail_cli_switch(
                    &r,
                    "The accepted handoff changed; resume the original Agent to repair it.",
                )
                .await?;
                return Ok(true);
            }
            let id = p.id.clone();
            let rr = r.clone();
            self.db.call(move |c| complete(c, &id, &rr)).await?;
            self.start_cli_target(p, &r).await?;
            return Ok(true);
        }
        if r.phase == "starting" {
            if let Some(record) = record {
                if crate::repair_lane::descriptor(p).is_none()
                    && herdr::check_native_resume(p, record).is_err()
                {
                    let current = p.clone();
                    let observed = record.clone();
                    let admitted = self
                        .db
                        .call(move |c| crate::repair_lane::admit(c, &current, &observed))
                        .await?;
                    if admitted.is_none() {
                        self.fail_cli_start(&r,"The resumed CLI forked its saved conversation; restoring the original repair owner with ordinary input held.").await?;
                    }
                    self.notify.notify_one();
                    return Ok(true);
                }
                if p.agent == r.target
                    && r.source["intent"] == "model"
                    && native_resume(&p.agent, &p.execution, &p.native_session)
                        == NativeResume::Forked
                {
                    self.fail_cli_start(&r, "The selected CLI resumed a different native conversation; keeping input held and restoring supervised repair.").await?;
                    return Ok(true);
                }
                if p.agent == r.target
                    && target_native_matches(&r, &p.execution, &p.native_session)
                    && crate::repair_lane::check_identity(p, record).is_ok()
                    && target_selection_matches(&r, &p.execution)
                    && p.ready
                    && p.execution["mcp_registered"] == true
                    && matches!(
                        record["status"]["state"].as_str(),
                        Some("idle" | "done" | "working")
                    )
                {
                    let project = p.id.clone();
                    let generation = p.generation.clone();
                    let rr = r.clone();
                    self.db
                        .call(move |c| ready(c, &project, &generation, &rr))
                        .await?;
                    return Ok(false);
                }
                if record["status"]["state"] == "blocked" {
                    if !recovery_budget_spent(&r) && matches!(p.agent.as_str(), "agy" | "claude")
                        && herdr::confirm_project_trust(cfg, p).await.is_ok()
                    {
                        self.set_phase(p, "switching_cli", record.clone()).await;
                        return Ok(true);
                    }
                    self.fail_cli_start(&r,"The selected CLI needs an explicit native login or input decision; restoring its repair conversation without guessing that decision.").await?;
                    return Ok(true);
                }
                if record["status"]["state"] == "error" {
                    self.fail_cli_start(&r,"The selected CLI reported a native startup error; restoring its repair conversation.").await?;
                    return Ok(true);
                }
                if record["state"] == "finished" || p.agent != r.target {
                    self.fail_cli_start(&r, "The selected CLI exited before native and MCP readiness; restoring its repair conversation.").await?;
                    return Ok(true);
                }
                if db::now() - r.phase_started_at > BUDGET_MS {
                    self.fail_cli_start(&r, "The selected CLI did not become ready in time; restoring its repair conversation.").await?;
                    return Ok(true);
                }
                self.set_phase(p, "switching_cli", record.clone()).await;
                return Ok(true);
            }
            self.start_cli_target(p, &r).await?;
            return Ok(true);
        }
        if let Some(record) = record {
            if r.recovery_generation == p.generation {
                let mut control = r.clone();
                control.sequence = record["state_change_seq"].as_i64().unwrap_or(r.sequence);
                control.baseline = handoff::fingerprint(artifact(p).await?.as_deref());
                if self.send_cli_repair_prompt(p,&control,cfg,
                "<dauerbot_cli_repair_control>New user instructions for this switch repair. Keep ordinary held work pending. After these instructions and all children settle, rewrite HANDOFF.md with the current nonce acknowledgement; an earlier acknowledgement cannot authorize retirement.</dauerbot_cli_repair_control>",false).await? {
                return Ok(true);
            }
            }
            if r.recovery_generation == p.generation
                && (record["status"]["state"] == "working"
                    || record["children"]["state"] == "active")
            {
                self.set_phase(p, "recovering_cli", record.clone()).await;
                return Ok(true);
            }
        }
        if db::now() - r.phase_started_at > BUDGET_MS {
            self.fail_cli_switch(&r, "The safe handoff did not finish in time; the original Agent will repair the transition.").await?;
            return Ok(true);
        }
        if p.terminal.is_none() {
            if !r.generation.is_empty() {
                self.fail_cli_switch(&r, "The original execution exited before its handoff; resuming native continuity for repair.").await?;
                return Ok(true);
            }
            if !p.native_session.is_empty() {
                match self.start_inner(&p.id, true).await {
                    Ok(p) => {
                        let id = r.id.clone();
                        let generation = p.generation;
                        self.db
                            .call(move |c| {
                                c.execute(
                                    "UPDATE cli_switch_requests SET generation=? WHERE id=?",
                                    params![generation, id],
                                )?;
                                Ok(())
                            })
                            .await?;
                    }
                    Err(error) => {
                        self.fail_cli_switch(
                            &r,
                            &format!("Preparing native continuity for repair: {error}"),
                        )
                        .await?
                    }
                }
            } else {
                let id = p.id.clone();
                let target = r.target.clone();
                let request = r.id.clone();
                self.db.call(move |c| {
                    let tx = c.unchecked_transaction()?;
                    let rr=latest(&tx,&id)?.ok_or_else(||anyhow::anyhow!("Missing launch transition"))?;
                    let (native,args)=target_launch_state(&tx,&id,&rr)?;
                    ensure!(tx.execute("UPDATE projects SET agent=?,extra_args=?,native_session=?,generation='',ready=0,execution='null' WHERE id=? AND terminal IS NULL",params![target,args,native,id])?==1,"Project woke during CLI selection");
                    phase(&tx,&request,"starting","")?;
                    tx.commit()?;
                    Ok(())
                }).await?;
                self.start_cli_target(p, &r).await?;
            }
            self.notify.notify_one();
            return Ok(true);
        }
        let record = record.expect("live execution observed");
        if record["state"] == "finished" {
            self.fail_cli_switch(
                &r,
                "The original Agent exited during handoff; recovering its native conversation.",
            )
            .await?;
            return Ok(true);
        }
        if p.generation != r.generation {
            self.fail_cli_switch(
                &r,
                "The original execution changed; recovering the verified native conversation.",
            )
            .await?;
            return Ok(true);
        }
        if r.phase == "preparing" {
            let text = match artifact(p).await {
                Ok(text) => text,
                Err(error) => {
                    self.fail_cli_switch(
                        &r,
                        &format!(
                            "Cannot read a safe project handoff: {error}. The current CLI was kept."
                        ),
                    )
                    .await?;
                    return Ok(false);
                }
            };
            let baseline = handoff::fingerprint(text.as_deref());
            let nonce = db::id();
            let sequence = record["state_change_seq"].as_i64().ok_or_else(|| {
                anyhow::anyhow!("The CLI has no native activity sequence for handoff")
            })?;
            let id = p.id.clone();
            let rid = r.id.clone();
            let baseline2 = baseline.clone();
            let nonce2 = nonce.clone();
            let saved=self.db.call(move|c|{let revision=self::revision(c,&id)?;Ok(c.execute("UPDATE cli_switch_requests SET phase='requested',phase_started_at=?,nonce=?,baseline=?,sequence=?,revision=?,attempts=attempts+1 WHERE id=? AND phase='preparing' AND holds_input=1",params![db::now(),nonce2,baseline2,sequence,revision,rid])?==1)}).await?;
            if !saved {
                return Ok(true);
            }
            r.nonce = nonce;
            r.baseline = baseline;
            r.sequence = sequence;
            r.phase = "requested".into();
            match herdr::send(cfg,p,&prompt(&r)).await {Ok(v)=>{let id=r.id.clone();let seq=v["native_seq"].as_i64().unwrap_or(sequence);self.db.call(move|c|{c.execute("UPDATE cli_switch_requests SET sequence=? WHERE id=?",params![seq,id])?;Ok(())}).await?;},Err(e)=>self.fail_cli_switch(&r,&format!("The handoff request could not be confirmed: {e}. The current CLI was kept.")).await?}
            self.set_phase(p, "preparing_handoff", record.clone()).await;
            return Ok(true);
        }
        self.set_phase(p, "preparing_handoff", record.clone()).await;
        if !crate::repair_lane::foreground_quiescent(p, record)
            || record["children"]["state"] == "active"
        {
            return Ok(true);
        }
        let id = p.id.clone();
        let generation = p.generation.clone();
        let seq = record["state_change_seq"].as_i64();
        self.db
            .call(move |c| work::observe_idle(c, &id, &generation, seq, db::now()))
            .await?;
        let text = match artifact(p).await {
            Ok(text) => text,
            Err(error) => {
                self.fail_cli_switch(
                    &r,
                    &format!(
                        "Cannot read a safe project handoff: {error}. The current CLI was kept."
                    ),
                )
                .await?;
                return Ok(false);
            }
        };
        if !proof(&r, text.as_deref(), record["state_change_seq"].as_i64()) {
            return Ok(true);
        }
        let check = herdr::observe(cfg, p).await?;
        if !crate::repair_lane::foreground_quiescent(p, &check)
            || check["children"]["state"] == "active"
            || check["children"] != record["children"]
            || !proof(&r, text.as_deref(), check["state_change_seq"].as_i64())
        {
            return Ok(true);
        }
        // Managed Pi provides a real dispatch fence. Other CLIs have no complete
        // registry: explicit user intent + the Agent's nonce acknowledgement is
        // cooperative evidence, not a claim of native child coverage.
        let gate = if p.agent == "pi" && p.execution["child_adapter"].is_string() {
            match native_children::RetirementGate::acquire(
                &crate::repair_lane::child_view(p),
                &check["children"],
                db::now(),
            ) {
                Ok(g) => Some(g),
                Err(_) => return Ok(true),
            }
        } else {
            None
        };
        let final_check = herdr::observe(cfg, p).await?;
        if !crate::repair_lane::foreground_quiescent(p, &final_check)
            || final_check["children"]["state"] == "active"
            || final_check["children"] != check["children"]
        {
            return Ok(true);
        }
        let final_text = artifact(p).await?;
        if handoff::fingerprint(final_text.as_deref()) != handoff::fingerprint(text.as_deref())
            || !proof(
                &r,
                final_text.as_deref(),
                final_check["state_change_seq"].as_i64(),
            )
        {
            return Ok(true);
        }
        if r.source["intent"] == "model" && p.native_session.is_empty() {
            self.fail_cli_switch(&r,"The model change needs this CLI's verified native session identity before retirement; repair the native continuity hook first.").await?;
            return Ok(true);
        }
        let id = p.id.clone();
        let rr = r.clone();
        let accepted_hash = handoff::fingerprint(final_text.as_deref());
        if !self
            .db
            .call(move |c| claim(c, &id, &rr, &accepted_hash))
            .await?
        {
            return Ok(true);
        }
        if let Err(e) = herdr::stop(cfg, p).await {
            if let Some(g) = gate {
                g.commit()
            };
            let id = r.id.clone();
            let error = format!(
                "Native stop has an uncertain result: {e}. Review this execution before stopping or starting it explicitly."
            );
            self.db
                .call(move |c| phase(c, &id, "uncertain", &error))
                .await?;
            return Ok(true);
        }
        if let Some(g) = gate {
            g.commit()
        };
        if !matches!(herdr::observe(cfg,p).await,Ok(closed) if closed["state"] == "finished") {
            let id = r.id.clone();
            self.db
                .call(move |c| {
                    phase(
                        c,
                        &id,
                        "uncertain",
                        "Verifying the original execution closed before selected CLI startup.",
                    )
                })
                .await?;
            return Ok(true);
        }
        let id = p.id.clone();
        let rr = r.clone();
        self.db.call(move |c| complete(c, &id, &rr)).await?;
        self.start_cli_target(p, &r).await?;
        self.notify.notify_one();
        Ok(true)
    }
    /// A verified live source can repair the transition even when an earlier
    /// close has an uncertain result. This sends no replacement process: a late
    /// close is handled by the ordinary verified-exit/native-resume recovery.
    async fn recover_uncertain_source(
        &self,
        p: &Project,
        r: &mut Request,
        record: &Value,
        reason: &str,
    ) -> Result<bool> {
        if herdr::check_pane_ownership(p, record).is_err()
            || crate::repair_lane::check_identity(p, record).is_err()
            || record["state_change_seq"].as_i64().is_none()
        {
            return Ok(false);
        }
        self.ensure_cli_source(p, r).await?;
        let mut effects = r.source["uncertain_stops"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !effects
            .iter()
            .any(|effect| effect["generation"] == r.generation && effect["nonce"] == r.nonce)
        {
            effects.push(json!({"generation":r.generation,"nonce":r.nonce,"accepted_handoff":r.baseline,"sequence":r.sequence,"result":"uncertain","recorded_at":db::now()}));
        }
        r.source["uncertain_stops"] = json!(effects);
        let rid = r.id.clone();
        let source = r.source.to_string();
        let reason = reason.to_owned();
        self.db.call(move|c|{
            let tx=c.unchecked_transaction()?;
            tx.execute("UPDATE cli_switch_requests SET source=? WHERE id=? AND phase='uncertain' AND holds_input=1",params![source,rid])?;
            record_failure(&tx,&rid,&reason)?;
            tx.commit()?;Ok(())
        }).await?;
        self.notify.notify_one();
        Ok(true)
    }

    /// Fault supervision is independent of ordinary message delivery and remains
    /// available even when no user task happens to be queued.
    pub(crate) async fn journal_cli_fault(&self, project: &str, diagnostic: &str) -> Result<()> {
        let project = project.to_owned();
        let diagnostic: String = diagnostic.chars().take(4000).collect();
        self.db
            .call(move |c| {
                let r = ensure_repair_request(c, &project)?;
                if r.phase == "recovery_exhausted" {
                    return Ok(());
                }
                record_failure(c, &r.id, &diagnostic)
            })
            .await?;
        self.notify.notify_one();
        Ok(())
    }

    /// Losing observation is not proof of native exit. Elect recovery once,
    /// retaining any in-flight repair phase and uncertain effects until a fresh
    /// observation can authorize an action on this exact execution.
    pub(crate) async fn supervise_unobserved_cli(&self, p: &Project) -> Result<bool> {
        let p = p.clone();
        self.db.call(move |c| {
            let tx = c.unchecked_transaction()?;
            let current: (String, Option<String>, String, bool) = tx.query_row(
                "SELECT generation,terminal,execution,paused FROM projects WHERE id=?",
                [&p.id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            )?;
            if current.0 != p.generation || current.1 != p.terminal
                || serde_json::from_str::<Value>(&current.2)? != p.execution
                || current.3 || work::has_takeover(&tx, &p.id, &p.generation)? {
                return Ok(false);
            }
            work::touch_execution(&tx, &p.id, &p.generation, db::now())?;
            let latest = latest(&tx, &p.id)?;
            if let Some(request) = latest.as_ref() {
                if request.phase == "recovery_exhausted" {
                    tx.commit()?;
                    return Ok(false);
                }
                if active(request) && recovery_budget_spent(request) {
                    suspend_recovery(&tx, &request.id)?;
                    tx.commit()?;
                    return Ok(false);
                }
            }
            if !latest.as_ref().is_some_and(active) {
                let request = ensure_repair_request(&tx, &p.id)?;
                record_failure(&tx, &request.id,
                    "Native execution observation is unavailable; supervised recovery is waiting for verified identity before taking any action.")?;
                // Explicit versioned, internal-only metadata. Older requests
                // without this marker always retain ordinary recovery semantics.
                let fence = json!({"schema":1,"agent":p.agent,"generation":p.generation,
                    "terminal":p.terminal,"native_session":p.native_session,"execution":p.execution});
                tx.execute("UPDATE cli_switch_requests SET source=json_set(source,'$.observation_fence',json(?)) WHERE id=?",
                    params![fence.to_string(),request.id])?;
            }
            tx.commit()?;
            Ok(true)
        }).await
    }

    /// Nudge the same durable transition. This neither abandons the selected
    /// target nor releases ordinary input or repeats an uncertain prompt.
    pub async fn request_cli_repair(&self, key: &str) -> Result<Project> {
        let p = self.db.project(key).await?;
        let lock = self.project_lock(&p.id).await;
        let _guard = lock.lock().await;
        let id = p.id.clone();
        self.db
            .call(move |c| {
                let r = ensure_repair_request(c, &id)?;
                if r.phase == "recovery_exhausted" {
                    c.execute("UPDATE cli_switch_requests SET phase='recovery_pending',holds_input=1,phase_started_at=?,next_attempt_at=0,recovery_attempts=0,source=json_set(source,'$.recovery_budget_started_at',?) WHERE id=?",
                        params![db::now(),db::now(),r.id])?;
                    return Ok(());
                }
                c.execute(
                    "UPDATE cli_switch_requests SET next_attempt_at=0 WHERE id=?",
                    [&r.id],
                )?;
                if matches!(
                    r.phase.as_str(),
                    "failed" | "startup_failed" | "preparing" | "starting"
                ) || (r.phase == "requested" && r.recovery_generation.is_empty())
                {
                    phase(c, &r.id, "recovery_pending", &r.error)?;
                }
                Ok(())
            })
            .await?;
        self.notify.notify_one();
        self.db.project(&p.id).await
    }

    /// Used while constructing an execution, before its launch permit opens.
    pub(crate) async fn cli_recovery_launch_id(&self, p: &Project) -> Result<Option<String>> {
        let id = p.id.clone();
        let r = self.db.call(move |c| latest(c, &id)).await?;
        Ok(r.filter(|r| {
            r.holds_input && r.phase == "recovery_starting" && r.source["agent"] == p.agent
        })
        .map(|r| r.id))
    }

    async fn defer_cli_recovery(&self, r: &Request, error: &str) -> Result<()> {
        let id = r.id.clone();
        let error: String = error.chars().take(4000).collect();
        let delay = 1000 * (1_i64 << r.recovery_attempts.clamp(0, 5));
        self.db.call(move |c| {
            c.execute("UPDATE cli_switch_requests SET error=?,next_attempt_at=?,recovery_attempts=recovery_attempts+1 WHERE id=? AND holds_input=1", params![error,db::now()+delay,id])?;
            Ok(())
        }).await
    }

    /// Recover old stores without inventing an original native conversation.
    /// New requests always snapshot arguments before the first cutover.
    async fn ensure_cli_source(&self, p: &Project, r: &mut Request) -> Result<()> {
        if r.source["agent"].as_str().is_some_and(|a| !a.is_empty()) {
            return Ok(());
        }
        let id = p.id.clone();
        let generation = r.generation.clone();
        let current = p.clone();
        let rid = r.id.clone();
        let last_failure = r.source["last_failure"].clone();
        r.source = self.db.call(move |c| {
            let historical: Option<(String,String)> = c.query_row(
                "SELECT agent,native_session FROM work_executions WHERE project_id=? AND generation=?",
                params![id,generation], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            let mut source = match historical {
                Some((agent,native)) if current.generation != generation =>
                    json!({"agent":agent,"native_session":native,"extra_args":[],"generation":generation,"historical_arguments_unavailable":true}),
                _ => json!({"agent":current.agent,"native_session":current.native_session,"extra_args":current.extra_args,"generation":generation}),
            };
            if !last_failure.is_null() { source["last_failure"] = last_failure; }
            source["selection"]=json!(crate::models::load(c,&id,source["agent"].as_str().unwrap_or(""))?);
            c.execute("UPDATE cli_switch_requests SET source=? WHERE id=?",params![source.to_string(),rid])?;
            Ok(source)
        }).await?;
        Ok(())
    }

    /// An observation-only hold must not turn a transient RPC loss into a
    /// replacement of a healthy native conversation. No repair effect or user
    /// repair instruction may be retired by this shortcut.
    fn settle_observation_fence(
        c: &Connection,
        p: &Project,
        r: &Request,
        record: &Value,
    ) -> Result<bool> {
        let fence = &r.source["observation_fence"];
        // Normal registration (api::register) may add only these two readiness
        // flags. Preserve every other stored authority/launch field, including
        // unknown future fields, rather than guessing an identity allowlist.
        let same_launch = match (fence["execution"].as_object(), p.execution.as_object()) {
            (Some(before), Some(current)) => {
                let (mut before, mut current) = (before.clone(), current.clone());
                for key in ["mcp_registered", "native_confirmed"] {
                    before.remove(key);
                    current.remove(key);
                }
                before == current
            }
            _ => false,
        };
        if fence["schema"] != 1
            || r.phase != "recovery_pending"
            || r.generation != p.generation
            || r.target != p.agent
            || !r.recovery_generation.is_empty()
            || !r.nonce.is_empty()
            || r.attempts != 0
            || !p.ready
            || p.execution["mcp_registered"] != true
            || p.execution["native_confirmed"] != true
            || p.native_session.is_empty()
            || crate::repair_lane::descriptor(p).is_some()
            || record["state"] != "live"
            || !matches!(
                record["status"]["state"].as_str(),
                Some("idle" | "done" | "working")
            )
            || !record["native_session"]["value"]
                .as_str()
                .is_some_and(|native| {
                    herdr::same_native_session(&p.agent, &p.native_session, native)
                })
            || herdr::check_identity(p, record).is_err()
            || fence["agent"] != p.agent
            || fence["generation"] != p.generation
            || fence["terminal"] != json!(p.terminal)
            || !fence["native_session"].as_str().is_some_and(|native| {
                native.is_empty() || herdr::same_native_session(&p.agent, native, &p.native_session)
            })
            || !same_launch
        {
            return Ok(false);
        }
        let tx = c.unchecked_transaction()?;
        let current: (String,Option<String>,String,String,bool,bool,String) = tx.query_row(
            "SELECT generation,terminal,native_session,agent,paused,ready,execution FROM projects WHERE id=?",
            [&p.id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)))?;
        if current.0 != p.generation
            || current.1 != p.terminal
            || current.2 != p.native_session
            || current.3 != p.agent
            || current.4
            || !current.5
            || serde_json::from_str::<Value>(&current.6)? != p.execution
            || work::has_takeover(&tx, &p.id, &p.generation)?
        {
            return Ok(false);
        }
        let changed = tx.execute("UPDATE cli_switch_requests SET phase='ready',holds_input=0,error='',next_attempt_at=0 WHERE id=? AND phase='recovery_pending' AND holds_input=1 AND generation=? AND nonce='' AND attempts=0 AND recovery_generation='' AND rowid=(SELECT MAX(rowid) FROM cli_switch_requests WHERE project_id=?) AND source=? AND NOT EXISTS(SELECT 1 FROM cli_repair_prompts WHERE request_id=?) AND NOT EXISTS(SELECT 1 FROM cli_repair_instructions WHERE request_id=?)",
            params![r.id,p.generation,p.id,r.source.to_string(),r.id,r.id])?;
        tx.commit()?;
        Ok(changed == 1)
    }

    async fn advance_cli_recovery(
        &self,
        p: &Project,
        r: &mut Request,
        cfg: &Config,
        record: Option<&Value>,
    ) -> Result<bool> {
        self.set_phase(p, "recovering_cli", record.cloned().unwrap_or(Value::Null))
            .await;
        if let Some(record) = record {
            let p = p.clone();
            let request = r.clone();
            let record = record.clone();
            if self
                .db
                .call(move |c| Self::settle_observation_fence(c, &p, &request, &record))
                .await?
            {
                return Ok(false);
            }
        }
        // Readiness wins over an elapsed deadline, including after a service
        // restart or when the previous observation reported a startup failure.
        if p.agent == r.target
            && target_native_matches(r, &p.execution, &p.native_session)
            && target_selection_matches(r, &p.execution)
            && p.execution["cli_switch_request"] == r.id
            && p.ready
            && p.execution["mcp_registered"] == true
            && record.is_some_and(|v| {
                crate::repair_lane::check_identity(p, v).is_ok()
                    && matches!(
                        v["status"]["state"].as_str(),
                        Some("idle" | "done" | "working")
                    )
            })
            && r.phase != "recovery_stopping"
        {
            let id = p.id.clone();
            let generation = p.generation.clone();
            let rr = r.clone();
            self.db
                .call(move |c| ready(c, &id, &generation, &rr))
                .await?;
            return Ok(false);
        }
        if db::now() < r.next_attempt_at {
            return Ok(true);
        }
        self.ensure_cli_source(p, r).await?;
        let lane = crate::repair_lane::descriptor(p);
        let native_fork = lane.is_none()
            && record.is_some_and(|record| herdr::check_native_resume(p, record).is_err());
        if native_fork && r.phase != "recovery_stopping" {
            let current = p.clone();
            let observed = record.expect("fork observation").clone();
            if self
                .db
                .call(move |c| crate::repair_lane::admit(c, &current, &observed))
                .await?
                .is_some()
            {
                self.notify.notify_one();
                return Ok(true);
            }
            let record = record.expect("native fork observation");
            if herdr::recovery_foreground_quiescent(p, record) {
                let id = r.id.clone();
                self.db.call(move|c|phase(c,&id,"recovery_stopping","Replacing the owned forked native execution while preserving the original saved conversation")).await?;
            } else {
                self.defer_cli_recovery(r,"Waiting for the forked execution's active work and children before restoring the original saved conversation").await?;
            }
            return Ok(true);
        }
        let source_agent = r.source["agent"].as_str().unwrap_or("").to_owned();
        let candidate = retiring_candidate(r, &p.execution, &p.generation);
        // Hooks may discover native continuity after switch acceptance. Capture
        // only the source's identity, never a failed target's different session.
        if !candidate
            && lane.is_none()
            && !native_fork
            && p.agent == source_agent
            && !p.native_session.is_empty()
            && r.source["native_session"] != p.native_session
        {
            r.source["native_session"] = json!(p.native_session);
            let rid = r.id.clone();
            let source = r.source.to_string();
            self.db
                .call(move |c| {
                    c.execute(
                        "UPDATE cli_switch_requests SET source=? WHERE id=?",
                        params![source, rid],
                    )?;
                    Ok(())
                })
                .await?;
        }
        if record.is_some_and(|value| value["vanished_execution"] == true) {
            let expected = serde_json::to_value(herdr::ExecutionRef::from_project(p)?)?;
            ensure!(
                record.expect("verified disappearance")["execution"] == expected,
                "Disappearance evidence refers to a different execution"
            );
            let current = p.clone();
            let request = r.clone();
            if self
                .db
                .call(move |c| retire_vanished(c, &current, &request))
                .await?
            {
                self.notify.notify_one();
            }
            return Ok(true);
        }
        if p.terminal.is_some()
            && (candidate || p.agent != source_agent || r.phase == "recovery_stopping")
        {
            let record = record.expect("live recovery has an observation");
            // A target has not received held user work, but may have an initial
            // tool in flight. Preserve its execution until that work settles.
            if record["state"] != "finished" && !herdr::recovery_foreground_quiescent(p, record) {
                self.defer_cli_recovery(r,"Waiting for the selected CLI's in-flight startup work before restoring the repair Agent.").await?;
                return Ok(true);
            }
            if record["state"] != "finished"
                && let Err(error) = herdr::check_pane_ownership(p, record)
            {
                self.defer_cli_recovery(
                    r,
                    &format!("Rechecking the selected execution identity before repair: {error}"),
                )
                .await?;
                return Ok(true);
            }
            let id = r.id.clone();
            self.db.call(move |c| phase(c,&id,"recovery_stopping","Verifying selected CLI closure before restoring the original conversation.")).await?;
            if let Err(error) = herdr::stop(cfg, p).await {
                self.defer_cli_recovery(r,&format!("Reobserving selected CLI closure before restoring the original conversation: {error}")).await?;
                return Ok(true);
            }
            match herdr::observe(cfg, p).await {
                Ok(closed) if closed["state"] == "finished" => {}
                _ => {
                    self.defer_cli_recovery(r,"Verifying the selected execution has actually closed before recovery startup.").await?;
                    return Ok(true);
                }
            }
            let id = p.id.clone();
            let generation = p.generation.clone();
            let rid = r.id.clone();
            self.db.call(move |c| {
                let tx = c.unchecked_transaction()?;
                work::finish_execution(&tx,&id,&generation,"stopped","Selected CLI stopped for automatic switch repair",db::now())?;
                ensure!(tx.execute("UPDATE projects SET terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null' WHERE id=? AND generation=?",params![id,generation])?==1,"Execution changed during recovery stop");
                phase(&tx,&rid,"recovery_pending","The selected CLI is closed; restoring the original native conversation.")?;
                tx.commit()?; Ok(())
            }).await?;
            self.notify.notify_one();
            return Ok(true);
        }
        if p.terminal.is_none() {
            let id = p.id.clone();
            let rid = r.id.clone();
            let source = r.source.clone();
            self.db
                .call(move |c| {
                    let tx = c.unchecked_transaction()?;
                    restore_source_state(&tx, &id, &source)?;
                    // Retain the phase clock across failed launch retries.
                    phase(
                        &tx,
                        &rid,
                        "recovery_starting",
                        "Starting the original CLI as the repair Agent.",
                    )?;
                    tx.commit()?;
                    Ok(())
                })
                .await?;
            let resume = r.source["native_session"]
                .as_str()
                .is_some_and(|s| !s.is_empty());
            match self.start_inner(&p.id, resume).await {
                Ok(source) => {
                    let rid = r.id.clone();
                    let generation = source.generation;
                    self.db
                        .call(move |c| {
                            c.execute(
                                "UPDATE cli_switch_requests SET recovery_generation=? WHERE id=?",
                                params![generation, rid],
                            )?;
                            Ok(())
                        })
                        .await?;
                }
                Err(error) => {
                    self.defer_cli_recovery(
                        r,
                        &format!("Retrying original native conversation startup: {error}"),
                    )
                    .await?
                }
            }
            return Ok(true);
        }
        let record = record.expect("source recovery observed");
        if record["state"] == "finished" {
            if let Err(error) = herdr::stop(cfg, p).await {
                self.defer_cli_recovery(
                    r,
                    &format!("Removing the exited repair execution: {error}"),
                )
                .await?;
                return Ok(true);
            }
            let id = p.id.clone();
            let generation = p.generation.clone();
            self.db
                .call(move |c| {
                    work::finish_execution(
                        c,
                        &id,
                        &generation,
                        "exited",
                        "Repair Agent exited; native continuity retained for retry",
                        db::now(),
                    )?;
                    Ok(())
                })
                .await?;
            self.db
                .set_session(&p.id, None, None, String::new())
                .await?;
            self.defer_cli_recovery(
                r,
                "Restarting the repair Agent with its preserved native conversation.",
            )
            .await?;
            return Ok(true);
        }
        if let Err(error) = crate::repair_lane::check_identity(p, record) {
            if error.is::<herdr::SessionStarting>() && db::now() - r.phase_started_at < BUDGET_MS {
                self.defer_cli_recovery(
                    r,
                    "Waiting for the original native repair execution to finish starting.",
                )
                .await?;
                return Ok(true);
            }
            if herdr::recovery_foreground_quiescent(p, record) {
                let id = r.id.clone();
                self.db.call(move|c|phase(c,&id,"recovery_stopping","Replacing the owned execution whose native Agent identity no longer matches.")).await?;
            }
            self.defer_cli_recovery(
                r,
                &format!("Reobserving the original native execution before repair: {error}"),
            )
            .await?;
            return Ok(true);
        }
        if record["status"]["state"] == "error" && r.source["error_restart_done"] != true {
            if record["children"]["state"] == "active" {
                self.defer_cli_recovery(
                    r,
                    "Waiting for native children before restarting the errored repair Agent.",
                )
                .await?;
                return Ok(true);
            }
            if herdr::recovery_foreground_quiescent(p, record) {
                let rid = r.id.clone();
                self.db.call(move|c|{
                    c.execute("UPDATE cli_switch_requests SET source=json_set(source,'$.error_restart_done',json('true')) WHERE id=?",[&rid])?;
                    phase(c,&rid,"recovery_stopping","Restarting the owned errored repair execution once before native diagnostic input.")
                }).await?;
                self.notify.notify_one();
                return Ok(true);
            }
        }
        if record["status"]["state"] == "blocked" {
            let _ = herdr::confirm_project_trust(cfg, p).await;
            self.defer_cli_recovery(r,"Rechecking native readiness while preserving the repair conversation and queued input.").await?;
            return Ok(true);
        }
        let native_diagnostic_ready = matches!(
            record["status"]["state"].as_str(),
            Some("idle" | "done" | "working")
        ) || (record["status"]["state"] == "error"
            && record["interactive_ready"] == true
            && r.source["error_restart_done"] == true);
        if record["state_change_seq"].as_i64().is_none() || !native_diagnostic_ready {
            self.defer_cli_recovery(
                r,
                "Waiting for a verified native repair channel; ordinary delivery stays held.",
            )
            .await?;
            return Ok(true);
        }
        let rid = r.id.clone();
        let generation = p.generation.clone();
        let uncertain = self.db.call(move |c| {
            c.query_row("SELECT EXISTS(SELECT 1 FROM cli_repair_prompts WHERE request_id=? AND generation=? AND status IN ('submitting','uncertain'))",params![rid,generation],|row|row.get::<_,bool>(0)).map_err(Into::into)
        }).await?;
        if uncertain && r.recovery_generation == p.generation && !r.nonce.is_empty() {
            let rid = r.id.clone();
            self.db.call(move|c|phase(c,&rid,"requested","Reobserving the previous repair submission; its uncertain prompt is not repeated.")).await?;
            return Ok(true);
        }
        let text = artifact(p).await.unwrap_or(None);
        let mut rr = r.clone();
        rr.generation = p.generation.clone();
        rr.recovery_generation = p.generation.clone();
        rr.nonce = db::id();
        rr.baseline = handoff::fingerprint(text.as_deref());
        rr.sequence = record["state_change_seq"].as_i64().unwrap_or(0);
        let id = rr.id.clone();
        let generation = p.generation.clone();
        let nonce = rr.nonce.clone();
        let baseline = rr.baseline.clone();
        let sequence = rr.sequence;
        self.db.call(move|c|{
            ensure!(c.execute("UPDATE cli_switch_requests SET phase_started_at=?,next_attempt_at=0,generation=?,recovery_generation=?,nonce=?,baseline=?,sequence=?,attempts=attempts+1 WHERE id=? AND holds_input=1 AND phase IN ('recovery_pending','recovery_starting','repairing','failed','startup_failed') AND rowid=(SELECT MAX(rowid) FROM cli_switch_requests WHERE project_id=(SELECT project_id FROM cli_switch_requests WHERE id=?))",params![db::now(),generation,generation,nonce,baseline,sequence,id,id])?==1,"Repair request changed before diagnostic preparation");
            c.execute("UPDATE projects SET execution=json_set(execution,'$.cli_recovery_request',?) WHERE id=(SELECT project_id FROM cli_switch_requests WHERE id=?) AND generation=?",params![id,id,generation])?;
            Ok(())
        }).await?;
        let repair_context = if let Some(lane) = crate::repair_lane::descriptor(p) {
            format!(
                " repair_only=\"true\" original_native=\"{}\" observed_native=\"{}\"",
                crate::xml(&lane.expected_native).replace('"', "&quot;"),
                crate::xml(&lane.observed_native).replace('"', "&quot;")
            )
        } else {
            String::new()
        };
        let diagnostic = format!(
            "<dauerbot_cli_repair{repair_context} request_id=\"{}\">The requested switch to {} failed. You are a repair-only Agent, not the selected successor. If repair_only is true, observed_native is a separate diagnostic conversation; original_native remains preserved and unavailable for ordinary work. Do not claim to be that original conversation. Diagnose and fix the safe handoff or target startup cause using the existing local CLI, project and runtime. The daemon may submit this diagnostic through verified native input before MCP is registered; fix MCP connectivity when needed. Native input alone does not mean MCP readiness or successful user replies. Preserve native login/model settings, queued user input, files and already completed effects. Do not change the daemon database, unlock delivery, close your own terminal, start a second project Agent, or replay held tasks. Investigate actively and finish the repair; reporting a stuck state alone is not completion. Latest diagnostic: {}\nAfter repair, {}\n</dauerbot_cli_repair>",
            rr.id,
            rr.target,
            crate::xml(&format!(
                "{}\nRetained switch failure: {}",
                rr.error,
                rr.source["last_failure"]
                    .as_str()
                    .unwrap_or("No additional failure recorded.")
            )),
            prompt(&rr)
        );
        self.send_cli_repair_prompt(p, &rr, cfg, &diagnostic, true)
            .await?;
        Ok(true)
    }

    async fn send_cli_repair_prompt(
        &self,
        p: &Project,
        r: &Request,
        cfg: &Config,
        body: &str,
        initial: bool,
    ) -> Result<bool> {
        let rid = r.id.clone();
        let generation = p.generation.clone();
        let body = body.to_owned();
        let sequence = r.sequence;
        let baseline = r.baseline.clone();
        let nonce = r.nonce.clone();
        let current = p.clone();
        let journal = self.db.call(move|c| {
            let tx=c.unchecked_transaction()?;
            if crate::repair_lane::descriptor(&current).is_some() && let Some(lane)=crate::repair_lane::authorize(&tx,&current,false)? {crate::repair_lane::effect_phase(&tx,&lane)?;}
            let controls:Vec<(String,String)>=tx.prepare("SELECT instruction_id,payload FROM cli_repair_instructions WHERE request_id=? AND prompt_id='' ORDER BY rowid")?
                .query_map([&rid],|row|Ok((row.get(0)?,row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            if !initial && controls.is_empty() { return Ok(None); }
            if !initial {
                tx.execute("UPDATE cli_switch_requests SET baseline=?,sequence=?,phase_started_at=? WHERE id=? AND holds_input=1",params![baseline,sequence,db::now(),rid])?;
            }
            if initial {
                ensure!(tx.execute("UPDATE cli_switch_requests SET phase='requested',phase_started_at=? WHERE id=? AND holds_input=1 AND generation=? AND nonce=? AND phase IN ('recovery_pending','recovery_starting','repairing','failed','startup_failed')",params![db::now(),rid,generation,nonce])?==1,"Repair request changed before prompt admission");
            }
            let id=db::id();
            let controls_payload:Vec<Value>=controls.iter().map(|(_,raw)|serde_json::from_str(raw).unwrap_or(Value::Null)).collect();
            let payload=format!("{body}\n<repair_user_instructions trust=\"user_input\">{}\n</repair_user_instructions>",crate::xml(&serde_json::to_string(&controls_payload)?));
            tx.execute("INSERT INTO cli_repair_prompts(id,request_id,generation,payload,status,sequence,created_at) VALUES(?,?,?,?,'submitting',?,?)",params![id,rid,generation,payload,sequence,db::now()])?;
            for (instruction,_) in controls { tx.execute("UPDATE cli_repair_instructions SET prompt_id=? WHERE request_id=? AND instruction_id=? AND prompt_id=''",params![id,rid,instruction])?; }
            tx.commit()?;
            Ok(Some((id,payload)))
        }).await?;
        let Some((id, payload)) = journal else {
            return Ok(false);
        };
        let result = if let Some(lane) = crate::repair_lane::descriptor(p) {
            let current = p.clone();
            match self
                .db
                .call(move |c| {
                    let lane = crate::repair_lane::authorize(c, &current, false)?
                        .context("Repair lane retired")?;
                    crate::repair_lane::effect_phase(c, &lane)
                })
                .await
            {
                Ok(()) => herdr::send(cfg, &crate::repair_lane::view(p, &lane), &payload).await,
                Err(error) => Err(herdr::NotSubmitted(error).into()),
            }
        } else {
            herdr::send(cfg, p, &payload).await
        };
        let (status, error) = match result {
            Ok(_) => ("submitted", String::new()),
            Err(error) if herdr::definitely_not_sent(&error) => ("not_sent", error.to_string()),
            Err(error) => ("uncertain", error.to_string()),
        };
        let retry = status == "not_sent";
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE cli_repair_prompts SET status=?,error=? WHERE id=?",
                    params![status, error, id],
                )?;
                // A proven preflight failure permits a new submission. Uncertain
                // submissions retain their prompt association forever.
                if status == "not_sent" {
                    c.execute(
                        "UPDATE cli_repair_instructions SET prompt_id='' WHERE prompt_id=?",
                        [&id],
                    )?;
                }
                Ok(())
            })
            .await?;
        if retry && crate::repair_lane::descriptor(p).is_some() {
            let current = p.clone();
            let request = r.id.clone();
            self.db.call(move|c|{
                if let Ok(Some(lane))=crate::repair_lane::authorize(c,&current,false)
                    && crate::repair_lane::effect_phase(c,&lane).is_ok() {
                    c.execute("UPDATE cli_switch_requests SET phase='recovery_pending',next_attempt_at=0,error='A repair prompt was proven not sent; reobserving before retry' WHERE id=? AND holds_input=1 AND phase IN ('requested','repairing','recovery_pending','recovery_starting')",[request])?;
                }
                Ok(())
            }).await?;
            return Ok(true);
        }
        if retry {
            self.fail_cli_switch(
                r,
                "The repair prompt was definitely not submitted; retrying after native readiness.",
            )
            .await?;
            self.defer_cli_recovery(
                r,
                "The repair prompt was definitely not submitted; retrying after native readiness.",
            )
            .await?;
        }
        Ok(true)
    }

    async fn fail_cli_start(&self, r: &Request, error: &str) -> Result<()> {
        let id = r.id.clone();
        let error = error.to_owned();
        self.db.call(move |c| record_failure(c, &id, &error)).await
    }
    async fn fail_cli_switch(&self, r: &Request, error: &str) -> Result<()> {
        let id = r.id.clone();
        let error = error.to_owned();
        self.db.call(move |c| record_failure(c, &id, &error)).await
    }
    async fn start_cli_target(&self, p: &Project, r: &Request) -> Result<()> {
        let current = self.db.project(&p.id).await?;
        let id = p.id.clone();
        let generation = current.generation.clone();
        let takeover = self
            .db
            .call(move |c| work::has_takeover(c, &id, &generation))
            .await?;
        if current.paused || takeover {
            return Ok(());
        }
        let resume = r.source["runtime_target"]["agent"] == current.agent
            && !current.native_session.is_empty();
        if let Err(e) = self.start_inner(&p.id, resume).await {
            self.fail_cli_start(r,&format!("The CLI selection was saved, but startup failed: {e}. Workspace files and saved input are preserved.")).await?;
            self.attention(p, "startup", &e.to_string()).await?;
        }
        self.set_phase(p, "switching_cli", Value::Null).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::ProjectInput};
    async fn fixture() -> (tempfile::TempDir, Arc<App>, Project) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().into(), Config::default()).unwrap();
        let p = app
            .db
            .create(ProjectInput {
                name: "Switch fixture".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: "old-native".into(),
                extra_args: vec!["--model".into(), "preserve".into()],
            })
            .await
            .unwrap();
        app.db.sync(|c|{c.execute("UPDATE projects SET generation='old-gen',terminal='old-pane',ready=1,execution='{\"api_protocol\":7,\"mcp_contract\":1}' WHERE id=?",[&p.id])?;work::begin_execution(c,&p.id,"old-gen",0)?;Ok(())}).unwrap();
        (dir, app.clone(), app.db.project(&p.id).await.unwrap())
    }
    fn requested(c: &Connection, p: &Project) -> Result<Request> {
        c.execute("UPDATE cli_switch_requests SET phase='requested',nonce='fresh',baseline='old',sequence=10,revision=? WHERE project_id=?",params![revision(c,&p.id)?,p.id])?;
        Ok(latest(c, &p.id)?.unwrap())
    }
    #[tokio::test]
    async fn migration_holds_outstanding_transitions_but_not_historical_settled_failures() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db
            .sync(|c| {
                let r = latest(c, &p.id)?.unwrap();
                c.execute_batch("ALTER TABLE cli_switch_requests DROP COLUMN holds_input")?;
                phase(c, &r.id, "failed", "historical settled failure")?;
                migrate(c, false)?;
                assert!(!holds_input(c, &p.id)?);
                c.execute_batch("ALTER TABLE cli_switch_requests DROP COLUMN holds_input")?;
                phase(c, &r.id, "requested", "")?;
                migrate(c, true)?;
                assert_eq!(latest(c, &p.id)?.unwrap().phase, "recovery_pending");
                assert!(
                    holds_input(c, &p.id)?,
                    "an interrupted outstanding request remains held after restart"
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn explicit_request_is_idempotent_and_never_stops_active_agent() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        assert!(app.request_cli_switch(&p.id, "claude").await.is_err());
        assert_eq!(app.db.project(&p.id).await.unwrap().terminal, p.terminal);
        assert_eq!(
            app.db
                .sync(|c| Ok(
                    c.query_row("SELECT count(*) FROM cli_switch_requests", [], |r| r
                        .get::<_, i64>(0))?
                ))
                .unwrap(),
            1
        );
        app.db.pause(&p.id, true).await.unwrap();
        assert!(app.request_cli_switch(&p.id, "pi").await.is_err());
        assert!(
            !app.advance_cli_switch(&app.db.project(&p.id).await.unwrap(), None, None)
                .await
                .unwrap()
        );
        assert_eq!(app.db.project(&p.id).await.unwrap().agent, "codex");
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "preparing"
        );
    }
    fn queue(c: &Connection, p: &Project, id: &str, now: i64) -> Result<()> {
        c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES(?,?,'in',?,'pending direction','queued',?,?)",params![id,p.id,id,now,now])?;
        work::sync_inbound(c)?;
        Ok(())
    }
    #[tokio::test]
    async fn vanished_execution_retirement_preserves_native_pending_and_uncertain_effects() {
        let (_dir, app, p) = fixture().await;
        let submitted = app.db
            .sync(|c| {
                let submitted = crate::manager::test_delegate(c, &p.id, "already-submitted", None)?;
                let run = work::claim_steering(c, &p.id, &p.generation, 2)?.unwrap();
                work::delivery(c, &run.id, "submitted", "", 2)?;
                queue(c, &p, "still-pending", 3)?;
                Ok(submitted)
            })
            .unwrap();
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        app.db
            .sync(|c| {
                let r = latest(c, &p.id)?.unwrap();
                assert!(retire_vanished(c, &p, &r)?);
                assert_eq!(
                    work::event(c, &p.id, &submitted)?.status,
                    "uncertain"
                );
                assert_eq!(work::event(c, &p.id, "still-pending")?.status, "managed");
                assert!(holds_input(c, &p.id)?);
                assert!(work::claim_steering(c, &p.id, "repair-generation", 4)?.is_none());
                let retired = latest(c, &p.id)?.unwrap();
                assert_eq!(retired.phase, "recovery_pending");
                assert_eq!(retired.source, r.source);
                assert!(
                    !retire_vanished(c, &p, &r)?,
                    "The same stale observation cannot clear a later execution"
                );
                Ok(())
            })
            .unwrap();
        let current = app.db.project(&p.id).await.unwrap();
        assert!(current.terminal.is_none());
        assert!(current.terminal_pid.is_none());
        assert!(!current.ready);
        assert!(current.execution.is_null());
        assert_eq!(current.native_session, p.native_session);
        assert_eq!(current.extra_args, p.extra_args);
        assert_eq!(current.agent, p.agent);
    }
    #[tokio::test]
    async fn vanished_retirement_cas_respects_pause_takeover_and_replaced_execution() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        app.db
            .sync(|c| {
                let r = latest(c, &p.id)?.unwrap();
                c.execute("UPDATE projects SET paused=1 WHERE id=?", [&p.id])?;
                assert!(!retire_vanished(c, &p, &r)?);
                c.execute("UPDATE projects SET paused=0 WHERE id=?", [&p.id])?;
                work::set_takeover(c, &p.id, &p.generation, true, 1)?;
                assert!(!retire_vanished(c, &p, &r)?);
                work::set_takeover(c, &p.id, &p.generation, false, 2)?;
                c.execute(
                    "UPDATE projects SET terminal='new-terminal' WHERE id=?",
                    [&p.id],
                )?;
                assert!(!retire_vanished(c, &p, &r)?);
                assert!(holds_input(c, &p.id)?);
                assert_eq!(latest(c, &p.id)?.unwrap().phase, r.phase);
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn accepted_switch_holds_all_unclaimed_input_without_invalidating_handoff() {
        let (_dir, app, p) = fixture().await;
        let submitted = app.db
            .sync(|c| {
                let submitted = crate::manager::test_delegate(c, &p.id, "already-submitted", None)?;
                let run = work::claim_steering(c, &p.id, &p.generation, 2)?.unwrap();
                work::delivery(c, &run.id, "submitted", "", 2)?;
                queue(c, &p, "pending-before", 3)?;
                Ok(submitted)
            })
            .unwrap();
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db.sync(|c|{
            let r=requested(c,&p)?;
            queue(c,&p,"pending-after",4)?;
            let held = crate::manager::test_delegate(c,&p.id,"held-delegation",Some("pending-after"))?;
            let timer=work::create_timer(c,&p.id,serde_json::from_value(serde_json::json!({"name":"While switching","prompt":"scheduled input","kind":"once","at":5}))?,0)?;
            assert_eq!(work::fire_due(c,6)?.len(),1);
            assert!(work::claim_steering(c,&p.id,&p.generation,7)?.is_none());
            assert!(work::claim_next(c,&p.id,&p.generation,7)?.is_none());
            assert_eq!(latest(c,&p.id)?.unwrap().nonce,r.nonce);
            assert!(claim(c,&p.id,&r,"accepted")?,"queued input cannot invalidate the handoff");
            complete(c,&p.id,&r)?;
            assert!(holds_input(c,&p.id)?);
            assert_eq!(work::event(c,&p.id,&submitted)?.status,"uncertain");
            // Old registration and a wrong observed generation cannot unlock input.
            assert!(!ready(c,&p.id,&p.generation,&r)?);
            c.execute("UPDATE projects SET generation='new-gen',terminal='new-pane',ready=1,execution=json_object('cli_switch_request',?,'mcp_registered',json('true')) WHERE id=?",params![r.id,p.id])?;
            assert!(!ready(c,&p.id,"stale-generation",&r)?);
            assert!(ready(c,&p.id,"new-gen",&r)?);
            assert!(!holds_input(c,&p.id)?);
            let delegation=work::claim_steering(c,&p.id,"new-gen",8)?.unwrap();
            assert_eq!(delegation.event_id,held);
            work::delivery(c,&delegation.id,"submitted","",8)?;
            for source in ["pending-before","pending-after"] {
                assert_eq!(work::event(c,&p.id,source)?.status,"managed");
                assert_eq!(c.query_row("SELECT status FROM manager_inputs WHERE origin_event_id=?",[source],|v|v.get::<_,String>(0))?,"queued");
            }
            let status: String = c.query_row("SELECT e.status FROM work_events e JOIN work_occurrences o ON o.event_id=e.id WHERE o.timer_id=?", [timer.id], |v|v.get(0))?;
            assert_eq!(status,"managed");
            assert!(work::claim_next(c,&p.id,"new-gen",10)?.is_none());
            Ok(())
        }).unwrap();
        let changed = app.db.project(&p.id).await.unwrap();
        assert_eq!(changed.agent, "pi");
        assert!(changed.native_session.is_empty() && changed.extra_args.is_empty());
        assert_eq!(changed.path, p.path);
    }
    #[tokio::test]
    async fn failed_restart_pause_retry_and_explicit_stop_never_leak_pending_input() {
        let (dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db
            .sync(|c| {
                queue(c, &p, "held", 1)?;
                let r = latest(c, &p.id)?.unwrap();
                phase(c, &r.id, "failed", "handoff failed")?;
                assert!(work::claim_steering(c, &p.id, &p.generation, 2)?.is_none());
                Ok(())
            })
            .unwrap();
        assert!(app.switch_agent(&p.id, "claude").await.is_err());
        app.db.pause(&p.id, true).await.unwrap();
        app.advance_cli_switch(&app.db.project(&p.id).await.unwrap(), None, None)
            .await
            .unwrap();
        assert!(app.db.sync(|c| holds_input(c, &p.id)).unwrap());
        app.db.pause(&p.id, false).await.unwrap();
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        assert!(app.db.sync(|c| holds_input(c, &p.id)).unwrap());
        drop(app);
        let db = crate::db::Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
        db.sync(|c| {
            assert_eq!(latest(c, &p.id)?.unwrap().phase, "failed");
            assert!(holds_input(c, &p.id)?);
            assert!(work::claim_next(c, &p.id, &p.generation, 3)?.is_none());
            assert!(stopped(c, &p.id, "stale-generation").is_err());
            assert!(holds_input(c, &p.id)?);
            stopped(c, &p.id, &p.generation)?;
            assert!(!holds_input(c, &p.id)?);
            assert_eq!(latest(c, &p.id)?.unwrap().phase, "cancelled");
            assert!(
                c.query_row("SELECT paused FROM projects WHERE id=?", [&p.id], |v| v
                    .get::<_, bool>(0))?
            );
            assert_eq!(work::event(c, &p.id, "held")?.status, "managed");
            assert_eq!(c.query_row("SELECT status FROM manager_inputs WHERE origin_event_id='held'", [], |r| r.get::<_, String>(0))?, "queued");
            Ok(())
        })
        .unwrap();
    }
    #[tokio::test]
    async fn lost_source_execution_cannot_be_mistaken_for_an_originally_sleeping_project() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db
            .set_session(&p.id, None, None, String::new())
            .await
            .unwrap();
        let exited = app.db.project(&p.id).await.unwrap();
        assert!(app.advance_cli_switch(&exited, None, None).await.unwrap());
        assert_eq!(app.db.project(&p.id).await.unwrap().agent, "codex");
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "recovery_pending"
        );
    }
    #[tokio::test]
    async fn selected_generation_waits_for_mcp_and_late_registration_resolves_startup_error() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        let id = app
            .db
            .sync(|c| {
                let r = latest(c, &p.id)?.unwrap();
                phase(c, &r.id, "starting", "")?;
                Ok(r.id)
            })
            .unwrap();
        assert!(
            app.cli_switch_holds_input(&p).await.unwrap(),
            "old generation must not receive pending input"
        );
        app.db.sync(|c|{c.execute("UPDATE projects SET agent='pi',generation='new-gen',execution=json_object('api_protocol',7,'mcp_contract',1,'cli_switch_request',?) WHERE id=?",params![id,p.id])?;c.execute("UPDATE cli_switch_requests SET phase='starting',phase_started_at=0 WHERE id=?",[&id])?;Ok(())}).unwrap();
        let waiting = app.db.project(&p.id).await.unwrap();
        assert!(app.cli_switch_holds_input(&waiting).await.unwrap());
        assert!(
            app.advance_cli_switch(
                &waiting,
                None,
                Some(&json!({"state":"live","status":{"state":"idle"}}))
            )
            .await
            .unwrap()
        );
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "recovery_pending"
        );
        app.db.pause(&p.id, true).await.unwrap();
        assert!(
            !app.advance_cli_switch(&app.db.project(&p.id).await.unwrap(), None, None)
                .await
                .unwrap()
        );
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "recovery_pending"
        );
        app.db.pause(&p.id, false).await.unwrap();
        app.db.sync(|c|{c.execute("UPDATE projects SET execution=json_set(execution,'$.mcp_registered',json('true')) WHERE id=?",[&p.id])?;Ok(())}).unwrap();
        app.db.sync(|c|{
            c.execute("UPDATE projects SET execution=json_set(execution,'$.backend','legacy','$.workspace_id','workspace','$.pane_id','old-pane','$.terminal_id','old-pane','$.managed_name','managed','$.generation','new-gen','$.shell_pid',NULL,'$.launch_permit','/private/fixture/launch') WHERE id=?",[&p.id])?;
            Ok(())
        }).unwrap();
        let connected = app.db.project(&p.id).await.unwrap();
        let observed = json!({"state":"live","status":{"state":"idle"},"foreground":{"name":"pi"},"execution":connected.execution,
            "agent":{"agent":"pi","pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace"},
            "tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":"new-gen","hfb_execution":"managed"}});
        assert!(herdr::check_identity(&connected, &observed).is_ok());
        assert!(app.cli_switch_holds_input(&connected).await.unwrap());
        assert!(
            !app.advance_cli_switch(&connected, None, Some(&observed))
                .await
                .unwrap()
        );
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "ready"
        );
        app.db.sync(|c|{phase(c,&id,"recovery_pending","old attempt")?;c.execute("UPDATE projects SET generation='unrelated',execution=json_object('api_protocol',7,'mcp_contract',1) WHERE id=?",[&p.id])?;Ok(())}).unwrap();
        assert!(
            !app.cli_switch_holds_input(&app.db.project(&p.id).await.unwrap())
                .await
                .unwrap(),
            "the completed request has released its hold"
        );
    }
    #[tokio::test]
    async fn pause_after_claim_keeps_uncertain_execution_fenced_without_startup() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db
            .sync(|c| {
                let r = requested(c, &p)?;
                assert!(claim(c, &p.id, &r, "accepted")?);
                phase(c, &r.id, "uncertain", "Lost stop response")?;
                Ok(())
            })
            .unwrap();
        app.db.pause(&p.id, true).await.unwrap();
        let paused = app.db.project(&p.id).await.unwrap();
        assert!(!app.advance_cli_switch(&paused, None, None).await.unwrap());
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "uncertain"
        );
        let preserved = app.db.project(&p.id).await.unwrap();
        assert_eq!(preserved.agent, "codex");
        assert_eq!(preserved.terminal, p.terminal);
        assert_eq!(preserved.native_session, p.native_session);
    }
    #[tokio::test]
    async fn nonce_native_transition_takeover_and_restart_cannot_authorize_stale_stop() {
        let (dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db.sync(|c|{
            let r=requested(c,&p)?;let text=format!("# Handoff\n{}\n<!-- dauerbot-switch-ready:fresh -->","Settled all children and external work; continue the current user goal without replaying completed effects. ".repeat(2));
            assert!(!proof(&r,Some(&text),Some(10)));assert!(proof(&r,Some(&text),Some(11)));
            assert!(!proof(&r,Some(&text.replace("fresh","stale")),Some(11)));
            work::set_takeover(c,&p.id,&p.generation,true,1)?;assert!(!claim(c,&p.id,&r,"accepted")?);work::set_takeover(c,&p.id,&p.generation,false,2)?;
            assert!(claim(c,&p.id,&r,"accepted")?);Ok(())
        }).unwrap();
        drop(app);
        let reopened = crate::db::Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
        reopened
            .sync(|c| {
                assert_eq!(latest(c, &p.id)?.unwrap().phase, "uncertain");
                Ok(())
            })
            .unwrap();
        let preserved = reopened.project(&p.id).await.unwrap();
        assert_eq!(preserved.agent, "codex");
        assert_eq!(preserved.native_session, "old-native");
        assert_eq!(preserved.terminal, p.terminal);
    }
    #[tokio::test]
    async fn source_snapshot_survives_cutover_and_target_gets_its_own_startup_clock() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        app.db
            .sync(|c| {
                let r = requested(c, &p)?;
                assert_eq!(r.source["agent"], "codex");
                assert_eq!(r.source["native_session"], "old-native");
                assert_eq!(r.source["extra_args"], json!(["--model", "preserve"]));
                c.execute(
                    "UPDATE cli_switch_requests SET phase_started_at=1 WHERE id=?",
                    [&r.id],
                )?;
                assert!(claim(c, &p.id, &r, "accepted")?);
                complete(c, &p.id, &r)?;
                let switched = latest(c, &p.id)?.unwrap();
                assert!(switched.phase_started_at > 1);
                assert_eq!(switched.source, r.source);
                assert!(switched.holds_input);
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn historical_failed_switch_recovers_original_native_identity_from_execution_journal() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        app.db.sync(|c|{
            c.execute("UPDATE cli_switch_requests SET source='{}',phase='failed' WHERE project_id=?",[&p.id])?;
            c.execute("UPDATE projects SET agent='agy',generation='candidate',native_session='' WHERE id=?",[&p.id])?;
            Ok(())
        }).unwrap();
        let current = app.db.project(&p.id).await.unwrap();
        let mut r = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        app.ensure_cli_source(&current, &mut r).await.unwrap();
        assert_eq!(r.source["agent"], "codex");
        assert_eq!(r.source["native_session"], "old-native");
        assert_eq!(r.source["historical_arguments_unavailable"], true);
        assert_eq!(r.source["extra_args"], json!([]));
    }
    #[tokio::test]
    async fn repair_controls_are_deduplicated_without_unlocking_ordinary_input() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        app.db
            .sync(|c| {
                let payload = json!({"content":"repair startup","message_id":"om_control"});
                journal_repair_instruction(c, &p.id, "om_control", &payload)?;
                journal_repair_instruction(c, &p.id, "om_control", &payload)?;
                assert!(
                    journal_repair_instruction(
                        c,
                        &p.id,
                        "om_control",
                        &json!({"content":"different"})
                    )
                    .is_err()
                );
                assert_eq!(
                    c.query_row("SELECT count(*) FROM cli_repair_instructions", [], |r| r
                        .get::<_, i64>(0))?,
                    1
                );
                queue(c, &p, "ordinary", 1)?;
                assert!(work::claim_steering(c, &p.id, &p.generation, 2)?.is_none());
                assert!(holds_input(c, &p.id)?);
                Ok(())
            })
            .unwrap();
        app.request_cli_repair(&p.id).await.unwrap();
        assert!(app.db.sync(|c| holds_input(c, &p.id)).unwrap());
    }
    #[tokio::test]
    async fn restart_preserves_uncertain_repair_payload_and_its_control_association() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        app.db.sync(|c|{
            let r=latest(c,&p.id)?.unwrap();
            journal_repair_instruction(c,&p.id,"control",&json!({"content":"fix"}))?;
            c.execute("INSERT INTO cli_repair_prompts(id,request_id,generation,payload,status,sequence,created_at) VALUES('prompt',?,'old-gen','exact payload','submitting',12,1)",[&r.id])?;
            c.execute("UPDATE cli_repair_instructions SET prompt_id='prompt' WHERE request_id=?",[&r.id])?;
            migrate(c,true)?;
            let saved:(String,String)=c.query_row("SELECT payload,status FROM cli_repair_prompts WHERE id='prompt'",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
            assert_eq!(saved,("exact payload".into(),"uncertain".into()));
            assert_eq!(c.query_row("SELECT prompt_id FROM cli_repair_instructions WHERE request_id=?",[&r.id],|row|row.get::<_,String>(0))?,"prompt");
            assert!(holds_input(c,&p.id)?);
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn post_ready_repair_reuses_original_source_and_creates_a_new_delivery_fence() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        app.db.sync(|c|{
            let r=requested(c,&p)?;
            assert!(claim(c,&p.id,&r,"accepted")?);
            complete(c,&p.id,&r)?;
            c.execute("UPDATE projects SET generation='agy-gen',terminal='agy-pane',ready=1,native_session='agy-live-native',extra_args=?,execution=json_object('cli_switch_request',?,'mcp_registered',json('true')) WHERE id=?",params![json!(["--model","agy-current-model"]).to_string(),r.id,p.id])?;
            assert!(ready(c,&p.id,"agy-gen",&r)?);
            assert!(!holds_input(c,&p.id)?);
            journal_repair_instruction(c,&p.id,"om_after_ready",&json!({"content":"repair runtime"}))?;
            let repair=latest(c,&p.id)?.unwrap();
            assert_ne!(repair.id,r.id);
            assert_eq!(repair.target,"agy");
            assert_eq!(repair.source["agent"],"codex");
            assert_eq!(repair.source["native_session"],"old-native");
            assert_eq!(repair.source["extra_args"],json!(["--model","preserve"]));
            assert!(repair.holds_input);
            assert_eq!(repair.phase,"recovery_pending");
            assert_eq!(repair.source["runtime_target"]["native_session"],"agy-live-native");
            assert_eq!(repair.source["runtime_target"]["extra_args"],json!(["--model","agy-current-model"]));
            Ok(())
        }).unwrap();
        let before = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap().id)).unwrap();
        app.request_cli_repair(&p.id).await.unwrap();
        assert_eq!(
            before,
            app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap().id)).unwrap()
        );
    }
    async fn healthy_observation_fixture() -> (tempfile::TempDir, Arc<App>, Project, Value) {
        healthy_observation_fixture_at_startup(false).await
    }
    async fn healthy_observation_fixture_at_startup(
        startup: bool,
    ) -> (tempfile::TempDir, Arc<App>, Project, Value) {
        let (dir, app, p) = fixture().await;
        let mut execution = json!({"api_protocol":7,"mcp_contract":1,"backend":"legacy",
            "workspace_id":"workspace","pane_id":"old-pane","terminal_id":"old-pane",
            "managed_name":"managed","generation":"old-gen","shell_pid":null,
            "launch_permit":"/private/fixture/launch","cli_switch_request":"completed",
            "intended_native_session":"old-native","native_confirmed":true,"mcp_registered":true});
        if startup {
            for key in [
                "mcp_registered",
                "native_confirmed",
                "intended_native_session",
            ] {
                execution.as_object_mut().unwrap().remove(key);
            }
        }
        app.db.sync(|c| {
            c.execute("UPDATE projects SET execution=?,ready=?,native_session=? WHERE id=?",params![execution.to_string(),!startup,if startup { "" } else { "old-native" },p.id])?;
            c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input,source) VALUES('completed',?,'codex','ready','previous-generation',1,0,?)",
                params![p.id,json!({"agent":"claude","native_session":"previous-claude-native","extra_args":[]}).to_string()])?;
            crate::manager::test_delegate(c,&p.id,"pending-observation",None)?;
            Ok(())
        }).unwrap();
        let p = app.db.project(&p.id).await.unwrap();
        assert!(app.supervise_unobserved_cli(&p).await.unwrap());
        if startup {
            // Exact ordinary registration updates, with no launch/identity edit.
            app.db.sync(|c| {
                c.execute("UPDATE projects SET ready=1,native_session='old-native',execution=json_set(execution,'$.native_confirmed',json('true'),'$.mcp_registered',json('true')) WHERE id=? AND generation=? AND terminal=?",params![p.id,p.generation,p.terminal])?;
                Ok(())
            }).unwrap();
        }
        let p = app.db.project(&p.id).await.unwrap();
        let record = json!({"state":"live","status":{"state":"working"},"foreground":{"name":"codex"},
            "processes":[{"pid":100,"name":"codex","argv":["codex"]}],"interactive_ready":true,"state_change_seq":22,
            "native_session":{"value":"old-native"},"execution":p.execution,"pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace",
            "agent":{"agent":"codex","pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace"},
            "tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":"old-gen","hfb_execution":"managed"},"children":{"state":"active"}});
        assert!(herdr::check_identity(&p, &record).is_ok());
        (dir, app, p, record)
    }
    #[tokio::test]
    async fn healthy_observation_resumes_same_cross_cli_execution_without_repair_or_restart() {
        let (_dir, app, p, record) = healthy_observation_fixture().await;
        // Restart migration must retain the explicit internal marker, while
        // legacy rows without it remain on the normal repair path.
        app.db.sync(|c| migrate(c, true)).unwrap();
        let request = app.db.sync(|c| latest(c, &p.id)).unwrap().unwrap();
        assert_eq!(request.source["observation_fence"]["schema"], 1);
        assert_eq!(
            request.source["agent"], "claude",
            "the historical repair owner is a different CLI"
        );
        assert!(
            !app.advance_cli_switch(&p, None, Some(&record))
                .await
                .unwrap()
        );
        let current = app.db.project(&p.id).await.unwrap();
        assert_eq!(current.generation, p.generation);
        assert_eq!(current.native_session, p.native_session);
        assert_eq!(current.terminal, p.terminal);
        assert_eq!(
            current.execution, p.execution,
            "do not forge new request ownership"
        );
        app.db
            .sync(|c| {
                let resumed = latest(c, &p.id)?.unwrap();
                assert_eq!(resumed.id, request.id);
                assert_eq!(resumed.phase, "ready");
                assert!(!resumed.holds_input);
                assert_eq!(
                    c.query_row("SELECT count(*) FROM cli_repair_prompts", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                let pending = work::claim_steering(c, &p.id, &p.generation, db::now())?.unwrap();
                assert_eq!(pending.generation, p.generation);
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn startup_observation_fence_accepts_verified_registration_progress() {
        let (_dir, app, p, record) = healthy_observation_fixture_at_startup(true).await;
        let request = app.db.sync(|c| latest(c, &p.id)).unwrap().unwrap();
        let fence = &request.source["observation_fence"];
        assert_eq!(fence["native_session"], "");
        assert!(fence["execution"].get("mcp_registered").is_none());
        assert!(fence["execution"].get("native_confirmed").is_none());
        assert_eq!(p.execution["mcp_registered"], true);
        assert_eq!(p.execution["native_confirmed"], true);
        assert_eq!(p.native_session, "old-native");
        assert!(
            !app.advance_cli_switch(&p, None, Some(&record))
                .await
                .unwrap()
        );
        let current = app.db.project(&p.id).await.unwrap();
        assert_eq!(current.execution, p.execution);
        assert_eq!(current.terminal, p.terminal);
        assert_eq!(current.generation, p.generation);
        app.db
            .sync(|c| {
                assert!(!holds_input(c, &p.id)?);
                assert_eq!(latest(c, &p.id)?.unwrap().phase, "ready");
                assert_eq!(
                    c.query_row("SELECT count(*) FROM cli_repair_prompts", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                assert!(work::claim_steering(c, &p.id, &p.generation, db::now())?.is_some());
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn startup_observation_fence_rejects_changed_launch_authority_after_registration() {
        for key in ["managed_name", "future_authority"] {
            let (_dir, app, mut p, mut record) = healthy_observation_fixture_at_startup(true).await;
            p.execution[key] = json!("replacement");
            record["execution"] = p.execution.clone();
            if key == "managed_name" {
                record["tokens"]["hfb_execution"] = json!("replacement");
            }
            app.db
                .sync(|c| {
                    c.execute(
                        "UPDATE projects SET execution=? WHERE id=?",
                        params![p.execution.to_string(), p.id],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(herdr::check_identity(&p, &record).is_ok());
            let request = app.db.sync(|c| latest(c, &p.id)).unwrap().unwrap();
            assert!(
                !app.db
                    .sync(|c| App::settle_observation_fence(c, &p, &request, &record))
                    .unwrap(),
                "{key}"
            );
            assert!(app.db.sync(|c| holds_input(c, &p.id)).unwrap());
        }
    }
    #[tokio::test]
    async fn observation_settlement_rejects_stale_authority_and_pending_repair_effects() {
        for mutation in [
            "pause",
            "takeover",
            "stop",
            "generation",
            "execution",
            "mcp",
            "native",
            "error",
            "repair-prompt",
            "repair-control",
            "old-marker",
        ] {
            let (_dir, app, p, mut record) = healthy_observation_fixture().await;
            let request = app.db.sync(|c| latest(c, &p.id)).unwrap().unwrap();
            app.db.sync(|c| {
                match mutation {
                    "pause" => {c.execute("UPDATE projects SET paused=1 WHERE id=?",[&p.id])?;},
                    "takeover" => {work::set_takeover(c,&p.id,&p.generation,true,db::now())?;},
                    "stop" => {stopped(c,&p.id,&p.generation)?;},
                    "generation" => {c.execute("UPDATE projects SET generation='replacement' WHERE id=?",[&p.id])?;},
                    "execution" => {c.execute("UPDATE projects SET execution=json_set(execution,'$.replacement',1) WHERE id=?",[&p.id])?;},
                    "mcp" => {c.execute("UPDATE projects SET execution=json_set(execution,'$.mcp_registered',json('false')) WHERE id=?",[&p.id])?;},
                    "repair-prompt" => {c.execute("INSERT INTO cli_repair_prompts(id,request_id,generation,payload,status,sequence,created_at) VALUES('uncertain',?,'old-gen','do not replay','uncertain',1,1)",[&request.id])?;},
                    "repair-control" => {journal_repair_instruction(c,&p.id,"control",&json!({"content":"repair explicitly"}))?;},
                    "old-marker" => {c.execute("UPDATE cli_switch_requests SET source=json_remove(source,'$.observation_fence') WHERE id=?",[&request.id])?;},
                    _ => {},
                }
                Ok(())
            }).unwrap();
            if mutation == "native" {
                record["native_session"]["value"] = json!("another-native");
            }
            if mutation == "error" {
                record["status"]["state"] = json!("error");
            }
            let stored = app.db.sync(|c| latest(c, &p.id)).unwrap().unwrap();
            assert!(
                !app.db
                    .sync(|c| App::settle_observation_fence(c, &p, &stored, &record))
                    .unwrap(),
                "{mutation}"
            );
            if mutation != "stop" {
                assert!(
                    app.db.sync(|c| holds_input(c, &p.id)).unwrap(),
                    "{mutation}"
                );
            }
        }
    }
    #[tokio::test]
    async fn post_ready_observation_loss_preserves_input_and_existing_repair_submission() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c| {
            c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input,source) VALUES('completed',?,'codex','ready','old-gen',1,0,?)",
                params![p.id,json!({"agent":"codex","native_session":"old-native","extra_args":p.extra_args}).to_string()])?;
            queue(c, &p, "pending-after-ready", 1)?;
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('uncertain-effect',?,'out','effect','preserve','uncertain',1,1)",[&p.id])?;
            Ok(())
        }).unwrap();
        assert!(app.supervise_unobserved_cli(&p).await.unwrap());
        let repair = app.db.sync(|c| {
            let repair = latest(c, &p.id)?.unwrap();
            assert_ne!(repair.id, "completed");
            assert_eq!(repair.phase, "recovery_pending");
            assert!(repair.holds_input);
            assert!(work::claim_steering(c, &p.id, &p.generation, 2)?.is_none());
            assert_eq!(work::event(c, &p.id, "pending-after-ready")?.status, "managed");
            assert_eq!(c.query_row("SELECT status FROM messages WHERE id='uncertain-effect'",[],|r|r.get::<_,String>(0))?, "uncertain");
            c.execute("UPDATE cli_switch_requests SET phase='requested',nonce='preserved-nonce',next_attempt_at=12345 WHERE id=?",[&repair.id])?;
            c.execute("INSERT INTO cli_repair_prompts(id,request_id,generation,payload,status,sequence,created_at) VALUES('uncertain-prompt',?,'old-gen','preserved prompt','uncertain',1,1)",[&repair.id])?;
            Ok(repair)
        }).unwrap();
        assert!(app.supervise_unobserved_cli(&p).await.unwrap());
        app.db
            .sync(|c| {
                let current = latest(c, &p.id)?.unwrap();
                assert_eq!(current.id, repair.id);
                assert_eq!(current.phase, "requested");
                assert_eq!(current.nonce, "preserved-nonce");
                assert_eq!(current.next_attempt_at, 12345);
                assert_eq!(
                    c.query_row("SELECT count(*) FROM cli_switch_requests", [], |r| r
                        .get::<_, i64>(0))?,
                    2
                );
                assert_eq!(
                    c.query_row(
                        "SELECT status FROM cli_repair_prompts WHERE id='uncertain-prompt'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "uncertain"
                );
                Ok(())
            })
            .unwrap();
        let current = app.db.project(&p.id).await.unwrap();
        assert_eq!(current.generation, p.generation);
        assert_eq!(current.terminal, p.terminal);
        assert_eq!(current.native_session, p.native_session);
    }
    #[tokio::test]
    async fn unobserved_cli_election_respects_pause_takeover_and_exact_execution() {
        for mutation in ["pause", "takeover", "generation", "terminal", "execution"] {
            let (_dir, app, p) = fixture().await;
            app.db.sync(|c| {
                match mutation {
                    "pause" => {c.execute("UPDATE projects SET paused=1 WHERE id=?",[&p.id])?;},
                    "takeover" => {work::set_takeover(c,&p.id,&p.generation,true,db::now())?;},
                    "generation" => {c.execute("UPDATE projects SET generation='replacement' WHERE id=?",[&p.id])?;},
                    "terminal" => {c.execute("UPDATE projects SET terminal='replacement' WHERE id=?",[&p.id])?;},
                    _ => {c.execute("UPDATE projects SET execution=json_set(execution,'$.replacement',1) WHERE id=?",[&p.id])?;},
                }
                Ok(())
            }).unwrap();
            assert!(
                !app.supervise_unobserved_cli(&p).await.unwrap(),
                "{mutation}"
            );
            assert!(
                app.db.sync(|c| latest(c, &p.id)).unwrap().is_none(),
                "{mutation}"
            );
        }
    }
    #[tokio::test]
    async fn runtime_fault_without_any_queued_message_journals_supervised_native_repair() {
        let (_dir, app, p) = fixture().await;
        app.journal_cli_fault(&p.id, "post-ready native error")
            .await
            .unwrap();
        app.db
            .sync(|c| {
                let r = latest(c, &p.id)?.unwrap();
                assert_eq!(r.phase, "recovery_pending");
                assert_eq!(r.target, "codex");
                assert_eq!(r.source["native_session"], "old-native");
                assert_eq!(r.source["last_failure"], "post-ready native error");
                assert!(r.holds_input);
                assert_eq!(
                    c.query_row("SELECT count(*) FROM messages", [], |row| row
                        .get::<_, i64>(0))?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn observation_loss_then_errored_source_supervises_restart_without_killing_children() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c|{
            c.execute("UPDATE projects SET execution=json_object('api_protocol',7,'mcp_contract',1,'backend','legacy','workspace_id','workspace','pane_id','old-pane','terminal_id','old-pane','managed_name','managed','generation','old-gen','shell_pid',NULL,'launch_permit','/private/fixture/launch') WHERE id=?",[&p.id])?;
            Ok(())
        }).unwrap();
        let p = app.db.project(&p.id).await.unwrap();
        assert!(app.supervise_unobserved_cli(&p).await.unwrap());
        let mut record = json!({"state":"live","status":{"state":"error"},"foreground":{"name":"codex"},"processes":[{"pid":100,"name":"codex","argv":["codex"]}],"interactive_ready":true,"state_change_seq":22,
            "execution":p.execution,"pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace",
            "agent":{"agent":"codex","pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace"},
            "tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":"old-gen","hfb_execution":"managed"},"children":{"state":"active"}});
        assert!(herdr::check_identity(&p, &record).is_ok());
        assert!(
            app.advance_cli_switch(&p, None, Some(&record))
                .await
                .unwrap()
        );
        let r = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        assert_ne!(r.source["error_restart_done"], true);
        assert_eq!(app.db.project(&p.id).await.unwrap().terminal, p.terminal);
        app.request_cli_repair(&p.id).await.unwrap();
        record["children"]["state"] = json!("quiescent");
        assert!(
            app.advance_cli_switch(&p, None, Some(&record))
                .await
                .unwrap()
        );
        let r = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        assert_eq!(r.phase, "recovery_stopping");
        assert_eq!(r.source["error_restart_done"], true);
        assert_eq!(
            app.db.project(&p.id).await.unwrap().terminal,
            p.terminal,
            "restart intent is durable before any native close"
        );
        assert!(r.holds_input);
    }
    #[tokio::test]
    async fn uncertain_stop_rechecks_activity_children_and_accepted_handoff_before_any_native_effect()
     {
        let (dir, app, p) = fixture().await;
        app.db.sync(|c|{
            c.execute("UPDATE projects SET execution=json_object('api_protocol',7,'mcp_contract',1,'backend','legacy','workspace_id','workspace','pane_id','old-pane','terminal_id','old-pane','managed_name','managed','generation','old-gen','shell_pid',NULL,'launch_permit','/private/fixture/launch') WHERE id=?",[&p.id])?;
            Ok(())
        }).unwrap();
        let p = app.db.project(&p.id).await.unwrap();
        app.request_cli_switch(&p.id, "agy").await.unwrap();
        let text = format!(
            "# Settled source handoff\n{}\n<!-- dauerbot-switch-ready:fresh -->",
            "Current context; all native children and external effects settled. ".repeat(3)
        );
        std::fs::write(dir.path().join("HANDOFF.md"), &text).unwrap();
        let r = app
            .db
            .sync(|c| {
                let r = requested(c, &p)?;
                assert!(claim(c, &p.id, &r, &handoff::fingerprint(Some(&text)))?);
                phase(c, &r.id, "uncertain", "Lost native stop response")?;
                Ok(latest(c, &p.id)?.unwrap())
            })
            .unwrap();
        let good = json!({"state":"live","status":{"state":"idle"},"foreground":{"name":"codex"},"state_change_seq":22,
            "execution":p.execution,"pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace",
            "agent":{"agent":"codex","pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace"},
            "tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":"old-gen","hfb_execution":"managed"},
            "children":{"state":"quiescent","revision":"settled"},"processes":[{"name":"codex","argv":["codex"]}]});
        assert!(stop_retry_safe(&p, &r, &good, Some(&text)));
        let mut bad_foreground = good.clone();
        bad_foreground["status"]["state"] = json!("working");
        let mut child = good.clone();
        child["children"]["state"] = json!("active");
        let mut tool = good.clone();
        tool["processes"] = json!([{"name":"python3","argv":["python3","ongoing.py"]}]);
        for observed in [&bad_foreground, &child, &tool] {
            app.db
                .sync(|c| phase(c, &r.id, "uncertain", "Uncertain original close"))
                .unwrap();
            app.request_cli_repair(&p.id).await.unwrap();
            assert!(
                app.advance_cli_switch(&p, None, Some(observed))
                    .await
                    .unwrap()
            );
            assert_eq!(app.db.project(&p.id).await.unwrap().terminal, p.terminal);
            let held = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
            assert_eq!(held.phase, "recovery_pending");
            assert!(held.error.contains("fresh diagnostic handoff"));
            assert_eq!(held.source["uncertain_stops"][0]["result"], "uncertain");
            assert!(held.holds_input);
        }
        for changed in [
            text.replace("fresh", "missing"),
            format!("{text}\nUnexpected handoff update"),
        ] {
            std::fs::write(dir.path().join("HANDOFF.md"), &changed).unwrap();
            app.db
                .sync(|c| phase(c, &r.id, "uncertain", "Uncertain original close"))
                .unwrap();
            app.request_cli_repair(&p.id).await.unwrap();
            assert!(app.advance_cli_switch(&p, None, Some(&good)).await.unwrap());
            let held = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
            assert_eq!(held.phase, "recovery_pending");
            assert!(held.error.contains("fresh diagnostic handoff"));
            assert_eq!(held.source["uncertain_stops"][0]["result"], "uncertain");
            assert_eq!(app.db.project(&p.id).await.unwrap().terminal, p.terminal);
        }
    }
    #[tokio::test]
    async fn runtime_repair_rejects_fork_before_source_snapshot_and_native_readiness_adoption() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c|{
            c.execute("UPDATE projects SET execution=json_object('api_protocol',7,'mcp_contract',1,'backend','legacy','workspace_id','workspace','pane_id','old-pane','terminal_id','old-pane','managed_name','managed','generation','old-gen','shell_pid',NULL,'launch_permit','/private/fixture/launch','intended_native_session','old-native') WHERE id=?",[&p.id])?;Ok(())
        }).unwrap();
        let p = app.db.project(&p.id).await.unwrap();
        app.journal_cli_fault(&p.id, "Resume recovery")
            .await
            .unwrap();
        let record = json!({"state":"live","status":{"state":"working"},"foreground":{"name":"codex"},"processes":[{"pid":100,"name":"codex","argv":["codex"]}],"native_session":{"value":"forked-native"},"interactive_ready":true,"execution":p.execution,"pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace","agent":{"agent":"codex","pane_id":"old-pane","terminal_id":"old-pane","workspace_id":"workspace"},"tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":"old-gen","hfb_execution":"managed"},"children":{"state":"quiescent"}});
        assert!(
            herdr::check_identity(&p, &record)
                .unwrap_err()
                .is::<herdr::NativeSessionChanged>()
        );
        assert!(
            app.advance_cli_switch(&p, None, Some(&record))
                .await
                .unwrap()
        );
        let r = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        assert_eq!(r.source["native_session"], "old-native");
        assert_eq!(r.source["runtime_target"]["native_session"], "old-native");
        assert!(r.holds_input);
        let current = app.db.project(&p.id).await.unwrap();
        let lane = crate::repair_lane::descriptor(&current).unwrap();
        assert_eq!(lane.expected_native, "old-native");
        assert_eq!(lane.observed_native, "forked-native");
        assert!(!current.ready);
        assert_ne!(current.execution["mcp_registered"], true);
        assert!(crate::repair_lane::check_identity(&current, &record).is_ok());
        assert!(herdr::check_identity(&current, &record).is_err());
        let mut execution = json!({"intended_native_session":"old-native","native_confirmed":true,"mcp_registered":true});
        assert!(!target_native_matches(&r, &execution, "forked-native"));
        execution["native_confirmed"] = json!(false);
        assert!(!target_native_matches(&r, &execution, "old-native"));
        execution["native_confirmed"] = json!(true);
        assert!(target_native_matches(&r, &execution, "old-native"));
    }
    #[tokio::test]
    async fn retiring_repair_lane_rejects_prompt_without_reactivating_its_phase() {
        let (_dir, app, p, _record) = crate::repair_lane::tests::fixture("codex").await;
        app.db.sync(|c|{
            let r=latest(c,&p.id)?.unwrap();
            c.execute("UPDATE cli_switch_requests SET phase='switching',nonce='accepted',generation=?,baseline='accepted-proof' WHERE id=?",params![p.generation,r.id])?;
            journal_repair_instruction(c,&p.id,"late-control",&json!({"content":"repair-only control"}))?;
            Ok(())
        }).unwrap();
        let r = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        for initial in [true, false] {
            assert!(
                app.send_cli_repair_prompt(
                    &p,
                    &r,
                    &Config::default(),
                    "<dauerbot_cli_repair />",
                    initial
                )
                .await
                .is_err()
            );
        }
        app.db
            .sync(|c| {
                let current = latest(c, &p.id)?.unwrap();
                assert_eq!(current.phase, "switching");
                assert_eq!(current.nonce, "accepted");
                assert!(current.holds_input);
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM cli_repair_prompts WHERE request_id=?",
                        [&r.id],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                assert!(
                    crate::repair_lane::authorize(c, &p, false)?.is_some(),
                    "cleanup authority remains available while effects are revoked"
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn runtime_repair_resumes_target_native_and_arguments_instead_of_erasing_its_conversation()
     {
        let (_dir, app, p) = fixture().await;
        app.journal_cli_fault(&p.id, "Recover same CLI")
            .await
            .unwrap();
        app.db
            .sync(|c| {
                let r = requested(c, &p)?;
                assert_eq!(r.source["runtime_target"]["native_session"], "old-native");
                assert!(claim(c, &p.id, &r, "accepted")?);
                complete(c, &p.id, &r)?;
                let saved: (String, String) = c.query_row(
                    "SELECT native_session,extra_args FROM projects WHERE id=?",
                    [&p.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(saved.0, "old-native");
                assert_eq!(
                    serde_json::from_str::<Value>(&saved.1)?,
                    json!(["--model", "preserve"])
                );
                assert!(holds_input(c, &p.id)?);
                Ok(())
            })
            .unwrap();
    }
    fn model(name: &str) -> crate::models::Selection {
        crate::models::Selection {
            model: name.into(),
            provider: None,
        }
    }
    #[tokio::test]
    async fn model_acceptance_keeps_source_preference_pending_and_scopes_new_effects_only() {
        let (_dir, app, p) = fixture().await;
        app.db
            .sync(|c| crate::models::save(c, &p.id, "codex", Some(&model("source-model"))))
            .unwrap();
        let scope = crate::models::Scope {
            agent: "codex".into(),
            generation: p.generation.clone(),
        };
        app.request_model_switch_with_id(
            &p.id,
            Some(model("target-model")),
            "model-action",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        let accepted = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        assert_eq!(accepted.id, "model-action");
        assert_eq!(accepted.source["selection"], json!(model("source-model")));
        assert_eq!(
            accepted.source["target_model"],
            json!(model("target-model"))
        );
        assert_eq!(
            app.db
                .sync(|c| crate::models::load(c, &p.id, "codex"))
                .unwrap(),
            Some(model("source-model"))
        );
        assert_eq!(
            app.db.project(&p.id).await.unwrap().native_session,
            p.native_session
        );
        app.db
            .sync(|c| {
                c.execute(
                    "UPDATE projects SET generation='different',agent='agy' WHERE id=?",
                    [&p.id],
                )?;
                Ok(())
            })
            .unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("target-model")),
            "model-action",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        assert!(
            app.request_model_switch_with_id(
                &p.id,
                Some(model("different-model")),
                "model-action",
                Some(scope.clone())
            )
            .await
            .is_err()
        );
        assert!(
            app.request_model_switch_with_id(
                &p.id,
                Some(model("target-model")),
                "new-stale-action",
                Some(scope)
            )
            .await
            .is_err()
        );
        assert_eq!(
            app.db
                .sync(|c| Ok(c.query_row(
                    "SELECT count(*) FROM cli_switch_requests",
                    [],
                    |row| row.get::<_, i64>(0)
                )?))
                .unwrap(),
            1
        );
    }
    #[test]
    fn model_native_resume_requires_current_confirmation_and_safe_pi_path_identity() {
        let mut execution = json!({"intended_native_session":"native-id"});
        assert_eq!(
            native_resume("codex", &execution, "native-id"),
            NativeResume::Unconfirmed
        );
        execution["native_confirmed"] = json!(true);
        assert_eq!(
            native_resume("codex", &execution, "native-id"),
            NativeResume::Matching
        );
        assert_eq!(
            native_resume("codex", &execution, "different-native-id"),
            NativeResume::Forked
        );
        assert_eq!(
            native_resume("codex", &execution, ""),
            NativeResume::Unconfirmed
        );
        assert_eq!(
            native_resume("codex", &json!({}), "new-native"),
            NativeResume::Fresh
        );
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let other = dir.path().join("other.jsonl");
        let alias = dir.path().join("alias.jsonl");
        std::fs::write(&session, "native session").unwrap();
        std::fs::write(&other, "different session").unwrap();
        std::os::unix::fs::symlink(&session, &alias).unwrap();
        execution["intended_native_session"] = json!(session);
        assert_eq!(
            native_resume("pi", &execution, alias.to_str().unwrap()),
            NativeResume::Matching
        );
        assert_eq!(
            native_resume("codex", &execution, alias.to_str().unwrap()),
            NativeResume::Forked
        );
        assert_eq!(
            native_resume("pi", &execution, other.to_str().unwrap()),
            NativeResume::Forked
        );
        assert_eq!(
            native_resume("pi", &execution, "session.jsonl"),
            NativeResume::Forked
        );
        assert_eq!(
            native_resume(
                "pi",
                &execution,
                dir.path().join("missing.jsonl").to_str().unwrap()
            ),
            NativeResume::Forked
        );
    }
    #[tokio::test]
    async fn model_launch_captures_actual_settled_native_and_fork_routes_to_repair() {
        let (_dir, app, p) = fixture().await;
        app.request_model_switch_with_id(&p.id, Some(model("target-model")), "native-fork", None)
            .await
            .unwrap();
        app.db.sync(|c| {
            c.execute("UPDATE cli_switch_requests SET phase='starting' WHERE project_id=?", [&p.id])?;
            c.execute("UPDATE projects SET native_session='latest-settled-native',generation='new-generation' WHERE id=?", [&p.id])?;
            Ok(())
        }).unwrap();
        let mut current = app.db.project(&p.id).await.unwrap();
        assert_eq!(
            app.cli_switch_launch_context(&current, true).await.unwrap(),
            Some(("native-fork".into(), Some("latest-settled-native".into())))
        );
        assert_eq!(
            app.cli_switch_launch_context(&current, false)
                .await
                .unwrap(),
            Some(("native-fork".into(), None))
        );
        current.native_session = "forked".into();
        current.execution = json!({"cli_switch_request":"native-fork", "intended_native_session":"latest-settled-native", "native_confirmed":true});
        assert!(
            app.advance_cli_switch(
                &current,
                Some(&Config::default()),
                Some(&json!({"state":"live"}))
            )
            .await
            .unwrap()
        );
        let request = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        assert_eq!(request.phase, "recovery_pending");
        assert!(request.holds_input);
        assert!(request.error.contains("different native conversation"));
    }
    #[tokio::test]
    async fn model_cutover_preserves_native_and_args_and_rejects_wrong_launch_selection() {
        let (_dir, app, p) = fixture().await;
        app.request_model_switch_with_id(&p.id, Some(model("target-model")), "model-cutover", None)
            .await
            .unwrap();
        app.db.sync(|c|{
            let r=requested(c,&p)?;
            assert!(claim(c,&p.id,&r,"accepted")?);
            complete(c,&p.id,&r)?;
            let saved:(String,String)=c.query_row("SELECT native_session,extra_args FROM projects WHERE id=?",[&p.id],|row|Ok((row.get(0)?,row.get(1)?)))?;
            assert_eq!(saved.0,"old-native");
            assert_eq!(serde_json::from_str::<Value>(&saved.1)?,json!(["--model","preserve"]));
            assert_eq!(crate::models::load(c,&p.id,"codex")?,Some(model("target-model")));
            c.execute("UPDATE projects SET generation='target-gen',terminal='target-pane',ready=1,execution=json_object('api_protocol',7,'mcp_contract',1,'cli_switch_request',?,'mcp_registered',json('true'),'model_selection',json(?)) WHERE id=?",params![r.id,serde_json::to_string(&model("wrong-model"))?,p.id])?;
            assert!(!ready(c,&p.id,"target-gen",&r)?);
            assert!(holds_input(c,&p.id)?);
            c.execute("UPDATE projects SET execution=json_set(execution,'$.model_selection',json(?),'$.intended_native_session','old-native') WHERE id=?",params![serde_json::to_string(&model("target-model"))?,p.id])?;
            // The inherited native pointer is not a new-generation confirmation.
            assert!(!ready(c,&p.id,"target-gen",&r)?);
            c.execute("UPDATE projects SET native_session='forked-native',execution=json_set(execution,'$.native_confirmed',json('true')) WHERE id=?",[&p.id])?;
            assert!(!ready(c,&p.id,"target-gen",&r)?);
            assert!(holds_input(c,&p.id)?);
            c.execute("UPDATE projects SET native_session='old-native' WHERE id=?",[&p.id])?;
            assert!(ready(c,&p.id,"target-gen",&r)?);
            assert!(!holds_input(c,&p.id)?);
            Ok(())
        }).unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("target-model")),
            "new-model-request-after-generation",
            None,
        )
        .await
        .unwrap();
        let next = app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap())).unwrap();
        assert_eq!(next.id, "new-model-request-after-generation");
        assert_eq!(next.phase, "preparing");
        assert!(next.holds_input);
    }
    #[tokio::test]
    async fn failed_model_target_restores_source_preference_and_can_retry_pending_selection() {
        let (_dir, app, p) = fixture().await;
        app.db
            .sync(|c| crate::models::save(c, &p.id, "codex", Some(&model("source-model"))))
            .unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("target-model")),
            "model-rollback",
            None,
        )
        .await
        .unwrap();
        app.db
            .sync(|c| {
                let r = requested(c, &p)?;
                assert!(claim(c, &p.id, &r, "accepted")?);
                complete(c, &p.id, &r)?;
                let tx = c.unchecked_transaction()?;
                restore_source_state(&tx, &p.id, &r.source)?;
                tx.commit()?;
                assert_eq!(
                    crate::models::load(c, &p.id, "codex")?,
                    Some(model("source-model"))
                );
                let saved: (String, String) = c.query_row(
                    "SELECT native_session,extra_args FROM projects WHERE id=?",
                    [&p.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(saved.0, "old-native");
                assert_eq!(
                    serde_json::from_str::<Value>(&saved.1)?,
                    json!(p.extra_args)
                );
                assert!(holds_input(c, &p.id)?);
                let tx = c.unchecked_transaction()?;
                let (native, _) = target_launch_state(&tx, &p.id, &r)?;
                assert_eq!(native, "old-native");
                tx.commit()?;
                assert_eq!(
                    crate::models::load(c, &p.id, "codex")?,
                    Some(model("target-model"))
                );
                assert_eq!(
                    latest(c, &p.id)?.unwrap().source["target_model"],
                    json!(model("target-model"))
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn explicit_model_reset_removes_project_model_flags_without_erasing_source_rollback_args()
    {
        let (_dir, app, p) = fixture().await;
        let args = vec![
            "--model",
            "old-flag",
            "--config",
            "model_provider=\"old-provider\"",
            "--config",
            "unrelated=true",
            "--other",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        app.db
            .sync(|c| {
                c.execute(
                    "UPDATE projects SET extra_args=? WHERE id=?",
                    params![serde_json::to_string(&args)?, p.id],
                )?;
                crate::models::save(c, &p.id, "codex", Some(&model("source-model")))
            })
            .unwrap();
        app.request_model_switch_with_id(&p.id, None, "model-default", None)
            .await
            .unwrap();
        app.db
            .sync(|c| {
                let r = requested(c, &p)?;
                assert_eq!(r.source["extra_args"], json!(args));
                assert!(claim(c, &p.id, &r, "accepted")?);
                complete(c, &p.id, &r)?;
                assert!(crate::models::load(c, &p.id, "codex")?.is_none());
                let saved: String = c.query_row(
                    "SELECT extra_args FROM projects WHERE id=?",
                    [&p.id],
                    |row| row.get(0),
                )?;
                assert_eq!(
                    serde_json::from_str::<Value>(&saved)?,
                    json!(["--config", "unrelated=true", "--other"])
                );
                assert_eq!(latest(c, &p.id)?.unwrap().source["extra_args"], json!(args));
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn corrected_model_supersedes_held_repair_without_losing_source_or_queued_controls() {
        let (_dir, app, p) = fixture().await;
        app.db
            .sync(|c| crate::models::save(c, &p.id, "codex", Some(&model("known-good-source"))))
            .unwrap();
        let scope = crate::models::Scope {
            agent: p.agent.clone(),
            generation: p.generation.clone(),
        };
        app.request_model_switch_with_id(
            &p.id,
            Some(model("unavailable")),
            "bad-model",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        let old=app.db.sync(|c|{
            phase(c,"bad-model","recovery_pending","Model not available")?;
            c.execute("UPDATE cli_switch_requests SET recovery_generation=? WHERE id='bad-model'",[&p.generation])?;
            c.execute("UPDATE projects SET execution=json_set(execution,'$.cli_recovery_request','bad-model') WHERE id=?",[&p.id])?;
            queue(c,&p,"ordinary-held",1)?;
            journal_repair_instruction(c,&p.id,"new-control",&json!({"content":"use the corrected model"}))?;
            journal_repair_instruction(c,&p.id,"uncertain-control",&json!({"content":"already submitted"}))?;
            c.execute("INSERT INTO cli_repair_prompts(id,request_id,generation,payload,status,sequence,created_at) VALUES('old-uncertain','bad-model',?,'original effect','uncertain',12,1)",[&p.generation])?;
            c.execute("UPDATE cli_repair_instructions SET prompt_id='old-uncertain' WHERE request_id='bad-model' AND instruction_id='uncertain-control'",[])?;
            Ok(latest(c,&p.id)?.unwrap())
        }).unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("available")),
            "corrected-model",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        let corrected=app.db.sync(|c|{
            let corrected=latest(c,&p.id)?.unwrap();
            assert_eq!(corrected.id,"corrected-model");
            assert_eq!(corrected.intent,"model");
            assert_eq!(corrected.requested_model,Some(model("available")));
            assert_eq!(corrected.source["selection"],old.source["selection"]);
            assert_eq!(corrected.source["native_session"],old.source["native_session"]);
            assert_eq!(corrected.source["extra_args"],old.source["extra_args"]);
            assert_eq!(corrected.source["runtime_target"]["native_session"],old.source["runtime_target"]["native_session"]);
            assert_eq!(corrected.recovery_generation,p.generation);
            assert!(holds_input(c,&p.id)?);
            assert!(work::claim_steering(c,&p.id,&p.generation,3)?.is_none());
            assert_eq!(c.query_row("SELECT request_id FROM cli_repair_instructions WHERE instruction_id='new-control'",[],|row|row.get::<_,String>(0))?,"corrected-model");
            assert_eq!(c.query_row("SELECT request_id FROM cli_repair_instructions WHERE instruction_id='uncertain-control'",[],|row|row.get::<_,String>(0))?,"bad-model");
            assert_eq!(c.query_row("SELECT phase FROM cli_switch_requests WHERE id='bad-model'",[],|row|row.get::<_,String>(0))?,"superseded");
            let old_payload:String=c.query_row("SELECT source FROM cli_switch_requests WHERE id='bad-model'",[],|row|row.get(0))?;
            assert_eq!(serde_json::from_str::<Value>(&old_payload)?["model_payload"],old.source["model_payload"]);
            let public=serde_json::to_value(&corrected)?;
            assert_eq!(public["intent"],"model");
            assert_eq!(public["requested_model"],json!(model("available")));
            assert!(public.get("source").is_none());
            Ok(corrected)
        }).unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("available")),
            "corrected-model",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("unavailable")),
            "bad-model",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        assert!(
            app.request_model_switch_with_id(
                &p.id,
                Some(model("another")),
                "corrected-model",
                Some(scope)
            )
            .await
            .is_err()
        );
        assert_eq!(
            app.db.sync(|c| Ok(latest(c, &p.id)?.unwrap().id)).unwrap(),
            corrected.id
        );
        assert_eq!(
            app.db
                .sync(|c| Ok(c.query_row(
                    "SELECT count(*) FROM cli_switch_requests",
                    [],
                    |row| row.get::<_, i64>(0)
                )?))
                .unwrap(),
            2
        );
    }
    #[tokio::test]
    async fn correcting_live_bad_candidate_preserves_retirement_identity_and_rejects_old_ack() {
        let (_dir, app, p) = fixture().await;
        app.db
            .sync(|c| crate::models::save(c, &p.id, "codex", Some(&model("source"))))
            .unwrap();
        app.request_model_switch_with_id(&p.id, Some(model("bad")), "bad-live-model", None)
            .await
            .unwrap();
        let old=app.db.sync(|c|{
            let r=requested(c,&p)?;
            assert!(claim(c,&p.id,&r,"accepted")?);
            complete(c,&p.id,&r)?;
            c.execute("UPDATE projects SET generation='bad-candidate',terminal='bad-pane',ready=1,execution=json_object('api_protocol',7,'mcp_contract',1,'cli_switch_request',?,'mcp_registered',json('true'),'model_selection',json(?)) WHERE id=?",params![r.id,serde_json::to_string(&model("bad"))?,p.id])?;
            phase(c,&r.id,"recovery_pending","Invalid target model")?;
            Ok(latest(c,&p.id)?.unwrap())
        }).unwrap();
        let scope = crate::models::Scope {
            agent: "codex".into(),
            generation: "bad-candidate".into(),
        };
        app.request_model_switch_with_id(
            &p.id,
            Some(model("fixed")),
            "fixed-live-model",
            Some(scope),
        )
        .await
        .unwrap();
        let current = app.db.project(&p.id).await.unwrap();
        app.db.sync(|c|{
            let corrected=latest(c,&p.id)?.unwrap();
            assert!(retiring_candidate(&corrected,&current.execution,&current.generation));
            assert_eq!(corrected.source["retiring_candidates"][0],json!({"generation":"bad-candidate","request_id":"bad-live-model"}));
            assert_eq!(corrected.source["selection"],json!(model("source")));
            assert_eq!(crate::models::load(c,&p.id,"codex")?,Some(model("bad")),"correction cannot rewrite a live candidate's model preference before its verified close");
            assert!(!claim(c,&p.id,&old,"stale handoff")?);
            assert!(!ready(c,&p.id,"bad-candidate",&old)?);
            assert!(!ready(c,&p.id,"bad-candidate",&corrected)?);
            assert!(holds_input(c,&p.id)?);
            assert!(complete(c,&p.id,&old).is_err());
            assert_eq!(latest(c,&p.id)?.unwrap().id,"fixed-live-model");
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn post_ready_model_runtime_failure_is_correctable_but_other_cli_holds_are_not() {
        let (_dir, app, p) = fixture().await;
        app.request_model_switch_with_id(
            &p.id,
            Some(model("bad-later")),
            "model-ready-first",
            None,
        )
        .await
        .unwrap();
        app.db.sync(|c|{
            let r=requested(c,&p)?;
            assert!(claim(c,&p.id,&r,"accepted")?);
            complete(c,&p.id,&r)?;
            c.execute("UPDATE projects SET generation='later-error',terminal='target-pane',ready=1,execution=json_object('api_protocol',7,'mcp_contract',1,'cli_switch_request',?,'mcp_registered',json('true'),'model_selection',json(?)) WHERE id=?",params![r.id,serde_json::to_string(&model("bad-later"))?,p.id])?;
            assert!(ready(c,&p.id,"later-error",&r)?);
            let recovery=ensure_repair_request(c,&p.id)?;
            assert_eq!(recovery.intent,"model");
            assert_eq!(recovery.requested_model,Some(model("bad-later")));
            assert!(model_repair_can_be_corrected(&recovery));
            Ok(())
        }).unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("working")),
            "model-correct-later",
            None,
        )
        .await
        .unwrap();
        let current = app.db.project(&p.id).await.unwrap();
        app.db
            .sync(|c| {
                let corrected = latest(c, &p.id)?.unwrap();
                assert!(retiring_candidate(
                    &corrected,
                    &current.execution,
                    "later-error"
                ));
                Ok(())
            })
            .unwrap();
        let (_dir2, other, other_p) = fixture().await;
        other.request_cli_switch(&other_p.id, "agy").await.unwrap();
        other
            .db
            .sync(|c| {
                phase(
                    c,
                    &latest(c, &other_p.id)?.unwrap().id,
                    "recovery_pending",
                    "CLI startup failed",
                )
            })
            .unwrap();
        assert!(
            other
                .request_model_switch_with_id(
                    &other_p.id,
                    Some(model("working")),
                    "forbidden-cli-model-correction",
                    None
                )
                .await
                .is_err()
        );
        assert!(other.db.sync(|c| holds_input(c, &other_p.id)).unwrap());
    }
    #[tokio::test]
    async fn offline_saved_native_selection_applies_corrected_model_before_any_old_model_bootstrap()
    {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c|{
            crate::models::save(c,&p.id,"codex",Some(&model("invalid-old-model")))?;
            work::finish_execution(c,&p.id,&p.generation,"stopped","Verified stopped fixture",1)?;
            c.execute("UPDATE projects SET terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null' WHERE id=?",[&p.id])?;
            Ok(())
        }).unwrap();
        let scope = crate::models::Scope {
            agent: "codex".into(),
            generation: String::new(),
        };
        app.request_model_switch_with_id(
            &p.id,
            Some(model("working")),
            "offline-correct-model",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        let current = app.db.project(&p.id).await.unwrap();
        assert!(current.terminal.is_none());
        assert_eq!(current.native_session, "old-native");
        app.db
            .sync(|c| {
                let r = latest(c, &p.id)?.unwrap();
                assert_eq!(r.phase, "starting");
                assert!(r.holds_input && r.nonce.is_empty() && r.attempts == 0);
                assert_eq!(r.source["selection"], json!(model("invalid-old-model")));
                assert_eq!(r.source["extra_args"], json!(p.extra_args));
                assert_eq!(r.source["direct_model_resume"], true);
                assert_eq!(
                    crate::models::load(c, &p.id, "codex")?,
                    Some(model("working"))
                );
                queue(c, &p, "offline-held", 2)?;
                assert!(work::claim_steering(c, &p.id, "old-gen", 3)?.is_none());
                let args = crate::adapter::launch_args_with_selection(
                    &current,
                    &std::path::PathBuf::from("mcp.json"),
                    &json!({"command":"hfb","args":["mcp"]}),
                    "native continuation",
                    true,
                    crate::models::load(c, &p.id, "codex")?.as_ref(),
                )?;
                assert!(args.windows(2).any(|pair| pair == ["--model", "working"]));
                assert!(args.windows(2).any(|pair| pair == ["resume", "old-native"]));
                assert!(!args.iter().any(|arg| arg == "invalid-old-model"));
                Ok(())
            })
            .unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("working")),
            "offline-correct-model",
            Some(scope),
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn offline_correction_after_candidate_close_never_adopts_its_forked_native() {
        let (_dir, app, p) = fixture().await;
        app.request_model_switch_with_id(&p.id, Some(model("bad")), "forked-model", None)
            .await
            .unwrap();
        app.db.sync(|c| {
            // The genuine source can report a newer native identity while
            // settling; complete must preserve that, not the initial snapshot.
            c.execute("UPDATE projects SET native_session='settled-source' WHERE id=?",[&p.id])?;
            let r=requested(c,&p)?;
            assert!(claim(c,&p.id,&r,"accepted")?);
            complete(c,&p.id,&r)?;
            work::begin_execution(c,&p.id,"forked-candidate",1)?;
            work::finish_execution(c,&p.id,"forked-candidate","stopped","Verified candidate close",2)?;
            c.execute("UPDATE projects SET native_session='wrong-candidate-fork',terminal=NULL,generation='',execution='null' WHERE id=?",[&p.id])?;
            phase(c,&r.id,"recovery_pending","Candidate fork closed; source not yet restored")?;
            Ok(())
        }).unwrap();
        let scope = crate::models::Scope {
            agent: "codex".into(),
            generation: String::new(),
        };
        app.request_model_switch_with_id(
            &p.id,
            Some(model("fixed")),
            "correct-after-fork-close",
            Some(scope.clone()),
        )
        .await
        .unwrap();
        let corrected = app.db.project(&p.id).await.unwrap();
        assert_eq!(corrected.native_session, "settled-source");
        assert_eq!(corrected.extra_args, p.extra_args);
        app.db
            .sync(|c| {
                let r = latest(c, &p.id)?.unwrap();
                assert_eq!(r.phase, "starting");
                assert!(r.holds_input);
                assert_eq!(r.source["native_session"], "settled-source");
                assert_eq!(
                    crate::models::load(c, &p.id, "codex")?,
                    Some(model("fixed"))
                );
                assert_eq!(
                    c.query_row(
                        "SELECT phase FROM cli_switch_requests WHERE id='forked-model'",
                        [],
                        |row| row.get::<_, String>(0)
                    )?,
                    "superseded"
                );
                Ok(())
            })
            .unwrap();
        app.request_model_switch_with_id(
            &p.id,
            Some(model("fixed")),
            "correct-after-fork-close",
            Some(scope),
        )
        .await
        .unwrap();
        assert_eq!(
            app.db.project(&p.id).await.unwrap().native_session,
            "settled-source"
        );
    }
    #[tokio::test]
    async fn offline_model_selection_never_assumes_an_unclosed_execution_is_gone() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c|{
            c.execute("UPDATE projects SET terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null' WHERE id=?",[&p.id])?;
            Ok(())
        }).unwrap();
        assert!(
            app.request_model_switch_with_id(
                &p.id,
                Some(model("working")),
                "unverified-offline-model",
                None
            )
            .await
            .is_err()
        );
        assert!(app.db.sync(|c| Ok(latest(c, &p.id)?.is_none())).unwrap());
    }
}
