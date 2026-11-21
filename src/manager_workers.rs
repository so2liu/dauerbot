//! Task-scoped native worker seats. The project's selected conversation and its
//! transition remain independent of these generations.
use crate::{App, adapter, config, db, db::Project, herdr, manager, mcp, models, work};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const PREFLIGHT_RETRY_MS: i64 = 30_000;

pub(crate) fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS manager_workers(
            generation TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            agent TEXT NOT NULL, token TEXT NOT NULL, state TEXT NOT NULL, error TEXT NOT NULL DEFAULT '',
            execution TEXT NOT NULL DEFAULT 'null', native_session TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL, origin_event_id TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS manager_worker_active_cli
        ON manager_workers(project_id,agent) WHERE state IN ('starting','ready');",
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
    p.extra_args.clear();
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
        !work::has_takeover(c, &p.id, &p.generation)?,
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
        !App::update_required(&seat.project),
        "The worker requires a compatible daemon"
    );
    if registered {
        controls(c, parent)?;
    }
    Ok(seat)
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
    let mut workers = vec![json!({
        "role":"primary","agent":p.agent,"generation":p.generation,
        "terminal_id":p.terminal,"native_session":p.native_session,"ready":p.ready,
        "mcp_registered":p.execution["mcp_registered"]==true
    })];
    let generations = c.prepare("SELECT generation FROM manager_workers WHERE project_id=? ORDER BY created_at DESC LIMIT 12")?
        .query_map([&p.id], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for generation in generations {
        let s = seat(c, p, &generation)?.context("Manager worker disappeared")?;
        let worker = s.project;
        workers.push(json!({"role":"task_worker","agent":worker.agent,"generation":generation,
            "terminal_id":worker.terminal,"native_session":worker.native_session,"ready":worker.ready,
            "mcp_registered":worker.ready,"status":s.state,"reason":s.error}));
    }
    Ok(json!(workers))
}

pub(crate) enum Selection {
    Ready(Box<Project>),
    Unavailable(Value),
}

fn unavailable(target: &str, reason: &str) -> Selection {
    Selection::Unavailable(
        json!({"status":"worker_unavailable","target_cli":target,"reason":reason,"work_queued":false}),
    )
}

/// A first request starts a seat only. Work is admitted after the native CLI
/// has proved its identity and registered its own generation-scoped companion.
pub(crate) async fn select(
    app: &Arc<App>,
    p: &Project,
    target: &str,
    origin: Option<String>,
) -> Result<Selection> {
    ensure!(
        ["codex", "claude", "pi", "agy"].contains(&target),
        "Unsupported target CLI; choose codex, claude, pi, or agy"
    );
    let parent = p.clone();
    let target_key = target.to_owned();
    let current = app
        .db
        .call(move |c| {
            controls(c, &parent)?;
            // No launch permit is released before the execution is committed.
            // A failed preflight with no recorded execution can be replaced after
            // a short cooldown, leaving the old generation and diagnostic intact.
            let generation = c
                .query_row(
                    "SELECT w.generation FROM manager_workers w
             WHERE w.project_id=? AND w.agent=? AND w.state!='closed'
               AND NOT (w.state='unavailable' AND w.execution='null' AND w.native_session=''
                 AND w.created_at<=?
                 AND NOT EXISTS(SELECT 1 FROM work_executions e WHERE e.generation=w.generation))
             ORDER BY w.rowid DESC LIMIT 1",
                    params![parent.id, target_key, db::now() - PREFLIGHT_RETRY_MS],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            generation
                .map(|g| seat(c, &parent, &g))
                .transpose()
                .map(Option::flatten)
        })
        .await?;
    if target == p.agent {
        let parent = p.clone();
        let usable = app
            .db
            .call(move |c| {
                Ok(!crate::cli_switch::latest(c, &parent.id)?
                    .as_ref()
                    .is_some_and(crate::cli_switch::blocks_worker))
            })
            .await?;
        if !usable || App::update_required(p) || !p.ready || p.execution["mcp_registered"] != true {
            return Ok(unavailable(
                target,
                "The selected native conversation is not ready for work",
            ));
        }
        let cfg = app.config.read().await.clone();
        return Ok(if available(&cfg, p).await {
            Selection::Ready(Box::new(p.clone()))
        } else {
            unavailable(target, "The selected native conversation is not available")
        });
    }
    if !matches!(target, "codex" | "claude") {
        return Ok(unavailable(
            target,
            "This CLI currently supports the project's selected native conversation only",
        ));
    }
    if let Some(current) = current {
        return Ok(match current.state.as_str() {
            "ready" if available(&app.config.read().await.clone(), &current.project).await => {
                Selection::Ready(Box::new(current.project))
            }
            "ready" => unavailable(target, "The worker's native session is not available"),
            "starting" => unavailable(
                target,
                "Starting the independent native session; check worker_status before retrying",
            ),
            _ => unavailable(target, &current.error),
        });
    }

    let generation = db::id();
    let token = db::id();
    let project_id = p.id.clone();
    let agent = target.to_owned();
    let generation_key = generation.clone();
    let credential = token.clone();
    app.db.call(move |c| {
        c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,created_at,origin_event_id) VALUES(?,?,?,?,'starting',?,?)",
            params![generation_key,project_id,agent,credential,db::now(),origin])?;
        Ok(())
    }).await?;
    let mut worker = p.clone();
    worker.name = format!("{} · {}", p.name, target);
    worker.agent = target.into();
    worker.mcp_token = token;
    worker.generation = generation;
    worker.native_session.clear();
    worker.terminal = None;
    worker.terminal_pid = None;
    worker.ready = false;
    worker.extra_args.clear();
    worker.execution = Value::Null;
    let app = Arc::clone(app);
    tokio::spawn(async move {
        let Some(_permit) = app.service.gate.enter() else {
            return;
        };
        let id = worker.id.clone();
        let lock = app.project_lock(&id).await;
        let _guard = lock.lock().await;
        let generation = worker.generation.clone();
        if launch(&app, worker).await.is_err() {
            // No tool input has been submitted. Do not surface arbitrary CLI
            // output, which can contain local configuration or credentials.
            let _ = app
                .db
                .call(move |c| {
                    fail(
                        c,
                        &id,
                        &generation,
                        "Native startup failed; verify that the CLI is installed and logged in",
                    )
                })
                .await;
        }
    });
    Ok(unavailable(
        target,
        "Starting the independent native session; check worker_status before retrying",
    ))
}

async fn observe(cfg: &config::Config, p: &Project) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(3), herdr::observe(cfg, p))
        .await
        .context("Worker observation exceeded the time budget")?
}

async fn available(cfg: &config::Config, p: &Project) -> bool {
    observe(cfg, p).await.is_ok_and(|r| {
        r["state"] == "live"
            && matches!(
                r["status"]["state"].as_str(),
                Some("idle" | "done" | "working")
            )
            && herdr::check_identity(p, &r).is_ok()
    })
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
    let session_dir = app.dir.join("sessions").join(&p.id).join(&p.generation);
    config::private_dir(&session_dir)?;
    let credentials = json!({"url":cfg.url(),"project":p.id,"token":p.mcp_token,
        "generation":p.generation,"mcp_contract":mcp::CONTRACT});
    config::write_private(
        &session_dir.join("credentials.json"),
        &serde_json::to_vec(&credentials)?,
    )?;
    let companion = app.companion()?;
    let entry = json!({"command":companion,"args":["mcp","--credentials",session_dir.join("credentials.json")]});
    let mcp_file = session_dir.join("mcp.json");
    config::write_private(
        &mcp_file,
        &serde_json::to_vec(&json!({"mcpServers":{"herdr_feishu":entry}}))?,
    )?;
    herdr::ensure_server(&cfg).await?;
    let integration = herdr::integration_status(&cfg, &p.agent).await?;
    ensure!(
        integration["state"] == "current",
        "The native integration is unavailable"
    );
    let id = p.id.clone();
    let agent = p.agent.clone();
    let selection = app.db.call(move |c| models::load(c, &id, &agent)).await?;
    let mut argv = adapter::launch_args_with_selection(
        &p,
        &mcp_file,
        &entry,
        "Wait for an explicit Dauerbot task. Work only on the assigned scope and coordinate shared workspace changes with the Manager. Use trusted event metadata for task identity and the discussion anchor. Use only the supplied project MCP tools to report results; preserve the original discussion anchor and request ID for a retry, and review uncertain effects before attempting any new send.",
        false,
        selection.as_ref(),
    )?;
    if p.agent == "claude" {
        argv.splice(1..1, ["--session-id".into(), p.generation.clone()]);
    }
    p.execution = herdr::prepare_execution(&cfg, &p, &argv, &p.generation).await?;
    p.execution["api_protocol"] = json!(crate::api::PROTOCOL);
    p.execution["mcp_contract"] = json!(mcp::CONTRACT);
    p.execution["companion"] = json!(companion);
    p.terminal = p.execution["terminal_id"].as_str().map(str::to_owned);
    p.terminal_pid = p.execution["shell_pid"].as_i64();
    if p.agent == "claude" {
        p.execution["intended_native_session"] = json!(p.generation);
    }
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
    let changed = tx.execute("UPDATE manager_workers SET state='unavailable',error=? WHERE generation=? AND project_id=? AND state IN ('starting','ready')",
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

pub(crate) async fn reconcile(app: &Arc<App>, parent: &Project) -> Result<()> {
    let p = parent.clone();
    let seats = app.db.call(move |c| active(c, &p)).await?;
    let cfg = app.config.read().await.clone();
    for s in seats {
        let worker = s.project;
        // Errors in one seat must never suppress the other seat or the primary.
        if let Err(error) = reconcile_one(app, &cfg, parent, &worker, &s.state, s.created_at).await
        {
            tracing::warn!(generation=%worker.generation, "Worker reconciliation failed: {error}");
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
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let status = record
        .as_ref()
        .and_then(|r| r["status"]["state"].as_str())
        .unwrap_or("");
    let failure = if record.as_ref().is_some_and(|r| r["state"] != "live") {
        Some("Independent worker exited before its pending work could continue")
    } else if matches!(status, "blocked" | "error") {
        Some("Independent worker needs native login, permission, or runtime attention")
    } else if state == "starting" && db::now() - created > 90_000 {
        Some("Independent worker did not establish native and MCP readiness")
    } else if state == "ready"
        && record
            .as_ref()
            .is_some_and(|r| herdr::check_identity(worker, r).is_err())
    {
        Some("Independent worker native identity changed")
    } else {
        None
    };
    let id = worker.id.clone();
    let generation = worker.generation.clone();
    if let Some(reason) = failure {
        return app
            .db
            .call(move |c| fail(c, &id, &generation, reason))
            .await;
    }
    if state != "ready" || !matches!(status, "idle" | "done") {
        return Ok(());
    }
    let sequence = record.as_ref().and_then(|r| r["state_change_seq"].as_i64());
    let parent = parent.clone();
    let run = app
        .db
        .call(move |c| {
            work::observe_idle(c, &id, &generation, sequence, db::now())?;
            if controls(c, &parent).is_err() || App::update_required(&parent) {
                return Ok(None);
            }
            work::claim_worker(c, &id, &generation, db::now())
        })
        .await?;
    if let Some(run) = run {
        app.deliver_run(worker, &run).await?;
    }
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
        authorize(c, &parent, generation, true)?;
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

    #[tokio::test]
    async fn task_worker_launches_an_exact_independent_pane_and_registers() {
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
                    "integration.list" => {
                        json!({"integrations":[{"target":"claude","state":"current"}]})
                    }
                    "workspace.create" => {
                        json!({"workspace":{"workspace_id":"w1"},"root_pane":{"pane_id":"w1:p1","terminal_id":"root","workspace_id":"w1"}})
                    }
                    "layout.apply" => json!({"layout":{"root":{"pane_id":"w1:p2"}}}),
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
                        json!({"process_info":{"shell_pid":8420,"foreground_processes":[{"name":"claude","argv":["claude"]}]}})
                    }
                    "agent.get" => {
                        json!({"agent":{"agent":"claude","agent_status":"idle","pane_id":"w1:p2","terminal_id":"worker-terminal","workspace_id":"w1","agent_session":{"value":tokens["hfb_generation"]}}})
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
        let retained = crate::companion::pin(
            &app.dir,
            &runtime,
            crate::update::CURRENT,
            crate::api::PROTOCOL,
        )
        .unwrap();
        assert!(app.companion.set(retained).is_ok());
        let parent = app
            .db
            .create(db::ProjectInput {
                name: "Independent launch fixture".into(),
                path: fixture.path().to_string_lossy().into(),
                agent: "agy".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,error,created_at)
                VALUES('failed-preflight',?,'claude','fixture-old','unavailable','Integration missing',?)",
                params![parent.id,db::now()])?;
            Ok(())
        }).unwrap();
        assert!(matches!(select(&app,&parent,"claude",None).await.unwrap(),
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
        assert!(
            matches!(select(&app,&parent,"claude",None).await.unwrap(), Selection::Unavailable(result) if result["work_queued"]==false)
        );
        let launched = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pending = app
                    .db
                    .sync(|c| {
                        Ok(active(c, &parent)?.into_iter().find(|s| {
                            s.project.execution["launch_permit"]
                                .as_str()
                                .is_some_and(|permit| std::path::Path::new(permit).exists())
                        }))
                    })
                    .unwrap();
                if let Some(pending) = pending {
                    break pending.project;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the independent generation should release its launch gate");
        assert_eq!(launched.terminal_pid, Some(8420));
        assert!(
            register(&app, launched.clone(), "w9:p9", Some(mcp::CONTRACT))
                .await
                .is_err()
        );
        assert_eq!(
            register(&app, launched.clone(), "w1:p2", Some(mcp::CONTRACT))
                .await
                .unwrap()["ok"],
            true
        );
        assert!(
            matches!(select(&app,&parent,"claude",None).await.unwrap(), Selection::Ready(worker) if worker.generation==launched.generation)
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
