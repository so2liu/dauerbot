//! Task-scoped native worker seats. The project's selected conversation and its
//! transition remain independent of these generations.
use crate::{App, adapter, config, db, db::Project, herdr, manager, mcp, models, work};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncReadExt;

const PREFLIGHT_RETRY_MS: i64 = 30_000;

pub(crate) fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS manager_workers(
            generation TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            agent TEXT NOT NULL, token TEXT NOT NULL DEFAULT '', state TEXT NOT NULL, error TEXT NOT NULL DEFAULT '',
            execution TEXT NOT NULL DEFAULT 'null', native_session TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL, origin_event_id TEXT
        );
        DROP INDEX IF EXISTS manager_worker_active_cli;
        CREATE INDEX IF NOT EXISTS manager_worker_project_cli
        ON manager_workers(project_id,agent,state);",
    )?;
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Seat {
    pub(crate) project: Project,
    pub(crate) state: String,
    error: String,
    created_at: i64,
}

pub(crate) fn seat(c: &Connection, parent: &Project, generation: &str) -> Result<Option<Seat>> {
    let row = c.query_row(
        "SELECT agent,token,state,error,execution,native_session,created_at FROM manager_workers WHERE project_id=? AND generation=?",
        params![parent.id, generation],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,String>(4)?,r.get(5)?,r.get(6)?)),
    ).optional()?;
    let Some((agent, token, state, error, raw, native_session, created_at)) = row else {
        return Ok(None);
    };
    let execution: Value = serde_json::from_str(&raw)?;
    let mut p = parent.clone();
    p.agent = agent;
    p.mcp_token = token;
    p.generation = generation.into();
    p.terminal = execution["terminal_id"].as_str().map(str::to_owned);
    p.terminal_pid = execution["shell_pid"].as_i64();
    p.ready = state == "ready";
    p.execution = execution;
    p.native_session = native_session;
    if p.agent != "custom" {
        p.extra_args.clear();
    }
    Ok(Some(Seat {
        project: p,
        state,
        error,
        created_at,
    }))
}

pub(crate) fn controls(c: &Connection, p: &Project) -> Result<()> {
    ensure!(!p.paused, "Resume this project before delegating work");
    ensure!(
        !c.query_row("SELECT EXISTS(SELECT 1 FROM work_leases WHERE project_id=? AND active=1)",
            [&p.id], |row| row.get::<_, bool>(0))?,
        "Terminal input is taken over by the user"
    );
    ensure!(
        !crate::cli_switch::holds_input(c, &p.id)?,
        "A project CLI transition holds delivery"
    );
    Ok(())
}

pub(crate) fn authorize(
    c: &Connection,
    parent: &Project,
    generation: &str,
    registered: bool,
) -> Result<Seat> {
    let seat = seat(c, parent, generation)?.context("Unknown manager worker generation")?;
    ensure!(
        seat.state == "ready" || (!registered && seat.state == "starting"),
        "The manager worker is unavailable"
    );
    ensure!(
        is_plain(&seat.project) || !App::update_required(&seat.project),
        "The worker requires a compatible daemon"
    );
    if registered {
        controls(c, parent)?;
    }
    Ok(seat)
}

pub(crate) fn is_plain(p: &Project) -> bool {
    p.mcp_token.is_empty()
}

pub(crate) fn diagnostic_worker(c: &Connection, p: &Project) -> Result<Project> {
    let generation = c
        .query_row(
            "SELECT w.generation FROM manager_workers w
         LEFT JOIN work_leases l ON l.project_id=w.project_id AND l.generation=w.generation
         WHERE w.project_id=? AND w.state IN ('reserved','starting','ready','unavailable')
           AND json_type(w.execution,'$.terminal_id')='text'
         ORDER BY COALESCE(l.active,0) DESC,
                  CASE w.state WHEN 'unavailable' THEN 0 WHEN 'ready' THEN 1 ELSE 2 END,
                  w.rowid DESC LIMIT 1",
            [&p.id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(generation
        .as_deref()
        .map(|g| seat(c, p, g))
        .transpose()?
        .flatten()
        .map_or_else(|| p.clone(), |seat| seat.project))
}

pub(crate) fn ready_worker(c: &Connection, p: &Project) -> Result<Project> {
    Ok(active(c, p)?
        .into_iter()
        .find(|seat| seat.project.ready && seat.project.terminal.is_some())
        .map_or_else(|| p.clone(), |seat| seat.project))
}

fn active(c: &Connection, p: &Project) -> Result<Vec<Seat>> {
    let generations = c.prepare("SELECT generation FROM manager_workers WHERE project_id=? AND state IN ('starting','ready') ORDER BY created_at")?
        .query_map([&p.id], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    generations
        .into_iter()
        .map(|g| seat(c, p, &g).and_then(|s| s.context("Manager worker disappeared")))
        .collect()
}

pub(crate) fn snapshot(c: &Connection, p: &Project) -> Result<Value> {
    let mut workers = Vec::new();
    if p.terminal.is_some() {
        workers.push(json!({
            "role":"legacy_primary","agent":p.agent,"generation":p.generation,
            "terminal_id":p.terminal,"native_session":p.native_session,"ready":p.ready,
            "mcp_registered":p.execution["mcp_registered"]==true
        }));
    }
    let generations = c.prepare("SELECT generation FROM manager_workers WHERE project_id=? ORDER BY created_at DESC LIMIT 12")?
        .query_map([&p.id], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for generation in generations {
        let s = seat(c, p, &generation)?.context("Manager worker disappeared")?;
        let worker = s.project;
        let tasks = c.prepare(
            "SELECT e.id,e.status,e.error,(SELECT id FROM work_runs r WHERE r.event_id=e.id ORDER BY rowid DESC LIMIT 1),
                    json_extract(e.payload,'$.worker_result')
             FROM work_events e WHERE e.project_id=? AND e.source='manager_worker'
               AND json_extract(e.payload,'$.target_generation')=? ORDER BY e.rowid DESC LIMIT 8"
        )?.query_map(params![p.id,generation], |r| {
            let run: Option<String> = r.get(3)?;
            let result = r.get::<_,Option<String>>(4)?
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok()).map(|result| {
                    json!({"status":result["status"],"reported_status":result["reported_status"],
                        "summary":result["summary"].as_str().map(|text| text.chars().take(320).collect::<String>()),
                        "semantic_task_success":result["semantic_task_success"]})
                });
            let path = if is_plain(&worker) { run.as_deref().and_then(|run|
                crate::worker_task::result_path(Path::new(&worker.path), &p.id, &generation, run).ok())
            } else { None };
            Ok(json!({"event_id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,
                "reason":r.get::<_,String>(2)?,"run_id":run,"result_file":path,"result":result}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        workers.push(json!({"role":"task_worker","agent":worker.agent,"generation":generation,
            "terminal_id":worker.terminal,"native_session":worker.native_session,"ready":worker.ready,
            "mcp_registered":worker.execution["mcp_registered"]==true,"status":s.state,"reason":s.error,
            "tasks":tasks}));
    }
    Ok(json!(workers))
}

pub(crate) enum Selection {
    Target(String),
    Unavailable(Value),
}

fn unavailable(target: &str, reason: &str) -> Selection {
    Selection::Unavailable(
        json!({"status":"worker_unavailable","target_cli":target,"reason":reason,"work_queued":false}),
    )
}

const CLI_NAMES: [&str; 5] = ["codex", "claude", "pi", "agy", "custom"];

fn custom_configured(p: &Project) -> bool {
    matches!(p.agent.as_str(), "custom" | "auto") && !p.extra_args.is_empty()
}

fn candidate(p: &Project, agent: &str) -> Project {
    let mut candidate = p.clone();
    candidate.agent = agent.into();
    if agent != "custom" || !custom_configured(p) {
        candidate.extra_args.clear();
    }
    candidate
}

pub(crate) async fn catalog(p: &Project) -> Value {
    let mut checks = tokio::task::JoinSet::new();
    for agent in CLI_NAMES {
        let candidate = candidate(p, agent);
        checks.spawn(async move { (agent, adapter::plain_worker_available(&candidate).await) });
    }
    let mut installed = Vec::new();
    while let Some(result) = checks.join_next().await {
        if let Ok((agent, true)) = result {
            installed.push(agent);
        }
    }
    json!(CLI_NAMES.map(|agent| {
        let available = installed.contains(&agent);
        let reason = if available {
            None
        } else if agent == "custom" && !custom_configured(p) {
            Some("No custom command is configured for this project")
        } else {
            Some("Command is missing or failed its bounded executable check")
        };
        json!({"agent":agent,"available":available,"reason":reason})
    }))
}

fn current_seat(c: &Connection, parent: &Project, target: &str) -> Result<Option<Seat>> {
    // The daemon releases a launch permit only after saving the execution.
    // A failed preflight without that evidence can be replaced after cooldown.
    let generation = c
        .query_row(
            "SELECT w.generation FROM manager_workers w
         WHERE w.project_id=? AND w.agent=? AND w.state!='closed'
           AND (w.token='' OR w.state IN ('reserved','starting','ready'))
           AND NOT (w.state='unavailable' AND w.execution='null' AND w.native_session=''
             AND w.created_at<=?
             AND NOT EXISTS(SELECT 1 FROM work_executions e WHERE e.generation=w.generation))
         ORDER BY CASE WHEN w.state IN ('ready','starting','reserved') THEN 0 ELSE 1 END, w.rowid DESC LIMIT 1",
            params![parent.id, target, db::now() - PREFLIGHT_RETRY_MS],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    generation
        .map(|g| seat(c, parent, &g))
        .transpose()
        .map(Option::flatten)
}

fn installed_candidates(
    c: &Connection,
    p: &Project,
    choices: &Value,
) -> Result<Vec<(&'static str, Option<Seat>)>> {
    controls(c, p)?;
    let available = choices
        .as_array()
        .context("Worker catalog is unavailable")?;
    let mut candidates = Vec::new();
    for name in CLI_NAMES {
        if available
            .iter()
            .any(|entry| entry["agent"] == name && entry["available"] == true)
        {
            let current = current_seat(c, p, name)?;
            if current
                .as_ref()
                .is_none_or(|seat| seat.state != "unavailable")
            {
                candidates.push((name, current));
            }
        }
    }
    candidates.sort_by_key(|(name, _)| *name != p.agent);
    Ok(candidates)
}

/// Resolve the CLI before accepting work. Seat allocation happens separately
/// against the durable queue, under the project reconciliation lock.
pub(crate) async fn select(app: &App, p: &Project, target: &str) -> Result<Selection> {
    ensure!(target.is_empty() || CLI_NAMES.contains(&target), "Unsupported target CLI");
    let parent = p.clone();
    if target.is_empty() {
        let choices = catalog(p).await;
        let observed = choices.clone();
        let candidates = app.db.call(move |c| installed_candidates(c, &parent, &observed)).await?;
        return Ok(candidates.first().map_or_else(
            || Selection::Unavailable(json!({"status":"worker_unavailable","reason":"No installed worker currently accepts assignments","work_queued":false,"catalog":choices})),
            |(name, _)| Selection::Target((*name).into()),
        ));
    }
    let name = target.to_owned();
    let seat = app.db.call(move |c| {
        controls(c, &parent)?;
        current_seat(c, &parent, &name)
    }).await?;
    if let Some(seat) = seat
        && seat.state == "unavailable"
    {
        return Ok(unavailable(target, &seat.error));
    }
    if !adapter::plain_worker_available(&candidate(p, target)).await {
        return Ok(unavailable(target, "Worker command unavailable; choose an installed CLI"));
    }
    Ok(Selection::Target(target.into()))
}

pub(crate) async fn warm(app: &App, parent: &Project) -> Result<()> {
    let Selection::Target(agent) = select(app, parent, "").await? else {
        anyhow::bail!("No worker is available");
    };
    let parent = parent.clone();
    let worker = app.db.call(move |c| {
        controls(c, &parent)?;
        if current_seat(c, &parent, &agent)?.is_some() {
            return Ok(None);
        }
        let generation = db::id();
        c.execute("INSERT INTO manager_workers(generation,project_id,agent,state,created_at)
            VALUES(?,?,?,'reserved',?)",params![generation,parent.id,agent,db::now()])?;
        Ok(seat(c, &parent, &generation)?.map(|s| s.project))
    }).await?;
    if let Some(worker) = worker {
        launch_assigned(app, worker).await?;
    }
    Ok(())
}

/// Bind accepted work and any new seat in the same transaction. A generation
/// with a queued assignment or an unresolved run is never reused.
fn allocate(c: &Connection, parent: &Project, limit: Option<usize>, idle_seats: &[String]) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    if controls(&tx, parent).is_err() {
        return Ok(());
    }
    let pending = tx.prepare(
        "SELECT id,json_extract(payload,'$.expected_agent'),json_extract(payload,'$.origin_event_id')
         FROM work_events WHERE project_id=? AND source='manager_worker' AND status='worker_waiting'
         AND json_extract(payload,'$.target_generation') IS NULL ORDER BY rowid",
    )?.query_map([&parent.id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (event, agent, origin) in pending {
        let idle: Option<String> = tx.query_row(
            "SELECT w.generation FROM manager_workers w
             WHERE w.project_id=?1 AND w.agent=?2 AND w.token='' AND w.state='ready'
             AND w.generation IN (SELECT value FROM json_each(?3))
             AND NOT EXISTS(SELECT 1 FROM work_events e WHERE e.project_id=w.project_id
                 AND e.status='worker_waiting' AND json_extract(e.payload,'$.target_generation')=w.generation)
             AND NOT EXISTS(SELECT 1 FROM work_runs r WHERE r.project_id=w.project_id AND r.generation=w.generation
                 AND r.status IN ('submitting','submitted','running','uncertain'))
             AND NOT EXISTS(SELECT 1 FROM work_leases t WHERE t.project_id=w.project_id AND t.generation=w.generation AND t.active=1)
             ORDER BY w.rowid LIMIT 1",
            params![parent.id, agent, json!(idle_seats).to_string()], |row| row.get(0),
        ).optional()?;
        let generation = if let Some(idle) = idle {
            idle
        } else {
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM manager_workers WHERE project_id=? AND agent=? AND token='' AND state IN ('reserved','starting','ready')",
                params![parent.id, agent], |row| row.get(0),
            )?;
            if limit.is_some_and(|limit| count as usize >= limit) {
                continue;
            }
            let generation = db::id();
            tx.execute(
                "INSERT INTO manager_workers(generation,project_id,agent,state,created_at,origin_event_id) VALUES(?,?,?,'reserved',?,?)",
                params![generation,parent.id,agent,db::now(),origin],
            )?;
            generation
        };
        tx.execute(
            "UPDATE work_events SET payload=json_set(payload,'$.target_generation',?),updated_at=? WHERE id=?",
            params![generation,db::now(),event],
        )?;
    }
    tx.commit()?;
    Ok(())
}

async fn observe(cfg: &config::Config, p: &Project) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(3), herdr::observe(cfg, p))
        .await
        .context("Worker observation exceeded the time budget")?
}

async fn resolve_program(worker: &Project, program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        return Path::new(&worker.path).join(path).canonicalize().ok();
    }
    // Resolve the same login-shell PATH used by the native worker. The
    // bounded output stays internal and may contain shell startup messages.
    let command = herdr::login_shell_args(vec!["/usr/bin/which".into(), program.into()]);
    let mut child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(&worker.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = tokio::time::timeout(Duration::from_secs(3), async {
        let mut buf = Vec::new();
        child
            .stdout
            .take()?
            .take(8193)
            .read_to_end(&mut buf)
            .await
            .ok()?;
        if buf.len() > 8192 || !child.wait().await.ok()?.success() {
            return None;
        }
        String::from_utf8(buf).ok()
    })
    .await
    .ok()??;
    output
        .lines()
        .rev()
        .map(Path::new)
        .find(|entry| {
            entry.is_absolute() && entry.file_name().and_then(|name| name.to_str()) == Some(program)
        })?
        .canonicalize()
        .ok()
}

/// An unhooked worker can prove terminal ownership and a foreground CLI
/// without a provider hook, agent ID, or MCP connection.
pub(crate) fn check_plain_target(p: &Project, record: &Value) -> Result<()> {
    herdr::check_pane_ownership(p, record)?;
    let expected = p.execution["worker_program"]
        .as_str()
        .context("Worker program identity is missing")?;
    let matches = |program: &str| {
        program == expected
            || (!expected.contains('/')
                && Path::new(program)
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some(expected))
    };
    let foreground = &record["foreground"];
    let resolved = p.execution["worker_program_resolved"]
        .as_str()
        .map(Path::new);
    let argv0 = foreground["argv"][0]
        .as_str()
        .or_else(|| foreground["argv0"].as_str());
    let program_name = argv0.and_then(|arg| Path::new(arg).file_name()?.to_str());
    let interpreted = matches!(
        program_name,
        Some(
            "node"
                | "nodejs"
                | "bun"
                | "deno"
                | "python"
                | "python3"
                | "ruby"
                | "bash"
                | "sh"
                | "zsh"
        )
    );
    let uses_script = interpreted
        && resolved.is_some_and(|known| {
            foreground["argv"][1].as_str().is_some_and(|script| {
                Path::new(&p.path)
                    .join(script)
                    .canonicalize()
                    .is_ok_and(|path| path == known)
            })
        });
    ensure!(
        foreground["name"].as_str().is_some_and(matches)
            || argv0.is_some_and(matches)
            || uses_script,
        "The expected CLI is not in the owned terminal foreground"
    );
    herdr::check_native_resume(p, record)?;
    ensure!(
        !matches!(
            record["status"]["state"].as_str(),
            Some("blocked" | "error")
        ),
        "Worker terminal needs attention"
    );
    Ok(())
}

async fn launch(app: &App, mut p: Project) -> Result<()> {
    let parent = app.db.project(&p.id).await?;
    let check = parent.clone();
    let generation_key = p.generation.clone();
    app.db
        .call(move |c| {
            controls(c, &check)?;
            ensure!(
                seat(c, &check, &generation_key)?.is_some_and(|s| s.state == "starting"),
                "Worker startup revoked"
            );
            Ok(())
        })
        .await?;
    let cfg = app.config.read().await.clone();
    let id = p.id.clone();
    let agent = p.agent.clone();
    let selection = app.db.call(move |c| models::load(c, &id, &agent)).await?;
    let argv = adapter::plain_worker_args(&p, selection.as_ref())?;
    let resolved = resolve_program(&p, &argv[0]).await;
    p.execution = herdr::prepare_plain_worker(&cfg, &p, &argv, &p.generation).await?;
    p.execution["api_protocol"] = json!(crate::api::PROTOCOL);
    p.execution["plain_worker"] = json!(true);
    p.execution["worker_program"] = json!(argv[0]);
    p.execution["worker_program_resolved"] = json!(resolved);
    p.terminal = p.execution["terminal_id"].as_str().map(str::to_owned);
    p.terminal_pid = p.execution["shell_pid"].as_i64();
    let saved = p.clone();
    app.db.call(move |c| {
        let tx = c.unchecked_transaction()?;
        ensure!(tx.execute("UPDATE manager_workers SET execution=? WHERE project_id=? AND generation=? AND state='starting'",
            params![saved.execution.to_string(),saved.id,saved.generation])?==1,"Worker startup revoked");
        tx.execute("INSERT INTO work_executions(generation,project_id,agent,terminal,status,last_activity,created_at,updated_at)
            VALUES(?,?,?,?,'running',?,?,?)",params![saved.generation,saved.id,saved.agent,saved.terminal,db::now(),db::now(),db::now()])?;
        tx.commit()?;
        Ok(())
    }).await?;
    herdr::launch_execution(&cfg, &p, &argv).await
}

pub(crate) async fn register(
    app: &App,
    mut worker: Project,
    pane: &str,
    contract: Option<u64>,
) -> Result<Value> {
    ensure!(!is_plain(&worker), "Plain workers have no MCP registration");
    ensure!(
        worker.execution["pane_id"] == pane,
        "Worker pane identity does not match"
    );
    let supported =
        mcp::generation_contract(&worker.execution).context("Unsupported Agent MCP contract")?;
    ensure!(
        contract.is_none_or(|value| value == supported),
        "Agent MCP contract differs from its execution"
    );
    let cfg = app.config.read().await.clone();
    let observed = observe(&cfg, &worker).await?;
    herdr::check_identity(&worker, &observed)?;
    let native = observed["native_session"]["value"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Native worker session is not ready")?
        .to_owned();
    ensure!(
        worker.native_session.is_empty()
            || herdr::same_native_session(&worker.agent, &worker.native_session, &native),
        "The native worker session changed"
    );
    worker.execution["mcp_registered"] = json!(true);
    worker.execution["intended_native_session"] = json!(native);
    let name = worker.name.clone();
    app.db.call(move |c| {
        let tx = c.unchecked_transaction()?;
        ensure!(tx.execute("UPDATE manager_workers SET state='ready',native_session=?,execution=? WHERE project_id=? AND generation=? AND state IN ('starting','ready') AND (native_session='' OR native_session=?)",
            params![native,worker.execution.to_string(),worker.id,worker.generation,native])?==1,"The worker changed before registration");
        work::set_native_session(&tx,&worker.id,&worker.generation,&native,db::now())?;
        observation(&tx,&worker.id,&worker.generation,"ready","")?;
        tx.commit()?;
        Ok(())
    }).await?;
    app.notify.notify_one();
    Ok(
        json!({"ok":true,"project":name,"api_protocol":crate::api::PROTOCOL,
        "mcp_contract":supported,"service_api_protocol":crate::api::PROTOCOL,"repair_only":false}),
    )
}

fn observation(
    c: &Connection,
    project: &str,
    generation: &str,
    state: &str,
    reason: &str,
) -> Result<()> {
    let (agent, origin): (String, Option<String>) = c.query_row(
        "SELECT agent,origin_event_id FROM manager_workers WHERE project_id=? AND generation=?",
        params![project, generation],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    manager::observation(
        c,
        project,
        "worker_seat",
        generation,
        state,
        origin.as_deref(),
        &json!({"kind":"worker_seat","agent":agent,"generation":generation,"status":state,"reason":reason}),
    )
}

pub(crate) fn fail(c: &Connection, project: &str, generation: &str, reason: &str) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    let changed = tx.execute("UPDATE manager_workers SET state='unavailable',error=? WHERE generation=? AND project_id=? AND state IN ('reserved','starting','ready')",
        params![reason,generation,project])?;
    if changed != 0 {
        observation(&tx, project, generation, "unavailable", reason)?;
    }
    tx.execute("UPDATE work_tasks SET status='failed',updated_at=? WHERE id IN
        (SELECT task_id FROM work_events WHERE project_id=? AND source='manager_worker' AND status='worker_waiting' AND json_extract(payload,'$.target_generation')=?)",
        params![db::now(),project,generation])?;
    tx.execute("UPDATE work_events SET status='failed',error=?,updated_at=? WHERE project_id=? AND source='manager_worker' AND status='worker_waiting' AND json_extract(payload,'$.target_generation')=?",
        params![reason,db::now(),project,generation])?;
    work::finish_execution(&tx, project, generation, "error", reason, db::now())?;
    tx.commit()?;
    Ok(())
}

async fn launch_assigned(app: &App, worker: Project) -> Result<()> {
    let project = worker.id.clone();
    let generation = worker.generation.clone();
    let reserved = worker.clone();
    let can_launch = app.db.call(move |c| {
        let parent = db::project_by_key(c, &reserved.id)?;
        if controls(c, &parent).is_err() {
            return Ok(false);
        }
        Ok(c.execute("UPDATE manager_workers SET state='starting',created_at=? WHERE project_id=? AND generation=? AND state='reserved'",
            params![db::now(),reserved.id,reserved.generation])? == 1)
    }).await?;
    if can_launch && launch(app, worker).await.is_err() {
        app.db.call(move |c| fail(c, &project, &generation,
            "Native startup failed; verify that the CLI is installed and logged in")).await?;
    }
    Ok(())
}

pub(crate) async fn reconcile(app: &Arc<App>, parent: &Project) -> Result<()> {
    let cfg = app.config.read().await.clone();
    let p = parent.clone();
    let ready = app.db.call(move |c| {
        let pending: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM work_events
            WHERE project_id=? AND source='manager_worker' AND status='worker_waiting'
            AND json_extract(payload,'$.target_generation') IS NULL)", [&p.id], |row| row.get(0))?;
        if !pending || controls(c, &p).is_err() {
            return Ok(None);
        }
        Ok(Some(active(c, &p)?.into_iter().filter(|s| s.state == "ready" && is_plain(&s.project))
            .map(|s| s.project).collect::<Vec<_>>()))
    }).await?;
    if let Some(ready) = ready {
        let mut probes = tokio::task::JoinSet::new();
        for worker in ready {
            let cfg = cfg.clone();
            probes.spawn(async move {
                let observed = observe(&cfg, &worker).await.ok()?;
                check_plain_target(&worker, &observed).ok()?;
                matches!(observed["status"]["state"].as_str(), None | Some("" | "unknown" | "idle" | "done"))
                    .then_some(worker.generation)
            });
        }
        let mut idle = Vec::new();
        while let Some(result) = probes.join_next().await {
            if let Ok(Some(generation)) = result {
                idle.push(generation);
            }
        }
        let p = parent.clone();
        let limit = cfg.manager_workers_per_cli.map(std::num::NonZeroUsize::get);
        app.db.call(move |c| allocate(c, &p, limit, &idle)).await?;
    }
    let p = parent.clone();
    let reserved = app.db.call(move |c| {
        if controls(c, &p).is_err() {
            return Ok(Vec::new());
        }
        let generations = c.prepare("SELECT generation FROM manager_workers WHERE project_id=? AND state='reserved' ORDER BY rowid")?
            .query_map([&p.id], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        generations.into_iter().map(|generation| {
            Ok(seat(c,&p,&generation)?.context("Reserved worker disappeared")?.project)
        }).collect::<Result<Vec<_>>>()
    }).await?;
    for worker in reserved {
        launch_assigned(app, worker).await?;
    }
    let p = parent.clone();
    let seats = app.db.call(move |c| {
        let mut seats = active(c,&p)?;
        let unavailable = c.prepare("SELECT w.generation FROM manager_workers w
            WHERE w.project_id=? AND w.token='' AND w.state='unavailable'
               AND EXISTS(SELECT 1 FROM work_runs r JOIN work_events e ON e.id=r.event_id
                   WHERE r.project_id=w.project_id AND r.generation=w.generation AND e.source='manager_worker'
                     AND r.status IN ('submitted','running','settled','failed','uncertain')
                     AND (json_extract(e.payload,'$.worker_result.run_id') IS NOT r.id
                       OR json_extract(e.payload,'$.worker_result.status')='result_missing'))")?
          .query_map([&p.id], |row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for generation in unavailable {
            if let Some(seat) = seat(c,&p,&generation)? {
                seats.push(seat);
            }
        }
        Ok(seats)
    }).await?;
    let mut tasks = tokio::task::JoinSet::new();
    for s in seats {
        let app = Arc::clone(app);
        let cfg = cfg.clone();
        let parent = parent.clone();
        tasks.spawn(async move {
            let worker = s.project;
            // Project controls are held by the caller. Each owned seat is
            // observed independently so one unavailable transport cannot
            // delay dispatch into a different verified terminal.
            let result = if s.state == "unavailable" {
                crate::worker_task::collect(&app,&cfg,&worker,None).await
            } else {
                reconcile_one(&app, &cfg, &parent, &worker, &s.state, s.created_at).await
            };
            if let Err(error) = result {
                tracing::warn!(generation=%worker.generation, "Worker reconciliation failed: {error}");
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::warn!("Worker reconciliation task stopped: {error}");
        }
    }
    Ok(())
}

async fn reconcile_one(
    app: &Arc<App>,
    cfg: &config::Config,
    parent: &Project,
    worker: &Project,
    state: &str,
    created: i64,
) -> Result<()> {
    let record = if worker.terminal.is_some() {
        match observe(cfg, worker).await {
            Ok(value) => Some(value),
            Err(_) if state == "starting" && db::now() - created > 90_000 => None,
            Err(error) => {
                if is_plain(worker) && state == "ready" {
                    crate::worker_task::collect(app, cfg, worker, None).await?;
                }
                return Err(error);
            }
        }
    } else {
        None
    };
    let plain = is_plain(worker);
    let status = record
        .as_ref()
        .and_then(|r| r["status"]["state"].as_str())
        .unwrap_or("");
    let failure = if record.as_ref().is_some_and(|r| r["state"] != "live") {
        Some("Independent worker exited before its pending work could continue")
    } else if matches!(status, "blocked" | "error") {
        Some("Independent worker needs native login, permission, or runtime attention")
    } else if state == "starting" && db::now() - created > 90_000 {
        Some("Independent worker did not establish terminal readiness")
    } else if state == "ready"
        && record.as_ref().is_some_and(|r| {
            if plain {
                herdr::check_pane_ownership(worker, r).is_err()
                    || herdr::check_native_resume(worker, r).is_err()
            } else {
                herdr::check_identity(worker, r).is_err()
            }
        })
    {
        Some("Independent worker native identity changed")
    } else {
        None
    };
    let id = worker.id.clone();
    let generation = worker.generation.clone();
    if let Some(reason) = failure {
        app.db
            .call(move |c| fail(c, &id, &generation, reason))
            .await?;
        if plain && state == "ready" {
            crate::worker_task::collect(app, cfg, worker, None).await?;
        }
        return Ok(());
    }
    let Some(record) = record.as_ref() else {
        return Ok(());
    };
    let mut worker = worker.clone();
    if plain && state == "ready" {
        crate::worker_task::collect(app, cfg, &worker, Some(record)).await?;
    }
    if plain && check_plain_target(&worker, record).is_err() {
        return Ok(());
    }
    if plain && state == "starting" {
        let native = record["native_session"]["value"]
            .as_str()
            .unwrap_or_default();
        if !native.is_empty() {
            worker.native_session = native.into();
            worker.execution["intended_native_session"] = json!(native);
        }
        let fresh = worker.clone();
        app.db.call(move |c| ready_plain(c, &fresh)).await?;
        worker.ready = true;
        app.notify.notify_one();
    }
    if !worker.ready || !matches!(status, "" | "unknown" | "idle" | "done") {
        return Ok(());
    }
    if !plain && !matches!(status, "idle" | "done") {
        return Ok(());
    }
    let sequence = record["state_change_seq"].as_i64();
    let parent = parent.clone();
    if !plain {
        let collect_id = id.clone();
        let collect_generation = generation.clone();
        app.db
            .call(move |c| {
                work::observe_idle(c, &collect_id, &collect_generation, sequence, db::now())
            })
            .await?;
    }
    let run = app
        .db
        .call(move |c| {
            if controls(c, &parent).is_err() || (!plain && App::update_required(&parent)) {
                return Ok(None);
            }
            work::claim_worker(c, &id, &generation, db::now())
        })
        .await?;
    if let Some(run) = run {
        if plain {
            crate::worker_task::dispatch(app, cfg, &worker, &run).await?;
        } else {
            app.deliver_run(&worker, &run).await?;
        }
    }
    Ok(())
}

fn ready_plain(c: &Connection, p: &Project) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    ensure!(
        tx.execute(
            "UPDATE manager_workers SET state='ready',native_session=?,execution=?
         WHERE project_id=? AND generation=? AND token='' AND state='starting'",
            params![
                p.native_session,
                p.execution.to_string(),
                p.id,
                p.generation
            ],
        )? == 1,
        "Worker changed before proving terminal readiness"
    );
    if !p.native_session.is_empty() {
        work::set_native_session(&tx, &p.id, &p.generation, &p.native_session, db::now())?;
    }
    observation(&tx, &p.id, &p.generation, "ready", "")?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn closed(c: &Connection, generation: &str) -> Result<()> {
    c.execute(
        "UPDATE manager_workers SET state='closed' WHERE generation=?",
        [generation],
    )?;
    Ok(())
}

pub(crate) async fn stop_all(app: &App, p: &Project) -> Result<()> {
    let project = p.id.clone();
    app.db.call(move |c| {
        let tx = c.unchecked_transaction()?;
        tx.execute("UPDATE work_tasks SET status='cancelled',updated_at=?1 WHERE id IN
            (SELECT task_id FROM work_events WHERE project_id=?2 AND source='manager_worker'
             AND status='worker_waiting' AND json_extract(payload,'$.target_generation') IS NULL)",
            params![db::now(),project])?;
        tx.execute("UPDATE work_events SET status='cancelled',error='Stopped by the user',updated_at=?1
            WHERE project_id=?2 AND source='manager_worker' AND status='worker_waiting'
            AND json_extract(payload,'$.target_generation') IS NULL",params![db::now(),project])?;
        tx.commit()?;
        Ok(())
    }).await?;

    let parent = p.clone();
    let seats = app
        .db
        .call(move |c| {
            let generations = c
                .prepare(
                    "SELECT generation FROM manager_workers WHERE project_id=? AND state!='closed'",
                )?
                .query_map([&parent.id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            generations
                .into_iter()
                .map(|g| seat(c, &parent, &g).and_then(|s| s.context("Manager worker disappeared")))
                .collect::<Result<Vec<_>>>()
        })
        .await?;
    let cfg = app.config.read().await.clone();
    let mut incomplete = false;
    for s in seats {
        let worker = s.project;
        let id = worker.id.clone();
        let generation_key = worker.generation.clone();
        let result = async {
            app.db
                .call(move |c| {
                    fail(
                        c,
                        &id,
                        &generation_key,
                        "Independent worker stopped by the user",
                    )
                })
                .await?;
            if worker.terminal.is_some() {
                herdr::stop(&cfg, &worker).await?;
                ensure!(
                    observe(&cfg, &worker).await?["vanished_execution"] == true,
                    "Task worker process disappearance has not been verified"
                );
            }
            app.db.call(move |c| closed(c, &worker.generation)).await
        }
        .await;
        incomplete |= result.is_err();
    }
    ensure!(
        !incomplete,
        "One or more task worker stops could not be verified; their generations were preserved"
    );
    Ok(())
}

pub(crate) fn validate_outbox(c: &Connection, project: &str, attachment: &Value) -> Result<()> {
    if let Some(generation) = attachment["task_worker_generation"].as_str() {
        let parent = db::project_by_key(c, project)?;
        let worker = authorize(c, &parent, generation, true)?;
        ensure!(
            !is_plain(&worker.project),
            "Plain workers have no MCP access"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    #[test]
    fn migration_removes_legacy_uniqueness_and_is_repeatable_with_parallel_rows() -> Result<()> {
        let c = Connection::open_in_memory()?;
        c.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY); INSERT INTO projects VALUES('project');")?;
        migrate(&c)?;
        c.execute_batch("CREATE UNIQUE INDEX manager_worker_active_cli ON manager_workers(project_id,agent)
            WHERE state IN ('starting','ready');
            INSERT INTO manager_workers(generation,project_id,agent,state,created_at)
            VALUES('first','project','codex','ready',1);")?;
        let tx = c.unchecked_transaction()?;
        migrate(&tx)?;
        tx.execute("INSERT INTO manager_workers(generation,project_id,agent,state,created_at)
            VALUES('second','project','codex','starting',2)", [])?;
        tx.commit()?;
        migrate(&c)?;
        let rows: i64 = c.query_row("SELECT count(*) FROM manager_workers", [], |r| r.get(0))?;
        assert_eq!(rows, 2);
        assert_eq!(c.query_row("SELECT count(*) FROM sqlite_schema WHERE name='manager_worker_active_cli'",
            [], |r| r.get::<_, i64>(0))?, 0);
        assert_eq!(c.query_row("SELECT [unique] FROM pragma_index_list('manager_workers')
            WHERE name='manager_worker_project_cli'", [], |r| r.get::<_, i64>(0))?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn attention_needed_worker_remains_visible_without_authorizing_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), config::Config::default()).unwrap();
        let p = app
            .db
            .create(db::ProjectInput {
                name: "Diagnostic fixture".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "auto".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,error,execution,created_at)
                VALUES('blocked',?,'pi','','unavailable','Native login required',json_object('terminal_id','diagnostic'),0)", [&p.id])?;
            let diagnostic = diagnostic_worker(c,&p)?;
            assert_eq!(diagnostic.generation,"blocked");
            assert_eq!(diagnostic.terminal.as_deref(),Some("diagnostic"));
            assert_ne!(ready_worker(c,&p)?.generation,"blocked");
            assert!(authorize(c,&p,"blocked",true).is_err());
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn default_worker_candidates_skip_installed_but_fenced_generations() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), config::Config::default()).unwrap();
        let p = app
            .db
            .create(db::ProjectInput {
                name: "Catalog fixture".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "agy".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let catalog = json!([{"agent":"agy","available":true},{"agent":"pi","available":true},
            {"agent":"codex","available":false}]);
        app.db.sync(|c| {
            let initial = installed_candidates(c,&p,&catalog)?;
            assert_eq!(initial.first().unwrap().0,"agy");
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,error,created_at)
                VALUES('agy-failed',?,'agy','','unavailable','Login required',?)",params![p.id,db::now()])?;
            let remaining = installed_candidates(c,&p,&catalog)?;
            assert_eq!(remaining.iter().map(|(cli,_)| *cli).collect::<Vec<_>>(),["pi"]);
            assert_eq!(current_seat(c,&p,"agy")?.unwrap().error,"Login required");
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn task_worker_launches_and_verifies_plain_foreground_without_mcp() {
        let fixture = tempfile::tempdir_in("/tmp").unwrap();
        let runtime = fixture.path().join("herdr");
        fs::write(&runtime, "#!/bin/sh\nprintf 'herdr 0.9.5\\n'\n").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = config::Config {
            herdr: runtime.to_string_lossy().into(),
            herdr_session: fixture.path().to_string_lossy().into(),
            ..config::Config::default()
        };
        let listener = UnixListener::bind(herdr::socket_path(&cfg).unwrap()).unwrap();
        let worker_program = runtime.clone();
        let server = tokio::spawn(async move {
            let mut tokens = Value::Null;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if !tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line))
                    .await
                    .is_ok_and(|read| read.is_ok_and(|bytes| bytes != 0))
                {
                    continue;
                }
                let req: Value = serde_json::from_str(&line).unwrap();
                let params = &req["params"];
                let result = match req["method"].as_str().unwrap() {
                    "ping" => json!({"protocol":22,"version":"0.9.5"}),
                    "workspace.create" => {
                        json!({"workspace":{"workspace_id":"w1"},"root_pane":{"pane_id":"w1:p1","terminal_id":"root","workspace_id":"w1"}})
                    }
                    "layout.apply" => {
                        let request = params.to_string();
                        assert!(!request.contains("mcp_servers"));
                        assert!(!request.contains("--mcp-config"));
                        assert!(!request.contains("credentials.json"));
                        json!({"layout":{"root":{"pane_id":"w1:p2"}}})
                    }
                    "pane.get" if params["pane_id"] == "w1:p1" => {
                        json!({"pane":{"pane_id":"w1:p1","workspace_id":"w1","terminal_id":"root"}})
                    }
                    "pane.get" => {
                        json!({"pane":{"pane_id":"w1:p2","workspace_id":"w1","terminal_id":"worker-terminal","tokens":tokens}})
                    }
                    "pane.report_metadata" => {
                        tokens = params["tokens"].clone();
                        json!({})
                    }
                    "pane.process_info" => {
                        json!({"process_info":{"shell_pid":8420,"foreground_processes":[{"name":"sh","argv":["/bin/sh",worker_program]}]}})
                    }
                    "agent.get" => {
                        json!({"agent":{"pane_id":"w1:p2","terminal_id":"worker-terminal","workspace_id":"w1"}})
                    }
                    "pane.close" if params["pane_id"] == "w1:p1" => json!({}),
                    other => panic!("Unexpected fixture method {other}"),
                };
                let reply = json!({"id":req["id"],"result":result});
                reader
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let app = App::new(fixture.path().join("store"), cfg).unwrap();
        let parent = app
            .db
            .create(db::ProjectInput {
                name: "Independent launch fixture".into(),
                path: fixture.path().to_string_lossy().into(),
                agent: "auto".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![runtime.to_string_lossy().into_owned()],
            })
            .await
            .unwrap();
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,error,created_at)
                VALUES('failed-preflight',?,'custom','','unavailable','Integration missing',?)",
                params![parent.id,db::now()])?;
            Ok(())
        }).unwrap();
        assert!(matches!(select(&app,&parent,"custom").await.unwrap(),
            Selection::Unavailable(result) if result["reason"] == "Integration missing"));
        app.db
            .sync(|c| {
                assert!(active(c, &parent)?.is_empty());
                c.execute(
                    "UPDATE manager_workers SET created_at=? WHERE generation='failed-preflight'",
                    [db::now() - PREFLIGHT_RETRY_MS],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(select(&app,&parent,"custom").await.unwrap(), Selection::Target(target) if target=="custom"));
        let generation = db::id();
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,state,created_at)
                VALUES(?,?,'custom','starting',?)",params![generation,parent.id,db::now()])?;
            Ok(())
        }).unwrap();
        let worker = app.db.sync(|c| Ok(seat(c,&parent,&generation)?.unwrap().project)).unwrap();
        launch(&app, worker).await.unwrap();
        let launched = app.db.sync(|c| Ok(seat(c,&parent,&generation)?.unwrap().project)).unwrap();
        assert_eq!(launched.terminal_pid, Some(8420));
        assert!(
            register(&app, launched.clone(), "w1:p2", Some(mcp::CONTRACT))
                .await
                .is_err()
        );
        reconcile(&app, &parent).await.unwrap();
        assert!(
            app.db.sync(|c| Ok(seat(c,&parent,&launched.generation)?.unwrap().project.ready)).unwrap()
        );
        app.db.sync(|c| {
            assert_ne!(db::project_by_key(c,&parent.id)?.generation,launched.generation);
            assert_eq!(seat(c,&parent,"failed-preflight")?.unwrap().error,"Integration missing");
            assert!(crate::cli_switch::latest(c,&parent.id)?.is_none());
            assert!(work::snapshot(c,&parent.id)?.execution.is_none());
            assert_eq!(c.query_row("SELECT count(*) FROM manager_inputs WHERE project_id=? AND json_extract(payload,'$.kind')='worker_seat'",[&parent.id],|r|r.get::<_,i64>(0))?,1);
            Ok(())
        }).unwrap();
        server.abort();
    }
}
