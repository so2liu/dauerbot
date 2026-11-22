//! Explicit manager decisions for obsolete owned panes; never a background reaper.
use crate::{App, db, db::Project, herdr, work};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloseArgs {
    generation: String,
    terminal_id: String,
    reason: String,
    task_finished: bool,
}

pub(crate) fn migrate(c: &Connection, recover: bool) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS manager_close_audit(effect_id TEXT PRIMARY KEY REFERENCES manager_effects(id) ON DELETE CASCADE, project_id TEXT NOT NULL, manager_run TEXT NOT NULL, target_generation TEXT NOT NULL, terminal_id TEXT NOT NULL, reason TEXT NOT NULL, evidence TEXT NOT NULL DEFAULT '{}', status TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);")?;
    // Existing stores predate the project-lifetime cascade. Rebuild inside the
    // caller's migration transaction, retaining every audit field verbatim.
    let cascade: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list('manager_close_audit') WHERE [table]='manager_effects' AND [from]='effect_id' AND on_delete='CASCADE')", [], |r|r.get(0))?;
    if !cascade {
        c.execute_batch("CREATE TABLE manager_close_audit_cascade(effect_id TEXT PRIMARY KEY REFERENCES manager_effects(id) ON DELETE CASCADE, project_id TEXT NOT NULL, manager_run TEXT NOT NULL, target_generation TEXT NOT NULL, terminal_id TEXT NOT NULL, reason TEXT NOT NULL, evidence TEXT NOT NULL DEFAULT '{}', status TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
            INSERT INTO manager_close_audit_cascade SELECT * FROM manager_close_audit;
            DROP TABLE manager_close_audit;
            ALTER TABLE manager_close_audit_cascade RENAME TO manager_close_audit;")?;
    }
    if recover {
        c.execute("UPDATE manager_close_audit SET status='uncertain' WHERE status IN ('checking','submitting')", [])?;
    }
    Ok(())
}

fn guard(c: &Connection, p: &Project, generation: &str, terminal: &str) -> Result<()> {
    let fresh = db::project_by_key(c, &p.id)?;
    let p = &fresh;
    let old = work::execution(c, &p.id, generation)?
        .context("Unknown execution ownership; preserve the pane")?;
    ensure!(
        old.terminal == terminal && !terminal.is_empty(),
        "Execution terminal identity differs"
    );
    ensure!(
        matches!(
            old.status.as_str(),
            "sleeping" | "stopped" | "exited" | "error"
        ),
        "Execution is not finished"
    );
    let bound: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE generation=?1 OR terminal=?2)
            OR EXISTS(SELECT 1 FROM manager_workers WHERE state IN ('starting','ready')
              AND (generation=?1 OR json_extract(execution,'$.terminal_id')=?2))",
        params![generation, terminal],
        |r| r.get(0),
    )?;
    ensure!(!bound, "A current project or task worker still binds this execution");
    ensure!(
        !p.paused && !work::has_project_takeover(c, &p.id)?,
        "Management is paused or under user takeover"
    );
    ensure!(
        !crate::cli_switch::latest(c, &p.id)?.is_some_and(|r| r.holds_input),
        "A CLI transition still owns execution delivery"
    );
    let active: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM work_runs WHERE project_id=?1 AND generation=?2 AND (finished_at IS NULL OR status NOT IN ('settled','completed','cancelled','interrupted','failed'))) OR EXISTS(SELECT 1 FROM work_leases WHERE project_id=?1 AND generation=?2 AND active=1) OR EXISTS(SELECT 1 FROM work_events WHERE project_id=?1 AND status IN ('queued','worker_waiting','submitting','submitted','uncertain') AND (json_extract(payload,'$.target_generation')=?2 OR id IN (SELECT event_id FROM work_runs WHERE project_id=?1 AND generation=?2))) OR EXISTS(SELECT 1 FROM manager_inputs WHERE project_id=?1 AND status IN ('queued','processing') AND (json_extract(payload,'$.generation')=?2 OR json_extract(payload,'$.worker_run.generation')=?2 OR json_extract(payload,'$.run_id') IN (SELECT id FROM work_runs WHERE project_id=?1 AND generation=?2)))", params![p.id,generation], |r| r.get(0))?;
    ensure!(
        !active,
        "Pending or uncertain work still refers to this execution"
    );
    let pending_messages:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE project_id=?1 AND ((direction='out' AND status IN ('queued','pending','sending','submitting','uncertain')) OR (direction='in' AND status='queued' AND json_extract(attachment,'$.target_generation')=?2))) OR EXISTS(SELECT 1 FROM manager_inputs i WHERE i.project_id=?1 AND i.status='processing' AND NOT EXISTS(SELECT 1 FROM manager_input_claims own JOIN manager_runs r ON r.id=own.run_id WHERE own.input_id=i.id AND r.status='working'))",params![p.id,generation],|r|r.get(0))?;
    ensure!(
        !pending_messages,
        "Unresolved input or message effects prevent a proven-safe close"
    );
    let uncertain: bool = c.query_row("SELECT EXISTS(
        SELECT 1 FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id
        WHERE r.project_id=?1 AND e.status='uncertain'
          AND (COALESCE(NULLIF(json_extract(e.result,'$.generation'),''),
                        NULLIF(json_extract(e.arguments,'$.generation'),''),
                        CASE WHEN e.name IN ('cli_send_keys','steer_worker') THEN json_extract(r.checkpoint,'$.target_generation') END)=?2
               OR json_extract(e.arguments,'$.terminal_id')=?3))
        OR EXISTS(SELECT 1 FROM manager_close_audit WHERE project_id=?1
          AND target_generation=?2 AND terminal_id=?3 AND status IN ('submitting','uncertain'))", params![p.id,generation,terminal], |r|r.get(0))?;
    ensure!(
        !uncertain,
        "Uncertain management effects require inspection before closure"
    );
    Ok(())
}

/// Read-only inventory has one total budget, independent of historical row count.
pub(crate) async fn inspect(app: &Arc<App>, p: &Project) -> Result<Value> {
    match tokio::time::timeout(std::time::Duration::from_secs(3), inspect_inner(app, p)).await {
        Ok(result) => result,
        Err(_) => Ok(
            json!([{"close_eligible":false,"observation":"incomplete","reason":"The total three-second inspection budget elapsed; runtime eligibility is unknown"}]),
        ),
    }
}

async fn inspect_inner(app: &Arc<App>, p: &Project) -> Result<Value> {
    let id = p.id.clone();
    let rows=app.db.call(move |c|Ok(c.prepare("SELECT generation,terminal,status FROM work_executions WHERE project_id=? ORDER BY updated_at DESC LIMIT 24")?.query_map([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)).await?;
    let config = app.config.read().await.clone();
    let mut targets = Vec::new();
    for (generation, terminal, status) in rows {
        if generation == p.generation || p.terminal.as_deref() == Some(&terminal) {
            continue;
        }
        let project = p.clone();
        let gen_copy = generation.clone();
        let term_copy = terminal.clone();
        let eligibility = app
            .db
            .call(move |c| guard(c, &project, &gen_copy, &term_copy))
            .await;
        let observed = async {
            eligibility?;
            herdr::owned_close::inspect(&config, p, &generation, &terminal).await
        }
        .await;
        targets.push(match observed {
            Ok(pane)=>json!({"generation":generation,"terminal_id":terminal,"execution_status":status,"pane_id":pane,"close_eligible":true,"task_success":"manager_must_verify"}),
            Err(error)=>json!({"generation":generation,"terminal_id":terminal,"execution_status":status,"close_eligible":false,"reason":error.to_string()})
        });
    }
    Ok(json!(targets))
}

fn audit(c: &Connection, effect: &str, status: &str, evidence: &Value) -> Result<()> {
    c.execute(
        "UPDATE manager_close_audit SET status=?,evidence=?,updated_at=? WHERE effect_id=?",
        params![status, evidence.to_string(), db::now(), effect],
    )?;
    Ok(())
}

/// Called only by an explicit manager tool effect while holding its project lock.
pub(crate) async fn close(
    app: &Arc<App>,
    p: &Project,
    authority: &crate::manager::Context,
    effect: &str,
    args: CloseArgs,
) -> Result<Value> {
    ensure!(
        args.task_finished && !args.reason.trim().is_empty(),
        "Confirm semantic task completion and give a close reason"
    );
    let project = p.clone();
    let run_id = authority.run.clone();
    let effect_id = effect.to_owned();
    let generation = args.generation.clone();
    let terminal = args.terminal_id.clone();
    let reason = args.reason.clone();
    let prior=app.db.call(move |c|{
        if let Some((status,evidence))=c.query_row("SELECT status,evidence FROM manager_close_audit WHERE effect_id=?",[&effect_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()? {return Ok(Some(json!({"status":if matches!(status.as_str(),"checking"|"submitting"){"uncertain"}else{&status},"evidence":serde_json::from_str::<Value>(&evidence)?})));}
        c.execute("INSERT INTO manager_close_audit(effect_id,project_id,manager_run,target_generation,terminal_id,reason,status,created_at,updated_at) VALUES(?,?,?,?,?,?,'checking',?,?)",params![effect_id,project.id,run_id,generation,terminal,reason,db::now(),db::now()])?;
        Ok(None)
    }).await?;
    if let Some(prior) = prior {
        return Ok(prior);
    }
    let config = app.config.read().await.clone();
    let preflight = async {
        let project = p.clone();
        let generation = args.generation.clone();
        let terminal = args.terminal_id.clone();
        app.db
            .call(move |c| guard(c, &project, &generation, &terminal))
            .await?;
        herdr::owned_close::prepare(&config, p, &args.generation, &args.terminal_id).await
    }
    .await;
    let prepared = match preflight {
        Ok(value) => value,
        Err(error) => {
            let result =
                json!({"status":"refused","reason":error.to_string(),"checked_at":db::now()});
            let id = effect.to_owned();
            let saved = result.clone();
            app.db
                .call(move |c| audit(c, &id, "refused", &saved))
                .await?;
            return Ok(result);
        }
    };
    let evidence = json!({"pane_id":prepared.pane_id(),"terminal_id":args.terminal_id,"generation":args.generation,"observed":prepared.snapshot(),"checked_at":db::now(),"task_finished_confirmed_by_manager":true});
    let id = effect.to_owned();
    let saved = evidence.clone();
    let auth = authority.clone();
    let generation = args.generation.clone();
    let terminal = args.terminal_id.clone();
    let admitted = app
        .db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            let fresh = crate::manager::valid(&tx, &auth)?;
            guard(&tx, &fresh, &generation, &terminal)?;
            audit(&tx, &id, "submitting", &saved)?;
            tx.commit()?;
            Ok(())
        })
        .await;
    if let Err(error) = admitted {
        let result = json!({"status":"refused","reason":error.to_string(),"checked_at":db::now()});
        let id = effect.to_owned();
        let saved = result.clone();
        app.db
            .call(move |c| audit(c, &id, "refused", &saved))
            .await?;
        return Ok(result);
    }
    let (acknowledged, absent) = prepared.dispatch(&config).await;
    let status = if absent { "closed" } else { "uncertain" };
    let result = json!({"status":status,"evidence":evidence,"dispatch_acknowledged":acknowledged,"verified_absent":absent});
    let id = effect.to_owned();
    let saved = result.clone();
    let generation = args.generation;
    app.db.call(move |c| {
        let tx = c.unchecked_transaction()?;
        if absent {
            crate::manager_workers::closed(&tx,&generation)?;
        }
        audit(&tx,&id,status,&saved)?;
        tx.commit()?;
        Ok(())
    }).await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::ProjectInput};
    use std::process::Stdio;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };
    const OLD: &str = "d18eb4b0-0200-4acc-8fe2-e0e380b7cf87";
    const CURRENT: &str = "eedb63cf-f4db-4a8f-a06f-5ff74cd7a66c";
    async fn fixture() -> (tempfile::TempDir, Arc<App>, Project) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            herdr_session: dir.path().to_string_lossy().into(),
            ..Config::default()
        };
        let app = App::new(dir.path().join("store"), cfg).unwrap();
        let p = app
            .db
            .create(ProjectInput {
                name: "Close fixture".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: "native-kept".into(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        app.db.sync(|c|{
            c.execute("UPDATE projects SET generation=?,terminal='current-terminal' WHERE id=?",params![CURRENT,p.id])?;
            c.execute("INSERT INTO work_executions(generation,project_id,agent,terminal,status,last_activity,created_at,updated_at) VALUES(?,?,'codex','old-terminal','exited',1,1,1)",params![OLD,p.id])?;
            c.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at) VALUES('close-run',?,1,1,'{}','working',?,1)",params![p.id,crate::manager::owner()])?;
            c.execute("UPDATE manager_config SET revision=1,input_revision=1 WHERE project_id=?",[&p.id])?;
            c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES('close-effect','close-run','one','close_agent','{}','submitting')",[])?;
            Ok(())
        }).unwrap();
        let p = app.db.project(&p.id).await.unwrap();
        (dir, app, p)
    }
    fn audit_rows(c: &Connection) -> Result<Vec<Vec<rusqlite::types::Value>>> {
        Ok(
            c.prepare("SELECT * FROM manager_close_audit ORDER BY effect_id")?
                .query_map([], |r| (0..10).map(|index| r.get(index)).collect())?
                .collect::<rusqlite::Result<_>>()?,
        )
    }
    #[tokio::test]
    async fn project_removal_cascades_audits_and_notes_after_fresh_and_legacy_migration() {
        for legacy in [false, true] {
            let (_dir, app, p) = fixture().await;
            let path = app.dir.join("bridge.sqlite3");
            let mut expected=app.db.sync(|c| {
                if legacy {
                    // Reconstruct the actually shipped constraints in a private
                    // production store, then reopen through Db's outer transaction.
                    for table in ["manager_close_audit","manager_turn_notes"] {
                        let sql:String=c.query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name=?",[table],|r|r.get(0))?;
                        c.execute_batch(&format!("DROP TABLE {table}; {}",sql.replace(" ON DELETE CASCADE","")))?;
                    }
                }
                c.execute("UPDATE projects SET terminal=NULL WHERE id=?",[&p.id])?;
                c.execute("INSERT INTO manager_turn_notes(run_id,note) VALUES('close-run','preserve this decision verbatim')",[])?;
                for status in ["refused","uncertain","checking"] {
                    let effect=format!("audit-{status}");
                    c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES(?,'close-run',?,'close_agent','{}','complete')",params![effect,status])?;
                    c.execute("INSERT INTO manager_close_audit(effect_id,project_id,manager_run,target_generation,terminal_id,reason,evidence,status,created_at,updated_at) VALUES(?,?,'close-run',?,'old-terminal','retain reason','{\"proof\":\"unchanged\"}',?,17,23)",params![effect,p.id,OLD,status])?;
                }
                audit_rows(c)
            }).unwrap();
            drop(app);
            for row in &mut expected {
                if row[7] == rusqlite::types::Value::Text("checking".into()) {
                    row[7] = rusqlite::types::Value::Text("uncertain".into());
                }
            }
            let db = db::Db::open_handover(&path).unwrap();
            db.sync(|c| {
                assert_eq!(audit_rows(c)?, expected);
                assert_eq!(
                    c.query_row("SELECT note FROM manager_turn_notes", [], |r| r
                        .get::<_, String>(0))?,
                    "preserve this decision verbatim"
                );
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
                        .get::<_, i64>(
                        0
                    ))?,
                    0
                );
                Ok(())
            })
            .unwrap();
            drop(db);
            let db = db::Db::open(&path).unwrap();
            db.sync(|c| {
                assert_eq!(c.query_row("SELECT status FROM manager_close_audit WHERE effect_id='audit-checking'",[],|r|r.get::<_,String>(0))?,"uncertain");
                assert_eq!(c.query_row("SELECT evidence FROM manager_close_audit WHERE effect_id='audit-uncertain'",[],|r|r.get::<_,String>(0))?,r#"{"proof":"unchanged"}"#);
                assert_eq!(c.query_row("SELECT COUNT(*) FROM manager_close_audit",[],|r|r.get::<_,i64>(0))?,3);
                Ok(())
            }).unwrap();
            db.remove_project(&p.id).await.unwrap();
            db.sync(|c| {
                for table in [
                    "projects",
                    "manager_runs",
                    "manager_effects",
                    "manager_close_audit",
                    "manager_turn_notes",
                ] {
                    assert_eq!(
                        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                            .get::<_, i64>(0))?,
                        0,
                        "{table}"
                    );
                }
                Ok(())
            })
            .unwrap();
        }
    }
    fn delivery_input(c: &Connection, p: &Project) -> Result<work::Event> {
        let event = crate::manager::test_delegate(c, &p.id, "delivery-fixture", None)?;
        c.execute("UPDATE work_events SET payload=json_set(payload,'$.target_generation',?) WHERE id=?", params![OLD,event])?;
        work::event(c, &p.id, &event)
    }
    #[tokio::test]
    async fn a_task_worker_binding_protects_even_an_old_execution_record() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c| {
            guard(c, &p, OLD, "old-terminal")?;
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,created_at)
                VALUES(?,?,'codex','fixture','ready',0)", params![OLD,p.id])?;
            assert!(guard(c, &p, OLD, "old-terminal").unwrap_err().to_string().contains("task worker"));
            crate::manager_workers::closed(c, OLD)?;
            guard(c, &p, OLD, "old-terminal")?;
            Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn resolved_delivery_attempts_allow_close_but_active_and_uncertain_work_do_not() {
        for resolution in ["retry", "dismiss", "failed"] {
            let (_dir, app, p) = fixture().await;
            app.db
                .sync(|c| {
                    let event = delivery_input(c, &p)?;
                    let first = work::claim_next(c, &p.id, OLD, 2)?.unwrap();
                    assert!(guard(c, &p, OLD, "old-terminal").is_err());
                    work::delivery(c, &first.id, "queued", "Definitely not submitted", 3)?;
                    assert_eq!(work::run(c, &p.id, &first.id)?.status, "interrupted");
                    assert!(work::run(c, &p.id, &first.id)?.finished_at.is_some());
                    assert!(
                        guard(c, &p, OLD, "old-terminal").is_err(),
                        "Queued retry still protects the target"
                    );
                    let next = work::claim_next(c, &p.id, OLD, 4)?.unwrap();
                    match resolution {
                        "retry" => {
                            work::delivery(c, &next.id, "submitted", "", 5)?;
                            assert!(guard(c, &p, OLD, "old-terminal").is_err());
                            work::observe_idle(c, &p.id, OLD, None, 3006)?;
                            assert_eq!(work::run(c, &p.id, &next.id)?.status, "settled");
                        }
                        "dismiss" => {
                            work::delivery(c, &next.id, "uncertain", "Ambiguous submission", 5)?;
                            assert!(guard(c, &p, OLD, "old-terminal").is_err());
                            work::resolve_event(c, &p.id, &event.id, false, 6)?;
                            assert_eq!(work::run(c, &p.id, &next.id)?.status, "interrupted");
                        }
                        "failed" => {
                            work::delivery(
                                c,
                                &next.id,
                                "failed",
                                "Definitely failed before submission",
                                5,
                            )?;
                            assert_eq!(work::run(c, &p.id, &next.id)?.status, "failed");
                            work::resolve_event(c, &p.id, &event.id, false, 6)?;
                        }
                        _ => unreachable!(),
                    }
                    // The manager has already inspected these delivery receipts.
                    c.execute("UPDATE manager_inputs SET status='consumed' WHERE project_id=? AND kind='worker'", [&p.id])?;
                    guard(c, &p, OLD, "old-terminal").context("Finished historical attempts must not pin the execution forever")?;
                    // Unknown/unfinished terminal records and explicit uncertainty
                    // remain blockers, even if an inconsistent timestamp is present.
                    c.execute(
                        "UPDATE work_runs SET status='interrupted',finished_at=NULL WHERE id=?",
                        [&first.id],
                    )?;
                    assert!(guard(c, &p, OLD, "old-terminal").is_err());
                    c.execute(
                        "UPDATE work_runs SET status='uncertain',finished_at=7 WHERE id=?",
                        [&first.id],
                    )?;
                    assert!(guard(c, &p, OLD, "old-terminal").is_err());
                    Ok(())
                })
                .unwrap();
        }
    }
    fn authority(p: &Project) -> crate::manager::Context {
        crate::manager::Context {
            project: p.id.clone(),
            run: "close-run".into(),
            revision: 1,
            input_revision: 1,
        }
    }
    fn args() -> CloseArgs {
        CloseArgs {
            generation: OLD.into(),
            terminal_id: "old-terminal".into(),
            reason: "Verified that the old task is complete".into(),
            task_finished: true,
        }
    }
    struct Runtime {
        close_count: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
        _shell: tokio::process::Child,
    }
    async fn runtime(app: &Arc<App>, p: &Project, scenario: &'static str) -> Runtime {
        let cfg = app.config.read().await.clone();
        let listener = UnixListener::bind(herdr::socket_path(&cfg).unwrap()).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let output_count = count.clone();
        let output_requests = requests.clone();
        let mut pane = json!({"pane_id":"w-old:p1","workspace_id":"w-old","terminal_id":"old-terminal","agent_status":"done","tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":OLD,"hfb_execution":herdr::managed_name(OLD).unwrap()}});
        if scenario == "unknown" {
            pane["tokens"] = json!({});
        }
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut shell = command.spawn().unwrap();
        let shell_pid = shell.id().unwrap();
        shell.wait().await.unwrap();
        let acting = app.clone();
        let acting_project = p.id.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (input, mut output) = stream.into_split();
                let raw = BufReader::new(input)
                    .lines()
                    .next_line()
                    .await
                    .unwrap()
                    .unwrap();
                let request: Value = serde_json::from_str(&raw).unwrap();
                let method = request["method"].as_str().unwrap();
                requests.lock().unwrap().push(method.into());
                let result = match method {
                    "pane.list" => {
                        json!({"panes":if closed.load(Ordering::SeqCst){vec![]}else{vec![pane.clone()]}})
                    }
                    "pane.get" => {
                        if scenario == "steer" {
                            acting.db.sync(|c|{c.execute("UPDATE manager_config SET input_revision=input_revision+1 WHERE project_id=?",[&acting_project])?;Ok(())}).unwrap();
                        }
                        json!({"pane":pane})
                    }
                    "workspace.list" => {
                        json!({"workspaces":if scenario=="group" {vec![json!({"workspace_id":"w-old","pane_count":1,"worktree":{"repo_key":"group","is_linked_worktree":false}}),json!({"workspace_id":"w-protected","pane_count":1,"worktree":{"repo_key":"group","is_linked_worktree":true}})]}else{vec![json!({"workspace_id":"w-old","pane_count":1,"worktree":null})]}})
                    }
                    "pane.process_info" => {
                        json!({"process_info":if scenario=="alive" {json!({"pane_id":"w-old:p1","shell_pid":shell_pid,"foreground_processes":[{"pid":shell_pid,"name":"codex"}]})}else{json!({"pane_id":"w-old:p1","shell_pid":shell_pid})}})
                    }
                    "pane.close" => {
                        count.fetch_add(1, Ordering::SeqCst);
                        if scenario == "uncertain" {
                            continue;
                        }
                        closed.store(true, Ordering::SeqCst);
                        json!({})
                    }
                    _ => panic!("Unexpected lifecycle request {method}"),
                };
                output
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        Runtime {
            close_count: output_count,
            requests: output_requests,
            task,
            _shell: shell,
        }
    }
    #[tokio::test]
    async fn explicit_close_is_observable_audited_and_never_replayed() {
        let (_dir, app, p) = fixture().await;
        let runtime = runtime(&app, &p, "safe").await;
        let targets = inspect(&app, &p).await.unwrap();
        assert_eq!(targets[0]["close_eligible"], true);
        assert_eq!(
            runtime.close_count.load(Ordering::SeqCst),
            0,
            "Inspection must never close anything"
        );
        let result = close(&app, &p, &authority(&p), "close-effect", args())
            .await
            .unwrap();
        assert_eq!(result["status"], "closed");
        assert_eq!(result["verified_absent"], true);
        let repeated = close(&app, &p, &authority(&p), "close-effect", args())
            .await
            .unwrap();
        assert_eq!(repeated["status"], "closed");
        assert_eq!(runtime.close_count.load(Ordering::SeqCst), 1);
        app.db
            .sync(|c| {
                let (status, reason, created, updated): (String, String, i64, i64) = c.query_row(
                    "SELECT status,reason,created_at,updated_at FROM manager_close_audit",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
                assert_eq!(status, "closed");
                assert!(!reason.is_empty());
                assert!(created > 0 && updated >= created);
                Ok(())
            })
            .unwrap();
        runtime.task.abort();
        let _ = runtime.task.await;
    }
    #[tokio::test]
    async fn done_native_process_and_unknown_ownership_are_retained() {
        for scenario in ["alive", "unknown", "group", "steer"] {
            let (_dir, app, p) = fixture().await;
            let runtime = runtime(&app, &p, scenario).await;
            let result = close(&app, &p, &authority(&p), "close-effect", args())
                .await
                .unwrap();
            assert_eq!(result["status"], "refused");
            assert_eq!(runtime.close_count.load(Ordering::SeqCst), 0);
            assert!(
                !runtime
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|method| method == "pane.close")
            );
            runtime.task.abort();
            let _ = runtime.task.await;
        }
    }
    #[tokio::test]
    async fn current_targets_and_unresolved_work_are_retained_with_schedules_enabled() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c|{
            work::begin_execution(c,&p.id,CURRENT,1)?;
            c.execute("UPDATE work_executions SET status='exited' WHERE generation=?",[CURRENT])?;
            assert!(guard(c,&p,CURRENT,"current-terminal").unwrap_err().to_string().contains("current project"));
            c.execute("INSERT INTO work_tasks(id,project_id,title,instructions,status,created_at,updated_at) VALUES('pending-task',?,'test','test','queued',1,1)",[&p.id])?;
            c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at) VALUES('pending-event',?,'pending-task','management','test',?,'uncertain',1,1)",params![p.id,json!({"target_generation":OLD}).to_string()])?;
            assert!(guard(c,&p,OLD,"old-terminal").is_err());
            c.execute("UPDATE work_events SET status='consumed' WHERE id='pending-event'",[])?;
            c.execute("INSERT INTO work_timers(id,project_id,task_id,name,prompt,kind,interval_seconds,timezone,enabled,created_at,updated_at) VALUES('timer',?,'pending-task','test','test','interval',60,'UTC',1,1,1)",[&p.id])?;
            assert!(guard(c,&p,OLD,"old-terminal").is_ok());
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('native-out',?,'out','native-send','fixture','uncertain',1,1)",[&p.id])?;
            assert!(guard(c,&p,OLD,"old-terminal").is_err());Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn off_page_processing_input_keeps_its_authoritative_owner_and_blocks_close() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c|{
            c.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at) VALUES('old-page',?,1,1,'{}','superseded','fixture',1)",[&p.id])?;
            c.execute("INSERT INTO manager_inputs(id,project_id,kind,payload,status,created_at) VALUES('off-page',?,'user','{}','processing',1)",[&p.id])?;
            c.execute("INSERT INTO manager_input_claims(input_id,run_id) VALUES('off-page','old-page')",[])?;
            assert!(guard(c,&p,OLD,"old-terminal").unwrap_err().to_string().contains("Unresolved input"));
            // A historical many-to-many join is not sufficient: only the one
            // durable owner may exempt the current, actively processed page.
            c.execute("INSERT INTO manager_run_inputs(run_id,input_id) VALUES('close-run','off-page')",[])?;
            assert!(guard(c,&p,OLD,"old-terminal").is_err());
            c.execute("UPDATE manager_input_claims SET run_id='close-run' WHERE input_id='off-page'",[])?;
            assert!(guard(c,&p,OLD,"old-terminal").is_ok());Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn obsolete_close_ignores_unrelated_queued_inputs_and_uncertain_effects() {
        let (_dir, app, p) = fixture().await;
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_inputs(id,project_id,kind,payload,channel_id,created_at)
                VALUES('unrelated',?,'user','{}','another-channel',1)", [&p.id])?;
            c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status)
                VALUES('history','close-run','previous','cli_send_keys',json_object('generation',?),'uncertain')", [CURRENT])?;
            c.execute("INSERT INTO manager_close_audit(effect_id,project_id,manager_run,target_generation,terminal_id,reason,status,created_at,updated_at)
                VALUES('history',?,'close-run',?,'other-terminal','fixture','uncertain',1,1)", params![p.id,CURRENT])?;
            guard(c, &p, OLD, "old-terminal")?;
            c.execute("UPDATE manager_effects SET arguments=json_object('generation',?) WHERE id='history'", [OLD])?;
            assert!(guard(c, &p, OLD, "old-terminal").is_err());
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn uncertain_close_stays_audited_without_a_second_dispatch() {
        let (_dir, app, p) = fixture().await;
        let runtime = runtime(&app, &p, "uncertain").await;
        let result = close(&app, &p, &authority(&p), "close-effect", args())
            .await
            .unwrap();
        assert_eq!(result["status"], "uncertain");
        let again = close(&app, &p, &authority(&p), "close-effect", args())
            .await
            .unwrap();
        assert_eq!(again["status"], "uncertain");
        app.db
            .sync(|c| {
                assert!(guard(c, &p, OLD, "old-terminal").is_err());
                Ok(())
            })
            .unwrap();
        assert_eq!(runtime.close_count.load(Ordering::SeqCst), 1);
        runtime.task.abort();
        let _ = runtime.task.await;
    }
}
