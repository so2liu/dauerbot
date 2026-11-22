//! Restricted management actor. Native CLI authority stays in the worker.
pub(crate) mod llm;
mod agent;
mod event;
pub(crate) use event::redact_envelope;
mod tools;
mod channel;
mod memory;
mod session;
use crate::{
    App, db,
    db::Project,
    work,
};
use self::{llm::{Failure, ProviderConfig}, tools::ToolCall};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

mod autonomy;

const SYSTEM: &str = include_str!("manager_prompt.md");

pub(crate) fn owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(db::id)
}

pub(crate) fn installed(c: &Connection) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='manager_config')",
        [],
        |r| r.get(0),
    )?)
}
pub(crate) fn uses_manager(c: &Connection, project: &str) -> Result<bool> {
    if !installed(c)? {
        return Ok(false);
    }
    Ok(!c.query_row("SELECT EXISTS(SELECT 1 FROM standalone_projects WHERE project_id=?)",
        [project], |row|row.get::<_,bool>(0))?)
}
pub fn migrate(c: &Connection, recover: bool) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS standalone_projects(
        project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE
    )")?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS manager_config(project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,mode TEXT NOT NULL DEFAULT 'manager',base_url TEXT NOT NULL DEFAULT '',model TEXT NOT NULL DEFAULT '',api_key TEXT NOT NULL DEFAULT '',revision INTEGER NOT NULL DEFAULT 0,input_revision INTEGER NOT NULL DEFAULT 0,error TEXT NOT NULL DEFAULT '',next_attempt_at INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS manager_mode_requests(project_id TEXT NOT NULL,request_id TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(project_id,request_id)); CREATE TABLE IF NOT EXISTS manager_inputs(id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,origin_event_id TEXT,kind TEXT NOT NULL,payload TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'queued',created_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS manager_runs(id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,revision INTEGER NOT NULL,input_revision INTEGER NOT NULL,origin_event_id TEXT,checkpoint TEXT NOT NULL,status TEXT NOT NULL,owner TEXT NOT NULL,error TEXT NOT NULL DEFAULT '',created_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS manager_effects(id TEXT PRIMARY KEY,run_id TEXT NOT NULL REFERENCES manager_runs(id) ON DELETE CASCADE,occurrence TEXT NOT NULL,name TEXT NOT NULL,arguments TEXT NOT NULL,status TEXT NOT NULL,result TEXT NOT NULL DEFAULT 'null',UNIQUE(run_id,occurrence)); CREATE TABLE IF NOT EXISTS manager_run_inputs(run_id TEXT NOT NULL REFERENCES manager_runs(id) ON DELETE CASCADE,input_id TEXT NOT NULL REFERENCES manager_inputs(id),PRIMARY KEY(run_id,input_id)); CREATE TABLE IF NOT EXISTS manager_turn_notes(run_id TEXT PRIMARY KEY REFERENCES manager_runs(id) ON DELETE CASCADE,note TEXT NOT NULL); CREATE TABLE IF NOT EXISTS manager_observations(project_id TEXT NOT NULL,source TEXT NOT NULL,source_id TEXT NOT NULL,stamp TEXT NOT NULL,PRIMARY KEY(project_id,source,source_id));")?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS manager_failures(project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,revision INTEGER NOT NULL,incident_id TEXT NOT NULL,attempts INTEGER NOT NULL,diagnostic TEXT NOT NULL,notified INTEGER NOT NULL DEFAULT 0,repair_probe INTEGER NOT NULL DEFAULT 0);")?;
    if !c
        .prepare("PRAGMA table_info(manager_config)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "supports_images")
    {
        c.execute(
            "ALTER TABLE manager_config ADD COLUMN supports_images INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    c.execute_batch("CREATE TABLE IF NOT EXISTS manager_provider_config(
        id INTEGER PRIMARY KEY CHECK(id=1), base_url TEXT NOT NULL DEFAULT '',
        model TEXT NOT NULL DEFAULT '', api_key TEXT NOT NULL DEFAULT '',
        supports_images INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL DEFAULT 0, migration_conflict INTEGER NOT NULL DEFAULT 0
    );")?;
    for (column, definition) in [
        ("provider_kind", "TEXT NOT NULL DEFAULT 'openai_completions'"),
        ("context_window", "INTEGER"),
    ] {
        let exists: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('manager_provider_config') WHERE name=?)",
            [column], |row| row.get(0),
        )?;
        if !exists {
            c.execute(&format!("ALTER TABLE manager_provider_config ADD COLUMN {column} {definition}"), [])?;
        }
    }
    // A blank singleton also marks a completed migration, including after a key is cleared.
    let migrated = c.execute("INSERT OR IGNORE INTO manager_provider_config(id,base_url,model,api_key,supports_images,migration_conflict)
        SELECT 1,old.base_url,old.model,old.api_key,old.supports_images,
            EXISTS(SELECT 1 FROM manager_config other WHERE
                trim(other.base_url)!='' AND trim(other.model)!='' AND trim(other.api_key)!=''
                AND (other.base_url,other.model,other.api_key,other.supports_images) !=
                    (old.base_url,old.model,old.api_key,old.supports_images))
        FROM manager_config old WHERE trim(old.base_url)!='' AND trim(old.model)!='' AND trim(old.api_key)!=''
        ORDER BY old.revision DESC,old.project_id LIMIT 1", [])?;
    let empty = c.execute("INSERT OR IGNORE INTO manager_provider_config(id) VALUES(1)", [])?;
    if migrated == 1 || empty == 1 {
        // An existing checkpoint belongs to its original provider identity.
        c.execute("UPDATE manager_config AS old SET revision=revision+1,input_revision=input_revision+1,error='',next_attempt_at=0
            WHERE old.mode='manager' AND EXISTS(SELECT 1 FROM manager_provider_config current WHERE
                (old.base_url,old.model,old.api_key,old.supports_images) !=
                (current.base_url,current.model,current.api_key,current.supports_images))", [])?;
        if empty == 1 {
            c.execute("UPDATE manager_config SET error='',next_attempt_at=0", [])?;
        }
        c.execute("UPDATE manager_runs SET status='superseded' WHERE status='working' AND
            revision!=(SELECT revision FROM manager_config WHERE project_id=manager_runs.project_id)", [])?;
    }
    c.execute("INSERT OR IGNORE INTO manager_config(project_id,mode) SELECT id,'manager' FROM projects", [])?;
    c.execute("UPDATE manager_config SET mode='manager',base_url='',model='',api_key='',supports_images=0
        WHERE mode!='manager' OR base_url!='' OR model!='' OR api_key!='' OR supports_images!=0", [])?;
    // Turn notes share the owning project's lifetime, just like their runs.
    let notes_cascade: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list('manager_turn_notes') WHERE [table]='manager_runs' AND [from]='run_id' AND on_delete='CASCADE')", [], |r|r.get(0))?;
    if !notes_cascade {
        c.execute_batch("CREATE TABLE manager_turn_notes_cascade(run_id TEXT PRIMARY KEY REFERENCES manager_runs(id) ON DELETE CASCADE,note TEXT NOT NULL);
            INSERT INTO manager_turn_notes_cascade SELECT * FROM manager_turn_notes;
            DROP TABLE manager_turn_notes;
            ALTER TABLE manager_turn_notes_cascade RENAME TO manager_turn_notes;")?;
    }
    crate::manager_close::migrate(c, recover)?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS manager_input_claims(input_id TEXT PRIMARY KEY REFERENCES manager_inputs(id) ON DELETE CASCADE,run_id TEXT NOT NULL REFERENCES manager_runs(id) ON DELETE CASCADE); CREATE INDEX IF NOT EXISTS manager_claim_run ON manager_input_claims(run_id);")?;
    session::migrate(c)?;
    session::migrate_inputs(c)?;
    let transcript_recorded = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('manager_effects') WHERE name='transcript_recorded')",
        [], |row| row.get::<_, bool>(0),
    )?;
    if !transcript_recorded {
        c.execute("ALTER TABLE manager_effects ADD COLUMN transcript_recorded INTEGER NOT NULL DEFAULT 0", [])?;
    }
    if recover {
        c.execute("UPDATE manager_effects SET status='uncertain',result=? WHERE status='submitting'",
            [json!({"status":"uncertain","message":"The host stopped during this effect; inspect evidence before retrying."}).to_string()])?;
    }
    c.execute(
        "INSERT OR IGNORE INTO manager_detached_effects(effect_id,project_id,channel_id)
         SELECT DISTINCT e.id,r.project_id,COALESCE(json_extract(r.checkpoint,'$.channel_id'),i.channel_id,'local:default')
         FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id
         LEFT JOIN manager_inputs i ON i.origin_event_id=r.origin_event_id AND i.project_id=r.project_id
         WHERE e.transcript_recorded=0 AND e.status IN ('complete','uncertain') AND r.status='working'
         AND json_valid(r.checkpoint) AND (? OR json_type(r.checkpoint,'$.channel_id') IS NULL)",
        [recover],
    )?;
    let effects = c.prepare("SELECT effect_id FROM manager_detached_effects")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for effect in effects {
        tools::wake_detached(c, &effect)?;
    }
    c.execute(
        "UPDATE manager_runs SET status='superseded' WHERE status='working'
         AND (? OR NOT json_valid(checkpoint) OR json_type(checkpoint,'$.channel_id') IS NULL)",
        [recover],
    )?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigInput {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub clear_api_key: bool,
    pub expected_revision: i64,
    #[serde(default)]
    pub supports_images: Option<bool>,
    pub provider_kind: Option<llm::ProviderKind>,
    #[serde(default, deserialize_with = "explicit_context_window")]
    pub context_window: Option<Option<u32>>,
}
fn explicit_context_window<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Option<u32>>, D::Error> {
    Option::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeInput {
    pub mode: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub expected_revision: i64,
}
const PROVIDER_REQUIRED: &str = "Global manager credentials are not configured. Open Settings to configure the manager endpoint, model, and API key.";
const PROVIDER_CONFIGURED: &str = "g.migration_conflict=0 AND trim(g.base_url)!='' AND trim(g.model)!='' AND trim(g.api_key)!=''";

pub fn public_provider(c: &Connection) -> Result<Value> {
    Ok(c.query_row(
        &format!("SELECT base_url,model,trim(api_key)!='',supports_images,revision,migration_conflict,
            ({PROVIDER_CONFIGURED}),provider_kind,context_window FROM manager_provider_config g WHERE g.id=1"),
        [],
        |row| Ok(json!({
            "base_url":row.get::<_,String>(0)?,"model":row.get::<_,String>(1)?,
            "has_api_key":row.get::<_,bool>(2)?,"supports_images":row.get::<_,bool>(3)?,
            "revision":row.get::<_,i64>(4)?,"migration_conflict":row.get::<_,bool>(5)?,
            "configured":row.get::<_,bool>(6)?,"provider_kind":row.get::<_,String>(7)?,
            "context_window":row.get::<_,Option<u32>>(8)?
        })),
    )?)
}
pub fn public(c: &Connection, project: &str) -> Result<Value> {
    let mut public = public_provider(c)?;
    if !uses_manager(c, project)? {
        return Ok(json!({"mode":"native","status":"native","configured":false}));
    }
    public["mode"] = json!("manager");
    let (paused, revision, error, active): (bool, i64, String, bool) = c.query_row(
        "SELECT p.paused, m.revision, m.error,
          EXISTS(SELECT 1 FROM manager_runs WHERE project_id=p.id AND status='working')
         FROM projects p JOIN manager_config m ON m.project_id=p.id WHERE p.id=?",
        [project], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let failure: Option<String> = c.query_row(
        "SELECT diagnostic FROM manager_failures WHERE project_id=? AND revision=?",
        params![project, revision], |row| row.get(0),
    ).optional()?;
    let error = if public["configured"] == true { error } else { PROVIDER_REQUIRED.into() };
    public["status"] = json!(if paused { "paused" } else if !error.is_empty() {
        "unavailable"
    } else if active { "working" } else { "ready" });
    public["error"] = json!(error);
    public["failure"] = failure.map(|raw|serde_json::from_str::<Value>(&raw)).transpose()?.unwrap_or(Value::Null);
    Ok(public)
}
pub fn save_provider(c: &Connection, input: ConfigInput) -> Result<Value> {
    let tx = c.unchecked_transaction()?;
    let current = public_provider(&tx)?;
    ensure!(
        current["revision"] == input.expected_revision,
        "Manager settings changed; refresh before saving"
    );
    ensure!(
        !(input.clear_api_key && input.api_key.is_some()),
        "Choose either a new key or clear key"
    );
    let previous: String = tx.query_row(
        "SELECT api_key FROM manager_provider_config WHERE id=1", [], |row| row.get(0),
    )?;
    let key = if input.clear_api_key {
        String::new()
    } else {
        input.api_key.unwrap_or(previous)
    };
    let config = ProviderConfig {
        base_url: input.base_url.clone(),
        model: input.model.clone(),
        supports_images: false,
        provider_kind: input.provider_kind.unwrap_or(serde_json::from_value(current["provider_kind"].clone())?),
        context_window: input.context_window.unwrap_or(serde_json::from_value(current["context_window"].clone())?),
        api_key: if key.is_empty() { "configuration-placeholder".into() } else { key.clone() },
    };
    config.validate()?;
    let supports_images = input.supports_images.unwrap_or_else(|| {
        current["base_url"] == input.base_url
            && current["model"] == input.model
            && current["provider_kind"] == config.provider_kind.as_str()
            && current["supports_images"] == true
    });
    tx.execute("UPDATE manager_provider_config SET base_url=?,model=?,api_key=?,supports_images=?,provider_kind=?,context_window=?,
        revision=revision+1,updated_at=?,migration_conflict=0 WHERE id=1",
        params![input.base_url,input.model,key,supports_images,config.provider_kind.as_str(),config.context_window,db::now()])?;
    tx.execute("UPDATE manager_config SET revision=revision+1,input_revision=input_revision+1,error='',next_attempt_at=0", [])?;
    tx.execute("UPDATE manager_runs SET status='superseded' WHERE status='working'", [])?;
    tx.execute("UPDATE manager_channels SET retry_at=0,last_input_tokens=NULL,last_estimate=NULL", [])?;
    let result = public_provider(&tx)?;
    tx.commit()?;
    Ok(result)
}
pub fn set_mode(c: &Connection, project: &str, input: ModeInput) -> Result<Value> {
    if input.mode == "automatic" {
        let tx = c.unchecked_transaction()?;
        let p = db::project_by_key(&tx, project)?;
        ensure!(public_provider(&tx)?["configured"] == true, "Configure the shared Manager first");
        ensure!(p.terminal.is_none() && !crate::cli_switch::holds_input(&tx,project)?,
            "Stop or finish the old execution before migrating this project");
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM work_events
            WHERE project_id=? AND (status IN ('submitting','submitted','uncertain')
            OR (source='management' AND status='queued')))",
            [project], |row|row.get(0))?;
        ensure!(!pending, "Resolve outstanding old execution effects before migrating");
        tx.execute("DELETE FROM standalone_projects WHERE project_id=?", [project])?;
        tx.execute("UPDATE projects SET agent='auto' WHERE id=?", [project])?;
        let queued = tx.prepare("SELECT id FROM work_events WHERE project_id=? AND status='queued'")?
            .query_map([project], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for event in queued {
            route_event(&tx, &event)?;
        }
        let result=public(&tx,project)?;
        tx.commit()?;
        return Ok(result);
    }
    if input.mode == "native" {
        bail!("Single-agent mode has been removed; all projects use manager/worker mode.");
    }
    ensure!(input.mode == "manager", "Invalid Agent mode");
    public(c, project)
}
pub fn migrate_queued(c: &Connection) -> Result<()> {
    let events = c.prepare("SELECT id FROM work_events WHERE status='queued' AND source NOT IN ('management','manager_worker') ORDER BY rowid")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for event in events {
        route_event(c, &event)?;
    }
    Ok(())
}
pub fn route_event(c: &Connection, event: &str) -> Result<()> {
    if !installed(c)? {
        return Ok(());
    }
    let e: Option<(String, String, String)> = c
        .query_row(
            "SELECT project_id,source,payload FROM work_events WHERE id=? AND status='queued'",
            [event],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((project, source, raw)) = e else {
        return Ok(());
    };
    if matches!(source.as_str(),"management"|"manager_worker") || !uses_manager(c,&project)? {
        return Ok(());
    }
    let payload: Value = serde_json::from_str(&raw)?;
    let native_event = work::event(c, &project, event)?;
    let trusted_source = if source == "feishu" || source == "local" {
        json!({"message_id":if source=="feishu"{Some(&native_event.source_id)}else{None},"local_source_id":if source=="local"{Some(&native_event.source_id)}else{None},"source":payload["attachment"]["source"],"bot_app_id":payload["attachment"]["bot_app_id"]})
    } else {
        json!({"timer_id":payload["timer_id"],"to":payload["to"],"scheduled_at":payload["scheduled_at"],"purpose":payload["purpose"]})
    };
    let trusted_source = if source == "autonomy" {
        json!({"proposal_id":payload["proposal_id"],"to":payload["to"],"message_id":payload["anchor"],"app_id":payload["app_id"]})
    } else {
        trusted_source
    };
    let user_input = if source == "timer" || source == "autonomy" {
        json!({"body":payload["instructions"]})
    } else {
        json!({"body":payload["content"],"resources":payload["attachment"]["resources"]})
    };
    input(
        c,
        &project,
        event,
        Some(event),
        match source.as_str() {
            "timer" => "schedule",
            "autonomy" => "proposal",
            _ => "user",
        },
        &json!({"event_id":event,"source":source,"trusted_source":trusted_source,"user_input":user_input}),
    )?;
    if source == "timer" {
        c.execute(
            "UPDATE work_events SET payload=json_set(payload,'$.origin_channel_id',
             (SELECT channel_id FROM manager_inputs WHERE id=?)) WHERE id=?",
            params![event,event],
        )?;
    }
    c.execute(
        "UPDATE work_events SET status='managed' WHERE id=? AND status='queued'",
        [event],
    )?;
    c.execute("UPDATE messages SET status='acknowledged' WHERE id=(SELECT message_id FROM work_events WHERE id=?) AND status='queued'",[event])?;
    Ok(())
}
fn input(
    c: &Connection,
    project: &str,
    id: &str,
    origin: Option<&str>,
    kind: &str,
    payload: &Value,
) -> Result<()> {
    c.execute("INSERT OR IGNORE INTO manager_config(project_id,mode) VALUES(?,'manager')", [project])?;
    let event = origin.map(|origin| {
        c.query_row(
            "SELECT source,payload FROM work_events WHERE id=? AND project_id=?",
            params![origin, project],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).optional()
    }).transpose()?.flatten();
    let channel_id = match event {
        Some((source, raw)) => channel::for_event(c, project, &source, &serde_json::from_str(&raw)?)?,
        None => None,
    };
    let channel_id = if let Some(event) = payload["event_id"].as_str()
        && let Some(channel) = c.query_row(
            "SELECT json_extract(payload,'$.origin_channel_id') FROM work_events WHERE project_id=? AND id=?",
            params![project,event], |row| row.get::<_,Option<String>>(0),
        ).optional()?.flatten()
    { channel } else { channel_id.unwrap_or_else(|| channel::LOCAL.into()) };
    let signal = if kind == "user" { channel::Signal::Steer } else { channel::Signal::Wake };
    if c.execute(
        "INSERT OR IGNORE INTO manager_inputs(id,project_id,origin_event_id,kind,payload,created_at,channel_id,signal)
         VALUES(?,?,?,?,?,?,?,?)",
        params![id,project,origin,kind,payload.to_string(),db::now(),channel_id,signal.as_str()],
    )? == 1 && kind == "user" {
        c.execute("UPDATE manager_channels SET retry_at=0 WHERE project_id=? AND channel_id=?",
            params![project,channel_id])?;
        c.execute("UPDATE manager_config SET next_attempt_at=0 WHERE project_id=?
            AND NOT (next_attempt_at=9223372036854775807 AND EXISTS(
                SELECT 1 FROM manager_failures WHERE project_id=manager_config.project_id
                AND json_extract(diagnostic,'$.kind') IN ('authorization','model_unavailable')
            ))", [project])?;
    }
    Ok(())
}
#[derive(Clone)]
pub(crate) struct Context {
    pub(crate) project: String,
    pub(crate) run: String,
    pub(crate) revision: i64,
    pub(crate) input_revision: i64,
}
pub(crate) fn valid(c: &Connection, ctx: &Context) -> Result<Project> {
    let p = crate::db::project_by_key(c, &ctx.project)?;
    ensure!(uses_manager(c, &p.id)?, "Project no longer uses Manager");
    let matches:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM manager_config WHERE project_id=? AND revision=? AND input_revision=?)",params![ctx.project,ctx.revision,ctx.input_revision],|r|r.get(0))?;
    ensure!(
        matches && !p.paused,
        "Management input or policy changed; effect cancelled"
    );
    ensure!(
        !work::has_project_takeover(c, &p.id)?,
        "Native input is under explicit user control"
    );
    let run_channel = c.query_row(
        "SELECT CASE WHEN json_valid(checkpoint) THEN json_extract(checkpoint,'$.channel_id') END
         FROM manager_runs WHERE id=? AND project_id=? AND owner=? AND status='working'",
        params![ctx.run,ctx.project,owner()], |row| row.get::<_,Option<String>>(0),
    ).optional()?.context("Management turn ownership changed")?;
    if let Some(channel_id) = run_channel.as_deref() {
        ensure!(channel::bound(c, &ctx.project, channel_id)?, "Manager channel belongs to a previous bot");
    }
    Ok(p)
}
fn intent(
    c: &Connection,
    p: &Project,
    id: &str,
    instructions: &str,
    origin: Option<&str>,
    steer: bool,
) -> Result<String> {
    intent_for_worker(c,p,id,instructions,origin,steer,None)
}
fn intent_for_worker(
    c: &Connection,
    p: &Project,
    id: &str,
    instructions: &str,
    origin: Option<&str>,
    steer: bool,
    worker: Option<(&str, Option<&Project>)>,
) -> Result<String> {
    ensure!(
        !instructions.trim().is_empty() && instructions.len() <= 1048576,
        "Worker instructions must contain 1 to 1048576 bytes"
    );
    if let Some(previous) = c
        .query_row(
            "SELECT id FROM work_events WHERE project_id=? AND source IN ('management','manager_worker') AND source_id=?",
            params![p.id, id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(previous);
    }
    let task = db::id();
    let event = db::id();
    let transition = if steer || worker.is_some() {
        None
    } else {
        crate::cli_switch::latest(c, &p.id)?.filter(crate::cli_switch::blocks_worker)
    };
    let mut payload = json!({"instructions":instructions,"origin_event_id":origin,"manager_effect_id":id,"steer":steer,"expected_agent":transition.as_ref().map(|r|r.target.as_str()).unwrap_or(&p.agent),"expected_native":if transition.is_some(){""}else{&p.native_session},"target_generation":if transition.is_none()&&p.terminal.is_some(){Some(&p.generation)}else{None},"transition_id":transition.as_ref().map(|r|&r.id)});
    let source = if let Some((agent, worker)) = worker {
        payload["expected_agent"] = json!(agent);
        payload["expected_native"] = json!(worker.map(|p| &p.native_session));
        payload["target_generation"] = json!(worker.map(|p| &p.generation));
        "manager_worker"
    } else {
        "management"
    };
    if let Some(origin) = origin {
        let proposal: Option<String> = c.query_row(
            "SELECT json_extract(payload,'$.proposal_id') FROM work_events
             WHERE id=? AND project_id=? AND source='autonomy'",
            params![origin, p.id],
            |r| r.get(0),
        ).optional()?.flatten();
        if let Some(proposal) = proposal {
            payload["proposal_id"] = json!(proposal);
        }
    }
    c.execute("INSERT INTO work_tasks(id,project_id,title,instructions,status,created_at,updated_at) VALUES(?,?,?,?,'queued',?,?)",params![task,p.id,"Management delegation",instructions,db::now(),db::now()])?;
    // A pre-seat daemon ignores worker_waiting. On rollback it must never
    // route the native work back through Manager or the primary conversation.
    let status = if worker.is_some() { "worker_waiting" } else { "queued" };
    c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?)",params![event,p.id,task,source,id,payload.to_string(),status,db::now(),db::now()])?;
    Ok(event)
}
#[cfg(test)]
pub(crate) fn test_delegate(c: &Connection, project: &str, key: &str, origin: Option<&str>) -> Result<String> {
    let p = db::project_by_key(c, project)?;
    intent(c, &p, key, "Fixture delegation", origin, false)
}
fn approved_origin(c: &Connection, project: &str, origin: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM work_events e
            JOIN proposals p ON p.execution_event_id=e.id AND p.project_id=e.project_id
            JOIN autonomy_settings a ON a.project_id=e.project_id
            JOIN project_bots b ON b.project_id=e.project_id
            WHERE e.project_id=? AND e.id=? AND e.source='autonomy' AND p.status='approved'
              AND p.id=json_extract(e.payload,'$.proposal_id')
              AND p.source_message_id=json_extract(e.payload,'$.anchor')
              AND a.enabled=1 AND a.app_id=b.app_id
              AND a.app_id=json_extract(e.payload,'$.app_id')
              AND a.chat_id=json_extract(e.payload,'$.to')
              AND (p.decided_by='local-admin' OR p.decided_by=a.owner_open_id)
        )",
        params![project,origin],|r|r.get(0),
    )?)
}

/// Called inside the ordinary native claim transaction, after readiness checks.
pub fn bind_event(c: &Connection, event: &work::Event, generation: &str) -> Result<bool> {
    if event.source != "management" {
        return Ok(!uses_manager(c, &event.project_id)?);
    }
    let p = db::project_by_key(c, &event.project_id)?;
    if event.payload["proposal_id"].is_string()
        && !event.payload["origin_event_id"].as_str()
            .map(|id| approved_origin(c, &event.project_id, id))
            .transpose()?.unwrap_or(false)
    {
        c.execute("UPDATE work_events SET status='failed',error='Approved proposal authority changed before submission',updated_at=? WHERE id=? AND status='queued'",
            params![db::now(),event.id])?;
        worker_changed(c,&event.project_id)?;
        return Ok(false);
    }
    if let Some(incident) = event.payload["repair_incident"].as_str() {
        let current:bool=c.query_row(&format!("SELECT EXISTS(SELECT 1 FROM manager_failures f JOIN manager_config m ON m.project_id=f.project_id
            CROSS JOIN manager_provider_config g WHERE f.project_id=? AND f.incident_id=? AND f.revision=m.revision AND m.revision=?
            AND ({PROVIDER_CONFIGURED}))"),params![p.id,incident,event.payload["repair_revision"].as_i64()],|r|r.get(0))?;
        if !current {
            cancel_repair(
                c,
                &event.id,
                "Management recovered or its repair authority changed",
            )?;
            return Ok(false);
        }
    }
    let same_agent = event.payload["expected_agent"] == p.agent;
    let same_generation = event.payload["target_generation"]
        .as_str()
        .is_none_or(|g| g == generation);
    let same_native = event.payload["expected_native"]
        .as_str()
        .filter(|s| !s.is_empty())
        .is_some_and(|s| s == p.native_session);
    let transition_valid = if let Some(id) = event.payload["transition_id"].as_str() {
        crate::cli_switch::latest(c, &p.id)?.is_some_and(|r| {
            r.id == id && r.phase == "ready" && p.execution["cli_switch_request"] == id
        })
    } else {
        true
    };
    let may_resume_same_native = event.payload["steer"] != true && same_native;
    if !transition_valid || !same_agent || (!same_generation && !may_resume_same_native) {
        c.execute("UPDATE work_events SET status='failed',error='Worker identity changed before submission; management can replan from this preserved intent',updated_at=? WHERE id=? AND status='queued'",params![db::now(),event.id])?;
        worker_changed(c, &event.project_id)?;
        return Ok(false);
    }
    c.execute("UPDATE work_events SET payload=json_set(payload,'$.target_generation',?) WHERE id=? AND status='queued'",params![generation,event.id])?;
    Ok(true)
}
pub async fn body(app: &App, p: &Project, event: &work::Event) -> Result<String> {
    ensure!(matches!(event.source.as_str(),"management"|"manager_worker"), "Only manager delegations may reach the coding worker");
    ensure!(
        event.payload["expected_agent"] == p.agent
            && event.payload["target_generation"] == p.generation,
        "Management delegation belongs to another native execution"
    );
    let origin = event.payload["origin_event_id"].as_str().map(str::to_owned);
    let mut context = String::new();
    if let Some(origin) = origin {
        let project = p.id.clone();
        let original = app
            .db
            .call(move |c| {
                let original = work::event(c, &project, &origin)?;
                if original.source == "autonomy" {
                    ensure!(approved_origin(c, &project, &origin)?, "Approved proposal authority changed");
                }
                Ok(original)
            })
            .await?;
        if original.source == "autonomy" {
            let field = |key: &str| crate::envelope::escape(original.payload[key].as_str().unwrap_or_default());
            context = format!(
                "<approved_proposal trust=\"daemon_metadata\" proposal_id=\"{}\" source_event_id=\"{}\" chat_id=\"{}\" message_id=\"{}\">Use this source_event_id, chat_id as to, and message_id as reply_to_message_id for a result in the approval discussion.</approved_proposal>",
                field("proposal_id"),
                crate::envelope::escape(&original.id),
                field("to"),
                field("anchor"),
            );
            if crate::manager_workers::is_plain(p) {
                context = format!("<approved_proposal trust=\"daemon_metadata\" proposal_id=\"{}\" source_event_id=\"{}\"/>", field("proposal_id"), crate::envelope::escape(&original.id));
            }
        } else if let Some(id) = original.message_id {
            let message = app.db.message(&id).await?;
            let group = crate::feishu::context(app, &message).await;
            let message = app.db.message(&message.id).await?;
            context = crate::envelope::render_resources(
                &message,
                &group,
                &crate::feishu::message_resources(&message),
            );
        } else {
            context = crate::envelope::scheduled_body(&original.payload)?;
        }
    }
    Ok(format!(
        "<management_delegation effect_id=\"{}\" trust=\"daemon_metadata\">\n{}\n<manager_instructions trust=\"agent_content\">{}</manager_instructions>\n</management_delegation>",
        crate::envelope::escape(&event.source_id),
        context,
        crate::envelope::escape(event.payload["instructions"].as_str().unwrap_or_default())
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    expected_sha256: Option<String>,
}
fn read_file(root: &Path, args: FileArgs) -> Result<Value> {
    use std::os::unix::fs::OpenOptionsExt;

    const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
    const MAX_CHUNK_BYTES: usize = 128 * 1024;
    let resolved = root.join(crate::feishu::expand_home(Path::new(&args.path)));
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(resolved)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Read a regular file");
    ensure!(
        metadata.len() <= MAX_READ_BYTES,
        "File exceeds the 8 MiB read limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_READ_BYTES,
        "File exceeds the 8 MiB read limit"
    );
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let full = String::from_utf8(bytes).context("Read UTF-8 files only")?;
    let total = full.len();
    let offset = args.offset.unwrap_or(0);
    ensure!(offset <= total, "Read offset exceeds file length");
    let start = full.floor_char_boundary(offset);
    let limit = args.limit.unwrap_or(MAX_CHUNK_BYTES).clamp(1, MAX_CHUNK_BYTES);
    let mut end = full.floor_char_boundary(start.saturating_add(limit).min(total));
    if end == start && start < total {
        end = full.ceil_char_boundary(start + 1);
    }
    let truncated = start > 0 || end < total;
    let mut result = json!({
        "path": args.path,
        "content": &full[start..end],
        "sha256": hash,
        "total_bytes": total,
        "truncated": truncated,
    });
    if truncated {
        result["offset"] = json!(start);
        result["next_offset"] = json!(if end < total { Some(end) } else { None::<usize> });
    }
    Ok(result)
}

#[cfg(test)]
fn write_file(root: &Path, args: WriteArgs) -> Result<Value> {
    write_file_checked(root, args, |mutate| mutate())
}

fn write_file_checked(
    root: &Path,
    args: WriteArgs,
    mut commit: impl FnMut(&mut dyn FnMut() -> Result<()>) -> Result<()>,
) -> Result<Value> {
    use std::io::ErrorKind;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = root.join(crate::feishu::expand_home(Path::new(&args.path)));
    let parent = path.parent().context("File path has no parent")?;
    let (target, permissions) = match fs::canonicalize(&path) {
        Ok(target) => {
            let metadata = fs::metadata(&target)?;
            ensure!(metadata.is_file(), "Write a regular file");
            if let Some(expected) = &args.expected_sha256 {
                let mut file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&target)?;
                ensure!(file.metadata()?.is_file(), "Write a regular file");
                let mut hash = Sha256::new();
                std::io::copy(&mut file, &mut hash)?;
                ensure!(
                    *expected == format!("{:x}", hash.finalize()),
                    "File changed; expected_sha256 does not match"
                );
            }
            let permissions = fs::Permissions::from_mode(metadata.permissions().mode() & 0o777);
            (target, Some(permissions))
        }
        Err(error) if error.kind() == ErrorKind::NotFound && args.expected_sha256.is_none() => {
            commit(&mut || {
                fs::create_dir_all(parent)?;
                Ok(())
            })?;
            (path, None)
        }
        Err(error) => return Err(error.into()),
    };
    if permissions.is_none() && fs::symlink_metadata(&target).is_ok_and(|m| m.is_symlink()) {
        // Creating through a dangling link must leave the link itself in place.
        commit(&mut || {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NONBLOCK)
                .open(&target)?;
            ensure!(file.metadata()?.is_file(), "Write a regular file");
            file.write_all(args.content.as_bytes())?;
            file.set_len(args.content.len() as u64)?;
            file.sync_all()?;
            Ok(())
        })?;
    } else {
        let mut file =
            tempfile::NamedTempFile::new_in(target.parent().context("File path has no parent")?)?;
        file.write_all(args.content.as_bytes())?;
        if let Some(permissions) = permissions {
            file.as_file().set_permissions(permissions)?;
        }
        file.as_file().sync_all()?;
        let mut file = Some(file);
        commit(&mut || {
            file.take()
                .expect("commit runs once")
                .persist(&target)?;
            Ok(())
        })?;
    }
    Ok(
        json!({"path":args.path,"sha256":format!("{:x}",Sha256::digest(args.content.as_bytes())),"status":"written"}),
    )
}
pub fn validate_outbox(c: &Connection, project: &str, attachment: &Value) -> Result<()> {
    if let Some(revision) = attachment["manager_provider_revision"].as_i64() {
        let allowed: bool = c.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM manager_provider_config g JOIN projects p ON p.id=?
                WHERE g.revision=? AND p.paused=0 AND NOT ({PROVIDER_CONFIGURED}))"),
            params![project, revision], |row| row.get(0),
        )?;
        ensure!(
            allowed && !work::has_project_takeover(c, project)?,
            "The manager configuration notice is no longer active"
        );
    }
    if let Some(origin) = attachment["manager_receipt_origin"].as_str() {
        let allowed: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_inputs i JOIN manager_config m ON m.project_id=i.project_id JOIN projects p ON p.id=m.project_id WHERE i.project_id=? AND i.origin_event_id=? AND i.kind='user' AND i.status IN ('queued','processing','consumed') AND m.revision=? AND p.paused=0)",
            params![project, origin, attachment["manager_revision"].as_i64()],
            |r| r.get(0),
        )?;
        ensure!(
            allowed,
            "The receipt's user source was stopped or superseded"
        );
        ensure!(
            !work::has_project_takeover(c, project)?,
            "Management reply is held by user takeover"
        );
        return Ok(());
    }
    if attachment["manager_notice"] == true {
        let allowed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM manager_failures f JOIN manager_config m ON m.project_id=f.project_id JOIN projects p ON p.id=m.project_id WHERE f.project_id=? AND f.incident_id=? AND f.revision=? AND f.revision=m.revision AND p.paused=0)",params![project,attachment["manager_incident"].as_str(),attachment["manager_revision"].as_i64()],|r|r.get(0))?;
        ensure!(
            allowed,
            "Management notice no longer belongs to an active incident"
        );
        ensure!(
            !work::has_project_takeover(c, project)?,
            "Management notice is held by user takeover"
        );
    }
    let Some(effect) = attachment["manager_effect"].as_str() else {
        return Ok(());
    };
    let allowed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id JOIN manager_config m ON m.project_id=r.project_id JOIN projects p ON p.id=m.project_id WHERE e.id=? AND r.project_id=? AND r.status IN ('working','complete') AND r.revision=m.revision AND r.input_revision=m.input_revision AND p.paused=0)",params![effect,project],|r|r.get(0))?;
    ensure!(allowed, "Management reply was superseded or stopped");
    ensure!(
        !work::has_project_takeover(c, project)?,
        "Management reply is held by user takeover"
    );
    Ok(())
}
fn proposal_run(c: &Connection, run: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM manager_run_inputs b
         JOIN manager_inputs i ON i.id=b.input_id
         JOIN work_events e ON e.id=i.origin_event_id
         WHERE b.run_id=? AND e.source='autonomy')",
        [run],
        |row| row.get(0),
    )?)
}

const FAILED_BEFORE_SUBMISSION: &str = "e.status='failed' AND e.received_at IS NULL
    AND NOT EXISTS(SELECT 1 FROM work_runs r WHERE r.event_id=e.id)";

fn worker_snapshot(c: &Connection, project: &str, generation: &str) -> Result<Value> {
    let transition = crate::cli_switch::latest(c, project)?;
    let delegations = c.prepare("SELECT e.id,e.status,r.status,e.error,json_extract(e.payload,'$.transition_id'),json_extract(e.payload,'$.expected_agent'),json_extract(e.payload,'$.target_generation'),e.source FROM work_events e LEFT JOIN work_runs r ON r.event_id=e.id WHERE e.project_id=? AND e.source IN ('management','manager_worker') ORDER BY e.rowid DESC LIMIT 24")?
        .query_map([project],|r| {
            let mut status: String = r.get(1)?;
            if status=="worker_waiting" {status="queued".into();}
            let independent = r.get::<_, String>(7)? == "manager_worker";
            let held = status == "queued" && transition.as_ref().is_some_and(|request|
                request.holds_input || (!independent && request.phase == "recovery_exhausted"));
            let generation: Option<String> = r.get(6)?;
            Ok(json!({"event_id":r.get::<_,String>(0)?,"event_status":status,"held":held,"run_status":r.get::<_,Option<String>>(2)?,"error":r.get::<_,String>(3)?,"transition_id":r.get::<_,Option<String>>(4)?,"expected_agent":r.get::<_,Option<String>>(5)?,"assignment_pending":independent && status=="queued" && generation.is_none(),"generation":generation}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let work = work::snapshot(c, project)?;
    let workers = crate::manager_workers::snapshot(c,&db::project_by_key(c,project)?)?;
    Ok(json!({
        "workers":workers,
        "delegations":delegations,
        "cli_transition":transition,
        "last_failure":transition.as_ref().and_then(|r|r.source["last_failure"].as_str()),
        "work_attention":work.attention,
        "takeover":work::has_takeover(c,project,generation)?,
    }))
}

pub(crate) fn observe_cli_transition(
    c: &Connection,
    project: &str,
    id: &str,
    state: &str,
    error: &str,
) -> Result<()> {
    if uses_manager(c,project)? {
        observation(
            c,
            project,
            "cli_transition",
            id,
            state,
            None,
            &json!({"kind":"cli_transition","request_id":id,"phase":state,"error":error}),
        )?;
        c.execute(
            "UPDATE manager_config SET next_attempt_at=0 WHERE project_id=? AND error=''",
            [project],
        )?;
    }
    worker_changed(c, project)
}
pub fn worker_changed(c: &Connection, project: &str) -> Result<()> {
    if !uses_manager(c,project)? {
        return Ok(());
    }
    observe(c, project)?;
    repair_finished(c, project)?;
    Ok(())
}
fn repair_finished(c: &Connection, project: &str) -> Result<()> {
    let current:Option<(String,i64)>=c.query_row("SELECT f.incident_id,f.revision FROM manager_failures f JOIN manager_config m ON m.project_id=f.project_id JOIN work_events e ON e.project_id=f.project_id AND json_extract(e.payload,'$.repair_incident')=f.incident_id AND json_extract(e.payload,'$.repair_revision')=f.revision JOIN work_runs r ON r.event_id=e.id WHERE f.project_id=? AND f.revision=m.revision AND f.repair_probe=0 AND r.finished_at IS NOT NULL AND r.status IN ('settled','completed') LIMIT 1",[project],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((incident, revision)) = current else {
        return Ok(());
    };
    c.execute(
        "UPDATE manager_failures SET repair_probe=1 WHERE project_id=? AND incident_id=?",
        params![project, incident],
    )?;
    input(
        c,
        project,
        &format!("repair-probe-{incident}"),
        None,
        "worker",
        &json!({"kind":"repair_finished","incident_id":incident,"semantic_task_success":"unconfirmed","instructions":"Inspect the preserved effect evidence and repair result; make one fresh management request without replaying old effects."}),
    )?;
    c.execute("UPDATE manager_runs SET status='superseded' WHERE project_id=? AND revision=? AND status='working'",params![project,revision])?;
    c.execute(
        "UPDATE manager_config SET next_attempt_at=0 WHERE project_id=? AND revision=?",
        params![project, revision],
    )?;
    Ok(())
}
/// Durable observations contain receipts, never invented task-completion signals.
fn observe(c: &Connection, project: &str) -> Result<()> {
    let rows=c.prepare("SELECT id,status,content,attachment FROM messages WHERE project_id=? AND direction='out' AND status IN ('sent','failed','uncertain') AND json_extract(attachment,'$.manager_effect') IS NULL AND json_extract(attachment,'$.manager_notice') IS NULL AND json_extract(attachment,'$.autonomy_publication_revision') IS NULL AND created_at >= COALESCE((SELECT MIN(created_at) FROM work_events WHERE project_id=? AND source IN ('management','manager_worker')),9223372036854775807) ORDER BY rowid DESC LIMIT 32")?.query_map(params![project,project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, status, content, raw) in rows {
        let a: Value = serde_json::from_str(&raw)?;
        let anchor = a["resolved_reply_to_message_id"].as_str().or_else(|| a["reply_to_message_id"].as_str());
        let origin=anchor.map(|anchor|c.query_row("SELECT e.id FROM work_events e JOIN messages m ON m.id=e.message_id WHERE e.project_id=? AND m.direction='in' AND m.source_id=?",params![project,anchor],|r|r.get::<_,String>(0)).optional()).transpose()?.flatten();
        let scoped_origin = a["discussion_scope"].as_str().map(|scope|
            c.query_row("SELECT id FROM work_events WHERE project_id=? AND id=?",
                params![project,scope], |row| row.get::<_,String>(0)).optional()
        ).transpose()?.flatten();
        let worker_origin = a["task_worker_generation"].as_str().map(|generation| {
            c.query_row(
                "SELECT origin_event_id FROM manager_workers WHERE project_id=? AND generation=?",
                params![project,generation], |row| row.get::<_,Option<String>>(0),
            ).optional()
        }).transpose()?.flatten().flatten();
        let origin = origin.or(scoped_origin).or(worker_origin);
        observation(
            c,
            project,
            "worker_message",
            &id,
            &status,
            origin.as_deref(),
            &json!({"kind":"worker_message","worker_generation":a["task_worker_generation"],"status":status,"already_delivered_to_user":status=="sent","content":content.chars().take(16000).collect::<String>(),"to":a["to"],"reply_to_message_id":anchor,"sent_message_id":a["sent_message_id"],"sent_chat_id":a["sent_chat_id"]}),
        )?;
    }
    let intents=c.prepare(&format!("SELECT e.id,e.error,json_extract(e.payload,'$.origin_event_id')
        FROM work_events e WHERE e.project_id=? AND e.source IN ('management','manager_worker') AND ({FAILED_BEFORE_SUBMISSION})
        ORDER BY e.rowid DESC LIMIT 32"))?.query_map([project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, error, origin) in intents {
        observation(
            c,
            project,
            "worker_intent",
            &id,
            "failed",
            origin.as_deref(),
            &json!({"event_id":id,"status":"definitely_not_submitted","error":error}),
        )?;
    }
    let runs=c.prepare("SELECT r.id,r.status,r.error,json_extract(e.payload,'$.origin_event_id'),r.event_id,r.generation,json_extract(e.payload,'$.worker_result') FROM work_runs r JOIN work_events e ON e.id=r.event_id WHERE r.project_id=? AND e.source IN ('management','manager_worker') AND r.status IN ('settled','completed','uncertain','failed','interrupted') ORDER BY r.rowid DESC LIMIT 32")?.query_map([project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<String>>(6)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, status, error, origin, event, generation, result) in runs {
        let result: Value = result.as_deref().and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(|result| result["run_id"] == id).unwrap_or(Value::Null);
        let stamp = result["status"].as_str().map_or_else(|| status.clone(),
            |report_status| format!("{status}:{report_status}"));
        observation(
            c,
            project,
            "worker_run",
            &id,
            &stamp,
            origin.as_deref(),
            &json!({"kind":"worker_run","run_id":id,"event_id":event,"worker_generation":generation,"delivery_status":status,"error":error,"worker_report":result,"semantic_task_success":"unconfirmed"}),
        )?;
    }
    Ok(())
}
pub(crate) fn observation(
    c: &Connection,
    project: &str,
    source: &str,
    id: &str,
    stamp: &str,
    origin: Option<&str>,
    payload: &Value,
) -> Result<()> {
    let previous:Option<String>=c.query_row("SELECT stamp FROM manager_observations WHERE project_id=? AND source=? AND source_id=?",params![project,source,id],|r|r.get(0)).optional()?;
    if previous.as_deref() == Some(stamp) {
        return Ok(());
    }
    c.execute("INSERT INTO manager_observations(project_id,source,source_id,stamp) VALUES(?,?,?,?) ON CONFLICT(project_id,source,source_id) DO UPDATE SET stamp=excluded.stamp",params![project,source,id,stamp])?;
    input(
        c,
        project,
        &format!("observation:{source}:{id}:{stamp}"),
        origin,
        "worker",
        payload,
    )
}
fn trusted_source(c: &Connection, project: &str, id: &str) -> Result<Value> {
    let e = work::event(c, project, id)?;
    if e.source == "autonomy" {
        return Ok(json!({"event_id":id,"source_kind":"autonomy","proposal_id":e.payload["proposal_id"],
            "message_id":e.payload["anchor"],"bot_app_id":e.payload["app_id"],"to":e.payload["to"]}));
    }
    if let Some(message) = e.message_id {
        let raw: String = c.query_row(
            "SELECT attachment FROM messages WHERE id=?",
            [message],
            |r| r.get(0),
        )?;
        let a: Value = serde_json::from_str(&raw)?;
        Ok(if e.source == "feishu" {
            let mut trusted = json!({"event_id":id,"source_kind":"feishu","message_id":e.source_id,"bot_app_id":a["bot_app_id"],"source":a["source"]});
            if let Some(group) = crate::feishu::cached_group_context(c, project, &a)? {
                trusted["group_context"] = group;
            }
            trusted
        } else {
            json!({"event_id":id,"source_kind":e.source,"local_source_id":e.source_id})
        })
    } else {
        Ok(json!({"event_id":id,"source_kind":e.source,"to":e.payload["to"],"timer_id":e.payload["timer_id"],"scheduled_at":e.payload["scheduled_at"],"purpose":e.payload["purpose"]}))
    }
}
fn prompt_input(raw: &str, event_id: Option<&str>) -> Result<Value> {
    let mut value: Value = serde_json::from_str(raw)?;
    if raw.len() > 96000 {
        if let Some(body) = value["user_input"]["body"].as_str() {
            let size = body.len();
            let preview = body.chars().take(12000).collect::<String>();
            value["user_input"]["body"] = json!(preview);
            value["user_input"]["full_body_bytes"] = json!(size);
        }
        value["full_input_retained"] = json!(true);
        value["source_event_id"] = json!(event_id);
        value["continuation"] = json!(
            "Only a bounded preview is shown. The original input is retained intact."
        );
    }
    if value.to_string().len() > 120000 {
        value = json!({"source_event_id":event_id,"full_input_retained":true,"continuation":"The original input and attachments are retained intact."});
    }
    Ok(value)
}
/// A durable claim survives process loss; the historical run/input join is not ownership.
fn own_input(c: &Connection, run: &str, input: &str) -> Result<()> {
    c.execute("INSERT INTO manager_input_claims(input_id,run_id) VALUES(?,?) ON CONFLICT(input_id) DO UPDATE SET run_id=excluded.run_id", params![input, run])?;
    ensure!(c.execute("UPDATE manager_inputs SET status='processing' WHERE id=? AND status IN ('queued','processing')", [input])? == 1, "Input is no longer claimable");
    Ok(())
}
fn attach_quote(
    c: &Connection,
    project: &str,
    origin: Option<&str>,
    data: &mut Value,
    remaining: &mut usize,
) -> Result<()> {
    let Some(origin) = origin else { return Ok(()) };
    let raw: Option<String> = c
        .query_row(
            "SELECT json_extract(m.attachment,'$.quoted_message') FROM work_events e
         JOIN messages m ON m.id=e.message_id AND m.project_id=e.project_id
         WHERE e.id=? AND e.project_id=? AND e.source='feishu' AND m.direction='in'
           AND json_extract(m.attachment,'$.quoted_message.status')='ok'",
            params![origin, project],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(raw) = raw {
        let mut quote: Value = serde_json::from_str(&raw)?;
        quote.as_object_mut().unwrap().remove("resources");
        let size = quote.to_string().len() + 64;
        if size <= *remaining {
            data["quoted_message"] = quote;
            *remaining -= size;
        }
    }
    Ok(())
}

fn prepared_input() -> String {
    format!("NOT EXISTS(SELECT 1 FROM work_events e JOIN messages m ON m.id=e.message_id
        WHERE e.id=manager_inputs.origin_event_id AND {})", crate::feishu::preparation_pending())
}
fn image_source(
    c: &Connection,
    project: &str,
    origin: &str,
) -> Result<Option<(String, Value)>> {
    let row: Option<(String, String, String)> = c.query_row("SELECT m.id,m.source_id,m.attachment FROM work_events e JOIN messages m ON m.id=e.message_id WHERE e.id=? AND e.project_id=? AND m.project_id=? AND m.direction='in'", params![origin,project,project], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((id, source_id, raw)) = row else {
        return Ok(None);
    };
    let mut a: Value = serde_json::from_str(&raw)?;
    a["source_message_id"] = json!(source_id);
    Ok(Some((id, a)))
}
fn prepare_images(
    c: &Connection,
    project: &str,
    origin: Option<&str>,
    data: &mut Value,
) -> Result<Vec<crate::multimodal::ImageRef>> {
    let Some(origin) = origin else {
        return Ok(vec![]);
    };
    let Some((message, attachment)) = image_source(c, project, origin)? else {
        return Ok(vec![]);
    };
    let source_id = attachment["source_message_id"].as_str().unwrap_or_default();
    let mut resources = crate::feishu::attachment_resources(&attachment, source_id);
    let mut images = Vec::new();
    for resource in &mut resources {
        if resource["kind"] != "image"
            && !resource["media_type"]
                .as_str()
                .is_some_and(|mime| mime.starts_with("image/"))
        {
            continue;
        }
        let verified = (|| {
            ensure!(
                if resource["quoted"] == true {
                    resource["cache_source_id"] == attachment["source"]["parent_id"]
                        && resource["cache_source_id"] == attachment["quoted_message"]["message_id"]
                        && attachment["quoted_message"]["status"] == "ok"
                } else {
                    resource["cache_source_id"] == attachment["source_message_id"]
                },
                "Image source identity changed"
            );
            let database_root = std::path::Path::new(
                c.path()
                    .context("Image cache needs a persistent project store")?,
            )
            .parent()
            .context("Image cache store is unavailable")?
            .canonicalize()?;
            crate::multimodal::ImageRef::from_resource(
                project,
                &message,
                resource,
                &database_root.join("files").join(project),
            )
        })();
        let reason = if images.len() >= crate::multimodal::MAX_IMAGES {
            "image_count_limit"
        } else if !resource["local_path"].is_string() {
            "image_resource_unavailable"
        } else if resource["bytes"]
            .as_u64()
            .is_some_and(|size| size > crate::multimodal::MAX_IMAGE_BYTES)
        {
            "image_size_limit"
        } else if resource["media_type"].as_str().is_some_and(|mime| {
            !matches!(
                mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        }) {
            "unsupported_image_type"
        } else if verified.is_err() {
            "image_cache_metadata_unverified"
        } else {
            "verified_image_reference"
        };
        resource["image_delivery"] = if reason == "verified_image_reference" {
            json!({"mode":"image_reference","reason":reason})
        } else {
            json!({"mode":"path_only","reason":reason,"note":"Image pixels are unavailable. Inline input requires a verified PNG, JPEG, GIF, or WebP of up to 2 MiB and at most 4 images per message."})
        };
        if reason == "verified_image_reference" {
            images.push(verified?);
        }
    }
    data["user_input"]["resources"] = json!((!resources.is_empty()).then_some(resources));
    Ok(images)
}
fn provider_unavailable(c: &Connection, project: &str) -> Result<bool> {
    let (configured, revision): (bool, i64) = c.query_row(
        &format!("SELECT ({PROVIDER_CONFIGURED}),revision FROM manager_provider_config g WHERE g.id=1"),
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if configured {
        return Ok(false);
    }
    let p = db::project_by_key(c, project)?;
    if p.paused || work::has_project_takeover(c, project)? {
        return Ok(true);
    }
    let Some(binding) = crate::bot::get(c, project)? else {
        return Ok(true);
    };
    let request = format!("manager-provider-required-{project}-{revision}");
    let source: Option<(String, String, String)> = c.query_row(
        "SELECT m.source_id,json_extract(m.attachment,'$.source.chat_id'),m.content
         FROM manager_inputs i JOIN work_events e ON e.id=i.origin_event_id
         JOIN messages m ON m.id=e.message_id
         WHERE i.project_id=? AND i.status IN ('queued','processing') AND e.source='feishu'
         AND json_extract(m.attachment,'$.bot_app_id')=?
         AND json_type(m.attachment,'$.source.chat_id')='text'
         AND m.source_id GLOB 'om_*'
         AND NOT EXISTS(SELECT 1 FROM messages WHERE project_id=i.project_id AND direction='out' AND source_id=?)
         ORDER BY i.rowid LIMIT 1",
        params![project,binding.app_id,request], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).optional()?;
    if let Some((anchor, to, body)) = source {
        let content = if body.chars().any(|ch| ('\u{3400}'..='\u{9fff}').contains(&ch)) {
            "尚未配置全局管理智能体凭据。消息已保留，请在 Dauerbot 的设置中配置服务地址、模型和 API 密钥。"
        } else {
            "Your message is saved. Configure the shared manager endpoint, model, and API key in Dauerbot Settings to continue."
        };
        db::enqueue_out_request(c, project, &binding.app_id, &request, content,
            json!({"to":to,"reply_to_message_id":anchor,"manager_provider_revision":revision}))?;
    }
    Ok(true)
}
fn close_turn(c: &Connection, context: &Context) -> Result<()> {
    c.execute(
        "UPDATE manager_runs SET status='complete' WHERE id=?",
        [&context.run],
    )?;
    c.execute("UPDATE manager_inputs SET status='consumed' WHERE id IN
        (SELECT input_id FROM manager_input_claims WHERE run_id=?) AND status='processing'",
        [&context.run])?;
    settle_inputs(c, &context.run)
}

fn cancel_repair(c: &Connection, id: &str, reason: &str) -> Result<()> {
    if c.execute("UPDATE work_events SET status='cancelled',error=?,updated_at=? WHERE id=? AND status='queued'",params![reason,db::now(),id])?==1 {
        c.execute("UPDATE work_tasks SET status='cancelled',updated_at=? WHERE id=(SELECT task_id FROM work_events WHERE id=?)",params![db::now(),id])?;
        c.execute("UPDATE work_occurrences SET status='cancelled' WHERE event_id=?",[id])?;
    }
    Ok(())
}
fn recovered(c: &Connection, ctx: &Context) -> Result<()> {
    let repairs=c.prepare("SELECT id FROM work_events WHERE project_id=? AND source='management' AND status='queued' AND json_extract(payload,'$.repair_incident') IN(SELECT incident_id FROM manager_failures WHERE project_id=? AND revision=?)")?.query_map(params![ctx.project,ctx.project,ctx.revision],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in repairs {
        cancel_repair(c, &id, "Management recovered before repair dispatch")?;
    }
    c.execute(
        "UPDATE manager_config SET error='',next_attempt_at=0 WHERE project_id=? AND revision=?",
        params![ctx.project, ctx.revision],
    )?;
    c.execute(
        "DELETE FROM manager_failures WHERE project_id=? AND revision=?",
        params![ctx.project, ctx.revision],
    )?;
    Ok(())
}
fn failure_notice(error: &Failure, delay: i64, repair: bool, chinese: bool) -> String {
    if chinese {
        let reason = match error.kind.as_str() {
            "timeout" => "管理模型请求超时",
            "transport" => "管理模型连接失败",
            "authorization" => "管理模型接口拒绝了访问",
            "model_unavailable" => "未找到配置的管理模型",
            "rate_limited" => "管理模型接口正在限流",
            "provider_unavailable" => "管理模型接口未能处理请求",
            "context_rejected" => "管理模型请求超过上下文限制",
            "request_rejected" => "管理模型接口拒绝了请求",
            "invalid_response" => "管理模型回复不完整或格式不合法",
            "context_preparation" => "管理上下文未能准备完成",
            _ => "管理流程未能继续",
        };
        let retry = if error.retryable {
            format!("将在 {} 秒后自动重试。", delay / 1000)
        } else {
            String::new()
        };
        let repair = if repair {
            "已安排排查恢复。"
        } else {
            "设置和修复入口仍可使用。"
        };
        return format!("{reason}。输入和执行记录均已保留。{retry}{repair}");
    }
    let retry = if error.retryable {
        format!(" Retrying automatically in {} seconds.", delay / 1000)
    } else {
        String::new()
    };
    let repair = if repair {
        " Recovery has been scheduled for investigation."
    } else {
        " Settings and repair controls remain available."
    };
    format!(
        "{}. Input and effect records are saved.{retry}{repair}",
        error.summary()
    )
}
fn failure(c: &Connection, ctx: &Context, error: &Failure) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    let Ok(p) = valid(&tx, ctx) else {
        return Ok(());
    };
    let previous:Option<(String,i64,bool)>=tx.query_row("SELECT incident_id,attempts,notified FROM manager_failures WHERE project_id=? AND revision=?",params![ctx.project,ctx.revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (incident, attempts, notified) = previous
        .map(|(id, n, notified)| (id, n.saturating_add(1), notified))
        .unwrap_or_else(|| (db::id(), 1, false));
    let delay = if error.retryable {
        (30_000_i64 * (1_i64 << attempts.saturating_sub(1).clamp(0, 4))).min(300_000)
    } else {
        0
    };
    let next_attempt = if error.retryable {
        db::now() + delay
    } else {
        i64::MAX
    };
    let diagnostic = json!({"incident_id":incident,"attempt":attempts,"kind":error.kind,"http_status":error.http_status,"retryable":error.retryable,"elapsed_ms":error.elapsed_ms,"output_tokens":error.output_tokens,"finish_reason":error.finish_reason,"failed_at":db::now(),"next_attempt_at":if error.retryable{Some(next_attempt)}else{None}});
    let diagnostic = diagnostic.to_string();
    tx.execute("INSERT INTO manager_failures(project_id,revision,incident_id,attempts,diagnostic,notified) VALUES(?,?,?,?,?,?) ON CONFLICT(project_id) DO UPDATE SET revision=excluded.revision,incident_id=excluded.incident_id,attempts=excluded.attempts,diagnostic=excluded.diagnostic,notified=excluded.notified,repair_probe=CASE WHEN manager_failures.incident_id=excluded.incident_id THEN manager_failures.repair_probe ELSE 0 END",params![ctx.project,ctx.revision,incident,attempts,diagnostic,notified])?;
    tx.execute(
        "UPDATE manager_runs SET status='failed',error=? WHERE id=? AND status='working'",
        params![serde_json::to_string(error)?, ctx.run],
    )?;
    tx.execute(
        "UPDATE manager_config SET error=?,next_attempt_at=? WHERE project_id=?",
        params![error.summary(), next_attempt, ctx.project],
    )?;
    // The legacy worker repair tool path is unavailable in automatic mode.
    // Keep the incident and input without manufacturing a primary-pane job.
    let repair = p.agent != "auto" && (!error.retryable || attempts >= 3);
    if repair {
        let repair_id = format!("manager-repair-{incident}");
        let instructions = format!(
            "<management_repair trust=\"daemon_metadata\" incident_id=\"{incident}\" revision=\"{}\">Repair only this project's management service. Sanitized diagnostic: {}. Verify the current manager failure still has this incident and revision before making changes. Diagnose the configured provider request or management loop; preserve queued user input and all effect receipts. Do not process or forward the user's ordinary tasks, replay uncertain effects, change native CLI login/global settings, or replace the selected provider without explicit authority. Use the existing project control path and report verified recovery or the concrete remaining cause. This is one bounded repair assignment, not authorization for recurring retries.</management_repair>",
            ctx.revision, error
        );
        let event = intent(&tx, &p, &repair_id, &instructions, None, false)?;
        tx.execute("UPDATE work_events SET payload=json_set(payload,'$.repair_incident',?,'$.repair_revision',?) WHERE id=?",params![incident,ctx.revision,event])?;
    }
    if notified
        || error.retryable
        || !matches!(error.kind.as_str(), "authorization" | "model_unavailable")
    {
        tx.commit()?;
        return Ok(());
    }
    let Some(binding) = crate::bot::get(&tx, &ctx.project)? else {
        tx.commit()?;
        return Ok(());
    };
    let origins=tx.prepare("SELECT DISTINCT i.origin_event_id FROM manager_inputs i JOIN manager_run_inputs b ON b.input_id=i.id WHERE b.run_id=? AND i.origin_event_id IS NOT NULL ORDER BY i.created_at,i.rowid")?.query_map([&ctx.run],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for origin in origins {
        let e = work::event(&tx, &ctx.project, &origin)?;
        let inbound = e.message_id.as_ref().map(|id| {
            tx.query_row(
                "SELECT attachment,content FROM messages WHERE id=?",
                [id],
                |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)),
            )
        }).transpose()?;
        let (source, content) = match inbound {
            Some((raw, content)) => (serde_json::from_str::<Value>(&raw)?["source"].clone(), content),
            None => (Value::Null, e.payload["instructions"].as_str().unwrap_or_default().to_owned()),
        };
        let to = source["chat_id"].as_str()
            .or_else(|| e.payload["to"].as_str()).filter(|value| !value.is_empty())
            .unwrap_or(&p.chat_id);
        if to.is_empty() {
            continue;
        }
        let chinese = content.chars().any(|ch| ('\u{3400}'..='\u{9fff}').contains(&ch));
        let action = match (error.kind.as_str(), chinese) {
            ("authorization", true) => "请检查管理模型服务的授权。",
            ("authorization", false) => " Check the management provider credentials.",
            (_, true) => "请在设置里选择可用的管理模型。",
            (_, false) => " Choose an available management model in Settings.",
        };
        let message = failure_notice(error, delay, repair, chinese) + action;
        let mut attachment = json!({"to":to,"discussion_intent":"decision","manager_notice":true,
            "manager_incident":incident,"manager_revision":ctx.revision});
        if let Err(error) = crate::discussion::origin(&tx, &ctx.project, &origin, &binding.app_id, to, false, &mut attachment) {
            tracing::warn!(%error, "Could not route management decision to its discussion");
            continue;
        }
        db::enqueue_out_request(
            &tx,
            &ctx.project,
            &binding.app_id,
            &format!("manager-unavailable-{}-{incident}", ctx.project),
            &message,
            attachment,
        )?;
        tx.execute(
            "UPDATE manager_failures SET notified=1 WHERE project_id=?",
            [&ctx.project],
        )?;
        break;
    }
    tx.commit()?;
    Ok(())
}
pub fn stop(c: &Connection, project: &str) -> Result<()> {
    crate::cli_control::classify(c)?;
    work::sync_inbound(c)?;
    // Do not resurrect a queued acknowledgement after Stop followed by resume.
    // Sending/uncertain effects may already have happened and remain untouched.
    c.execute(
        "UPDATE messages SET status='failed',error='Stopped by the user',updated_at=? WHERE project_id=? AND direction='out' AND status='pending' AND json_extract(attachment,'$.manager_receipt_origin') IS NOT NULL",
        params![db::now(), project],
    )?;
    let repairs=c.prepare("SELECT id FROM work_events WHERE project_id=? AND source='management' AND status='queued' AND json_extract(payload,'$.repair_incident') IS NOT NULL")?.query_map([project],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in repairs {
        cancel_repair(c, &id, "Stopped by the user")?;
    }
    c.execute("DELETE FROM manager_failures WHERE project_id=?", [project])?;
    c.execute(
        "UPDATE manager_config SET error='',next_attempt_at=0 WHERE project_id=?",
        [project],
    )?;
    c.execute("UPDATE manager_effects SET status='uncertain',result=? WHERE status='submitting' AND run_id IN (SELECT id FROM manager_runs WHERE project_id=? AND status='working')", params![json!({"status":"uncertain","message":"Stopped during submission; inspect original effect evidence before retrying."}).to_string(),project])?;
    c.execute(
        "UPDATE manager_config SET input_revision=input_revision+1 WHERE project_id=?",
        [project],
    )?;
    c.execute(
        "UPDATE manager_runs SET status='stopped' WHERE project_id=? AND status='working'",
        [project],
    )?;
    Ok(())
}
/// Prime at most the newest ready group input before building a Manager prompt.
async fn prepare_group_context(app: &App, project: &str, channel_id: &str) {
    let project = project.to_owned();
    let channel_id = channel_id.to_owned();
    let prepared = prepared_input();
    let due = app.db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT m.id FROM manager_inputs JOIN work_events e ON e.id=manager_inputs.origin_event_id
             JOIN messages m ON m.id=e.message_id AND m.project_id=manager_inputs.project_id
             JOIN project_bots b ON b.project_id=manager_inputs.project_id
             CROSS JOIN manager_provider_config g WHERE manager_inputs.project_id=? AND manager_inputs.channel_id=? AND manager_inputs.status IN ('queued','processing')
             AND e.source='feishu' AND m.direction='in' AND ({PROVIDER_CONFIGURED}) AND {prepared}
             AND json_extract(m.attachment,'$.source.chat_type')='group'
             AND b.app_id=json_extract(m.attachment,'$.bot_app_id') ORDER BY manager_inputs.rowid DESC LIMIT 1"),
            params![project,channel_id], |r| r.get::<_, String>(0),
        ).optional()?)
    }).await;
    if let Ok(Some(id)) = due && let Ok(message) = app.db.message(&id).await {
        crate::feishu::context(app, &message).await;
    }
}

pub async fn run(app: Arc<App>) {
    agent::run(app).await;
}
fn settle_inputs(c: &Connection, _run: &str) -> Result<()> {
    settle_sources(c)
}
pub fn settle_sources(c: &Connection) -> Result<()> {
    if !installed(c)? {
        return Ok(());
    }
    c.execute("UPDATE work_events SET status='consumed',updated_at=? WHERE status='managed' AND id IN(SELECT origin_event_id FROM manager_inputs WHERE status IN ('consumed','transferred')) AND NOT EXISTS(SELECT 1 FROM work_events d WHERE d.source IN ('management','manager_worker') AND json_extract(d.payload,'$.origin_event_id')=work_events.id AND d.status IN ('queued','worker_waiting','submitting','submitted','uncertain'))",[db::now()])?;
    c.execute("UPDATE work_occurrences SET status='consumed' WHERE event_id IN(SELECT id FROM work_events WHERE status='consumed')",[])?;
    c.execute("UPDATE work_tasks SET status='consumed',updated_at=? WHERE id IN(SELECT task_id FROM work_events WHERE status='consumed')",[db::now()])?;
    Ok(())
}

pub(crate) fn session_context(c: &Connection, project: &str) -> Result<Option<Value>> {
    let latest = c.query_row(
        "SELECT id,json_extract(checkpoint,'$.channel_id') FROM manager_runs
         WHERE project_id=? AND json_valid(checkpoint) AND json_type(checkpoint,'$.channel_id')='text'
         ORDER BY rowid DESC LIMIT 1",
        [project],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).optional()?;
    let Some((run, channel)) = latest else { return Ok(None) };
    let mut messages = vec![rig_core::message::Message::system(SYSTEM)];
    messages.extend(session::load(c, project, &channel)?.into_iter().map(|entry| entry.message));
    let mut view = crate::manager_context::normalized(&messages, &run);
    view["channel_id"] = json!(channel);
    Ok(Some(view))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::ProjectInput};
    pub(super) async fn fixture() -> (tempfile::TempDir, Arc<App>, Project) {
        let dir = tempfile::tempdir().unwrap();
        let (app, project) = fixture_at(dir.path()).await;
        (dir, app, project)
    }

    async fn fixture_at(dir: &Path) -> (Arc<App>, Project) {
        let app = App::new(dir.join("store"), Config::default()).unwrap();
        let path = dir.join("workspace");
        fs::create_dir(&path).unwrap();
        let p = app
            .db
            .create(ProjectInput {
                name: "Management fixture".into(),
                path: path.to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: "native-A".into(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        app.db
            .sync(|c| {
                crate::bot::save(c, &p.id, "cli_fixture", "private-fixture-key", false)?;
                save_provider(
                    c,
                    ConfigInput { provider_kind: None, context_window: None,
                        base_url: "http://127.0.0.1:9/v1".into(),
                        model: "fixture-manager".into(),
                        api_key: Some("private-manager-key".into()),
                        clear_api_key: false,
                        expected_revision: 0,
                        supports_images: None,
                    },
                )?;
                Ok(())
            })
            .unwrap();
        (app, p)
    }
    #[tokio::test]
    async fn group_context_reaches_manager_and_worker_without_crossing_source_scope() -> Result<()> {
        use axum::{Json, Router, routing::{get, post}};
        let (_dir, app, p) = fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        app.config.write().await.feishu_base = format!("http://{}", listener.local_addr()?);
        let fixture = Router::new()
            .route("/open-apis/auth/v3/tenant_access_token/internal", post(|| async {
                Json(json!({"code":0,"tenant_access_token":"fixture-token","expire":3600}))
            }))
            .route("/open-apis/im/v1/chats/oc_original", get(|| async {
                Json(json!({"code":0,"data":{"name":"Original group"}}))
            }))
            .route("/open-apis/im/v1/chats/oc_original/members", get(|| async {
                Json(json!({"code":0,"data":{"items":[{"member_id":"ou_fixture","name":"Fixture sender"}],"has_more":true}}))
            }));
        let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
        let id = app.db.ingest_bot(&p.id,"cli_fixture","oc_original","om_group","hello",
            &json!({"source":{
                "chat_id":"oc_original","chat_type":"group","message_type":"text",
                "sender":{"open_id":"ou_fixture"}
            }}))?.unwrap();
        app.db.sync(work::sync_inbound)?;
        prepare_group_context(&app, &p.id, "feishu:cli_fixture:oc_original").await;
        let mut worker = app.db.sync(|c| {
            let managed = trusted_source(c, &p.id, &id)?;
            assert_eq!(managed["group_context"]["name"], "Original group");
            assert_eq!(managed["group_context"]["sender_name"], "Fixture sender");
            assert_eq!(managed["group_context"]["truncated"], true);
            let event = test_delegate(c, &p.id, "group-context-worker", Some(&id))?;
            work::event(c, &p.id, &event)
        })?;
        worker.payload["target_generation"] = json!(p.generation);
        let xml = body(&app, &p, &worker).await?;
        assert!(xml.contains(r#"<chat name="Original group"/>"#));
        assert!(xml.contains(r#"name="Fixture sender""#));
        let mut assigned_worker = p.clone();
        assigned_worker.generation = "fixture-generation".into();
        let mut assignment_event = worker.clone();
        assignment_event.payload["target_generation"] = json!(assigned_worker.generation);
        let assignment = crate::worker_task::fixture_assignment(&app, &assigned_worker, &assignment_event).await?;
        let assignment = fs::read_to_string(assignment)?;
        assert!(assignment.contains("Original group"));

        app.db.ingest_bot(&p.id,"cli_fixture","oc_other","om_other_group","hello",
            &json!({"source":{"chat_id":"oc_other","chat_type":"group","message_type":"text"}}))?;
        app.db.sync(|c| {
            work::sync_inbound(c)?;
            let other: String = c.query_row(
                "SELECT id FROM work_events WHERE source_id='om_other_group'",
                [], |r| r.get(0),
            )?;
            assert!(trusted_source(c, &p.id, &other)?["group_context"].is_null());
            crate::bot::save(c, &p.id, "cli_other", "fixture-replacement", false)?;
            assert!(trusted_source(c, &p.id, &id)?["group_context"].is_null());
            Ok(())
        })?;
        let rebound = body(&app, &p, &worker).await?;
        assert!(!rebound.contains("Original group"));
        assert!(!rebound.contains("Fixture sender"));
        server.abort();
        Ok(())
    }

    fn ingest(app: &App, p: &Project, id: &str, text: &str) {
        app.db.ingest_bot(&p.id,"cli_fixture","oc_fixture",id,text,&json!({"source":{"message_type":"text","chat_id":"oc_fixture","chat_type":"p2p","sender":{"open_id":"ou_fixture"},"mentions":[]}})).unwrap();
    }
    #[tokio::test]
    async fn feishu_quote_cache_matches_exact_bot_chat_and_project() -> Result<()> {
        let (_dir, app, project) = fixture().await;
        let body = "文字".repeat(800);
        ingest(&app, &project, "om_cached", &body);
        app.db.sync(|c| {
            let quote = crate::feishu::cached_quote(c, &project.id, "cli_fixture", "oc_fixture", "om_cached")?.unwrap();
            assert_eq!(quote["status"], "ok");
            assert_eq!(quote["preview_truncated"], true);
            let preview = quote["body"].as_str().unwrap();
            assert!(preview.len() <= 3072 && body.starts_with(preview));
            for (project, bot, chat) in [
                ("wrong", "cli_fixture", "oc_fixture"),
                (&project.id, "wrong", "oc_fixture"),
                (&project.id, "cli_fixture", "wrong"),
            ] {
                assert!(crate::feishu::cached_quote(c, project, bot, chat, "om_cached")?.is_none());
            }
            Ok(())
        })?;
        Ok(())
    }

    #[tokio::test]
    async fn image_capability_is_explicit_and_resets_when_provider_identity_changes() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c| {
            let original = public(c, &p.id)?;
            assert_eq!(original["supports_images"], false);
            let configure = |model: &str, declared: Option<bool>| -> Result<Value> {
                let current = public(c, &p.id)?;
                save_provider(c, ConfigInput { provider_kind: None, context_window: None, base_url: current["base_url"].as_str().unwrap().into(), model: model.into(), api_key: None, clear_api_key: false, expected_revision: current["revision"].as_i64().unwrap(), supports_images: declared })
            };
            assert_eq!(configure("fixture-vision-name-is-not-proof", None)?["supports_images"], false);
            assert_eq!(configure("fixture-vision-name-is-not-proof", Some(true))?["supports_images"], true);
            assert_eq!(configure("fixture-vision-name-is-not-proof", None)?["supports_images"], true);
            assert_eq!(configure("different-model", None)?["supports_images"], false);
            assert_eq!(configure("different-model", Some(true))?["supports_images"], true);
            let current = public(c, &p.id)?;
            let changed: ConfigInput = serde_json::from_value(json!({"base_url":"http://127.0.0.1:8/v1","model":"different-model","expected_revision":current["revision"]}))?;
            assert_eq!(save_provider(c, changed)?["supports_images"], false);
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn prepared_image_limits_keep_explicit_reasons_and_regular_file_references() {
        let (_dir, app, p) = fixture().await;
        let id = app
            .db
            .ingest_bot(
                &p.id,
                "cli_fixture",
                "oc_fixture",
                "om_limits",
                "Inspect attachments",
                &json!({"source":{"chat_id":"oc_fixture","chat_type":"p2p"},"resources":[]}),
            )
            .unwrap()
            .unwrap();
        let mut resources = Vec::new();
        for index in 0..7 {
            let mut resource = crate::multimodal::fixture_resource(&app.dir, &p.id, &id);
            let original = std::path::PathBuf::from(resource["local_path"].as_str().unwrap());
            let path = original.with_file_name(format!("{id}-{index}.png"));
            std::fs::copy(original, &path).unwrap();
            resource["local_path"] = json!(path);
            resource["cache_source_id"] = json!("om_limits");
            if index == 0 {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(crate::multimodal::MAX_IMAGE_BYTES + 1)
                    .unwrap();
                resource["bytes"] = json!(crate::multimodal::MAX_IMAGE_BYTES + 1);
            } else if index == 1 {
                resource["media_type"] = json!("image/svg+xml");
            }
            resources.push(resource);
        }
        resources.push(json!({"kind":"file","local_path":"/ordinary/document.txt"}));
        app.db.sync(|c| {
            c.execute("UPDATE messages SET attachment=json_set(attachment,'$.resources_ready',json('true'),'$.resources',json(?)) WHERE id=?",params![json!(resources).to_string(),id])?;
            work::sync_inbound(c)?;
            let origin: String = c.query_row("SELECT id FROM work_events WHERE message_id=?",[&id],|r|r.get(0))?;

            let mut data = json!({});
            let refs = prepare_images(c,&p.id,Some(&origin),&mut data)?;
            assert_eq!(refs.len(),crate::multimodal::MAX_IMAGES);
            let resources = data["user_input"]["resources"].as_array().unwrap();
            assert_eq!(resources[0]["image_delivery"]["reason"],"image_size_limit");
            assert_eq!(resources[1]["image_delivery"]["reason"],"unsupported_image_type");
            assert_eq!(resources[6]["image_delivery"]["reason"],"image_count_limit");
            assert!(resources[0]["image_delivery"]["note"].as_str().unwrap().contains("2 MiB"));
            assert!(resources[6]["image_delivery"]["note"].as_str().unwrap().contains("at most 4"));
            assert_eq!(resources[7]["local_path"],"/ordinary/document.txt");
            assert!(resources[7]["image_delivery"].is_null());
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn independent_seats_claim_only_their_own_work_and_failure_is_fenced() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c| {
            c.execute("UPDATE projects SET agent='agy',generation='primary' WHERE id=?",[&p.id])?;
            let parent=db::project_by_key(c,&p.id)?;
            let transition=crate::cli_switch::request(c,&parent,"codex")?.unwrap();
            c.execute("UPDATE cli_switch_requests SET phase='recovery_exhausted',holds_input=0 WHERE id=?",[transition.id])?;
            for (agent,generation) in [("codex","worker-one"),("claude","worker-two")] {
                c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,created_at) VALUES(?,?,?,'isolated','ready',?)",
                    params![generation,parent.id,agent,db::now()])?;
            }
            let one=crate::manager_workers::seat(c,&parent,"worker-one")?.unwrap().project;
            let two=crate::manager_workers::seat(c,&parent,"worker-two")?.unwrap().project;
            let a=intent_for_worker(c,&parent,"effect-one","first task",None,false,Some((&one.agent,Some(&one))))?;
            let b=intent_for_worker(c,&parent,"effect-two","second task",None,false,Some((&two.agent,Some(&two))))?;
            let later=intent_for_worker(c,&parent,"effect-later","held by pause",None,false,Some((&one.agent,Some(&one))))?;
            let workers=worker_snapshot(c,&parent.id,&parent.generation)?;
            assert_eq!(workers["workers"].as_array().unwrap().len(),2);
            assert!(work::claim_next(c,&parent.id,"primary",db::now())?.is_none());
            c.execute("UPDATE cli_switch_requests SET holds_input=1 WHERE project_id=?",[&parent.id])?;
            assert!(work::claim_worker(c,&parent.id,"worker-one",db::now())?.is_none());
            c.execute("UPDATE cli_switch_requests SET holds_input=0 WHERE project_id=?",[&parent.id])?;
            let first=work::claim_worker(c,&parent.id,"worker-one",db::now())?.unwrap();
            assert_eq!(first.event_id,a);
            let second=work::claim_worker(c,&parent.id,"worker-two",db::now())?.unwrap();
            assert_eq!(second.event_id,b);
            work::delivery(c,&first.id,"submitted","",db::now())?;
            work::delivery(c,&second.id,"submitted","",db::now())?;
            crate::manager_workers::fail(c,&parent.id,"worker-two","fixture outage")?;
            assert_eq!(work::event(c,&parent.id,&b)?.status,"uncertain");
            assert_eq!(work::event(c,&parent.id,&a)?.status,"submitted");
            c.execute("UPDATE projects SET paused=1 WHERE id=?",[&parent.id])?;
            assert!(work::claim_worker(c,&parent.id,"worker-one",db::now())?.is_none());
            assert_eq!(work::event(c,&parent.id,&later)?.status,"worker_waiting");
            Ok(())
        }).unwrap();
    }

    #[test]
    fn file_tools_follow_standard_paths_and_check_explicit_hashes() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let skill = ".dauerbot/skills/example/SKILL.md";
        let absolute = root.join(skill);
        let write = |path: &str, content: &str, expected_sha256| {
            write_file(
                &root,
                WriteArgs {
                    path: path.into(),
                    content: content.into(),
                    expected_sha256,
                },
            )
        };

        let read = |path: &str, offset: Option<usize>| {
            read_file(
                &root,
                FileArgs {
                    path: path.into(),
                    offset,
                    limit: None,
                },
            )
            .unwrap()
        };

        let large = "a".repeat(300_000);
        let first = write(skill, &large, None).unwrap();
        let mut assembled = String::new();
        let mut offset = Some(0);
        while let Some(next) = offset {
            let chunk = read(skill, Some(next));
            assert_eq!(chunk["sha256"], first["sha256"]);
            assert_eq!(chunk["total_bytes"], large.len());
            assert_eq!(chunk["truncated"], true);
            assembled.push_str(chunk["content"].as_str().unwrap());
            offset = chunk["next_offset"].as_u64().map(|n| n as usize);
        }
        assert_eq!(assembled, large);
        fs::set_permissions(&absolute, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("skill-link");
        symlink(&absolute, &link).unwrap();
        assert_eq!(read("../skill-link", None)["sha256"], first["sha256"]);
        assert!(write("../skill-link", "wrong", Some("stale hash".into())).is_err());
        assert_eq!(fs::read_to_string(&absolute).unwrap(), large);
        write(
            link.to_str().unwrap(),
            "checked replacement",
            first["sha256"].as_str().map(str::to_owned),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&absolute).unwrap(), "checked replacement");
        write(absolute.to_str().unwrap(), "unchecked replacement", None).unwrap();
        assert_eq!(fs::read_to_string(&link).unwrap(), "unchecked replacement");
        assert_eq!(fs::metadata(&absolute).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(fs::symlink_metadata(link).unwrap().is_symlink());
    }
}
