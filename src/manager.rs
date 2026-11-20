//! Restricted management actor. Native CLI authority stays in the worker.
use crate::{
    App, db,
    db::Project,
    manager_loop::{self, Failure, LoopAction, ProviderConfig, ToolCall},
    work,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Component, Path},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

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
pub fn migrate(c: &Connection, recover: bool) -> Result<()> {
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
    if recover {
        c.execute("UPDATE manager_effects SET status='uncertain',result=? WHERE status='submitting'",[json!({"status":"uncertain","message":"The host stopped during this effect; do not repeat it without inspecting evidence."}).to_string()])?;
    }
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
            ({PROVIDER_CONFIGURED}) FROM manager_provider_config g WHERE g.id=1"),
        [],
        |row| Ok(json!({
            "base_url":row.get::<_,String>(0)?,"model":row.get::<_,String>(1)?,
            "has_api_key":row.get::<_,bool>(2)?,"supports_images":row.get::<_,bool>(3)?,
            "revision":row.get::<_,i64>(4)?,"migration_conflict":row.get::<_,bool>(5)?,
            "configured":row.get::<_,bool>(6)?
        })),
    )?)
}
pub fn public(c: &Connection, project: &str) -> Result<Value> {
    let mut public = public_provider(c)?;
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
        api_key: if key.is_empty() { "configuration-placeholder".into() } else { key.clone() },
    };
    config.validate()?;
    let supports_images = input.supports_images.unwrap_or_else(|| {
        current["base_url"] == input.base_url
            && current["model"] == input.model
            && current["supports_images"] == true
    });
    tx.execute("UPDATE manager_provider_config SET base_url=?,model=?,api_key=?,supports_images=?,
        revision=revision+1,updated_at=?,migration_conflict=0 WHERE id=1",
        params![input.base_url,input.model,key,supports_images,db::now()])?;
    tx.execute("UPDATE manager_config SET revision=revision+1,input_revision=input_revision+1,error='',next_attempt_at=0", [])?;
    tx.execute("UPDATE manager_runs SET status='superseded' WHERE status='working'", [])?;
    let result = public_provider(&tx)?;
    tx.commit()?;
    Ok(result)
}
pub fn set_mode(c: &Connection, project: &str, input: ModeInput) -> Result<Value> {
    if input.mode == "native" {
        bail!("Single-agent mode has been removed; all projects use manager/worker mode.");
    }
    ensure!(input.mode == "manager", "Invalid Agent mode");
    public(c, project)
}
pub fn migrate_queued(c: &Connection) -> Result<()> {
    let events = c.prepare("SELECT id FROM work_events WHERE status='queued' AND source!='management' ORDER BY rowid")?
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
    if source == "management" {
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
    if c.execute("INSERT OR IGNORE INTO manager_inputs(id,project_id,origin_event_id,kind,payload,created_at) VALUES(?,?,?,?,?,?)",params![id,project,origin,kind,payload.to_string(),db::now()])?==1 {
 c.execute("UPDATE manager_config SET input_revision=input_revision+CASE WHEN ?='user' THEN 1 ELSE 0 END,next_attempt_at=CASE WHEN ?='user' THEN 0 ELSE next_attempt_at END WHERE project_id=?",params![kind,kind,project])?; }
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
    let matches:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM manager_config WHERE project_id=? AND revision=? AND input_revision=?)",params![ctx.project,ctx.revision,ctx.input_revision],|r|r.get(0))?;
    ensure!(
        matches && !p.paused,
        "Management input or policy changed; effect cancelled"
    );
    ensure!(
        !work::has_takeover(c, &p.id, &p.generation)?,
        "Native input is under explicit user control"
    );
    let owned:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM manager_runs WHERE id=? AND project_id=? AND owner=? AND status='working')",params![ctx.run,ctx.project,owner()],|r|r.get(0))?;
    ensure!(owned, "Management turn ownership changed");
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
    ensure!(
        !instructions.trim().is_empty() && instructions.len() <= 1048576,
        "Worker instructions must contain 1 to 1048576 bytes"
    );
    if let Some(previous) = c
        .query_row(
            "SELECT id FROM work_events WHERE project_id=? AND source='management' AND source_id=?",
            params![p.id, id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(previous);
    }
    let task = db::id();
    let event = db::id();
    let transition = if steer {
        None
    } else {
        crate::cli_switch::latest(c, &p.id)?.filter(crate::cli_switch::blocks_worker)
    };
    let mut payload = json!({"instructions":instructions,"origin_event_id":origin,"manager_effect_id":id,"steer":steer,"expected_agent":transition.as_ref().map(|r|r.target.as_str()).unwrap_or(&p.agent),"expected_native":if transition.is_some(){""}else{&p.native_session},"target_generation":if transition.is_none()&&p.terminal.is_some(){Some(&p.generation)}else{None},"transition_id":transition.as_ref().map(|r|&r.id)});
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
    c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at) VALUES(?,?,?,'management',?,?,'queued',?,?)",params![event,p.id,task,id,payload.to_string(),db::now(),db::now()])?;
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
        return Ok(!installed(c)?);
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
    ensure!(event.source == "management", "Only manager delegations may reach the coding worker");
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
        } else if let Some(id) = original.message_id {
            let message = app.db.message(&id).await?;
            context = crate::envelope::render_resources(
                &message,
                &Value::Null,
                message.attachment["resources"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
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
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    expected_sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    instructions: String,
    source_event_id: Option<String>,
    target_cli: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerStatusArgs {
    input_id: Option<String>,
    #[serde(default)]
    after_effect: i64,
    #[serde(default)]
    detail_offset: usize,
}
/// A bounded read path for the exact input owned by this active batch. Historical
/// joins identify its effect ancestry, but never grant authority on their own.
fn input_effect_journal(
    c: &Connection,
    ctx: &Context,
    args: WorkerStatusArgs,
    current_effect: &str,
) -> Result<Value> {
    valid(c, ctx)?;
    let Some(input) = args.input_id else {
        ensure!(
            args.after_effect == 0 && args.detail_offset == 0,
            "Choose an owned input_id for effect journal paging"
        );
        return Ok(Value::Null);
    };
    ensure!(
        args.after_effect >= 0 && args.detail_offset <= 262144,
        "Effect journal cursor exceeds its bound"
    );
    let owned:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM manager_inputs i JOIN manager_input_claims own ON own.input_id=i.id WHERE i.id=? AND i.project_id=? AND i.status='processing' AND own.run_id=?)",params![input,ctx.project,ctx.run],|r|r.get(0))?;
    ensure!(
        owned,
        "Effect evidence requires an input currently owned by this batch"
    );
    let records=c.prepare("SELECT DISTINCT e.rowid,e.id,e.run_id,e.name,e.status,e.arguments,e.result FROM manager_effects e JOIN manager_run_inputs ancestry ON ancestry.run_id=e.run_id WHERE ancestry.input_id=? AND e.rowid>? AND e.id!=? ORDER BY e.rowid LIMIT 9")?.query_map(params![input,args.after_effect,current_effect],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let more = records.len() > 8;
    let mut cursor = args.after_effect;
    let mut effects = Vec::new();
    for (row, id, run, name, status, arguments, result) in records.into_iter().take(8) {
        cursor = row;
        let argument_chars = arguments.chars().count();
        let result_chars = result.chars().count();
        effects.push(json!({"effect_cursor":row,"effect_id":id,"manager_run_id":run,"tool":name,"status":status,"detail_offset":args.detail_offset,"arguments_chunk":arguments.chars().skip(args.detail_offset).take(2000).collect::<String>(),"result_chunk":result.chars().skip(args.detail_offset).take(2000).collect::<String>(),"arguments_chars":argument_chars,"result_chars":result_chars,"more_details":argument_chars>args.detail_offset+2000 || result_chars>args.detail_offset+2000}));
    }
    Ok(
        json!({"input_id":input,"effects":effects,"next_effect_cursor":if more{Some(cursor)}else{None},"instructions":"These are recorded effects, not new requests. Never replay confirmed or uncertain actions. Continue with after_effect for older/later entries; for long arguments/results repeat the preceding cursor with detail_offset advanced by 2000. Delivery or idle is not semantic task success."}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    to: String,
    text: String,
    reply_to_message_id: Option<String>,
    intent: Option<MessageIntent>,
    source_event_id: Option<String>,
}
#[derive(Copy, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum MessageIntent {
    Receipt,
    Result,
    Decision,
    Progress,
}
impl MessageIntent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Receipt => "receipt",
            Self::Result => "result",
            Self::Decision => "decision",
            Self::Progress => "progress",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendFileArgs {
    to: String,
    path: String,
    caption: Option<String>,
    reply_to_message_id: Option<String>,
    source_event_id: Option<String>,
    intent: Option<MessageIntent>,
}
fn file_parent(root: &Path, path: &str) -> Result<(fs::File, std::ffi::CString)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    ensure!(
        !path.is_empty() && !Path::new(path).is_absolute(),
        "Use a relative workspace path"
    );
    let parts = Path::new(path)
        .components()
        .map(|part| match part {
            Component::Normal(v) => Ok(v.to_owned()),
            _ => bail!("Workspace traversal is not allowed"),
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        parts.iter().all(|v| {
            let name = v.to_string_lossy().to_ascii_lowercase();
            !matches!(
                name.as_str(),
                ".dauerbot" | ".codex" | ".claude" | ".gemini" | ".pi" | ".ssh" | ".git"
            ) && name != ".env"
                && !name.starts_with(".env.")
        }),
        "Private state and credential paths are unavailable"
    );
    let mut dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    for part in &parts[..parts.len() - 1] {
        let part = std::ffi::CString::new(part.as_encoded_bytes())?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(
            fd >= 0,
            "Workspace directory is missing or not a regular directory"
        );
        dir = unsafe { fs::File::from_raw_fd(fd) };
    }
    Ok((
        dir,
        std::ffi::CString::new(parts.last().unwrap().as_encoded_bytes())?,
    ))
}
fn read_at(dir: &fs::File, name: &std::ffi::CStr) -> Result<Vec<u8>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "Workspace file is unavailable");
    let file = unsafe { fs::File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= 262144,
        "Read a regular file up to 256 KiB"
    );
    let mut bytes = Vec::new();
    file.take(262145).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 262144, "File grew beyond the read bound");
    Ok(bytes)
}
fn read_file(root: &Path, path: &str) -> Result<Value> {
    let (dir, name) = file_parent(root, path)?;
    let bytes = read_at(&dir, &name)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let content = String::from_utf8(bytes).context("Read UTF-8 files only")?;
    let result = json!({"path":path,"content":content,"sha256":hash});
    ensure!(
        result.to_string().len() <= 240000,
        "File content exceeds the management tool response bound; delegate larger files to the coding worker"
    );
    Ok(result)
}
fn write_file(root: &Path, args: WriteArgs) -> Result<Value> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = 0o600;
    ensure!(
        args.content.len() <= 262144,
        "Writes are limited to 256 KiB"
    );
    let (dir, name) = file_parent(root, &args.path)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        let file = unsafe { fs::File::from_raw_fd(fd) };
        ensure!(file.metadata()?.is_file(), "Write regular files only");
        permissions = file.metadata()?.permissions().mode() & 0o777;
        drop(file);
        let old = read_at(&dir, &name)?;
        ensure!(
            args.expected_sha256.as_deref() == Some(format!("{:x}", Sha256::digest(&old)).as_str()),
            "File changed; read it and supply expected_sha256 before replacing"
        );
    } else {
        ensure!(
            std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
                && args.expected_sha256.is_none(),
            "File is unavailable or expected content disappeared"
        );
    }
    let temporary = std::ffi::CString::new(format!(".dauerbot-write-{}", db::id()))?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(fd >= 0, "Cannot prepare workspace write");
    let result = (|| -> Result<()> {
        let mut file = unsafe { fs::File::from_raw_fd(fd) };
        file.write_all(args.content.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(permissions))?;
        file.sync_all()?;
        ensure!(
            unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    temporary.as_ptr(),
                    dir.as_raw_fd(),
                    name.as_ptr(),
                )
            } == 0,
            "Cannot publish workspace write"
        );
        Ok(())
    })();
    if result.is_err() {
        unsafe { libc::unlinkat(dir.as_raw_fd(), temporary.as_ptr(), 0) };
    }
    result?;
    Ok(
        json!({"path":args.path,"sha256":format!("{:x}",Sha256::digest(args.content.as_bytes())),"status":"written"}),
    )
}
fn select_origin(c: &Connection, ctx: &Context, requested: Option<&str>) -> Result<Option<String>> {
    let origins=c.prepare("SELECT DISTINCT i.origin_event_id FROM manager_inputs i JOIN manager_run_inputs b ON b.input_id=i.id WHERE b.run_id=? AND i.origin_event_id IS NOT NULL")?.query_map([&ctx.run],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(requested) = requested {
        ensure!(
            origins.iter().any(|id| id == requested),
            "Choose source_event_id from this turn's trusted_sources"
        );
        return Ok(Some(requested.into()));
    }
    ensure!(
        origins.len() <= 1,
        "Multiple discussions are present; choose source_event_id from trusted_sources"
    );
    Ok(origins.into_iter().next())
}
fn effect_result(c: &Connection, id: &str, result: &Value, status: &str) -> Result<()> {
    c.execute(
        "UPDATE manager_effects SET status=?,result=? WHERE id=?",
        params![status, result.to_string(), id],
    )?;
    Ok(())
}
/// A receipt belongs to an explicit user source, not a transient model turn.
/// Reuse its immutable outbox entry; a new model occurrence is not a new send.
fn enqueue_receipt(
    c: &Connection,
    ctx: &Context,
    effect: &str,
    args: &MessageArgs,
) -> Result<(String, bool)> {
    valid(c, ctx)?;
    let origin = select_origin(c, ctx, args.source_event_id.as_deref())?
        .context("A receipt requires a trusted user source")?;
    let request = format!("manager-receipt-{}-{origin}", ctx.project);
    if let Some(id) = c
        .query_row(
            "SELECT id FROM messages WHERE project_id=? AND direction='out' AND source_id=?",
            params![ctx.project, request],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok((id, false));
    }
    let user_source: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM manager_inputs i JOIN manager_run_inputs b ON b.input_id=i.id WHERE b.run_id=? AND i.origin_event_id=? AND i.kind='user' AND i.status='processing' AND EXISTS(SELECT 1 FROM manager_input_claims own WHERE own.input_id=i.id AND own.run_id=b.run_id))",
        params![ctx.run, origin],
        |r| r.get(0),
    )?;
    ensure!(user_source, "Only new user input can receive a receipt");
    let (source, raw): (String, String) = c.query_row(
        "SELECT m.source_id,m.attachment FROM work_events e JOIN messages m ON m.id=e.message_id WHERE e.id=? AND e.project_id=? AND m.direction='in'",
        params![origin, ctx.project],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let attachment: Value = serde_json::from_str(&raw)?;
    let binding = crate::bot::get(c, &ctx.project)?.context("Connect this project's bot first")?;
    ensure!(
        attachment["bot_app_id"] == binding.app_id,
        "A receipt requires this project's trusted Feishu source"
    );
    let source_meta = &attachment["source"];
    ensure!(
        source_meta["chat_id"].as_str() == Some(args.to.as_str())
            || (source_meta["chat_type"] == "p2p"
                && source_meta["sender"]["open_id"].as_str() == Some(args.to.as_str())),
        "A receipt must address its original user conversation"
    );
    let anchor = args
        .reply_to_message_id
        .as_deref()
        .context("A receipt requires an explicit trusted discussion anchor")?;
    ensure!(
        anchor == source
            || source_meta["root_id"].as_str() == Some(anchor)
            || source_meta["parent_id"].as_str() == Some(anchor),
        "A receipt must use its original trusted discussion"
    );
    let id = db::enqueue_out_request(
        c,
        &ctx.project,
        &binding.app_id,
        &request,
        &args.text,
        json!({"to":args.to,"reply_to_message_id":anchor,"manager_effect":effect,
            "manager_receipt_origin":origin,"manager_revision":ctx.revision}),
    )?;
    Ok((id, true))
}
pub fn validate_outbox(c: &Connection, project: &str, attachment: &Value) -> Result<()> {
    if let Some(revision) = attachment["manager_provider_revision"].as_i64() {
        let allowed: bool = c.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM manager_provider_config g JOIN projects p ON p.id=?
                WHERE g.revision=? AND p.paused=0 AND NOT ({PROVIDER_CONFIGURED}))"),
            params![project, revision], |row| row.get(0),
        )?;
        let p = db::project_by_key(c, project)?;
        ensure!(allowed && !work::has_takeover(c, project, &p.generation)?,
            "The manager configuration notice is no longer active");
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
        let p = db::project_by_key(c, project)?;
        ensure!(
            !work::has_takeover(c, project, &p.generation)?,
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
        let p = db::project_by_key(c, project)?;
        ensure!(
            !work::has_takeover(c, project, &p.generation)?,
            "Management notice is held by user takeover"
        );
    }
    let Some(effect) = attachment["manager_effect"].as_str() else {
        return Ok(());
    };
    let allowed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id JOIN manager_config m ON m.project_id=r.project_id JOIN projects p ON p.id=m.project_id WHERE e.id=? AND r.project_id=? AND r.status IN ('working','complete') AND r.revision=m.revision AND r.input_revision=m.input_revision AND p.paused=0)",params![effect,project],|r|r.get(0))?;
    ensure!(allowed, "Management reply was superseded or stopped");
    let p = db::project_by_key(c, project)?;
    ensure!(
        !work::has_takeover(c, project, &p.generation)?,
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

fn proposal_tool_allowed(name: &str, arguments: &Value) -> bool {
    match name {
        "worker_status" => true,
        "delegate" => arguments["target_cli"].is_null(),
        "send_md" => !matches!(arguments["intent"].as_str(), Some("receipt" | "progress")),
        _ => false,
    }
}

async fn tool(app: &Arc<App>, ctx: &Context, call: ToolCall) -> Result<Value> {
    let Some(_permit) = app.service.gate.enter() else {
        bail!("Service ownership changed")
    };
    let lock = app.project_lock(&ctx.project).await;
    let guard = lock.lock().await;
    let context = ctx.clone();
    let occurrence = call.occurrence.clone();
    let name = call.name.clone();
    let arguments = call.arguments.to_string();
    let approved_proposal = app.db.call({
        let run = ctx.run.clone();
        move |c| proposal_run(c, &run)
    }).await?;
    if approved_proposal && !proposal_tool_allowed(&call.name, &call.arguments) {
        return Ok(json!({"error":"Approved proposals can only delegate to the current worker and update the approved discussion"}));
    }
    let admitted=app.db.call(move|c|{
  let tx=c.unchecked_transaction()?;valid(&tx,&context)?;
  if let Some((id,old_name,old_arguments,status,result))=tx.query_row("SELECT id,name,arguments,status,result FROM manager_effects WHERE run_id=? AND occurrence=?",params![context.run,occurrence],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?))).optional()? {
   ensure!(old_name==name&&old_arguments==arguments,"Tool occurrence changed content");let value=if status=="submitting"{json!({"status":"uncertain","message":"A previous attempt may have committed; inspect existing evidence before any new effect."})}else{serde_json::from_str(&result)?};return Ok((id,Some(value)));
  }
  let id=format!("manager-{}",db::id());tx.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES(?,?,?,?,?,'submitting')",params![id,context.run,occurrence,name,arguments])?;tx.commit()?;Ok((id,None))
 }).await?;
    let (effect, previous) = admitted;
    if let Some(previous) = previous {
        return Ok(previous);
    }
    let p = app.db.project(&ctx.project).await?;
    let result:Result<Value>=async {
  match call.name.as_str() {
   "read_file"=>{let a:FileArgs=serde_json::from_value(call.arguments)?;read_file(Path::new(&p.path),&a.path)},
   "write_file"=>{let a:WriteArgs=serde_json::from_value(call.arguments)?;write_file(Path::new(&p.path),a)},
   "delegate"|"steer_worker"=>{
    let a:DelegateArgs=serde_json::from_value(call.arguments)?;let steer=call.name=="steer_worker";
    let context=ctx.clone();let request=effect.clone();app.db.call(move|c|{
      let tx=c.unchecked_transaction()?;let p=valid(&tx,&context)?;
      let origin=select_origin(&tx,&context,a.source_event_id.as_deref())?;
      let proposal = origin.as_deref().map(|id| work::event(&tx, &p.id, id)).transpose()?
        .filter(|event| event.source == "autonomy");
      let instructions = if let Some(proposal) = &proposal {
          ensure!(!steer && a.target_cli.is_none(), "Approved proposal must use the current worker");
          if let Some(existing) = tx.query_row(
              "SELECT id FROM work_events WHERE project_id=? AND source='management'
               AND json_extract(payload,'$.origin_event_id')=? ORDER BY rowid LIMIT 1",
              params![p.id,proposal.id], |row| row.get::<_,String>(0)
          ).optional()? {
              let event = work::event(&tx, &p.id, &existing)?;
              return Ok(json!({"event_id":existing,"status":event.status,"proposal_id":proposal.payload["proposal_id"]}));
          }
          proposal.payload["instructions"].as_str().context("Approved proposal instructions are missing")?
      } else {
          &a.instructions
      };
      if let Some(target) = &a.target_cli { crate::cli_switch::request(&tx,&p,target)?; }
      let held=crate::cli_switch::latest(&tx,&p.id)?.as_ref().is_some_and(crate::cli_switch::blocks_worker);
      // An explicit target participates in the ordinary cooperative handoff.
      // Its instructions remain held for that successor, never sent to the old CLI.
      let steer_current = steer && !(a.target_cli.is_some() && held);
      ensure!(!steer_current||(p.terminal.is_some()&&p.ready),"Worker is not ready; delegate to wake it");
      ensure!(!steer_current||!held,"Worker steering is held by a CLI transition");
      let id=intent(&tx,&p,&request,instructions,origin.as_deref(),steer_current)?;
      let event=work::event(&tx,&p.id,&id)?;tx.commit()?;
      Ok(json!({"effect_id":request,"event_id":id,"status":if held{"held"}else{"queued"},"worker_agent":event.payload["expected_agent"],"generation":event.payload["target_generation"],"transition_id":event.payload["transition_id"]}))
    }).await
   },
   "worker_status"=>{drop(guard);let args:WorkerStatusArgs=serde_json::from_value(call.arguments)?;let context=ctx.clone();let own_effect=effect.clone();let journal=app.db.call(move|c|input_effect_journal(c,&context,args,&own_effect)).await?;let obsolete=crate::manager_close::inspect(app,&p).await?;let authority=ctx.clone();let mut snapshot=app.db.call(move|c|{let current=valid(c,&authority)?;worker_snapshot(c,&current.id,&current.generation)}).await?;snapshot["agent"]=json!(p.agent);snapshot["generation"]=json!(p.generation);snapshot["native_session"]=json!(p.native_session);snapshot["ready"]=json!(p.ready);snapshot["paused"]=json!(p.paused);snapshot["mcp_registered"]=json!(p.execution["mcp_registered"]==true);snapshot["obsolete_executions"]=json!(obsolete);snapshot["input_effect_journal"]=journal;Ok(snapshot)},
   "cli_send_keys"=>{drop(guard);let args:crate::manager_terminal::SendArgs=serde_json::from_value(call.arguments)?;crate::manager_terminal::send(app,ctx,args).await},
   "cli_view"=>{drop(guard);let args:crate::manager_terminal::ViewArgs=serde_json::from_value(call.arguments)?;crate::manager_terminal::view(app,ctx,args).await},
   "close_agent"=>{let args:crate::manager_close::CloseArgs=serde_json::from_value(call.arguments)?;crate::manager_close::close(app,&p,ctx,&effect,args).await},
   "send_md"|"send_file"=>{
    let (to,text,anchor,path,intent,source)=if call.name=="send_md" {
        let a:MessageArgs=serde_json::from_value(call.arguments)?;
        (a.to,a.text,a.reply_to_message_id,None,a.intent,a.source_event_id)
    }else{
        let a:SendFileArgs=serde_json::from_value(call.arguments)?;
        let path=crate::feishu::allowed_file(&p,Path::new(&a.path),&app.dir)?;
        let root=fs::canonicalize(&p.path)?;
        if let Ok(relative)=path.strip_prefix(&root) {
            file_parent(&root,&relative.to_string_lossy())?;
        } else {
            ensure!(path.starts_with(app.dir.join("files")),"Only workspace files and downloaded attachments are available");
        }
        ensure!(a.intent.is_none() || a.intent == Some(MessageIntent::Result),"Use result for an independent file");
        (a.to,a.caption.unwrap_or_default(),a.reply_to_message_id,Some(path),a.intent,a.source_event_id)
    };
    crate::feishu::recipient_type(&to)?;
    ensure!(!text.trim().is_empty()||path.is_some(),"Message content is empty");
    ensure!(text.len()<=131072,"Message is too large");
    let context=ctx.clone();let request=effect.clone();
    let (message,dispatch,progress)=app.db.call(move|c|{
        let tx=c.unchecked_transaction()?;
        let p=valid(&tx,&context)?;
        if intent==Some(MessageIntent::Receipt) {
            let args=MessageArgs{to,text,reply_to_message_id:anchor,intent,source_event_id:source};
            let (message, dispatch)=enqueue_receipt(&tx,&context,&request,&args)?;
            tx.commit()?;return Ok((message,dispatch,None));
        }
        let binding=crate::bot::get(&tx,&p.id)?.context("Connect this project's bot first")?;
        let mut attachment=json!({"to":to,"manager_effect":request,"reply_to_message_id":anchor});
        if let Some(intent)=intent {
            attachment["discussion_intent"]=json!(intent.as_str());
        }
        if (approved_proposal || anchor.is_none() || source.is_some() || matches!(intent, Some(MessageIntent::Decision | MessageIntent::Progress)))
            && let Some(event)=select_origin(&tx,&context,source.as_deref())?
        {
            crate::discussion::origin(&tx,&p.id,&event,&binding.app_id,&to,true,&mut attachment)?;
        }
        if !attachment["reply_to_message_id"].is_string() {
            ensure!(matches!(intent, Some(MessageIntent::Result | MessageIntent::Decision)) && attachment["discussion_scope"].is_string(),
                "Choose a trusted source discussion or a specific work occurrence for this requested result or decision");
        }
        if intent==Some(MessageIntent::Progress) {
            tx.commit()?;
            return Ok((String::new(),false,Some((binding.app_id,text,attachment))));
        }
        if let Some(path)=path {attachment["path"]=json!(path);}
        let id=db::enqueue_out_request(&tx,&p.id,&binding.app_id,&request,&text,attachment)?;
        tx.commit()?;Ok((id,true,None))
    }).await?;
    // The journal is committed before remote IO. Do not hold the project lock
    // across Feishu: fresh input and deterministic controls remain immediate.
    drop(guard);
    if let Some((app_id,text,attachment))=progress {
        return crate::discussion::progress(app,p.id,app_id,effect.clone(),text,attachment).await;
    }
    let send_error=if dispatch {app.send_out(&message).await.err().map(|e|e.to_string())}else{None};let m=app.db.message(&message).await?;Ok(json!({"message_id":m.attachment["sent_message_id"],"status":m.status,"outbox_id":m.id,"error":m.error,"dispatch_error":send_error}))
   },
   _=>bail!("This management tool is not available"),
  }
 }.await;
    // Result persistence belongs to the admitted effect even if steering arrived
    // meanwhile. Only the actor checkpoint has the stricter current-revision CAS.
    let value = match result {
        Ok(value) => value,
        Err(error) => json!({"error":error.to_string()}),
    };
    let id = effect.clone();
    let saved = value.clone();
    app.db
        .call(move |c| {
            let status = if saved["status"] == "uncertain" {
                "uncertain"
            } else {
                "complete"
            };
            effect_result(c, &id, &saved, status)
        })
        .await?;
    app.notify.notify_one();
    Ok(value)
}
fn worker_snapshot(c: &Connection, project: &str, generation: &str) -> Result<Value> {
    let transition = crate::cli_switch::latest(c, project)?;
    let delegations = c.prepare("SELECT e.id,e.status,r.status,e.error,json_extract(e.payload,'$.transition_id'),json_extract(e.payload,'$.expected_agent') FROM work_events e LEFT JOIN work_runs r ON r.event_id=e.id WHERE e.project_id=? AND e.source='management' ORDER BY e.rowid DESC LIMIT 24")?
        .query_map([project],|r| {
            let status: String = r.get(1)?;
            let held = status == "queued" && transition.as_ref().is_some_and(|request| request.holds_input || request.phase == "recovery_exhausted");
            Ok(json!({"event_id":r.get::<_,String>(0)?,"event_status":status,"held":held,"run_status":r.get::<_,Option<String>>(2)?,"error":r.get::<_,String>(3)?,"transition_id":r.get::<_,Option<String>>(4)?,"expected_agent":r.get::<_,Option<String>>(5)?}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let work = work::snapshot(c, project)?;
    Ok(json!({
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
    if installed(c)? {
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
    if !installed(c)? {
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
    let rows=c.prepare("SELECT id,status,content,attachment FROM messages WHERE project_id=? AND direction='out' AND status IN ('sent','failed','uncertain') AND json_extract(attachment,'$.manager_effect') IS NULL AND json_extract(attachment,'$.manager_notice') IS NULL AND json_extract(attachment,'$.autonomy_publication_revision') IS NULL AND created_at >= COALESCE((SELECT MIN(created_at) FROM work_events WHERE project_id=? AND source='management'),9223372036854775807) ORDER BY rowid DESC LIMIT 32")?.query_map(params![project,project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, status, content, raw) in rows {
        let a: Value = serde_json::from_str(&raw)?;
        let anchor = a["resolved_reply_to_message_id"].as_str().or_else(|| a["reply_to_message_id"].as_str());
        let origin=anchor.map(|anchor|c.query_row("SELECT e.id FROM work_events e JOIN messages m ON m.id=e.message_id WHERE e.project_id=? AND m.direction='in' AND m.source_id=?",params![project,anchor],|r|r.get::<_,String>(0)).optional()).transpose()?.flatten();
        let scoped_origin = a["discussion_scope"].as_str().map(|scope|
            c.query_row("SELECT id FROM work_events WHERE project_id=? AND id=?",
                params![project,scope], |row| row.get::<_,String>(0)).optional()
        ).transpose()?.flatten();
        let origin = origin.or(scoped_origin);
        observation(
            c,
            project,
            "worker_message",
            &id,
            &status,
            origin.as_deref(),
            &json!({"kind":"worker_message","status":status,"already_delivered_to_user":status=="sent","content":content.chars().take(16000).collect::<String>(),"to":a["to"],"reply_to_message_id":anchor,"sent_message_id":a["sent_message_id"],"sent_chat_id":a["sent_chat_id"]}),
        )?;
    }
    let intents=c.prepare("SELECT id,error,json_extract(payload,'$.origin_event_id') FROM work_events WHERE project_id=? AND source='management' AND status='failed' ORDER BY rowid DESC LIMIT 32")?.query_map([project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
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
    let runs=c.prepare("SELECT r.id,r.status,r.error,json_extract(e.payload,'$.origin_event_id') FROM work_runs r JOIN work_events e ON e.id=r.event_id WHERE r.project_id=? AND e.source='management' AND r.status IN ('settled','completed','uncertain','failed','interrupted') ORDER BY r.rowid DESC LIMIT 32")?.query_map([project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, status, error, origin) in runs {
        observation(
            c,
            project,
            "worker_run",
            &id,
            &status,
            origin.as_deref(),
            &json!({"kind":"worker_run","run_id":id,"delivery_status":status,"error":error,"semantic_task_success":"unconfirmed"}),
        )?;
    }
    Ok(())
}
fn observation(
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
struct Turn {
    context: Context,
    config: ProviderConfig,
    checkpoint: Value,
    debug_dispatch: Option<DebugDispatch>,
}
struct DebugDispatch {
    bot_app_id: String,
    to: String,
    anchor: String,
    payload: String,
}

fn debug_dispatch(
    c: &Connection,
    project: &str,
    origins: impl Iterator<Item = impl AsRef<str>>,
    payload: &str,
) -> Result<Option<DebugDispatch>> {
    let mut source = c.prepare(
        "SELECT b.app_id, json_extract(m.attachment,'$.source.chat_id'), m.source_id
         FROM work_events e JOIN messages m ON m.id=e.message_id
         JOIN project_bots b ON b.project_id=e.project_id
         WHERE e.id=? AND e.project_id=? AND e.source='feishu' AND m.direction='in'
           AND json_extract(m.attachment,'$.bot_app_id')=b.app_id
           AND json_extract(m.attachment,'$.source.chat_id') GLOB 'oc_*'
           AND m.source_id GLOB 'om_*'",
    )?;
    for origin in origins {
        let target = source
            .query_row(params![origin.as_ref(), project], |row| {
                Ok(DebugDispatch {
                    bot_app_id: row.get(0)?,
                    to: row.get(1)?,
                    anchor: row.get(2)?,
                    payload: payload.to_owned(),
                })
            })
            .optional()?;
        if target.is_some() {
            return Ok(target);
        }
    }
    Ok(None)
}
fn trusted_source(c: &Connection, project: &str, id: &str) -> Result<Value> {
    let e = work::event(c, project, id)?;
    if e.source == "autonomy" {
        return Ok(json!({"event_id":id,"source_kind":"autonomy","proposal_id":e.payload["proposal_id"],
            "message_id":e.payload["anchor"],"to":e.payload["to"]}));
    }
    if let Some(message) = e.message_id {
        let raw: String = c.query_row(
            "SELECT attachment FROM messages WHERE id=?",
            [message],
            |r| r.get(0),
        )?;
        let a: Value = serde_json::from_str(&raw)?;
        Ok(if e.source == "feishu" {
            json!({"event_id":id,"source_kind":"feishu","message_id":e.source_id,"source":a["source"]})
        } else {
            json!({"event_id":id,"source_kind":e.source,"local_source_id":e.source_id})
        })
    } else {
        Ok(json!({"event_id":id,"source_kind":e.source,"to":e.payload["to"]}))
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
            "Only a bounded preview is shown. The original input is retained intact. Delegate with this source_event_id to inspect the full original before treating its request as complete."
        );
    }
    if value.to_string().len() > 120000 {
        value = json!({"source_event_id":event_id,"full_input_retained":true,"continuation":"The full original input and attachments are retained intact. Delegate using this source_event_id to inspect them before treating the request as complete."});
    }
    Ok(value)
}
/// A durable claim survives process loss; the historical run/input join is not ownership.
fn own_input(c: &Connection, run: &str, input: &str) -> Result<()> {
    c.execute("INSERT INTO manager_input_claims(input_id,run_id) VALUES(?,?) ON CONFLICT(input_id) DO UPDATE SET run_id=excluded.run_id", params![input, run])?;
    ensure!(c.execute("UPDATE manager_inputs SET status='processing' WHERE id=? AND status IN ('queued','processing')", [input])? == 1, "Input is no longer claimable");
    Ok(())
}
fn adopt_legacy_claim(c: &Connection, project: &str) -> Result<()> {
    let run: Option<String> = c.query_row("SELECT id FROM manager_runs WHERE project_id=? AND status='working' AND checkpoint!='null' ORDER BY rowid DESC LIMIT 1", [project], |r|r.get(0)).optional()?;
    if let Some(run) = run {
        let ids = c.prepare("SELECT i.id FROM manager_inputs i JOIN manager_run_inputs b ON b.input_id=i.id WHERE b.run_id=? AND i.status='queued' AND NOT EXISTS(SELECT 1 FROM manager_input_claims own WHERE own.input_id=i.id) ORDER BY i.rowid")?.query_map([&run], |r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            own_input(c, &run, &id)?;
        }
    }
    Ok(())
}
fn continuation_input(raw: &str, origin: Option<&str>) -> Result<Value> {
    let value = prompt_input(raw, origin)?;
    if value.to_string().len() <= 2400 {
        return Ok(value);
    }
    let preview = value["user_input"]["body"]
        .as_str()
        .or_else(|| value["instructions"].as_str())
        .unwrap_or("")
        .chars()
        .take(500)
        .collect::<String>();
    Ok(
        json!({"source_event_id":origin,"full_input_retained":true,"preview":preview,"continuation":"Already owned input, not a new request. Its full original payload and attachments remain durable; inspect through the original trusted source before declaring completion. Do not repeat confirmed or uncertain effects."}),
    )
}
const PREPARED_INPUT: &str = "NOT EXISTS(SELECT 1 FROM work_events e JOIN messages m ON m.id=e.message_id WHERE e.id=manager_inputs.origin_event_id AND (json_extract(m.attachment,'$.file_key') IS NOT NULL OR json_array_length(m.attachment,'$.resources')>0) AND COALESCE(json_extract(m.attachment,'$.resources_ready'),0)!=1)";
fn image_source(
    c: &Connection,
    project: &str,
    origin: &str,
) -> Result<Option<(String, Value, String)>> {
    let row: Option<(String, String, String)> = c.query_row("SELECT m.id,m.source_id,m.attachment FROM work_events e JOIN messages m ON m.id=e.message_id WHERE e.id=? AND e.project_id=? AND m.project_id=? AND m.direction='in'", params![origin,project,project], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((id, source_id, raw)) = row else {
        return Ok(None);
    };
    let mut a: Value = serde_json::from_str(&raw)?;
    a["source_message_id"] = json!(source_id);
    let source = &a["source"];
    let discussion = source["root_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| source["thread_id"].as_str().filter(|v| !v.is_empty()))
        .unwrap_or(if source["chat_type"] == "group" {
            &source_id
        } else {
            "main"
        });
    let scope = json!([a["bot_app_id"], source["chat_id"], discussion]).to_string();
    Ok(Some((id, a, scope)))
}
fn prepare_images(
    c: &Connection,
    project: &str,
    origin: Option<&str>,
    scope: Option<&str>,
    supports_images: bool,
    used: usize,
    data: &mut Value,
) -> Result<Vec<crate::multimodal::ImageRef>> {
    let Some(origin) = origin else {
        return Ok(vec![]);
    };
    let Some((message, attachment, source_scope)) = image_source(c, project, origin)? else {
        return Ok(vec![]);
    };
    let mut resources = attachment["resources"]
        .as_array()
        .cloned()
        .unwrap_or_default();
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
                resource["cache_source_id"] == attachment["source_message_id"],
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
        let reason = if !supports_images {
            "image_capability_not_declared"
        } else if scope != Some(source_scope.as_str()) {
            "different_discussion_scope"
        } else if used + images.len() >= crate::multimodal::MAX_IMAGES {
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
            "image_part"
        };
        resource["image_delivery"] = json!({"mode":if reason == "image_part" {"image_part"}else{"path_only"},"reason":reason,"note":if reason == "image_part" {"Image bytes will be verified and supplied as image content to this configured model."}else{"The current model has not received image pixels. Inline input requires an explicit endpoint/model declaration, verified PNG/JPEG/GIF/WebP files up to 2 MiB, and at most 4 images. A local path is only a file reference; do not claim to have viewed the image. Delegate inspection when needed and report any inability."}});
        if reason == "image_part" {
            images.push(verified?);
        }
    }
    data["user_input"]["resources"] = json!(resources);
    Ok(images)
}
type InputRow = (String, Option<String>, String, String);

fn isolate_proposal_inputs(
    c: &Connection,
    project: &str,
    queued: &mut Vec<InputRow>,
    continuing: &mut Vec<InputRow>,
) -> Result<bool> {
    let mut proposal_source = c.prepare(
        "SELECT source='autonomy' FROM work_events WHERE project_id=? AND id=?",
    )?;
    let mut proposals = HashSet::new();
    for (_, origin, _, _) in continuing.iter().chain(queued.iter()) {
        if let Some(origin) = origin
            && proposal_source.query_row(params![project, origin], |r| r.get::<_,bool>(0))
                .optional()?.unwrap_or(false)
        {
            proposals.insert(origin.clone());
        }
    }
    if proposals.is_empty() {
        return Ok(false);
    }
    let ordinary = continuing.iter().chain(queued.iter())
        .any(|(_, origin, _, _)| origin.as_ref().is_none_or(|origin| !proposals.contains(origin)));
    let scope = (!ordinary).then(|| {
        continuing.iter().chain(queued.iter()).find_map(|(_, origin, _, _)| origin.clone())
    }).flatten();
    let include = |(_, origin, _, _): &InputRow| match &scope {
        Some(scope) => origin.as_ref() == Some(scope),
        None => origin.as_ref().is_none_or(|origin| !proposals.contains(origin)),
    };
    queued.retain(&include);
    continuing.retain(include);
    Ok(scope.is_some())
}

fn claim(c: &Connection, project: &str) -> Result<Option<Turn>> {
    let tx = c.unchecked_transaction()?;
    let p = db::project_by_key(&tx, project)?;
    let config:Option<(String,String,String,i64,i64,bool)>=tx.query_row(&format!("SELECT g.base_url,g.model,g.api_key,m.revision,m.input_revision,g.supports_images FROM manager_config m CROSS JOIN manager_provider_config g WHERE m.project_id=? AND m.next_attempt_at<=? AND ({PROVIDER_CONFIGURED})"),params![project,db::now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let Some((base_url, model, api_key, revision, _, supports_images)) = config else {
        return Ok(None);
    };
    if p.paused || work::has_takeover(&tx, project, &p.generation)? {
        return Ok(None);
    }
    let input_revision: i64 = tx.query_row(
        "SELECT input_revision FROM manager_config WHERE project_id=?",
        [project],
        |r| r.get(0),
    )?;
    adopt_legacy_claim(&tx, project)?;
    let existing=tx.query_row("SELECT id,checkpoint,owner FROM manager_runs WHERE project_id=? AND revision=? AND input_revision=? AND status='working' AND checkpoint!='null' ORDER BY rowid DESC LIMIT 1",params![project,revision,input_revision],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?;
    let (run, checkpoint, debug_dispatch) = if let Some((run, raw, previous_owner)) = existing {
        if previous_owner != owner() {
            tx.execute("UPDATE manager_effects SET status='uncertain',result=? WHERE run_id=? AND status='submitting'",params![json!({"status":"uncertain","message":"The previous host stopped during this effect; inspect evidence, do not replay."}).to_string(),run])?;
            tx.execute(
                "UPDATE manager_runs SET owner=? WHERE id=?",
                params![owner(), run],
            )?;
        }
        (run, serde_json::from_str(&raw)?, None)
    } else {
        let mut candidates=tx.prepare(&format!("SELECT id,origin_event_id,kind,payload FROM manager_inputs WHERE project_id=? AND status='queued' AND {PREPARED_INPUT} ORDER BY rowid LIMIT 32"))?.query_map([project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut continuing=tx.prepare("SELECT i.id,i.origin_event_id,i.kind,i.payload FROM manager_inputs i JOIN manager_input_claims own ON own.input_id=i.id WHERE i.project_id=? AND i.status='processing' ORDER BY i.rowid LIMIT 32")?.query_map([project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let proposal_only = isolate_proposal_inputs(&tx, project, &mut candidates, &mut continuing)?;
        if candidates.is_empty() && continuing.is_empty() {
            tx.commit()?;
            return Ok(None);
        }
        let history=tx.prepare("SELECT e.name,e.status,e.result,e.id FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id WHERE r.project_id=? ORDER BY e.rowid DESC LIMIT 32")?.query_map([project],|r|Ok(json!({"effect_id":r.get::<_,String>(3)?,"tool":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"result_summary":r.get::<_,String>(2)?.chars().take(2000).collect::<String>()})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let prior=tx.prepare("SELECT kind,payload FROM manager_inputs WHERE project_id=? AND status='consumed' ORDER BY rowid DESC LIMIT 8")?.query_map([project],|r|Ok(json!({"kind":r.get::<_,String>(0)?,"data_summary":r.get::<_,String>(1)?.chars().take(3000).collect::<String>()})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut instructions = json!({});
        for path in ["AGENTS.md", "HANDOFF.md"] {
            if let Ok(file) = read_file(Path::new(&p.path), path) {
                instructions[path] = json!(
                    file["content"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(24000)
                        .collect::<String>()
                );
            }
        }
        let notes=tx.prepare("SELECT n.note FROM manager_turn_notes n JOIN manager_runs r ON r.id=n.run_id WHERE r.project_id=? ORDER BY r.rowid DESC LIMIT 8")?.query_map([project],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let latest=if proposal_only { None } else { tx.query_row(&format!("SELECT id,origin_event_id,kind,payload FROM manager_inputs WHERE project_id=? AND status='queued' AND kind='user' AND {PREPARED_INPUT} ORDER BY rowid DESC LIMIT 1"),[project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()? };
        // A new trusted user source selects the discussion; otherwise retain only
        // the selected, still-owned user objective, never consumed conversation history.
        let image_scope = latest
            .as_ref()
            .or_else(|| continuing.iter().find(|v| v.2 == "user"))
            .and_then(|v| v.1.as_deref())
            .map(|origin| image_source(&tx, project, origin))
            .transpose()?
            .flatten()
            .map(|(_, _, scope)| scope);
        let latest_context=if proposal_only { None } else { tx.query_row("SELECT id,origin_event_id,payload,status FROM manager_inputs WHERE project_id=? AND kind='user' AND status IN ('queued','processing','consumed') ORDER BY rowid DESC LIMIT 1",[project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()? };
        // User steering may interrupt an owned batch; observations never do. Reserve
        // one of the 32 NEW claim slots for that control input, without reordering
        // ordinary fresh batches or changing their persisted input order.
        let steering_priority = !continuing.is_empty() && latest.is_some();
        let mut ordered = candidates;
        if steering_priority && let Some(latest) = latest {
            ordered.retain(|item| item.0 != latest.0);
            ordered.truncate(31);
            ordered.insert(0, latest);
        }
        let mut inputs = Vec::new();
        let mut origins = HashSet::new();
        let mut current = Vec::new();
        let mut previous = Vec::new();
        let mut images = Vec::new();
        let mut trusted_sources = Vec::new();
        let latest_steering = match &latest_context {
            Some((id, origin, raw, status)) => {
                json!({"input_id":id,"source_event_id":origin,"history_only_not_reply_target":status=="consumed","already_consumed":status=="consumed","context_only_not_batch_member":true,"data":prompt_input(raw,origin.as_deref())?})
            }
            None => Value::Null,
        };
        let mut selected_continuing = Vec::new();
        for candidate in continuing {
            let (id, origin, kind, raw) = &candidate;
            let mut data = continuation_input(raw, origin.as_deref())?;
            let candidate_images = if kind == "user" {
                prepare_images(
                    &tx,
                    project,
                    origin.as_deref(),
                    image_scope.as_deref(),
                    supports_images,
                    images.len(),
                    &mut data,
                )?
            } else {
                vec![]
            };
            let item = json!({"input_id":id,"kind":kind,"journal_available":true,"journal_instructions":"Before any external action, call worker_status with this input_id to inspect its paginated original-run effect journal, including confirmed and uncertain effects. Do not treat this input as a new task.","data":data});
            let mut next = previous.clone();
            next.push(item.clone());
            let metadata = origin
                .as_ref()
                .filter(|id| !origins.contains(*id))
                .map(|id| trusted_source(&tx, project, id))
                .transpose()?;
            let mut sources = trusted_sources.clone();
            if let Some(ref metadata) = metadata {
                sources.push(metadata.clone());
            }
            // Reserve space for a genuine new steering control before selecting a
            // bounded continuation page. Unselected inputs keep their OLD owner.
            let mut reserved = Vec::new();
            if steering_priority && let Some((_, origin, kind, raw)) = ordered.first() {
                reserved.push(json!({"kind":kind,"data":prompt_input(raw,origin.as_deref())?}));
                if let Some(origin) = origin
                    && !origins.contains(origin)
                {
                    sources.push(trusted_source(&tx, project, origin)?);
                }
            }
            let required = json!({"trusted_project":{"id":p.id,"name":p.name,"workspace":p.path},"trusted_sources":sources,"new_inputs":reserved,"continuing_inputs":next,"latest_user_steering":latest_steering});
            if required.to_string().len() > 215000 {
                ensure!(
                    !selected_continuing.is_empty() || steering_priority,
                    "Trusted continuation metadata exceeds the management context bound"
                );
                break;
            }
            previous.push(item);
            images.extend(candidate_images);
            if let Some(metadata) = metadata {
                trusted_sources.push(metadata);
            }
            if let Some(origin) = origin {
                origins.insert(origin.clone());
            }
            selected_continuing.push(candidate);
        }
        let continuing = selected_continuing;
        for candidate in ordered {
            let (_, origin, kind, raw) = &candidate;
            let mut data = prompt_input(raw, origin.as_deref())?;
            let candidate_images = if kind == "user" {
                prepare_images(
                    &tx,
                    project,
                    origin.as_deref(),
                    image_scope.as_deref(),
                    supports_images,
                    images.len(),
                    &mut data,
                )?
            } else {
                vec![]
            };
            let item = json!({"kind":kind,"data":data});
            let metadata = origin
                .as_ref()
                .filter(|id| !origins.contains(*id))
                .map(|id| trusted_source(&tx, project, id))
                .transpose()?;
            let mut next = current.clone();
            next.push(item.clone());
            let mut sources = trusted_sources.clone();
            if let Some(ref metadata) = metadata {
                sources.push(metadata.clone());
            }
            let required = json!({"trusted_project":{"id":p.id,"name":p.name,"workspace":p.path},"trusted_sources":sources,"new_inputs":next,"continuing_inputs":previous,"latest_user_steering":latest_steering});
            if required.to_string().len() > 215000 && (!inputs.is_empty() || !continuing.is_empty())
            {
                break;
            }
            ensure!(
                required.to_string().len() <= 215000,
                "Trusted input metadata exceeds the management context bound"
            );
            current.push(item);
            images.extend(candidate_images);
            if let Some(metadata) = metadata {
                trusted_sources.push(metadata);
            }
            if let Some(origin) = origin {
                origins.insert(origin.clone());
            }
            inputs.push(candidate);
        }
        if steering_priority && current.len() > 1 {
            current.rotate_left(1);
        }
        let mut latest_steering = latest_steering;
        if let Some((id, _, _, _)) = &latest_context {
            let present = inputs.iter().any(|item| &item.0 == id);
            let owned = continuing.iter().any(|item| &item.0 == id);
            if let Some(object) = latest_steering.as_object_mut() {
                object.insert("present_in_new_inputs".into(), json!(present));
                object.insert("present_in_continuing_inputs".into(), json!(owned));
                object.insert(
                    "context_only_not_batch_member".into(),
                    json!(!present && !owned),
                );
            }
        }
        let origin = if origins.len() == 1 {
            origins.iter().next().cloned()
        } else {
            None
        };
        let mut context = json!({"trusted_project":{"id":p.id,"name":p.name,"workspace":p.path},"trusted_sources":trusted_sources,"new_inputs":current,"continuing_inputs":previous,"continuation_policy":"Continuing inputs are already claimed goals, not new messages. Continue only unfinished work, preserve confirmed/uncertain effect receipts, and follow latest_user_steering. A delivered/settled worker receipt is not semantic task success.","steering_priority":steering_priority,"latest_user_steering":latest_steering,"workspace_instructions":instructions,"prior_conversation":prior,"effect_evidence":history,"manager_decisions":notes});
        if proposal_only {
            for field in ["prior_conversation", "effect_evidence", "manager_decisions"] {
                context[field] = json!([]);
            }
        }
        if context.to_string().len() > 250000
            && let Some(evidence) = context["effect_evidence"].as_array_mut()
        {
            for effect in evidence {
                if let Some(summary) = effect["result_summary"].as_str() {
                    effect["result_summary"] = json!(summary.chars().take(64).collect::<String>());
                }
            }
        }
        for optional in [
            "prior_conversation",
            "manager_decisions",
            "workspace_instructions",
        ] {
            if context.to_string().len() <= 250000 {
                break;
            }
            context.as_object_mut().unwrap().remove(optional);
        }
        let prompt = context.to_string();
        ensure!(
            prompt.len() <= 250000,
            "Management context exceeds its bound"
        );
        let debug_dispatch = if p.debug_mode {
            let admitted_steering = latest_context.as_ref().and_then(|(id, origin, _, _)| {
                origin
                    .as_ref()
                    .filter(|_| inputs.iter().any(|input| &input.0 == id))
            });
            let origins = admitted_steering.into_iter().chain(
                inputs
                    .iter()
                    .rev()
                    .chain(continuing.iter().rev())
                    .filter_map(|input| input.1.as_ref()),
            );
            match debug_dispatch(&tx, project, origins, &prompt) {
                Ok(Some(dispatch)) => Some(dispatch),
                Ok(None) => {
                    tracing::debug!(
                        project,
                        "Manager prompt debug has no trusted Feishu discussion"
                    );
                    None
                }
                Err(_) => {
                    tracing::warn!(project, "Manager prompt debug discussion lookup failed");
                    None
                }
            }
        } else {
            None
        };
        let checkpoint = manager_loop::begin_with_images(prompt, SYSTEM.into(), images)?;
        let run = db::id();
        tx.execute(
            "UPDATE manager_runs SET status='superseded' WHERE project_id=? AND status='working'",
            [project],
        )?;
        tx.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,origin_event_id,checkpoint,status,owner,created_at) VALUES(?,?,?,?,?,?,'working',?,?)",params![run,project,revision,input_revision,origin,checkpoint.to_string(),owner(),db::now()])?;
        for (id, _, _, _) in continuing.iter().chain(inputs.iter()) {
            own_input(&tx, &run, id)?;
            tx.execute(
                "INSERT INTO manager_run_inputs(run_id,input_id) VALUES(?,?)",
                params![run, id],
            )?;
        }
        (run, checkpoint, debug_dispatch)
    };
    tx.commit()?;
    Ok(Some(Turn {
        context: Context {
            project: project.into(),
            run,
            revision,
            input_revision,
        },
        config: ProviderConfig {
            base_url,
            model,
            api_key,
            supports_images,
        },
        checkpoint,
        debug_dispatch,
    }))
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
    if p.paused || work::has_takeover(c, project, &p.generation)? {
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
fn claim_or_repair(c: &Connection, project: &str) -> Result<Option<Turn>> {
    if provider_unavailable(c, project)? { return Ok(None); }
    match claim(c, project) {
        Ok(turn) => Ok(turn),
        Err(_) => {
            let p = db::project_by_key(c, project)?;
            if p.paused || work::has_takeover(c, project, &p.generation)? {
                return Ok(None);
            }
            let config:Option<(i64,i64)>=c.query_row("SELECT revision,input_revision FROM manager_config WHERE project_id=?",[project],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((revision, input_revision)) = config else {
                return Ok(None);
            };
            // Reserve a failed claim's own generation-bound repair context.
            // Its inputs stay queued and no ordinary task body reaches a worker.
            let ctx = Context {
                project: project.into(),
                run: db::id(),
                revision,
                input_revision,
            };
            let tx = c.unchecked_transaction()?;
            tx.execute("UPDATE manager_runs SET status='superseded' WHERE project_id=? AND status='working'",[project])?;
            tx.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at) VALUES(?,?,?,?,'null','working',?,?)",params![ctx.run,project,revision,input_revision,owner(),db::now()])?;
            tx.execute("INSERT INTO manager_run_inputs(run_id,input_id) SELECT ?,id FROM manager_inputs WHERE project_id=? AND status='queued' ORDER BY rowid LIMIT 32",params![ctx.run,project])?;
            tx.commit()?;
            let mut error = Failure::internal();
            error.kind = "context_preparation".into();
            failure(c, &ctx, &error)?;
            Ok(None)
        }
    }
}
async fn current(app: &App, ctx: &Context) -> bool {
    let Some(_permit) = app.service.gate.enter() else {
        return false;
    };
    let context = ctx.clone();
    app.db
        .call(move |c| Ok(valid(c, &context).is_ok()))
        .await
        .unwrap_or(false)
}
async fn checkpoint(app: &App, ctx: &Context, value: &Value, model_responded: bool) -> Result<()> {
    let Some(_permit) = app.service.gate.enter() else {
        bail!("Service ownership changed")
    };
    let context = ctx.clone();
    let correction = value
        .get("last_correction")
        .filter(|v| !v.is_null())
        .map(Value::to_string);
    let value = value.to_string();
    ensure!(
        value.len() <= 2_097_152,
        "Management checkpoint exceeds its bound"
    );
    app.db.call(move|c|{let tx=c.unchecked_transaction()?;valid(&tx,&context)?;ensure!(tx.execute("UPDATE manager_runs SET checkpoint=? WHERE id=? AND owner=? AND status='working'",params![value,context.run,owner()])?==1,"Management turn ownership changed");if let Some(correction)=correction {tx.execute("UPDATE manager_runs SET error=json_set(CASE WHEN json_valid(error) THEN error ELSE '{}' END,'$.last_correction',json(?)) WHERE id=?",params![correction,context.run])?;}
if model_responded { recovered(&tx,&context)?; }tx.commit()?;Ok(())}).await
}
async fn drive(app: Arc<App>, mut turn: Turn) -> Result<()> {
    loop {
        // A completed iteration has checkpointed both the provider response and
        // every tool result. Yield only here, never by cancelling live model I/O.
        if app.service.manager_gate.is_closed() {
            return Ok(());
        }
        ensure!(
            current(&app, &turn.context).await,
            "Management turn superseded"
        );
        if let Some(dispatch) = turn.debug_dispatch.take() {
            let echo = crate::feishu::send_debug_reply(
                &app,
                &turn.context.project,
                &dispatch.bot_app_id,
                &dispatch.to,
                &dispatch.anchor,
                &dispatch.payload,
            );
            tokio::pin!(echo);
            loop {
                tokio::select! {
                    result = &mut echo => {
                        if let Err(error) = result {
                            tracing::warn!(project = %turn.context.project, run = %turn.context.run, anchor = %dispatch.anchor, %error, "Debug echo failed; continuing manager dispatch");
                        }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if app.shutdown.is_cancelled() || !current(&app, &turn.context).await {
                            tracing::warn!(project = %turn.context.project, run = %turn.context.run, anchor = %dispatch.anchor, "Debug echo interrupted by management turn supersession");
                            bail!("Management turn superseded");
                        }
                        if app.service.manager_gate.is_closed() {
                            tracing::warn!(project = %turn.context.project, run = %turn.context.run, anchor = %dispatch.anchor, "Debug echo interrupted by service handover");
                            return Ok(());
                        }
                    }
                }
            }
            // Revalidate after Feishu I/O, which may overlap new steering.
            if app.service.manager_gate.is_closed() {
                return Ok(());
            }
            ensure!(
                current(&app, &turn.context).await,
                "Management turn superseded"
            );
        }
        let cancel = CancellationToken::new();
        let model = manager_loop::advance_observed(
            &turn.config,
            turn.checkpoint.clone(),
            cancel.clone(),
            Some(crate::manager_context::Observer {
                store: app.manager_context.clone(),
                project: turn.context.project.clone(),
                run: turn.context.run.clone(),
            }),
        );
        tokio::pin!(model);
        let step = loop {
            tokio::select! {result=&mut model=>break result?,_=tokio::time::sleep(Duration::from_millis(100))=>{if app.shutdown.is_cancelled()||!current(&app,&turn.context).await{cancel.cancel();bail!("Management turn superseded");}}}
        };
        checkpoint(&app, &turn.context, &step.checkpoint, step.model_responded).await?;
        turn.checkpoint = step.checkpoint;
        match step.action {
            LoopAction::Continue => continue,
            LoopAction::Tools(calls) => {
                let mut results = Vec::new();
                for call in calls {
                    let occurrence = call.occurrence.clone();
                    let value = tool(&app, &turn.context, call).await?;
                    results.push((occurrence, value));
                }
                turn.checkpoint = manager_loop::supply_tool_results(turn.checkpoint, results)?;
                checkpoint(&app, &turn.context, &turn.checkpoint, false).await?;
            }
            LoopAction::Done(note) => {
                let context = turn.context.clone();
                let Some(_permit) = app.service.gate.enter() else {
                    return Ok(());
                };
                app.db
                    .call(move |c| finish_turn(c, &context, &note))
                    .await?;
                return Ok(());
            }
        }
    }
}
fn finish_turn(c: &Connection, context: &Context, note: &str) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    valid(&tx, context)?;
    tx.execute(
        "UPDATE manager_runs SET status='complete' WHERE id=?",
        [&context.run],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO manager_turn_notes(run_id,note) VALUES(?,?)",
        params![context.run, note.chars().take(8000).collect::<String>()],
    )?;
    tx.execute("UPDATE manager_inputs SET status='consumed' WHERE id IN(SELECT input_id FROM manager_input_claims WHERE run_id=?) AND status='processing'",[&context.run])?;
    settle_inputs(&tx, &context.run)?;
    recovered(&tx, context)?;
    tx.commit()?;
    Ok(())
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
    let diagnostic = json!({"incident_id":incident,"attempt":attempts,"kind":error.kind,"http_status":error.http_status,"retryable":error.retryable,"elapsed_ms":error.elapsed_ms,"output_tokens":error.output_tokens,"finish_reason":error.finish_reason,"failed_at":db::now(),"next_attempt_at":if error.retryable{Some(next_attempt)}else{None}}).to_string();
    tx.execute("INSERT INTO manager_failures(project_id,revision,incident_id,attempts,diagnostic,notified) VALUES(?,?,?,?,?,?) ON CONFLICT(project_id) DO UPDATE SET revision=excluded.revision,incident_id=excluded.incident_id,attempts=excluded.attempts,diagnostic=excluded.diagnostic,notified=excluded.notified,repair_probe=CASE WHEN manager_failures.incident_id=excluded.incident_id THEN manager_failures.repair_probe ELSE 0 END",params![ctx.project,ctx.revision,incident,attempts,diagnostic,notified])?;
    tx.execute(
        "UPDATE manager_runs SET error=json_set(CASE WHEN json_valid(error) THEN error ELSE '{}' END,'$.last_failure',json(?)) WHERE id=?",
        params![diagnostic, ctx.run],
    )?;
    tx.execute(
        "UPDATE manager_config SET error=?,next_attempt_at=? WHERE project_id=?",
        params![error.summary(), next_attempt, ctx.project],
    )?;
    if !error.retryable || attempts >= 3 {
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
        let message = failure_notice(error, delay, true, chinese) + action;
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
pub async fn run(app: Arc<App>) {
    tokio::spawn(autonomy::run(app.clone()));
    let running = Arc::new(tokio::sync::Mutex::new(HashSet::<String>::new()));
    loop {
        tokio::select! {_=app.shutdown.cancelled()=>return,_=tokio::time::sleep(Duration::from_millis(200))=>{}}
        let Some(_permit) = app.service.gate.enter() else {
            continue;
        };
        let projects=app.db.call(|c|Ok(c.prepare("SELECT m.project_id FROM manager_config m JOIN projects p ON p.id=m.project_id WHERE p.paused=0 AND m.next_attempt_at<=? AND (EXISTS(SELECT 1 FROM manager_inputs i WHERE i.project_id=m.project_id AND i.status IN ('queued','processing')) OR EXISTS(SELECT 1 FROM manager_runs r WHERE r.project_id=m.project_id AND r.status='working'))")?.query_map([db::now()],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)).await.unwrap_or_default();
        for project in projects {
            if !running.lock().await.insert(project.clone()) {
                continue;
            }
            let app = app.clone();
            let running = running.clone();
            tokio::spawn(async move {
                // Retain the actor through claim, all step/effect checkpoints,
                // and failure persistence before service ownership may transfer.
                let Some(_manager_permit) = app.service.manager_gate.enter() else {
                    running.lock().await.remove(&project);
                    return;
                };
                let claim_project = project.clone();
                let turn = {
                    let Some(_permit) = app.service.gate.enter() else {
                        running.lock().await.remove(&project);
                        return;
                    };
                    app.db
                        .call(move |c| claim_or_repair(c, &claim_project))
                        .await
                };
                if let Ok(Some(turn)) = turn {
                    let context = turn.context.clone();
                    if let Err(error) = drive(app.clone(), turn).await {
                        let Some(_permit) = app.service.gate.enter() else {
                            running.lock().await.remove(&project);
                            return;
                        };
                        if !app.shutdown.is_cancelled() {
                            let error = error
                                .downcast_ref::<Failure>()
                                .cloned()
                                .unwrap_or_else(Failure::internal);
                            let _ = app.db.call(move |c| failure(c, &context, &error)).await;
                        }
                        app.notify.notify_one();
                    }
                }
                running.lock().await.remove(&project);
            });
        }
    }
}
fn settle_inputs(c: &Connection, _run: &str) -> Result<()> {
    settle_sources(c)
}
pub fn settle_sources(c: &Connection) -> Result<()> {
    if !installed(c)? {
        return Ok(());
    }
    c.execute("UPDATE work_events SET status='consumed',updated_at=? WHERE status='managed' AND id IN(SELECT origin_event_id FROM manager_inputs WHERE status IN ('consumed','transferred')) AND NOT EXISTS(SELECT 1 FROM work_events d WHERE d.source='management' AND json_extract(d.payload,'$.origin_event_id')=work_events.id AND d.status IN ('queued','submitting','submitted','uncertain'))",[db::now()])?;
    c.execute("UPDATE work_occurrences SET status='consumed' WHERE event_id IN(SELECT id FROM work_events WHERE status='consumed')",[])?;
    c.execute("UPDATE work_tasks SET status='consumed',updated_at=? WHERE id IN(SELECT task_id FROM work_events WHERE status='consumed')",[db::now()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::ProjectInput};
    async fn fixture() -> (tempfile::TempDir, Arc<App>, Project) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), Config::default()).unwrap();
        let path = dir.path().join("workspace");
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
                    ConfigInput {
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
        (dir, app, p)
    }
    #[tokio::test]
    async fn proposal_turn_is_scoped_and_delegates_the_approved_instructions_exactly_once() -> Result<()> {
        let (_dir, app, project) = fixture().await;
        let approved = "Change only the workspace README and verify that its example is accurate.";
        let now = db::now();
        let approval = json!({"proposal_id":"prop","to":"oc_fixture","app_id":"cli_fixture","anchor":"om_approval","instructions":approved});
        app.db.sync(|c| {
            c.execute("INSERT INTO work_tasks(id,project_id,title,instructions,status,created_at,updated_at)
              VALUES('proposal-task',?,'Approved proposal',?,'queued',?,?)",
              params![project.id,approved,now,now])?;
            c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at)
              VALUES('proposal-event',?,'proposal-task','autonomy','prop',?,'queued',?,?)",
              params![project.id,approval.to_string(),now,now])?;
            route_event(c, "proposal-event")?;
            input(c, &project.id, "ordinary", None, "user", &json!({"body":"An unrelated request"}))
        })?;
        let proposal = app.db.sync(|c| {
            let ordinary = claim(c, &project.id)?.unwrap();
            assert!(!proposal_run(c, &ordinary.context.run)?);
            finish_turn(c, &ordinary.context, "")?;
            let proposal = claim(c, &project.id)?.unwrap();
            assert!(proposal_run(c, &proposal.context.run)?);
            let prompt = prompt_context(&proposal.checkpoint).unwrap();
            assert_eq!(prompt["trusted_sources"].as_array().unwrap().len(), 1);
            assert_eq!(prompt["trusted_sources"][0]["message_id"], "om_approval");
            assert_eq!(prompt["prior_conversation"], json!([]));
            Ok(proposal)
        })?;
        let locate = |occurrence: &str, name: &str, arguments: Value| ToolCall {
            occurrence: occurrence.to_owned(), name: name.to_owned(), arguments,
        };
        let denied = tool(&app, &proposal.context, locate("write", "write_file",
            json!({"path":"should-not-exist","content":"fixed"}))).await?;
        assert!(denied["error"].is_string());
        assert!(!Path::new(&project.path).join("should-not-exist").exists());
        let wrong_origin = tool(&app, &proposal.context, locate("wrong", "delegate",
            json!({"source_event_id":"ordinary","instructions":"broader changes"}))).await?;
        assert!(wrong_origin["error"].is_string());
        let mut delegated = None;
        for call in ["delegate-first", "delegate-duplicate"] {
            let result = tool(&app, &proposal.context, locate(call, "delegate",
                json!({"source_event_id":"proposal-event","instructions":"broader changes"}))).await?;
            let id = result["event_id"].as_str().unwrap().to_owned();
            assert!(delegated.as_ref().is_none_or(|previous| previous == &id));
            delegated = Some(id);
        }
        app.db.sync(|c| {
            let count:u32 = c.query_row("SELECT count(*) FROM work_events WHERE source='management'", [], |r| r.get(0))?;
            assert_eq!(count,1);
            let event = work::event(c,&project.id,&delegated.unwrap())?;
            assert_eq!(event.payload["proposal_id"],"prop");
            assert_eq!(event.payload["instructions"],approved);
            Ok(())
        })
    }

    #[tokio::test]
    async fn unconfigured_projects_hold_input_without_delegating_and_share_the_saved_provider() {
        let (_dir, app, first) = fixture().await;
        let second = app.db.create(ProjectInput {
            name: "Second fixture".into(),
            path: first.path.clone(),
            agent: "codex".into(),
            chat_id: String::new(),
            native_session: String::new(),
            extra_args: vec![],
        }).await.unwrap();
        // A nonempty model/key still requires an endpoint.
        app.db.sync(|c| {
            c.execute("UPDATE manager_provider_config SET base_url='' WHERE id=1", [])?;
            Ok(())
        }).unwrap();
        ingest(&app, &first, "om_provider_required", "Hold this request");
        ingest(&app, &first, "om_provider_required_again", "Also keep this request");
        app.db.sync(|c| {
            work::sync_inbound(c)?;
            for project in [&first.id, &second.id] {
                let state = public(c, project)?;
                assert_eq!(state["mode"], "manager");
                assert_eq!(state["status"], "unavailable");
                assert_eq!(state["error"], PROVIDER_REQUIRED);
                assert!(state.get("api_key").is_none());
                assert!(claim(c, project)?.is_none());
                assert!(claim_or_repair(c, project)?.is_none());
                assert!(work::claim_next(c, project, "fixture", db::now())?.is_none());
            }
            // Repeated actor polling retains the original topic and at most one notice.
            assert!(claim_or_repair(c, &first.id)?.is_none());
            for table in ["manager_failures", "manager_runs", "work_runs"] {
                let count: u32 = c.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))?;
                assert_eq!(count, 0, "{table}");
            }
            let pending: u32 = c.query_row("SELECT count(*) FROM manager_inputs WHERE status='queued'", [], |row| row.get(0))?;
            assert_eq!(pending, 2);
            let notices = c.prepare("SELECT attachment FROM messages WHERE direction='out'")?
                .query_map([], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(notices.len(), 1);
            let notice: Value = serde_json::from_str(&notices[0])?;
            assert_eq!(notice["reply_to_message_id"], "om_provider_required");
            validate_outbox(c, &first.id, &notice)?;
            assert!(set_mode(c, &first.id, ModeInput {
                mode:"native".into(), request_id:String::new(), expected_revision:0
            }).is_err());

            let current = public_provider(c)?;
            save_provider(c, ConfigInput {
                base_url:"https://provider.example/v1".into(), model:"shared-fixture".into(), api_key:None,
                clear_api_key:false, expected_revision:current["revision"].as_i64().unwrap(),
                supports_images:Some(true),
            })?;
            assert!(validate_outbox(c, &first.id, &notice).is_err());
            for project in [&first.id, &second.id] {
                let state = public(c, project)?;
                assert_eq!(state["model"], "shared-fixture");
                assert_eq!(state["status"], "ready");
                assert_eq!(state["supports_images"], true);
            }
            assert!(claim_or_repair(c, &first.id)?.is_some());
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn old_provider_migration_is_single_shot_and_preserves_unclaimed_and_uncertain_work() {
        let (_dir, app, first) = fixture().await;
        let new_project = |name: &str| ProjectInput {
            name:name.into(), path:first.path.clone(), agent:"codex".into(),
            chat_id:String::new(), native_session:String::new(), extra_args:vec![],
        };
        let different = app.db.create(new_project("Compared legacy fixture")).await.unwrap();
        let native = app.db.create(new_project("Legacy native fixture")).await.unwrap();
        let absent = app.db.create(new_project("Legacy fixture without configuration")).await.unwrap();
        app.db.sync(|c| {
            c.execute_batch("DROP TABLE manager_provider_config;")?;
            c.execute("UPDATE manager_config SET base_url='https://fixture.example',model='old-fixture',
                api_key='fixture-key-a',mode='manager',revision=3 WHERE project_id=?", [&first.id])?;
            c.execute("UPDATE manager_config SET base_url='https://fixture.example/v1',model='chosen-fixture',
                api_key='fixture-key-b',supports_images=1,mode='manager',revision=4 WHERE project_id=?", [&different.id])?;
            c.execute("UPDATE manager_config SET mode='native' WHERE project_id=?", [&native.id])?;
            c.execute("DELETE FROM manager_config WHERE project_id=?", [&absent.id])?;
            for (id, state) in [("legacy-queued", "queued"), ("legacy-uncertain", "uncertain")] {
                c.execute("INSERT INTO work_tasks(id,project_id,title,instructions,status,created_at,updated_at)
                    VALUES(?,?,'Legacy task','Legacy payload',?,1,1)", params![id,native.id,state])?;
                c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at)
                    VALUES(?,?,?,'local',?,?,?,1,1)",
                    params![id,native.id,id,id,json!({"content":"preserve this"}).to_string(),state])?;
            }
            c.execute("INSERT INTO work_runs(id,project_id,task_id,event_id,generation,status,started_at,updated_at)
                VALUES('legacy-run',?,'legacy-uncertain','legacy-uncertain','legacy-generation','uncertain',1,1)", [&native.id])?;
            Ok(())
        }).unwrap();
        let path = app.dir.join("bridge.sqlite3");
        // Handover opens the real migration pipeline on a private fixture store.
        let db = db::Db::open_handover(&path).unwrap();
        db.sync(|c| {
            let schema: i64 = c.pragma_query_value(None, "user_version", |row| row.get(0))?;
            assert_eq!(schema, 7);
            assert_eq!(crate::api::PROTOCOL, 7);
            let selected = public_provider(c)?;
            assert_eq!(selected["model"], "chosen-fixture");
            assert_eq!(selected["migration_conflict"], true);
            assert_eq!(selected["configured"], false);
            assert!(claim(c, &native.id)?.is_none());
            assert_eq!(selected["supports_images"], true);
            for project in [&first.id,&different.id,&native.id,&absent.id] {
                assert_eq!(public(c, project)?["model"], "chosen-fixture");
            }
            assert_eq!(work::event(c, &native.id, "legacy-queued")?.status, "managed");
            assert_eq!(work::event(c, &native.id, "legacy-uncertain")?.status, "uncertain");
            assert_eq!(work::run(c, &native.id, "legacy-run")?.status, "uncertain");
            let moved: u32 = c.query_row("SELECT count(*) FROM manager_inputs WHERE origin_event_id='legacy-queued'", [], |row| row.get(0))?;
            assert_eq!(moved, 1);
            let confirmed = save_provider(c, ConfigInput {
                base_url:"https://fixture.example/v1".into(),model:"chosen-fixture".into(),api_key:None,
                clear_api_key:false,expected_revision:selected["revision"].as_i64().unwrap(),supports_images:None,
            })?;
            assert_eq!(confirmed["configured"], true);
            assert!(claim(c, &native.id)?.is_some());
            save_provider(c, ConfigInput {
                base_url:"https://fixture.example/v1".into(),model:"chosen-fixture".into(),api_key:None,
                clear_api_key:true,expected_revision:confirmed["revision"].as_i64().unwrap(),supports_images:None,
            })?;
            Ok(())
        }).unwrap();
        drop(db);
        db::Db::open_handover(&path).unwrap().sync(|c| {
            let provider = public_provider(c)?;
            assert_eq!(provider["has_api_key"], false);
            assert_eq!(provider["migration_conflict"], false);
            let stale: u32 = c.query_row("SELECT count(*) FROM manager_config WHERE api_key!=''", [], |row| row.get(0))?;
            assert_eq!(stale, 0);
            Ok(())
        }).unwrap();
    }
    fn ingest(app: &App, p: &Project, id: &str, text: &str) {
        app.db.ingest_bot(&p.id,"cli_fixture","oc_fixture",id,text,&json!({"source":{"message_type":"text","chat_id":"oc_fixture","chat_type":"p2p","sender":{"open_id":"ou_fixture"},"mentions":[]}})).unwrap();
    }
    #[tokio::test]
    async fn debug_replies_echo_first_prompt_before_provider_without_outbox_records() {
        use axum::{
            Json, Router,
            extract::{Path as RoutePath, State},
            http::StatusCode,
            routing::post,
        };

        type Journal = Arc<std::sync::Mutex<Vec<(String, Value)>>>;
        async fn auth() -> Json<Value> {
            Json(json!({"code":0,"tenant_access_token":"fixture-token","expire":3600}))
        }
        async fn echo(
            State(journal): State<Journal>,
            RoutePath(anchor): RoutePath<String>,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            let status = if anchor == "om_debug_rate_limit" {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            };
            journal.lock().unwrap().push((anchor, body));
            (
                status,
                Json(json!({"code":0,"data":{"thread_id":"omt_fixture","message_id":"om_debug_sent"}})),
            )
        }
        async fn complete(State(journal): State<Journal>, Json(body): Json<Value>) -> Json<Value> {
            journal.lock().unwrap().push(("provider".into(), body));
            Json(
                json!({"id":"fixture","object":"chat.completion","created":1,"model":"fixture-manager","choices":[{"index":0,"message":{"role":"assistant","content":"Done"},"finish_reason":"stop"}]}),
            )
        }

        let (_dir, app, p) = fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        app.config.write().await.feishu_base = base.clone();
        let journal = Journal::default();
        let router = Router::new()
            .route(
                "/open-apis/auth/v3/tenant_access_token/internal",
                post(auth),
            )
            .route("/open-apis/im/v1/messages/{anchor}/reply", post(echo))
            .route("/v1/chat/completions", post(complete))
            .with_state(journal.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        for (anchor, enabled, resume) in [
            ("om_debug_off", false, false),
            ("om_debug_on", true, false),
            ("om_debug_rate_limit", true, false),
            ("om_debug_resume", true, true),
        ] {
            app.db.set_debug_mode(&p.id, enabled).await.unwrap();
            ingest(&app, &p, anchor, "保留 \"引号\" 和 <event>\\&\n");
            let mut turn = app
                .db
                .sync(|c| {
                    work::sync_inbound(c)?;
                    let turn = claim(c, &p.id)?.unwrap();
                    assert_eq!(turn.debug_dispatch.is_some(), enabled);
                    if resume {
                        let saved = claim(c, &p.id)?.unwrap();
                        assert_eq!(saved.context.run, turn.context.run);
                        assert!(saved.debug_dispatch.is_none());
                        return Ok(saved);
                    }
                    Ok(turn)
                })
                .unwrap();
            turn.config.base_url = format!("{base}/v1");
            tokio::time::timeout(Duration::from_secs(4), drive(app.clone(), turn))
                .await
                .unwrap()
                .unwrap();

            let events = std::mem::take(&mut *journal.lock().unwrap());
            assert_eq!(events.last().unwrap().0, "provider");
            if enabled && !resume {
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].0, anchor);
                assert_eq!(events[0].1["msg_type"], "text");
                assert_eq!(events[0].1["reply_in_thread"], true);
                let content: Value =
                    serde_json::from_str(events[0].1["content"].as_str().unwrap()).unwrap();
                let sent_to_model = events[1].1["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["role"] == "user")
                    .unwrap();
                let model_text = sent_to_model["content"]
                    .as_str()
                    .or_else(|| sent_to_model["content"][0]["text"].as_str())
                    .unwrap();
                assert_eq!(content["text"].as_str(), Some(model_text));
            } else {
                assert_eq!(events.len(), 1);
            }
        }
        let mut scheduled = app
            .db
            .sync(|c| {
                let now = db::now();
                work::create_timer(
                    c,
                    &p.id,
                    serde_json::from_value(json!({
                        "name":"Debug timer fixture", "kind":"once", "at":now,
                        "prompt":"Produce the requested result", "to":"oc_fixture", "purpose":"user"
                    }))?,
                    now,
                )?;
                assert_eq!(work::fire_due(c, now)?.len(), 1);
                let turn = claim(c, &p.id)?.unwrap();
                assert!(turn.debug_dispatch.is_none());
                Ok(turn)
            })
            .unwrap();
        scheduled.config.base_url = format!("{base}/v1");
        tokio::time::timeout(Duration::from_secs(4), drive(app.clone(), scheduled))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(journal.lock().unwrap()[0].0, "provider");
        assert_eq!(journal.lock().unwrap().len(), 1);
        app.db
            .sync(|c| {
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM messages WHERE direction='out'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM manager_failures", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                Ok(())
            })
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn upgrade_yield_preserves_run_claims_and_stop_authority() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_upgrade", "Continue this task across upgrade");
        let turn = app.db.sync(|c| {
            work::sync_inbound(c)?;
            Ok(claim(c, &p.id)?.unwrap())
        }).unwrap();
        let run = turn.context.run.clone();
        let checkpoint = turn.checkpoint.clone();
        app.service.manager_gate.close_for_test().await.unwrap();
        // The fixture provider is unreachable: yielding must not issue a request
        // or settle/requeue the claimed input as a new task.
        drive(app.clone(), turn).await.unwrap();
        assert!(app.service.gate.enter().is_some());
        let resumed = app.db.sync(|c| Ok(claim(c, &p.id)?.unwrap())).unwrap();
        assert_eq!(resumed.context.run, run);
        assert_eq!(resumed.checkpoint, checkpoint);
        app.db.sync(|c| {
            assert_eq!(c.query_row("SELECT count(*) FROM manager_input_claims WHERE run_id=?", [&run], |r| r.get::<_, i64>(0))?, 1);
            stop(c, &p.id)?;
            assert!(valid(c, &resumed.context).is_err());
            assert_eq!(c.query_row("SELECT status FROM manager_runs WHERE id=?", [&run], |r| r.get::<_, String>(0))?, "stopped");
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn image_capability_is_explicit_and_resets_when_provider_identity_changes() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c| {
            let original = public(c, &p.id)?;
            assert_eq!(original["supports_images"], false);
            let configure = |model: &str, declared: Option<bool>| -> Result<Value> {
                let current = public(c, &p.id)?;
                save_provider(c, ConfigInput { base_url: current["base_url"].as_str().unwrap().into(), model: model.into(), api_key: None, clear_api_key: false, expected_revision: current["revision"].as_i64().unwrap(), supports_images: declared })
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
    async fn prepared_image_claim_uses_fresh_host_metadata_and_plain_steering_does_not_wait() {
        let (_dir, app, p) = fixture().await;
        let id = app.db.ingest_bot(&p.id,"cli_fixture","oc_fixture","om_pixels","Inspect picture",&json!({"source":{"chat_id":"oc_fixture","chat_type":"p2p"},"resources":[{"kind":"image","file_key":"img_fixture","local_path":"/untrusted/body/spy.png"}]})).unwrap().unwrap();
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                assert!(claim(c, &p.id)?.is_none());
                Ok(())
            })
            .unwrap();
        ingest(&app, &p, "om_plain_after_pixels", "Change direction now");
        let inline_checkpoint = app.db.sync(|c| {
            work::sync_inbound(c)?;
            let plain = claim(c, &p.id)?.unwrap();
            assert!(plain.checkpoint["images"].as_array().unwrap().is_empty());
            finish_turn(c, &plain.context, "accepted steering")?;
            let mut resource = crate::multimodal::fixture_resource(&app.dir, &p.id, &id);
            resource["cache_source_id"] = json!("om_pixels");
            c.execute("UPDATE messages SET attachment=json_set(attachment,'$.resources_ready',json('true'),'$.resources',json(?)) WHERE id=?",params![json!([resource]).to_string(),id])?;
            let text_only = claim(c, &p.id)?.unwrap();
            assert!(text_only.checkpoint["images"].as_array().unwrap().is_empty());
            assert!(text_only.checkpoint.to_string().contains("image_capability_not_declared"));
            let image_input: String = c.query_row("SELECT i.id FROM manager_inputs i JOIN work_events e ON e.id=i.origin_event_id WHERE e.message_id=?", [&id], |r|r.get(0))?;
            assert_eq!(c.query_row("SELECT status FROM manager_inputs WHERE id=?", [&image_input], |r|r.get::<_,String>(0))?, "processing");
            let current = public(c,&p.id)?;
            let config: ConfigInput = serde_json::from_value(json!({"base_url":current["base_url"],"model":current["model"],"expected_revision":current["revision"],"supports_images":true}))?;
            save_provider(c,config)?;
            let inline = claim(c,&p.id)?.unwrap();
            assert_eq!(inline.checkpoint["images"].as_array().unwrap().len(),1);
            assert_eq!(inline.checkpoint["images"][0]["message_id"],id);
            assert!(!inline.checkpoint.to_string().contains("/untrusted/body/spy.png"));
            assert_eq!(c.query_row("SELECT run_id FROM manager_input_claims WHERE input_id=?", [&image_input], |r|r.get::<_,String>(0))?, inline.context.run);
            Ok(inline.checkpoint)
        }).unwrap();
        let rig_agent::run::AgentRunStep::CallModel {
            mut prompt,
            mut history,
            ..
        } = serde_json::from_value(inline_checkpoint["pending"].clone()).unwrap()
        else {
            panic!("initial model request")
        };
        let refs: Vec<crate::multimodal::ImageRef> =
            serde_json::from_value(inline_checkpoint["images"].clone()).unwrap();
        let decisions = crate::multimodal::hydrate(
            &mut prompt,
            &mut history,
            &refs,
            inline_checkpoint["image_input_digest"].as_str(),
            true,
        )
        .await;
        assert_eq!(decisions[0]["mode"], "image_part");
        let encoded_prompt = serde_json::to_string(&prompt).unwrap();
        assert!(encoded_prompt.contains(r#""type":"image""#));
        assert!(encoded_prompt.contains("continuing_inputs"));
    }
    #[tokio::test]
    async fn image_pixels_never_cross_the_newest_trusted_discussion_scope() {
        let (_dir, app, p) = fixture().await;
        let mut messages = Vec::new();
        for (index, chat) in ["oc_first", "oc_second", "oc_second"]
            .into_iter()
            .enumerate()
        {
            let id = app.db.ingest_bot(&p.id,"cli_fixture",chat,&format!("om_{chat}_{index}"),"Inspect image",&json!({"source":{"chat_id":chat,"chat_type":"p2p"},"resources":[{"kind":"image","file_key":"img_fixture"}]})).unwrap().unwrap();
            let mut resource = crate::multimodal::fixture_resource(&app.dir, &p.id, &id);
            resource["cache_source_id"] = json!(format!("om_{chat}_{index}"));
            app.db.sync(|c| { c.execute("UPDATE messages SET attachment=json_set(attachment,'$.resources_ready',json('true'),'$.resources',json(?)) WHERE id=?",params![json!([resource]).to_string(),id])?;Ok(()) }).unwrap();
            messages.push(id);
        }
        app.db.sync(|c| {
            work::sync_inbound(c)?;
            let current = public(c,&p.id)?;
            save_provider(c,serde_json::from_value(json!({"base_url":current["base_url"],"model":current["model"],"expected_revision":current["revision"],"supports_images":true}))?)?;
            let turn = claim(c,&p.id)?.unwrap();
            assert_eq!(turn.checkpoint["images"].as_array().unwrap().len(),2);
            let ids = turn.checkpoint["images"].as_array().unwrap().iter().map(|image|image["message_id"].as_str().unwrap()).collect::<Vec<_>>();
            assert!(ids.contains(&messages[1].as_str()) && ids.contains(&messages[2].as_str()) && !ids.contains(&messages[0].as_str()));
            assert!(turn.checkpoint.to_string().contains("different_discussion_scope"));
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
            let (_,_,scope) = image_source(c,&p.id,&origin)?.unwrap();
            let mut data = json!({});
            let refs = prepare_images(c,&p.id,Some(&origin),Some(&scope),true,0,&mut data)?;
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
    fn receipt_args(anchor: &str) -> MessageArgs {
        MessageArgs {
            to: "oc_fixture".into(),
            text: "收到，我先核对现状再给你结论。".into(),
            reply_to_message_id: Some(anchor.into()),
            intent: Some(MessageIntent::Receipt),
            source_event_id: None,
        }
    }
    #[tokio::test]
    async fn owned_single_and_full_batches_ignore_receipts_and_recover_effects_once() {
        for count in [1, 3, 32] {
            let (_dir, app, p) = fixture().await;
            for n in 0..count {
                ingest(&app, &p, &format!("om_owned_{n}"), &format!("ordered-{n}"));
            }
            let first = app
                .db
                .sync(|c| {
                    work::sync_inbound(c)?;
                    Ok(claim(c, &p.id)?.unwrap())
                })
                .unwrap();
            let origin=app.db.sync(|c|Ok(c.query_row("SELECT origin_event_id FROM manager_inputs WHERE project_id=? ORDER BY rowid LIMIT 1",[&p.id],|r|r.get::<_,String>(0))?)).unwrap();
            let call = ToolCall {
                occurrence: "test:confirmed-delegate".into(),
                name: "delegate".into(),
                arguments: json!({"instructions":"one complete assignment","source_event_id":origin}),
            };
            let delegated = tool(&app, &first.context, call.clone()).await.unwrap();
            let saved = first.checkpoint.clone();
            app.db.sync(|c| {
                assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE status='processing'",[],|r|r.get::<_,i64>(0))?,count);
                assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE status='queued'",[],|r|r.get::<_,i64>(0))?,0);
                let prompt=prompt_context(&first.checkpoint).unwrap();
                let bodies=prompt["new_inputs"].as_array().unwrap().iter().map(|v|v["data"]["user_input"]["body"].as_str().unwrap()).collect::<Vec<_>>();
                assert_eq!(bodies,(0..count).map(|n|format!("ordered-{n}")).collect::<Vec<_>>());
                db::enqueue_out_request(c,&p.id,"cli_fixture","confirmed-fixture-reply","one result",json!({"to":"oc_fixture","reply_to_message_id":"om_owned_0"}))?;
                for n in 0..4 {observation(c,&p.id,"worker_run",&format!("receipt-{n}"),"settled",Some(&origin),&json!({"kind":"worker_run","semantic_task_success":"unconfirmed"}))?;}
                assert!(valid(c,&first.context).is_ok());
                let same=claim(c,&p.id)?.unwrap();assert_eq!(same.context.run,first.context.run);assert_eq!(same.checkpoint,saved);
                assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_run_inputs WHERE run_id=?",[&same.context.run],|r|r.get::<_,i64>(0))?,count);
                // Process loss after a tool was reserved must preserve uncertainty, not replay it.
                c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES('interrupted-tool',?,'test:uncertain','delegate',?,'submitting')",params![first.context.run,json!({"instructions":"uncertain assignment","source_event_id":origin}).to_string()])?;
                c.execute("UPDATE manager_runs SET owner='previous-host' WHERE id=?",[&first.context.run])?;
                migrate(c,true)?;
                let resumed=claim(c,&p.id)?.unwrap();assert_eq!(resumed.context.run,first.context.run);assert_eq!(resumed.checkpoint,saved);
                assert_eq!(c.query_row("SELECT status FROM manager_effects WHERE id='interrupted-tool'",[],|r|r.get::<_,String>(0))?,"uncertain");
                Ok(())
            }).unwrap();
            assert_eq!(tool(&app, &first.context, call).await.unwrap(), delegated);
            let uncertain=tool(&app,&first.context,ToolCall{occurrence:"test:uncertain".into(),name:"delegate".into(),arguments:json!({"instructions":"uncertain assignment","source_event_id":origin})}).await.unwrap();
            assert_eq!(uncertain["status"], "uncertain");
            app.db
                .sync(|c| {
                    assert_eq!(
                        c.query_row(
                            "SELECT COUNT(*) FROM work_events WHERE source='management'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )?,
                        1
                    );
                    assert_eq!(
                        c.query_row(
                            "SELECT COUNT(*) FROM messages WHERE direction='out'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )?,
                        1
                    );
                    finish_turn(c, &first.context, "original batch complete")?;
                    assert_eq!(
                        c.query_row(
                            "SELECT COUNT(*) FROM manager_inputs WHERE status='consumed'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )?,
                        count
                    );
                    let receipts = claim(c, &p.id)?.unwrap();
                    assert_ne!(receipts.context.run, first.context.run);
                    let prompt = prompt_context(&receipts.checkpoint).unwrap();
                    assert_eq!(prompt["new_inputs"].as_array().unwrap().len(), 4);
                    assert!(
                        prompt["new_inputs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|v| v["kind"] == "worker"
                                && v["data"]["semantic_task_success"] == "unconfirmed")
                    );
                    finish_turn(c, &receipts.context, "receipt evidence considered")?;
                    assert!(claim(c, &p.id)?.is_none());
                    Ok(())
                })
                .unwrap();
        }
    }
    #[tokio::test]
    async fn legacy_queued_working_members_are_adopted_atomically_without_new_claim() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_legacy_batch", "legacy request");
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let original = claim(c, &p.id)?.unwrap();
                c.execute("DELETE FROM manager_input_claims", [])?;
                c.execute("UPDATE manager_inputs SET status='queued'", [])?;
                c.execute(
                    "UPDATE manager_runs SET owner='legacy-host' WHERE id=?",
                    [&original.context.run],
                )?;
                let resumed = claim(c, &p.id)?.unwrap();
                assert_eq!(resumed.context.run, original.context.run);
                assert_eq!(resumed.checkpoint, original.checkpoint);
                assert_eq!(
                    c.query_row("SELECT status FROM manager_inputs", [], |r| r
                        .get::<_, String>(0))?,
                    "processing"
                );
                assert_eq!(
                    c.query_row("SELECT run_id FROM manager_input_claims", [], |r| r
                        .get::<_, String>(0))?,
                    original.context.run
                );
                finish_turn(c, &resumed.context, "legacy work continued")?;
                assert!(claim(c, &p.id)?.is_none());
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn full_owned_batch_accepts_urgent_user_steering_without_claiming_it_twice() {
        let (_dir, app, p) = fixture().await;
        for n in 0..32 {
            ingest(&app, &p, &format!("om_full_{n}"), &format!("old goal {n}"));
        }
        let first = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                Ok(claim(c, &p.id)?.unwrap())
            })
            .unwrap();
        app.db
            .sync(|c| {
                for n in 0..40 {
                    input(
                        c,
                        &p.id,
                        &format!("worker-pending-{n}"),
                        None,
                        "worker",
                        &json!({"status":"settled","semantic_task_success":"unconfirmed"}),
                    )?;
                }
                assert!(valid(c, &first.context).is_ok());
                Ok(())
            })
            .unwrap();
        ingest(
            &app,
            &p,
            "om_urgent_correction",
            "Do not release; preserve the latest correction.",
        );
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                assert!(valid(c, &first.context).is_err());
                let next = claim(c, &p.id)?.unwrap();
                let context = prompt_context(&next.checkpoint).unwrap();
                assert_ne!(next.context.run, first.context.run);
                assert_eq!(context["continuing_inputs"].as_array().unwrap().len(), 32);
                assert_eq!(context["new_inputs"].as_array().unwrap().len(), 32);
                assert_eq!(context["steering_priority"], true);
                assert_eq!(
                    context["new_inputs"].as_array().unwrap().last().unwrap()["data"]["user_input"]
                        ["body"],
                    "Do not release; preserve the latest correction."
                );
                let correction = context["latest_user_steering"]["source_event_id"]
                    .as_str()
                    .unwrap();
                assert!(select_origin(c, &next.context, Some(correction)).is_ok());
                let mut receipt = receipt_args("om_urgent_correction");
                receipt.source_event_id = Some(correction.into());
                assert!(enqueue_receipt(c, &next.context, "correction-receipt", &receipt)?.1);
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM manager_input_claims WHERE run_id=?",
                        [&next.context.run],
                        |r| r.get::<_, i64>(0)
                    )?,
                    64
                );
                finish_turn(c, &next.context, "continued goals with latest correction")?;
                let rest = claim(c, &p.id)?.unwrap();
                let context = prompt_context(&rest.checkpoint).unwrap();
                assert_eq!(context["new_inputs"].as_array().unwrap().len(), 9);
                assert_eq!(context["latest_user_steering"]["already_consumed"], true);
                assert_eq!(
                    context["latest_user_steering"]["data"]["user_input"]["body"],
                    "Do not release; preserve the latest correction."
                );
                finish_turn(c, &rest.context, "remaining receipt batch")?;
                assert!(claim(c, &p.id)?.is_none());
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn repeated_user_steering_pages_continuations_without_unbounded_context_or_loss() {
        let (_dir, app, p) = fixture().await;
        // Large trusted metadata makes unconstrained continuation growth exceed
        // the real context ceiling after only a few interruptions.
        for n in 0..32 {
            app.db.ingest_bot(&p.id,"cli_fixture","oc_fixture",&format!("om_page_{n}"),&format!("original-{n}"),&json!({"source":{"chat_id":"oc_fixture","chat_type":"p2p","sender":{"open_id":"ou_fixture"},"extra_metadata":"m".repeat(2800)}})).unwrap();
        }
        let mut previous = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                Ok(claim(c, &p.id)?.unwrap())
            })
            .unwrap();
        for round in 0..5 {
            app.db
                .sync(|c| {
                    for n in 0..36 {
                        input(
                            c,
                            &p.id,
                            &format!("page-worker-{round}-{n}"),
                            None,
                            "worker",
                            &json!({"kind":"worker_run","semantic_task_success":"unconfirmed"}),
                        )?;
                    }
                    Ok(())
                })
                .unwrap();
            ingest(
                &app,
                &p,
                &format!("om_page_steer_{round}"),
                &format!("latest-directive-{round}"),
            );
            previous=app.db.sync(|c|{
                work::sync_inbound(c)?;assert!(valid(c,&previous.context).is_err());let next=claim(c,&p.id)?.unwrap();let context=prompt_context(&next.checkpoint).unwrap();
                assert!(context["continuing_inputs"].as_array().unwrap().len()<=32);
                assert!(context["new_inputs"].as_array().unwrap().len()<=32);
                assert!(context.to_string().len()<=250000);
                assert_eq!(context["new_inputs"].as_array().unwrap().last().unwrap()["data"]["user_input"]["body"],format!("latest-directive-{round}"));Ok(next)
            }).unwrap();
        }
        app.db.sync(|c| {
            // Many inputs have durable owners, but only one bounded page moves
            // to the latest run. Unseen pages are not consumed or re-queued.
            let owned:i64=c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE status='processing'",[],|r|r.get(0))?;
            assert!(owned>64);
            finish_turn(c,&previous.context,"latest steering applied")?;
            assert!(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE status='processing'",[],|r|r.get::<_,i64>(0))?>0);
            for _ in 0..20 {
                let Some(next)=claim(c,&p.id)? else {break};let context=prompt_context(&next.checkpoint).unwrap();
                assert!(context.to_string().len()<=250000);
                assert!(context["latest_user_steering"].to_string().contains("latest-directive-4"));
                finish_turn(c,&next.context,"bounded continuation page consumed")?;
            }
            assert!(claim(c,&p.id)?.is_none());
            assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE status IN ('queued','processing')",[],|r|r.get::<_,i64>(0))?,0);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE status='consumed'",[],|r|r.get::<_,i64>(0))?,32+5*37);Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn continuation_journal_pages_evicted_uncertainty_and_rejects_unowned_inputs() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_journal", "original task");
        app.db.sync(|c| {
            work::sync_inbound(c)?;let original=claim(c,&p.id)?.unwrap();
            let input_id:String=c.query_row("SELECT input_id FROM manager_input_claims WHERE run_id=?",[&original.context.run],|r|r.get(0))?;
            let args=json!({"instructions":"original uncertain delegation"});
            c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status,result) VALUES('old-uncertain',?,'original','delegate',?,'uncertain',?)",params![original.context.run,args.to_string(),json!({"status":"uncertain","event_id":"not-safe-to-replay"}).to_string()])?;
            for n in 0..40 {c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status,result) VALUES(?,?,?,'read_file','{}','complete','{}')",params![format!("recent-{n}"),original.context.run,format!("read-{n}")])?;}
            input(c,&p.id,"journal-user-steer",None,"user",&json!({"user_input":{"body":"inspect prior effects before continuing"}}))?;
            let current=claim(c,&p.id)?.unwrap();let context=prompt_context(&current.checkpoint).unwrap();assert_eq!(context["continuing_inputs"][0]["input_id"],input_id);assert_eq!(context["continuing_inputs"][0]["journal_available"],true);
            let recent=context["effect_evidence"].as_array().unwrap();assert!(!recent.iter().any(|x|x["effect_id"]=="old-uncertain"),"The regression must actually evict the old receipt from the legacy recent window");
            let mut cursor=0;let mut found=false;let mut total=0;
            loop {
                let page=input_effect_journal(c,&current.context,WorkerStatusArgs{input_id:Some(input_id.clone()),after_effect:cursor,detail_offset:0},"current-inspection")?;
                let effects=page["effects"].as_array().unwrap();assert!(effects.len()<=8);assert!(page.to_string().len()<100000);
                for effect in effects {total+=1;if effect["effect_id"]=="old-uncertain" {found=true;assert_eq!(effect["status"],"uncertain");assert!(effect["result_chunk"].as_str().unwrap().contains("not-safe-to-replay"));}}
                match page["next_effect_cursor"].as_i64(){Some(next)=>{assert!(next>cursor);cursor=next;},None=>break}
            }
            assert!(found);assert_eq!(total,41);
            c.execute("INSERT INTO manager_inputs(id,project_id,kind,payload,status,created_at) VALUES('off-page-input',?,'user','{}','processing',1)",[&p.id])?;
            c.execute("INSERT INTO manager_input_claims(input_id,run_id) VALUES('off-page-input',?)",[&original.context.run])?;
            assert!(input_effect_journal(c,&current.context,WorkerStatusArgs{input_id:Some("off-page-input".into()),after_effect:0,detail_offset:0},"inspection").is_err());
            assert!(input_effect_journal(c,&current.context,WorkerStatusArgs{input_id:Some("other-project-or-invented".into()),after_effect:0,detail_offset:0},"inspection").is_err());
            assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_effects WHERE name='delegate'",[],|r|r.get::<_,i64>(0))?,1,"Journal inspection must never replay the uncertain delegation");Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn processing_inputs_obey_pause_and_stop() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_processing_stop", "preserve this instruction");
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let first = claim(c, &p.id)?.unwrap();
                c.execute("UPDATE projects SET paused=1 WHERE id=?", [&p.id])?;
                assert!(claim(c, &p.id)?.is_none());
                assert!(valid(c, &first.context).is_err());
                c.execute("UPDATE projects SET paused=0 WHERE id=?", [&p.id])?;
                assert_eq!(claim(c, &p.id)?.unwrap().context.run, first.context.run);
                let tx = c.unchecked_transaction()?;
                stop(&tx, &p.id)?;
                tx.commit()?;
                assert_eq!(
                    c.query_row("SELECT status FROM manager_inputs", [], |r| r
                        .get::<_, String>(0))?,
                    "processing"
                );
                assert!(valid(c, &first.context).is_err());
                assert_ne!(claim(c, &p.id)?.unwrap().context.run, first.context.run);
                Ok(())
            })
            .unwrap();
    }
    async fn stalled_status_fixture() -> (
        tempfile::TempDir,
        Arc<App>,
        Project,
        Turn,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (dir, app, p) = fixture().await;
        app.config.write().await.herdr_session = dir.path().to_string_lossy().into();
        let config = app.config.read().await.clone();
        let listener =
            tokio::net::UnixListener::bind(crate::herdr::socket_path(&config).unwrap()).unwrap();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = arrived.clone();
        let permit = release.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let line = BufReader::new(read)
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "pane.list");
            seen.notify_one();
            permit.notified().await;
            let response = json!({"id":request["id"],"result":{"panes":[]}}).to_string() + "\n";
            let _ = write.write_all(response.as_bytes()).await;
        });
        ingest(
            &app,
            &p,
            "om_slow_inventory",
            "Inspect only; do not close anything",
        );
        let turn=app.db.sync(|c| {
            work::sync_inbound(c)?;
            c.execute("INSERT INTO work_executions(generation,project_id,agent,terminal,status,last_activity,created_at,updated_at) VALUES('old-inspection',?,'codex','obsolete-terminal','exited',1,1,1)",[&p.id])?;
            Ok(claim(c,&p.id)?.unwrap())
        }).unwrap();
        (dir, app, p, turn, arrived, release, server)
    }
    #[tokio::test]
    async fn stalled_status_does_not_hold_stop_lock_or_publish_stale_observations() {
        let (_dir, app, p, turn, arrived, release, server) = stalled_status_fixture().await;
        let acting = app.clone();
        let authority = turn.context.clone();
        let inspection = tokio::spawn(async move {
            tool(
                &acting,
                &authority,
                ToolCall {
                    occurrence: "stalled-status".into(),
                    name: "worker_status".into(),
                    arguments: json!({}),
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), arrived.notified())
            .await
            .unwrap();
        // The runtime request is still blocked. Stop must acquire the actual
        // project mutex and complete, rather than waiting for the 3s budget.
        app.db.sync(|c| {c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status,result) VALUES('keep-uncertain',?,'prior-send','send_md','{}','uncertain','{}')",[&turn.context.run])?;Ok(())}).unwrap();
        tokio::time::timeout(Duration::from_secs(2), app.stop(&p.id))
            .await
            .unwrap()
            .unwrap();
        assert!(!inspection.is_finished());
        release.notify_one();
        server.await.unwrap();
        let result = inspection.await.unwrap().unwrap();
        assert!(result["error"].is_string());
        assert!(result.get("obsolete_executions").is_none());
        app.db
            .sync(|c| {
                assert!(valid(c, &turn.context).is_err());
                assert_eq!(
                    c.query_row(
                        "SELECT status FROM manager_effects WHERE id='keep-uncertain'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "uncertain"
                );
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM manager_inputs WHERE status='processing'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM work_events WHERE source='management'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn stalled_inventory_has_one_total_budget_and_never_claims_eligibility() {
        let (_dir, app, p, turn, arrived, _release, server) = stalled_status_fixture().await;
        let acting = app.clone();
        let authority = turn.context.clone();
        let inspection = tokio::spawn(async move {
            tool(
                &acting,
                &authority,
                ToolCall {
                    occurrence: "bounded-status".into(),
                    name: "worker_status".into(),
                    arguments: json!({}),
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), arrived.notified())
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), inspection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result["obsolete_executions"][0]["close_eligible"], false);
        assert_eq!(
            result["obsolete_executions"][0]["observation"],
            "incomplete"
        );
        assert!(app.db.sync(|c| valid(c, &turn.context)).is_ok());
        assert!(p.terminal.is_none());
        server.abort();
        let _ = server.await;
    }
    #[test]
    fn prompt_keeps_receipt_cohesion_disclosure_exceptions_and_truthful_failures_together() {
        // These are the explicitly requested policy invariants, not an LLM quality test.
        for policy in [
            "skip acknowledgements and routine progress",
            "intent=progress",
            "Answer simple questions directly",
            "follow-ups, corrections, work results, recovery retries",
            "Keep related work together by default",
            "调研 A 和 B",
            "并行开发互不依赖的 feature A、B、C",
            "The user's natural-language organization overrides this default",
            "Keep internal execution mechanisms private by default",
            "The only exception is an explicit question about the architecture",
            "is the user's actual task",
            "Never hide failures, blockers, unfinished work or capability limits",
        ] {
            assert!(SYSTEM.contains(policy), "Missing policy: {policy}");
        }
        let mut failure = Failure::timeout();
        failure.kind = "rate_limited".into();
        for chinese in [false, true] {
            let notice = failure_notice(&failure, 30_000, true, chinese);
            assert!(!notice.contains("原生编程智能体"));
            assert!(!notice.contains("native Agent"));
            assert!(notice.contains("限流") || notice.contains("rate limited"));
            assert!(notice.contains("保留") || notice.contains("saved"));
        }
    }
    #[tokio::test]
    async fn receipt_is_immutable_across_turns_and_does_not_redispatch_existing_states() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_receipt", "Please research A and B together.");
        let (next, id, attachment) = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                let first = claim(c, &p.id)?.unwrap();
                let args = receipt_args("om_receipt");
                let (id, fresh) = enqueue_receipt(c, &first.context, "original-effect", &args)?;
                assert!(fresh);
                let raw: String =
                    c.query_row("SELECT attachment FROM messages WHERE id=?", [&id], |r| {
                        r.get(0)
                    })?;
                let attachment: Value = serde_json::from_str(&raw)?;
                input(
                    c,
                    &p.id,
                    "new-observation",
                    None,
                    "user",
                    &json!({"status":"working"}),
                )?;
                let next = claim(c, &p.id)?.unwrap();
                assert_ne!(first.context.run, next.context.run);
                // The original receipt retains authority when its model checkpoint is superseded.
                validate_outbox(c, &p.id, &attachment)?;
                Ok((next, id, attachment))
            })
            .unwrap();
        for status in ["pending", "sent", "uncertain", "failed"] {
            app.db
                .sync(|c| {
                    c.execute(
                        "UPDATE messages SET status=? WHERE id=?",
                        params![status, id],
                    )?;
                    Ok(())
                })
                .unwrap();
            let result = tool(&app, &next.context, ToolCall {
                occurrence: format!("regenerated-{status}"), name: "send_md".into(),
                arguments: json!({"to":"oc_fixture","text":"Different regenerated wording", "reply_to_message_id":"om_different", "intent":"receipt"}),
            }).await.unwrap();
            assert_eq!(result["outbox_id"], id);
            assert_eq!(result["status"], status);
        }
        app.db
            .sync(|c| {
                let (text, raw): (String, String) = c.query_row(
                    "SELECT content,attachment FROM messages WHERE id=?",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                assert_eq!(text, receipt_args("om_receipt").text);
                assert_eq!(serde_json::from_str::<Value>(&raw)?, attachment);
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE direction='out'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn receipts_are_source_scoped_and_reject_worker_local_or_foreign_targets() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_scope", "Task");
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let turn = claim(c, &p.id)?.unwrap();
                let mut args = receipt_args("om_scope");
                args.to = "oc_foreign".into();
                assert!(enqueue_receipt(c, &turn.context, "bad-target", &args).is_err());
                args.to = "oc_fixture".into();
                args.reply_to_message_id = Some("om_foreign".into());
                assert!(enqueue_receipt(c, &turn.context, "bad-anchor", &args).is_err());
                args.reply_to_message_id = Some("om_scope".into());
                args.source_event_id = Some("invented-event".into());
                assert!(enqueue_receipt(c, &turn.context, "bad-origin", &args).is_err());
                args.source_event_id = None;
                for kind in ["worker", "schedule"] {
                    c.execute(
                        "UPDATE manager_inputs SET kind=? WHERE project_id=?",
                        params![kind, p.id],
                    )?;
                    assert!(enqueue_receipt(c, &turn.context, "non-user-ack", &args).is_err());
                }
                c.execute(
                    "UPDATE manager_inputs SET kind='user' WHERE project_id=?",
                    [&p.id],
                )?;
                c.execute(
                    "UPDATE messages SET attachment='{}' WHERE direction='in' AND project_id=?",
                    [&p.id],
                )?;
                assert!(enqueue_receipt(c, &turn.context, "local-ack", &args).is_err());
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE direction='out'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn receipt_pause_takeover_and_stop_cannot_resurrect_a_consumed_inputs_pending_ack() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_stop_receipt", "Task");
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let first = claim(c, &p.id)?.unwrap();
                let (id, _) = enqueue_receipt(
                    c,
                    &first.context,
                    "receipt-effect",
                    &receipt_args("om_stop_receipt"),
                )?;
                let raw: String =
                    c.query_row("SELECT attachment FROM messages WHERE id=?", [&id], |r| {
                        r.get(0)
                    })?;
                let attachment: Value = serde_json::from_str(&raw)?;
                c.execute("UPDATE projects SET paused=1 WHERE id=?", [&p.id])?;
                assert!(validate_outbox(c, &p.id, &attachment).is_err());
                c.execute(
                    "UPDATE projects SET paused=0,generation='g' WHERE id=?",
                    [&p.id],
                )?;
                work::set_takeover(c, &p.id, "g", true, db::now())?;
                assert!(validate_outbox(c, &p.id, &attachment).is_err());
                work::set_takeover(c, &p.id, "g", false, db::now())?;
                finish_turn(c, &first.context, "done")?;
                validate_outbox(c, &p.id, &attachment)?;
                let tx = c.unchecked_transaction()?;
                stop(&tx, &p.id)?;
                tx.commit()?;
                c.execute("UPDATE projects SET paused=0 WHERE id=?", [&p.id])?;
                input(
                    c,
                    &p.id,
                    "worker-after-stop",
                    attachment["manager_receipt_origin"].as_str(),
                    "worker",
                    &json!({"status":"done"}),
                )?;
                let after_stop = claim(c, &p.id)?.unwrap();
                let (same, dispatch) = enqueue_receipt(
                    c,
                    &after_stop.context,
                    "late-receipt",
                    &receipt_args("om_stop_receipt"),
                )?;
                assert_eq!(same, id);
                assert!(!dispatch);
                // Even if a later turn still references the original source, the stable failed
                // outbox entry is never requeued and send_out will never dispatch it again.
                assert_eq!(
                    c.query_row("SELECT status FROM messages WHERE id=?", [&id], |r| r
                        .get::<_, String>(0))?,
                    "failed"
                );
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE direction='out'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn failures_are_incident_scoped_backoff_survives_observations_and_valid_response_recovers()
     {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_retry", "请修复问题，保留输入");
        let turn = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                Ok(claim(c, &p.id)?.unwrap())
            })
            .unwrap();
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status,result) VALUES('uncertain-effect',?,'old','send_md','{}','uncertain','{}')",[&turn.context.run])?;
            for attempt in 1..=4 {
                failure(c,&turn.context,&Failure::timeout())?;
                let state=public(c,&p.id)?;assert_eq!(state["failure"]["attempt"],attempt);
                assert_eq!(c.query_row("SELECT COUNT(*) FROM messages WHERE json_extract(attachment,'$.manager_notice')=1",[],|r|r.get::<_,i64>(0))?,0);
            }
            assert_eq!(c.query_row("SELECT COUNT(*) FROM work_events WHERE source='management'",[],|r|r.get::<_,i64>(0))?,1);
            let retry:i64=c.query_row("SELECT next_attempt_at FROM manager_config WHERE project_id=?",[&p.id],|r|r.get(0))?;
            input(c,&p.id,"observation-new",None,"worker",&json!({"status":"working"}))?;
            assert_eq!(c.query_row("SELECT next_attempt_at FROM manager_config WHERE project_id=?",[&p.id],|r|r.get::<_,i64>(0))?,retry);
            assert!(claim(c,&p.id)?.is_none());
            Ok(())
        }).unwrap();
        ingest(&app, &p, "om_retry_new", "补充新的要求");
        let next = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                let next = claim(c, &p.id)?.unwrap();
                failure(c, &next.context, &Failure::timeout())?;
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM messages WHERE direction='out'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(next)
            })
            .unwrap();
        let mut corrected = next.checkpoint.clone();
        corrected["last_correction"] = json!({"kind":"invalid_response","http_status":200,"retryable":false,"elapsed_ms":21000,"output_tokens":4096,"finish_reason":"length"});
        checkpoint(&app, &next.context, &corrected, false)
            .await
            .unwrap();
        assert!(
            !app.db
                .sync(|c| Ok(public(c, &p.id)?["error"].as_str().unwrap().is_empty()))
                .unwrap()
        );
        checkpoint(&app, &next.context, &corrected, true)
            .await
            .unwrap();
        app.db
            .sync(|c| {
                let state = public(c, &p.id)?;
                assert_eq!(state["error"], "");
                assert!(state["failure"].is_null());
                let raw: String = c.query_row(
                    "SELECT error FROM manager_runs WHERE id=?",
                    [&next.context.run],
                    |r| r.get(0),
                )?;
                let retained: Value = serde_json::from_str(&raw)?;
                assert_eq!(retained["last_failure"]["kind"], "timeout");
                assert_eq!(retained["last_correction"]["kind"], "invalid_response");
                assert_eq!(retained["last_correction"]["output_tokens"], 4096);
                assert_eq!(
                    c.query_row(
                        "SELECT status FROM work_events WHERE source='management'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "cancelled"
                );
                assert_eq!(
                    c.query_row(
                        "SELECT status FROM manager_effects WHERE id='uncertain-effect'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "uncertain"
                );
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM manager_inputs WHERE status='consumed'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                failure(c, &next.context, &Failure::timeout())?;
                assert_eq!(public(c, &p.id)?["failure"]["attempt"], 1);
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM messages WHERE direction='out'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn permanent_failure_prepares_one_scoped_repair_without_forwarding_input_and_honors_controls()
     {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_permanent", "ordinary-private-task-marker");
        let turn = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                Ok(claim(c, &p.id)?.unwrap())
            })
            .unwrap();
        app.db
            .sync(|c| {
                let mut error = Failure::internal();
                error.kind = "context_rejected".into();
                error.http_status = Some(400);
                failure(c, &turn.context, &error)?;
                failure(c, &turn.context, &error)?;
                assert!(claim(c, &p.id)?.is_none());
                let (id, raw): (String, String) = c.query_row(
                    "SELECT id,payload FROM work_events WHERE source='management'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let payload: Value = serde_json::from_str(&raw)?;
                assert!(payload["origin_event_id"].is_null());
                assert!(payload["repair_incident"].is_string());
                assert!(!raw.contains("ordinary-private-task-marker"));
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM work_events WHERE source='management'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                c.execute(
                    "UPDATE manager_config SET revision=revision+1 WHERE project_id=?",
                    [&p.id],
                )?;
                let event = work::event(c, &p.id, &id)?;
                assert!(!bind_event(c, &event, "generation")?);
                assert_eq!(
                    c.query_row("SELECT status FROM work_events WHERE id=?", [id], |r| {
                        r.get::<_, String>(0)
                    })?,
                    "cancelled"
                );
                Ok(())
            })
            .unwrap();
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_controls", "preserve");
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let turn = claim(c, &p.id)?.unwrap();
                c.execute("UPDATE projects SET paused=1 WHERE id=?", [&p.id])?;
                failure(c, &turn.context, &Failure::internal())?;
                c.execute(
                    "UPDATE projects SET paused=0,generation='g' WHERE id=?",
                    [&p.id],
                )?;
                work::set_takeover(c, &p.id, "g", true, db::now())?;
                failure(c, &turn.context, &Failure::internal())?;
                work::set_takeover(c, &p.id, "g", false, db::now())?;
                stop(c, &p.id)?;
                failure(c, &turn.context, &Failure::internal())?;
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM manager_failures", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM work_events WHERE source='management'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn oversized_claim_preserves_input_and_schedules_bounded_native_repair_instead_of_forever_retry()
     {
        let (_dir, app, p) = fixture().await;
        app.db.ingest_bot(&p.id,"cli_fixture","oc_fixture","om_large_context","preserve the task",&json!({"source":{"chat_id":"oc_fixture","chat_type":"p2p","extra_metadata":"x".repeat(300000)}})).unwrap();
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                assert!(claim_or_repair(c, &p.id)?.is_none());
                let state = public(c, &p.id)?;
                assert_eq!(state["failure"]["kind"], "context_preparation");
                assert_eq!(state["failure"]["retryable"], false);
                assert!(claim_or_repair(c, &p.id)?.is_none());
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM work_events WHERE source='management'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                assert_eq!(
                    c.query_row("SELECT status FROM manager_inputs", [], |r| r
                        .get::<_, String>(0))?,
                    "queued"
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn only_current_repair_completion_wakes_one_fresh_probe_and_never_cancels_submitted_work()
    {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_probe", "ordinary request");
        app.db.sync(|c| {
            work::sync_inbound(c)?;let first=claim(c,&p.id)?.unwrap();failure(c,&first.context,&Failure::internal())?;
            let (id,task):(String,String)=c.query_row("SELECT id,task_id FROM work_events WHERE source='management'",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
            c.execute("UPDATE work_events SET status='submitted' WHERE id=?",[&id])?;
            c.execute("INSERT INTO work_runs(id,project_id,task_id,event_id,generation,status,started_at,updated_at,finished_at) VALUES('repair-run',?,?,?,'generation','settled',?,?,?)",params![p.id,task,id,db::now(),db::now(),db::now()])?;
            worker_changed(c,&p.id)?;
            assert_eq!(c.query_row("SELECT next_attempt_at FROM manager_config WHERE project_id=?",[&p.id],|r|r.get::<_,i64>(0))?,0);
            let next=claim(c,&p.id)?.unwrap();assert_ne!(first.context.run,next.context.run);
            failure(c,&next.context,&Failure::internal())?;
            worker_changed(c,&p.id)?;
            assert!(claim(c,&p.id)?.is_none());
            assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE id LIKE 'repair-probe-%'",[],|r|r.get::<_,i64>(0))?,1);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM work_events WHERE source='management'",[],|r|r.get::<_,i64>(0))?,1);
            recovered(c,&next.context)?;
            assert_eq!(c.query_row("SELECT status FROM work_events WHERE id=?",[id],|r|r.get::<_,String>(0))?,"submitted");
            assert_ne!(c.query_row("SELECT status FROM work_tasks WHERE id=?",[task],|r|r.get::<_,String>(0))?,"cancelled");
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn queued_incident_notices_are_fenced_by_recovery_pause_takeover_and_stop() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_notice", "request");
        app.db.sync(|c| {
            work::sync_inbound(c)?;let turn=claim(c,&p.id)?.unwrap();
            failure(c,&turn.context,&Failure::timeout())?;
            assert_eq!(c.query_row("SELECT count(*) FROM messages WHERE direction='out'",[],|r|r.get::<_,i64>(0))?,0);
            let mut actionable=Failure::internal();
            actionable.kind="authorization".into();
            failure(c,&turn.context,&actionable)?;
            let raw:String=c.query_row("SELECT attachment FROM messages WHERE direction='out'",[],|r|r.get(0))?;
            let notice:Value=serde_json::from_str(&raw)?;validate_outbox(c,&p.id,&notice)?;
            c.execute("UPDATE projects SET paused=1 WHERE id=?",[&p.id])?;assert!(validate_outbox(c,&p.id,&notice).is_err());
            c.execute("UPDATE projects SET paused=0,generation='generation' WHERE id=?",[&p.id])?;
            work::set_takeover(c,&p.id,"generation",true,db::now())?;assert!(validate_outbox(c,&p.id,&notice).is_err());
            work::set_takeover(c,&p.id,"generation",false,db::now())?;
            recovered(c,&turn.context)?;assert!(validate_outbox(c,&p.id,&notice).is_err());
            failure(c,&turn.context,&actionable)?;
            let raw:String=c.query_row("SELECT attachment FROM messages WHERE direction='out' ORDER BY rowid DESC LIMIT 1",[],|r|r.get(0))?;
            let notice:Value=serde_json::from_str(&raw)?;validate_outbox(c,&p.id,&notice)?;
            stop(c,&p.id)?;assert!(validate_outbox(c,&p.id,&notice).is_err());
            assert_eq!(c.query_row("SELECT status FROM work_tasks WHERE title='Management delegation'",[],|r|r.get::<_,String>(0))?,"cancelled");
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn stop_preserves_queued_inputs_without_dispatch_while_paused() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_before_stop", "preserve until resumed");
        app.db
            .sync(|c| {
                crate::cli_switch::stopped(c, &p.id, &p.generation)?;
                work::sync_inbound(c)?;
                assert_eq!(
                    c.query_row("SELECT status FROM manager_inputs", [], |r| r
                        .get::<_, String>(0))?,
                    "queued"
                );
                assert!(claim(c, &p.id)?.is_none());
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn off_batch_latest_user_control_preserves_its_tail_and_counts_toward_context_budget() {
        for older_size in [20, 8000] {
            let (_dir, app, p) = fixture().await;
            for n in 0..32 {
                ingest(
                    &app,
                    &p,
                    &format!("om_tail_old_{n}"),
                    &format!("old instruction {n}: {}", "x".repeat(older_size)),
                );
            }
            let latest = format!(
                "{}\n不要发布。Do not release or deploy.",
                "latest control context ".repeat(150)
            );
            assert!(latest.len() > 2400);
            ingest(&app, &p, "om_tail_latest", &latest);
            app.db.sync(|c|{
                work::sync_inbound(c)?;let turn=claim(c,&p.id)?.unwrap();let context=prompt_context(&turn.checkpoint).unwrap();
                let batch=context["new_inputs"].as_array().unwrap();assert!(!batch.is_empty()&&batch.len()<=32);
                for (n,item) in batch.iter().enumerate(){assert!(item["data"]["user_input"]["body"].as_str().unwrap().starts_with(&format!("old instruction {n}: ")));}
                assert_eq!(context["latest_user_steering"]["data"]["user_input"]["body"],latest);
                assert_eq!(context["latest_user_steering"]["context_only_not_batch_member"],true);
                assert_eq!(context["latest_user_steering"]["present_in_new_inputs"],false);
                assert!(context.to_string().len()<=250000);
                assert_eq!(c.query_row("SELECT i.status FROM manager_inputs i JOIN work_events e ON e.id=i.origin_event_id WHERE e.source_id='om_tail_latest'",[],|r|r.get::<_,String>(0))?,"queued");
                if older_size==20 {assert_eq!(batch.len(),32);}else{assert!(batch.len()<32,"The complete mandatory control must share the same bounded context budget");}
                Ok(())
            }).unwrap();
        }
    }
    #[tokio::test]
    async fn exact_batch_completion_keeps_the_33rd_input_and_settles_managed_schedule() {
        let (_dir, app, p) = fixture().await;
        for n in 0..33 {
            ingest(
                &app,
                &p,
                &format!("om_{n}"),
                &if n == 32 {
                    "不要发版".to_owned()
                } else {
                    format!("bounded instruction {n}")
                },
            );
        }
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let first = claim(c, &p.id)?.unwrap();
            let prompt=prompt_context(&first.checkpoint).unwrap();assert_eq!(prompt["new_inputs"][0]["data"]["user_input"]["body"],"bounded instruction 0");assert_eq!(prompt["new_inputs"][31]["data"]["user_input"]["body"],"bounded instruction 31");assert_eq!(prompt["latest_user_steering"]["data"]["user_input"]["body"],"不要发版","Newest boundary remains mandatory even outside the FIFO batch");
                finish_turn(c, &first.context, "")?;
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM manager_inputs WHERE status='consumed'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    32
                );
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM manager_inputs WHERE status='queued'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                let second = claim(c, &p.id)?.unwrap();
            assert!(prompt_context(&second.checkpoint).unwrap()["latest_user_steering"].to_string().contains("不要发版"),"Remaining old backlog must retain the newest explicit user boundary without inheriting its reply anchor");
                finish_turn(c, &second.context, "completed bounded backlog")?;
                assert!(claim(c, &p.id)?.is_none());
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM work_events WHERE status='consumed'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    33
                );
                Ok(())
            })
            .unwrap();
        app.db
            .sync(|c| {
                let now = db::now();
                work::create_timer(
                    c,
                    &p.id,
                    work::TimerInput {
                        name: "Managed repeat".into(),
                        prompt: "inspect scheduled work".into(),
                        kind: "interval".into(),
                        at: None,
                        interval_seconds: Some(1),
                        cron: None,
                        timezone: "UTC".into(),
                        to: Some("oc_fixture".into()),
                        purpose: "user".into(),
                        request_id: None,
                    },
                    now,
                )?;
                let fired = work::fire_due(c, now + 1000)?;
                assert_eq!(fired.len(), 1);
                let turn = claim(c, &p.id)?.unwrap();
                finish_turn(c, &turn.context, "")?;
                assert_eq!(
                    work::fire_due(c, now + 2000)?.len(),
                    1,
                    "Consumed managed timer must not suppress every future occurrence"
                );
                Ok(())
            })
            .unwrap();
        let (_dir, app, p) = fixture().await;
        for n in 0..12 {
            ingest(&app, &p, &format!("om_budget_{n}"), &"x".repeat(80000));
        }
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let turn = claim(c, &p.id)?.unwrap();
                let chosen: i64 = c.query_row(
                    "SELECT COUNT(*) FROM manager_run_inputs WHERE run_id=?",
                    [&turn.context.run],
                    |r| r.get(0),
                )?;
                assert!(chosen > 0 && chosen < 12);
                finish_turn(c, &turn.context, "")?;
                assert_eq!(
                    c.query_row(
                        "SELECT COUNT(*) FROM manager_inputs WHERE status='queued'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    12 - chosen
                );
                assert!(
                    claim(c, &p.id)?.is_some(),
                    "The next fitting batch must continue automatically"
                );
                Ok(())
            })
            .unwrap();
        let (_dir, app, p) = fixture().await;
        ingest(
            &app,
            &p,
            "om_large_body",
            &"large original input ".repeat(25000),
        );
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let turn = claim(c, &p.id)?.unwrap();
                let prompt = prompt_context(&turn.checkpoint).unwrap();
                assert_eq!(prompt["new_inputs"][0]["data"]["full_input_retained"], true);
                assert!(select_origin(c, &turn.context, None)?.is_some());
                let bytes: i64 = c.query_row(
                    "SELECT length(content) FROM messages WHERE source_id='om_large_body'",
                    [],
                    |r| r.get(0),
                )?;
                assert!(
                    bytes > 400000,
                    "Only the model prompt preview may be bounded; original input stays intact"
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn fresh_input_fences_stale_effects_and_recovered_submissions_remain_uncertain() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_first", "initial instructions");
        let turn = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                Ok(claim(c, &p.id)?.unwrap())
            })
            .unwrap();
        ingest(&app, &p, "om_steer", "new steering");
        app.db.sync(work::sync_inbound).unwrap();
        let result = tool(
            &app,
            &turn.context,
            ToolCall {
                occurrence: "turn:1:call:0".into(),
                name: "write_file".into(),
                arguments: json!({"path":"stale.txt","content":"late effect"}),
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!Path::new(&p.path).join("stale.txt").exists());
        app.db.sync(|c|{let next=claim(c,&p.id)?.unwrap();c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES('uncertain-effect',?,'turn:1:call:0','delegate','{}','submitting')",[next.context.run])?;migrate(c,true)?;assert_eq!(c.query_row("SELECT status FROM manager_effects WHERE id='uncertain-effect'",[],|r|r.get::<_,String>(0))?,"uncertain");Ok(())}).unwrap();
    }
    fn prompt_context(value: &Value) -> Option<Value> {
        match value {
            Value::String(text) => serde_json::from_str::<Value>(text)
                .ok()
                .filter(|v| v.get("trusted_sources").is_some()),
            Value::Array(all) => all.iter().find_map(prompt_context),
            Value::Object(all) => all.values().find_map(prompt_context),
            _ => None,
        }
    }
    #[tokio::test]
    async fn late_worker_result_keeps_its_actual_source_and_originless_results_do_not_guess() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_topic_A", "coding task A");
        let origin_a = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                let turn = claim(c, &p.id)?.unwrap();
                let origin = select_origin(c, &turn.context, None)?.unwrap();
                intent(c, &p, "delegate-A", "work on A", Some(&origin), false)?;
                finish_turn(c, &turn.context, "")?;
                Ok(origin)
            })
            .unwrap();
        ingest(&app, &p, "om_topic_B", "unrelated discussion B");
        let origin_b = app
            .db
            .sync(|c| {
                work::sync_inbound(c)?;
                let turn = claim(c, &p.id)?.unwrap();
                let origin = select_origin(c, &turn.context, None)?.unwrap();
                finish_turn(c, &turn.context, "")?;
                Ok(origin)
            })
            .unwrap();
        app.db.sync(|c|{
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES('reply-A',?,'out','worker-reply-A','Already delivered A result','sent',?, ?,?)",params![p.id,json!({"to":"oc_fixture","reply_to_message_id":"om_topic_A","sent_message_id":"om_sent_A"}).to_string(),db::now(),db::now()])?;
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES('manager-own',?,'out','manager-own-reply','Do not loop this manager output','sent',?,?,?)",params![p.id,json!({"manager_effect":"owned"}).to_string(),db::now(),db::now()])?;
            worker_changed(c,&p.id)?;worker_changed(c,&p.id)?;
            assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE kind='worker'",[],|r|r.get::<_,i64>(0))?,1);
            let turn=claim(c,&p.id)?.unwrap();let prompt=prompt_context(&turn.checkpoint).unwrap();
            assert_eq!(prompt["trusted_sources"][0]["message_id"],"om_topic_A");assert_eq!(prompt["trusted_sources"].as_array().unwrap().len(),1);
            assert_eq!(select_origin(c,&turn.context,None)?.as_deref(),Some(origin_a.as_str()));assert!(select_origin(c,&turn.context,Some(&origin_b)).is_err());finish_turn(c,&turn.context,"")?;
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES('originless',?,'out','worker-notification','Independent notification','sent','{}',?,?)",params![p.id,db::now(),db::now()])?;worker_changed(c,&p.id)?;
            let next=claim(c,&p.id)?.unwrap();assert_eq!(prompt_context(&next.checkpoint).unwrap()["trusted_sources"],json!([]));assert!(select_origin(c,&next.context,None)?.is_none());finish_turn(c,&next.context,"")?;Ok(())
        }).unwrap();
        ingest(&app, &p, "om_C", "thread C");
        ingest(&app, &p, "om_D", "thread D");
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                let turn = claim(c, &p.id)?.unwrap();
                assert!(select_origin(c, &turn.context, None).is_err());
                let sources = prompt_context(&turn.checkpoint).unwrap()["trusted_sources"].clone();
                let chosen = sources[0]["event_id"].as_str().unwrap();
                assert_eq!(
                    select_origin(c, &turn.context, Some(chosen))?.as_deref(),
                    Some(chosen)
                );
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn explicit_cli_steer_waits_for_selected_generation_and_preserves_recovery() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_target_cli", "Use agy for this task");
        let turn = app.db.sync(|c| { work::sync_inbound(c)?; Ok(claim(c, &p.id)?.unwrap()) }).unwrap();
        let call = ToolCall { occurrence: "targeted-steer".into(), name: "steer_worker".into(),
            arguments: json!({"instructions":"selected CLI assignment","target_cli":"agy"}) };
        let receipt = tool(&app, &turn.context, call.clone()).await.unwrap();
        assert_eq!(receipt["status"], "held");
        assert_eq!(receipt["worker_agent"], "agy");
        assert_eq!(receipt["generation"], Value::Null);
        assert_eq!(tool(&app, &turn.context, call).await.unwrap(), receipt);
        let transition = receipt["transition_id"].as_str().unwrap().to_owned();
        let event_id = receipt["event_id"].as_str().unwrap().to_owned();
        app.db.sync(|c| {
            assert_eq!(db::project_by_key(c, &p.id)?.agent, "codex");
            assert!(crate::cli_switch::holds_input(c, &p.id)?);
            assert!(work::claim_next(c, &p.id, "old-generation", db::now())?.is_none());
            c.execute("UPDATE cli_switch_requests SET phase='recovery_pending',error='Fixture target is not installed' WHERE id=?", [&transition])?;
            assert!(work::claim_next(c, &p.id, "wrong-generation", db::now())?.is_none());
            assert_eq!(work::event(c, &p.id, &event_id)?.status, "queued");
            // A manager correction remains claimable while ordinary worker input is held.
            Ok(())
        }).unwrap();
        ingest(&app, &p, "om_correct_during_switch", "Keep the input; report the unavailable CLI");
        let correction = app.db.sync(|c| {work::sync_inbound(c)?;Ok(claim(c,&p.id)?.unwrap())}).unwrap();
        assert!(prompt_context(&correction.checkpoint).unwrap().to_string().contains("report the unavailable CLI"));
        assert!(app.db.sync(|c|crate::cli_switch::holds_input(c,&p.id)).unwrap());
        app.db.sync(|c| {
            c.execute("UPDATE projects SET agent='agy',generation='ready-agy',terminal='fixture-agy',ready=1,execution=? WHERE id=?",
                params![json!({"mcp_registered":true,"cli_switch_request":transition}).to_string(),p.id])?;
            c.execute("UPDATE cli_switch_requests SET phase='ready',holds_input=0,error='' WHERE id=?",[&transition])?;
            let native = work::claim_next(c,&p.id,"ready-agy",db::now())?.unwrap();
            assert_eq!(native.event_id,event_id);
            assert_eq!(native.generation,"ready-agy");
            let event=work::event(c,&p.id,&event_id)?;
            assert_eq!(event.payload["expected_agent"],"agy");
            assert_eq!(event.payload["target_generation"],"ready-agy");
            assert_eq!(event.payload["steer"],false);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM cli_switch_requests",[],|r|r.get::<_,i64>(0))?,1);
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn cli_target_validation_and_legacy_steering_are_atomic() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c| {c.execute("UPDATE projects SET generation='current-generation',terminal='current-pane',ready=1,execution=? WHERE id=?",params![json!({"api_protocol":7,"mcp_contract":1}).to_string(),p.id])?;Ok(())}).unwrap();
        ingest(&app,&p,"om_legacy_cli","Keep current CLI unless explicitly requested");
        let turn=app.db.sync(|c|{work::sync_inbound(c)?;Ok(claim(c,&p.id)?.unwrap())}).unwrap();
        let old=tool(&app,&turn.context,ToolCall{occurrence:"legacy-steer".into(),name:"steer_worker".into(),arguments:json!({"instructions":"current CLI input"})}).await.unwrap();
        assert_eq!(old["status"],"queued");assert_eq!(old["worker_agent"],"codex");assert_eq!(old["generation"],"current-generation");
        for (n,args) in [json!({"instructions":"never fallback","target_cli":"not-a-cli"}),json!({"instructions":"","target_cli":"claude"})].into_iter().enumerate() {
            let rejected=tool(&app,&turn.context,ToolCall{occurrence:format!("rejected-target-{n}"),name:"delegate".into(),arguments:args}).await.unwrap();
            assert!(rejected["error"].is_string());
        }
        app.db.sync(|c| {
            assert_eq!(c.query_row("SELECT COUNT(*) FROM cli_switch_requests",[],|r|r.get::<_,i64>(0))?,0);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM work_events WHERE source='management'",[],|r|r.get::<_,i64>(0))?,1);
            let event=work::event(c,&p.id,old["event_id"].as_str().unwrap())?;assert_eq!(event.payload["steer"],true);
            Ok(())
        }).unwrap();
        let selected=tool(&app,&turn.context,ToolCall{occurrence:"select-claude".into(),name:"delegate".into(),arguments:json!({"instructions":"claude work","target_cli":"claude"})}).await.unwrap();
        assert_eq!(selected["status"],"held");assert_eq!(selected["worker_agent"],"claude");
        let conflict=tool(&app,&turn.context,ToolCall{occurrence:"conflicting-target".into(),name:"steer_worker".into(),arguments:json!({"instructions":"do not replace active switch","target_cli":"agy"})}).await.unwrap();
        assert!(conflict["error"].as_str().unwrap().contains("handoff is already in progress"));
        app.db.sync(|c| {assert_eq!(crate::cli_switch::latest(c,&p.id)?.unwrap().target,"claude");assert_eq!(c.query_row("SELECT COUNT(*) FROM work_events WHERE source='management'",[],|r|r.get::<_,i64>(0))?,2);Ok(())}).unwrap();
    }

    #[tokio::test]
    async fn running_manager_supersedes_blocked_provider_before_its_response() {
        use axum::{Json, Router, extract::State, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        #[derive(Clone)]
        struct Provider {
            requests: Arc<AtomicUsize>,
            bodies: tokio::sync::mpsc::UnboundedSender<Value>,
            release: Arc<tokio::sync::Notify>,
        }
        async fn respond(State(p): State<Provider>,Json(body):Json<Value>)->Json<Value> {
            let index=p.requests.fetch_add(1,Ordering::SeqCst);
            p.bodies.send(body).unwrap();
            if index==0 {p.release.notified().await;}
            let calls=if index==0 {json!([{"id":"stale-write","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"stale-provider.txt\",\"content\":\"must not execute\"}"}}])}else{Value::Null};
            Json(json!({"id":"fixture","object":"chat.completion","created":1,"model":"fixture-manager","choices":[{"index":0,"message":{"role":"assistant","content":if index==0{Value::Null}else{json!("Accepted the latest user correction")},"tool_calls":calls},"finish_reason":if index==0{"tool_calls"}else{"stop"}}]}))
        }
        let (_dir,app,p)=fixture().await;
        let (send,mut bodies)=tokio::sync::mpsc::unbounded_channel();
        let provider=Provider{requests:Arc::new(AtomicUsize::new(0)),bodies:send,release:Arc::new(tokio::sync::Notify::new())};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let router=Router::new().route("/v1/chat/completions",post(respond)).with_state(provider.clone());
        let server=tokio::spawn(async move{axum::serve(listener,router).await.unwrap()});
        app.db.sync(|c| {let current=public(c,&p.id)?;save_provider(c,ConfigInput{base_url:format!("http://{addr}/v1"),model:"fixture-manager".into(),api_key:None,clear_api_key:false,expected_revision:current["revision"].as_i64().unwrap(),supports_images:None})?;Ok(())}).unwrap();
        let binding=crate::bot::Binding{project_id:p.id.clone(),app_id:"cli_fixture".into(),app_secret:"fixture-bot-secret".into()};
        let input=|id:&str,text:&str|serde_json::to_vec(&json!({"header":{"app_id":"cli_fixture","event_type":"im.message.receive_v1"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_fixture"}},"message":{"message_id":id,"chat_id":"oc_fixture","chat_type":"p2p","message_type":"text","content":json!({"text":text}).to_string()}}})).unwrap();
        crate::feishu::ingest(&app,&binding,&input("om_first_blocked","Start original task")).unwrap();
        app.db.sync(|c| {work::sync_inbound(c)?;Ok(())}).unwrap();
        let acting=app.clone();let host=tokio::spawn(async move{run(acting).await});
        let first=tokio::time::timeout(Duration::from_secs(3),bodies.recv()).await.unwrap().unwrap();
        assert!(first.to_string().contains("Start original task"));
        let started=std::time::Instant::now();
        crate::feishu::ingest(&app,&binding,&input("om_second_interrupt","Replace direction immediately")).unwrap();
        app.db.sync(|c| {work::sync_inbound(c)?;Ok(())}).unwrap();
        let second=tokio::time::timeout(Duration::from_secs(3),bodies.recv()).await.unwrap().unwrap();
        eprintln!("manager_append_fixture_admission_to_provider_ms={}",started.elapsed().as_millis());
        assert!(second.to_string().contains("Replace direction immediately"));
        assert!(!Path::new(&p.path).join("stale-provider.txt").exists());
        provider.release.notify_one();
        tokio::time::timeout(Duration::from_secs(3),async {
            loop {
                if app.db.sync(|c|Ok(c.query_row("SELECT COUNT(*) FROM manager_inputs WHERE project_id=? AND status IN ('queued','processing')",[&p.id],|r|r.get::<_,i64>(0))?)).unwrap()==0 {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        app.shutdown.cancel();host.await.unwrap();server.abort();let _=server.await;
        assert!(!Path::new(&p.path).join("stale-provider.txt").exists());
        assert_eq!(app.db.sync(|c|Ok(c.query_row("SELECT COUNT(*) FROM manager_effects WHERE name='write_file'",[],|r|r.get::<_,i64>(0))?)).unwrap(),0);
    }

    #[tokio::test]
    async fn delegate_waits_for_exact_selected_transition_and_abandoned_intents_do_not_block() {
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_transition", "code on selected Pi when ready");
        let turn=app.db.sync(|c|{work::sync_inbound(c)?;c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,revision,holds_input) VALUES('selected-transition',?,'pi','preparing','source-generation',?,'revision',1)",params![p.id,db::now()])?;Ok(claim(c,&p.id)?.unwrap())}).unwrap();
        let reply = tool(
            &app,
            &turn.context,
            ToolCall {
                occurrence: "turn:1:call:0".into(),
                name: "delegate".into(),
                arguments: json!({"instructions":"run the selected coding task"}),
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["status"], "held");
        assert_eq!(reply["worker_agent"], "pi");
        assert_eq!(reply["generation"], Value::Null);
        app.db.sync(|c|{assert!(work::claim_next(c,&p.id,"old-generation",db::now())?.is_none());c.execute("UPDATE projects SET agent='pi',generation='selected-generation',terminal='immutable-pi',ready=1,execution=? WHERE id=?",params![json!({"mcp_registered":true,"cli_switch_request":"selected-transition"}).to_string(),p.id])?;c.execute("UPDATE cli_switch_requests SET phase='ready',holds_input=0 WHERE id='selected-transition'",[])?;let run=work::claim_next(c,&p.id,"selected-generation",db::now())?.unwrap();assert_eq!(run.generation,"selected-generation");let event=work::event(c,&p.id,&run.event_id)?;assert_eq!(event.payload["target_generation"],"selected-generation");Ok(())}).unwrap();
        let (_dir, app, p) = fixture().await;
        ingest(&app, &p, "om_obsolete", "preserve abandoned target intent");
        let turn=app.db.sync(|c|{work::sync_inbound(c)?;c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,revision,holds_input) VALUES('abandoned',?,'pi','preparing','old',?,'revision',1)",params![p.id,db::now()])?;Ok(claim(c,&p.id)?.unwrap())}).unwrap();
        let reply = tool(
            &app,
            &turn.context,
            ToolCall {
                occurrence: "turn:1:call:0".into(),
                name: "delegate".into(),
                arguments: json!({"instructions":"abandoned selected task"}),
            },
        )
        .await
        .unwrap();
        app.db.sync(|c|{c.execute("UPDATE cli_switch_requests SET phase='cancelled',holds_input=0 WHERE id='abandoned'",[])?;assert!(work::claim_next(c,&p.id,"other-generation",db::now())?.is_none());assert_eq!(work::event(c,&p.id,reply["event_id"].as_str().unwrap())?.status,"failed");let current=db::project_by_key(c,&p.id)?;intent(c,&current,"fresh-source-task","new authorized source work",None,false)?;assert!(work::claim_next(c,&p.id,"other-generation",db::now())?.is_some());Ok(())}).unwrap();
    }

    #[tokio::test]
    async fn delegated_resources_wait_for_original_preparation_without_blocking_later_steering() {
        let (_dir, app, p) = fixture().await;
        app.db.ingest_bot(&p.id,"cli_fixture","oc_fixture","om_resource","inspect original image",&json!({"source":{"message_type":"post","chat_id":"oc_fixture","chat_type":"p2p"},"resources":[{"kind":"image","resource_key":"img_fixture"}]})).unwrap();
        app.db
            .sync(|c| {
                work::sync_inbound(c)?;
                assert!(claim(c, &p.id)?.is_none());
                // A durable delegation created before this readiness gate must still
                // obey the original resource fence after an upgrade or restart.
                let origin: Option<String> = Some(c.query_row(
                    "SELECT id FROM work_events WHERE source_id='om_resource'",
                    [],
                    |r| r.get(0),
                )?);
                intent(
                    c,
                    &p,
                    "resource-delegate",
                    "inspect full attached image",
                    origin.as_deref(),
                    false,
                )?;
                assert!(work::claim_next(c, &p.id, "generation", db::now())?.is_none());
                Ok(())
            })
            .unwrap();
        ingest(&app, &p, "om_later", "later unrelated plain instructions");
        app.db.sync(|c|{work::sync_inbound(c)?;let turn=claim(c,&p.id)?.unwrap();let origins=c.prepare("SELECT i.origin_event_id FROM manager_inputs i JOIN manager_run_inputs b ON b.input_id=i.id WHERE b.run_id=?")?.query_map([&turn.context.run],|r|r.get::<_,Option<String>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;let later=c.query_row("SELECT id FROM work_events WHERE source_id='om_later'",[],|r|r.get::<_,String>(0))?;assert!(origins.contains(&Some(later)));c.execute("UPDATE messages SET attachment=json_set(attachment,'$.resources_ready',json('true')) WHERE source_id='om_resource'",[])?;assert!(work::claim_next(c,&p.id,"generation",db::now())?.is_some());Ok(())}).unwrap();
    }

    #[tokio::test]
    async fn resource_preparation_unblocks_consumed_origins_without_replaying_stale_workers() {
        for stale_worker in [false, true] {
            let (_dir, app, p) = fixture().await;
            let message = app
                .db
                .ingest_bot(
                    &p.id,
                    "cli_fixture",
                    "oc_fixture",
                    "om_prior_delegation",
                    "Inspect the attached evidence",
                    &json!({
                        "source":{"message_type":"image","chat_type":"p2p","chat_id":"oc_fixture"},
                        "kind":"image",
                        "file_key":"img_fixture"
                    }),
                )
                .unwrap()
                .unwrap();
            let delegation = app
                .db
                .sync(|c| {
                    work::sync_inbound(c)?;
                    c.execute(
                    "UPDATE projects SET terminal='fixture-pane',generation='generation' WHERE id=?",
                    [&p.id],
                )?;
                    let source = work::event(c, &p.id, &message)?;
                    assert_eq!(source.status, "managed");
                    assert!(claim(c, &p.id)?.is_none());
                    let current = db::project_by_key(c, &p.id)?;
                    let delegation = intent(
                        c,
                        &current,
                        "prior-delegation",
                        "Inspect attached evidence",
                        Some(&source.id),
                        false,
                    )?;
                    assert_eq!(
                        c.execute(
                            "UPDATE manager_inputs SET status='consumed' WHERE origin_event_id=?",
                            [&source.id],
                        )?,
                        1
                    );
                    settle_sources(c)?;
                    assert_eq!(work::event(c, &p.id, &source.id)?.status, "managed");
                    assert!(work::claim_next(c, &p.id, "generation", db::now())?.is_none());
                    Ok(delegation)
                })
                .unwrap();

            let original = app.db.message(&message).await.unwrap();
            assert_eq!(original.status, "acknowledged");
            assert!(original.attachment["resources_ready"].is_null());
            app.db.sync(|c| {
                let preparation = crate::feishu::begin_resource_preparation(c, &original)?
                    .expect("a queued delegation owns the consumed input's resources");
                assert!(crate::feishu::begin_resource_preparation(c, &original)?.is_none());
                let resources = [json!({
                    "kind":"image","file_key":"img_fixture","error":"Fixture resource unavailable"
                })];
                assert!(crate::feishu::finish_resource_preparation(
                    c,
                    &preparation,
                    &resources,
                    &json!({"members_status":"unavailable"}),
                )?);
                if stale_worker {
                    c.execute(
                        "UPDATE projects SET agent='agy',generation='replacement',native_session='other-session' WHERE id=?",
                        [&p.id],
                    )?;
                    assert!(work::claim_next(c, &p.id, "replacement", db::now())?.is_none());
                    let rejected = work::event(c, &p.id, &delegation)?;
                    assert_eq!(rejected.status, "failed");
                    assert!(rejected.error.contains("Worker identity changed"));
                    let runs: u32 = c.query_row(
                        "SELECT COUNT(*) FROM work_runs WHERE event_id=?",
                        [&delegation],
                        |row| row.get(0),
                    )?;
                    assert_eq!(runs, 0);
                } else {
                    let run = work::claim_next(c, &p.id, "generation", db::now())?.unwrap();
                    assert_eq!(run.event_id, delegation);
                }
                Ok(())
            }).unwrap();
            let prepared = app.db.message(&message).await.unwrap();
            assert_eq!(prepared.attachment["resources_ready"], true);
            assert!(prepared.attachment["resource_preparation"].is_null());
            assert_eq!(
                prepared.attachment["resources"][0]["error"],
                "Fixture resource unavailable"
            );
        }
    }

    #[test]
    fn file_capabilities_reject_private_case_aliases_and_symlinks_and_preserve_executability() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "private").unwrap();
        symlink(outside.path(), dir.path().join("escape")).unwrap();
        for path in [
            "../secret",
            "escape/secret",
            ".DAUERBOT/sessions/key",
            ".SSH/id_rsa",
            ".Env.production",
            "/tmp/outside",
        ] {
            assert!(read_file(dir.path(), path).is_err());
            assert!(
                write_file(
                    dir.path(),
                    WriteArgs {
                        path: path.into(),
                        content: "changed".into(),
                        expected_sha256: None
                    }
                )
                .is_err()
            );
        }
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "private"
        );
        fs::write(dir.path().join("oversized.txt"), "x".repeat(256000)).unwrap();
        assert!(
            read_file(dir.path(), "oversized.txt").is_err(),
            "Oversized tool results must fail before poisoning a durable model checkpoint"
        );
        let path = dir.path().join("run.sh");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let old = read_file(dir.path(), "run.sh").unwrap();
        assert!(
            write_file(
                dir.path(),
                WriteArgs {
                    path: "run.sh".into(),
                    content: "new".into(),
                    expected_sha256: None
                }
            )
            .is_err()
        );
        write_file(
            dir.path(),
            WriteArgs {
                path: "run.sh".into(),
                content: "new".into(),
                expected_sha256: old["sha256"].as_str().map(str::to_owned),
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
