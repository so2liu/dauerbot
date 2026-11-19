//! Explicit manager decisions for obsolete owned panes; never a background reaper.
use crate::{App, config::Config, db, db::Project, herdr, work};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloseArgs {
    generation: String,
    terminal_id: String,
    reason: String,
    task_finished: bool,
}

pub(crate) fn migrate(c: &Connection, recover: bool) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS manager_close_audit(effect_id TEXT PRIMARY KEY REFERENCES manager_effects(id), project_id TEXT NOT NULL, manager_run TEXT NOT NULL, target_generation TEXT NOT NULL, terminal_id TEXT NOT NULL, reason TEXT NOT NULL, evidence TEXT NOT NULL DEFAULT '{}', status TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);")?;
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
        "SELECT EXISTS(SELECT 1 FROM projects WHERE generation=? OR terminal=?)",
        params![generation, terminal],
        |r| r.get(0),
    )?;
    ensure!(!bound, "A current project still binds this execution");
    ensure!(
        !p.paused && !work::has_takeover(c, &p.id, &p.generation)?,
        "Management is paused or under user takeover"
    );
    ensure!(
        !crate::cli_switch::latest(c, &p.id)?.is_some_and(|r| r.holds_input),
        "A CLI transition still owns execution delivery"
    );
    let active: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM work_runs WHERE project_id=?1 AND generation=?2 AND status NOT IN ('settled','completed','cancelled')) OR EXISTS(SELECT 1 FROM work_leases WHERE project_id=?1 AND generation=?2 AND active=1) OR EXISTS(SELECT 1 FROM work_events WHERE project_id=?1 AND status IN ('queued','submitting','submitted','uncertain') AND (json_extract(payload,'$.target_generation')=?2 OR json_extract(payload,'$.target_generation') IS NULL)) OR EXISTS(SELECT 1 FROM manager_inputs WHERE project_id=?1 AND status IN ('queued','processing') AND (json_extract(payload,'$.generation')=?2 OR json_extract(payload,'$.worker_run.generation')=?2 OR json_extract(payload,'$.run_id') IN (SELECT id FROM work_runs WHERE project_id=?1 AND generation=?2)))", params![p.id,generation], |r| r.get(0))?;
    ensure!(
        !active,
        "Pending or uncertain work still refers to this execution"
    );
    let pending_messages:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE project_id=?1 AND ((direction='out' AND status IN ('queued','pending','sending','submitting','uncertain')) OR (direction='in' AND status='queued'))) OR EXISTS(SELECT 1 FROM manager_inputs i WHERE i.project_id=?1 AND i.status IN ('queued','processing') AND NOT EXISTS(SELECT 1 FROM manager_input_claims own JOIN manager_runs r ON r.id=own.run_id WHERE own.input_id=i.id AND r.status='working'))",[&p.id],|r|r.get(0))?;
    ensure!(
        !pending_messages,
        "Unresolved input or message effects prevent a proven-safe close"
    );
    let timers: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_timers WHERE project_id=? AND enabled=1)",
        [&p.id],
        |r| r.get(0),
    )?;
    ensure!(
        !timers,
        "Enabled project schedules prevent an unambiguous close"
    );
    let uncertain: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id WHERE r.project_id=?1 AND e.status='uncertain') OR EXISTS(SELECT 1 FROM manager_close_audit WHERE project_id=?1 AND status IN ('submitting','uncertain'))", [&p.id], |r|r.get(0))?;
    ensure!(
        !uncertain,
        "Uncertain management effects require inspection before closure"
    );
    Ok(())
}

fn owns(p: &Project, generation: &str, terminal: &str, pane: &Value) -> bool {
    pane["terminal_id"] == terminal
        && pane["tokens"]["hfb_owner"] == "dauerbot"
        && pane["tokens"]["hfb_project"] == p.id
        && pane["tokens"]["hfb_generation"] == generation
        && herdr::managed_name(generation).is_ok_and(|name| pane["tokens"]["hfb_execution"] == name)
        && pane["pane_id"].as_str().is_some_and(|id| !id.is_empty())
        && pane["workspace_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
}

fn shell_snapshot(info: &Value) -> Result<Value> {
    let processes = info["foreground_processes"]
        .as_array()
        .context("Foreground state is unknown")?;
    ensure!(!processes.is_empty(), "Foreground state is unknown");
    let mut ids = Vec::new();
    for process in processes {
        let name = process["name"]
            .as_str()
            .context("Process identity is unknown")?;
        let basename = Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim_start_matches('-');
        ensure!(
            matches!(
                basename,
                "sh" | "bash" | "zsh" | "fish" | "dash" | "nu" | "pwsh" | "login"
            ),
            "A native or other foreground process is still alive"
        );
        let pid = process["pid"]
            .as_i64()
            .filter(|pid| *pid > 0)
            .context("Process identity is unknown")?;
        ids.push(json!({"pid":pid,"name":basename}));
    }
    ids.sort_by_key(|v| v["pid"].as_i64());
    let shell_pid = info["shell_pid"]
        .as_i64()
        .filter(|pid| *pid > 0)
        .context("Shell identity is unknown")?;
    Ok(json!({"shell_pid":shell_pid,"foreground":ids}))
}

#[derive(Clone)]
struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    session: i32,
    started: String,
    name: String,
}
fn process_census(root: i32, rows: &[Process]) -> Result<Value> {
    let leader = rows
        .iter()
        .find(|p| p.pid == root)
        .context("Owned shell process is unavailable")?;
    ensure!(
        leader.session == root && leader.group > 0,
        "Owned process session is unknown or shared"
    );
    let mut included = std::collections::BTreeSet::from([root]);
    loop {
        let count = included.len();
        for process in rows {
            if included.contains(&process.parent)
                || process.session == leader.session
                || process.group == leader.group
            {
                included.insert(process.pid);
            }
        }
        if included.len() == count {
            break;
        }
    }
    let mut evidence = Vec::new();
    for process in rows.iter().filter(|p| included.contains(&p.pid)) {
        ensure!(
            process.session > 0 && process.pid > 0 && !process.started.is_empty(),
            "Process lineage is unknown"
        );
        ensure!(
            matches!(
                process.name.as_str(),
                "sh" | "bash" | "zsh" | "fish" | "dash" | "nu" | "pwsh" | "login"
            ),
            "A native or background process is still alive"
        );
        evidence.push(json!({"pid":process.pid,"parent":process.parent,"group":process.group,"session":process.session,"started":process.started,"name":process.name}));
    }
    evidence.sort_by_key(|v| v["pid"].as_i64());
    Ok(json!(evidence))
}
async fn process_snapshot(info: &Value) -> Result<Value> {
    let foreground = shell_snapshot(info)?;
    let root = info["shell_pid"]
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .filter(|v| *v > 0)
        .context("Owned shell identity is unavailable")?;
    // Fixed host inspection only: no shell, user-supplied command or arguments.
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,pgid=,lstart=,comm="])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Process inspection timed out")??;
    ensure!(
        output.status.success() && output.stdout.len() <= 8 * 1024 * 1024,
        "Process inspection is unavailable or exceeds its bound"
    );
    let text = std::str::from_utf8(&output.stdout).context("Process inspection is not UTF-8")?;
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(fields.len() >= 9, "Process inspection is incomplete");
        let pid: i32 = fields[0].parse()?;
        let parent: i32 = fields[1].parse()?;
        let group: i32 = fields[2].parse()?;
        let executable = fields[8..].join(" ");
        let name = Path::new(&executable)
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .trim_start_matches('-')
            .to_owned();
        let session = unsafe { libc::getsid(pid) };
        rows.push(Process {
            pid,
            parent,
            group,
            session,
            started: fields[3..8].join(" "),
            name,
        });
    }
    let census = process_census(root, &rows)?;
    for process in foreground["foreground"]
        .as_array()
        .context("Foreground inspection is unavailable")?
    {
        ensure!(
            census
                .as_array()
                .is_some_and(|rows| rows.iter().any(|p| p["pid"] == process["pid"])),
            "Foreground process lineage changed"
        );
    }
    Ok(json!({"foreground":foreground,"process_census":census}))
}

fn workspace_scope(pane: &Value, list: &Value) -> Result<Value> {
    let workspaces = list["workspaces"]
        .as_array()
        .context("Workspace registry is unavailable")?;
    let matches: Vec<_> = workspaces
        .iter()
        .filter(|w| w["workspace_id"] == pane["workspace_id"])
        .collect();
    ensure!(
        matches.len() == 1,
        "Workspace identity is missing or ambiguous"
    );
    let target = matches[0];
    let count = target["pane_count"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("Workspace pane count is unknown")?;
    if let Some(worktree) = target.get("worktree").filter(|v| !v.is_null()) {
        let key = worktree["repo_key"]
            .as_str()
            .filter(|v| !v.is_empty())
            .context("Workspace group identity is unknown")?;
        ensure!(
            worktree["is_linked_worktree"].is_boolean(),
            "Workspace group lineage is unknown"
        );
        let associated = workspaces
            .iter()
            .filter(|w| w["worktree"]["repo_key"] == key)
            .count();
        ensure!(
            associated == 1,
            "Closing this pane could affect an associated workspace group"
        );
    }
    Ok(json!({"workspace_id":pane["workspace_id"],"pane_count":count,"associated_group":false}))
}
async fn workspace_snapshot(config: &Config, pane: &Value) -> Result<Value> {
    let list = herdr::rpc(config, "workspace.list", json!({})).await?;
    workspace_scope(pane, &list)
}

async fn pane_for(config: &Config, terminal: &str) -> Result<Value> {
    let list = herdr::rpc(config, "pane.list", json!({})).await?;
    let panes = list["panes"]
        .as_array()
        .context("Pane inventory is unavailable")?;
    let found: Vec<_> = panes
        .iter()
        .filter(|p| p["terminal_id"] == terminal)
        .collect();
    ensure!(found.len() == 1, "Execution pane is missing or ambiguous");
    Ok(found[0].clone())
}

pub(crate) async fn inspect(app: &Arc<App>, p: &Project) -> Result<Value> {
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
            let pane = pane_for(&config, &terminal).await?;
            ensure!(
                owns(p, &generation, &terminal, &pane),
                "Runtime ownership is unknown; historical session labels are not authority"
            );
            let info = herdr::rpc(
                &config,
                "pane.process_info",
                json!({"pane_id":pane["pane_id"]}),
            )
            .await?;
            process_snapshot(&info["process_info"]).await?;
            workspace_snapshot(&config, &pane).await?;
            Ok::<_, anyhow::Error>(pane["pane_id"].clone())
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
        args.generation.len() <= 128 && args.terminal_id.len() <= 128,
        "Execution identity exceeds its bound"
    );
    ensure!(
        args.task_finished && !args.reason.trim().is_empty() && args.reason.len() <= 2000,
        "Confirm semantic task completion and give a bounded close reason"
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
        let pane = pane_for(&config, &args.terminal_id).await?;
        ensure!(
            owns(p, &args.generation, &args.terminal_id, &pane),
            "Runtime ownership is unknown; preserve the pane"
        );
        let info = herdr::rpc(
            &config,
            "pane.process_info",
            json!({"pane_id":pane["pane_id"]}),
        )
        .await?;
        let snapshot = process_snapshot(&info["process_info"]).await?;
        let scope = workspace_snapshot(&config, &pane).await?;
        let fresh = herdr::rpc(&config, "pane.get", json!({"pane_id":pane["pane_id"]})).await?;
        ensure!(
            owns(p, &args.generation, &args.terminal_id, &fresh["pane"])
                && fresh["pane"]["workspace_id"] == pane["workspace_id"],
            "Execution identity changed before close"
        );
        let final_info = herdr::rpc(
            &config,
            "pane.process_info",
            json!({"pane_id":pane["pane_id"]}),
        )
        .await?;
        ensure!(
            process_snapshot(&final_info["process_info"]).await? == snapshot,
            "Foreground identity changed before close"
        );
        ensure!(
            workspace_snapshot(&config, &pane).await? == scope,
            "Workspace scope changed before close"
        );
        Ok::<_, anyhow::Error>((pane, json!({"process":snapshot,"workspace":scope})))
    }
    .await;
    let (pane, snapshot) = match preflight {
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
    let evidence = json!({"pane_id":pane["pane_id"],"terminal_id":args.terminal_id,"generation":args.generation,"observed":snapshot,"checked_at":db::now(),"task_finished_confirmed_by_manager":true});
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
    let sent = herdr::rpc(&config, "pane.close", json!({"pane_id":pane["pane_id"]})).await;
    // The legacy runtime has no conditional-close RPC. Keep the final preflight
    // adjacent to dispatch; ambiguous transport never permits a second close.
    let absent = match herdr::rpc(&config, "pane.list", json!({})).await {
        Ok(list) => list["panes"]
            .as_array()
            .is_some_and(|panes| panes.iter().all(|p| p["terminal_id"] != args.terminal_id)),
        Err(_) => false,
    };
    let status = if absent { "closed" } else { "uncertain" };
    let result = json!({"status":status,"evidence":evidence,"dispatch_acknowledged":sent.is_ok(),"verified_absent":absent});
    let id = effect.to_owned();
    let saved = result.clone();
    app.db.call(move |c| audit(c, &id, status, &saved)).await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ProjectInput;
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
            c.execute("INSERT INTO manager_config(project_id,mode,revision,input_revision) VALUES(?,'manager',1,1)",[&p.id])?;
            c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES('close-effect','close-run','one','close_agent','{}','submitting')",[])?;
            Ok(())
        }).unwrap();
        let p = app.db.project(&p.id).await.unwrap();
        (dir, app, p)
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
        let shell = command.spawn().unwrap();
        let shell_pid = shell.id().unwrap();
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
                        json!({"process_info":{"shell_pid":shell_pid,"foreground_processes":[{"pid":shell_pid,"name":if scenario=="alive"{"codex"}else{"zsh"}}]}})
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
    async fn current_targets_pending_work_and_enabled_schedules_are_retained() {
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
            assert!(guard(c,&p,OLD,"old-terminal").is_err());
            c.execute("UPDATE work_timers SET enabled=0 WHERE id='timer'",[])?;
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
    #[test]
    fn background_and_orphan_session_processes_veto_closure() {
        let shell = Process {
            pid: 10,
            parent: 1,
            group: 10,
            session: 10,
            started: "start".into(),
            name: "zsh".into(),
        };
        assert!(process_census(10, std::slice::from_ref(&shell)).is_ok());
        for process in [
            Process {
                pid: 11,
                parent: 10,
                group: 11,
                session: 10,
                started: "start".into(),
                name: "codex".into(),
            },
            Process {
                pid: 12,
                parent: 1,
                group: 12,
                session: 10,
                started: "start".into(),
                name: "agy".into(),
            },
            Process {
                pid: 13,
                parent: 10,
                group: 13,
                session: -1,
                started: "start".into(),
                name: "sh".into(),
            },
        ] {
            assert!(process_census(10, &[shell.clone(), process]).is_err());
        }
        assert!(process_census(99, &[shell]).is_err());
    }
    #[test]
    fn shell_identity_cannot_be_missing_or_change_unnoticed() {
        assert!(shell_snapshot(&json!({"foreground_processes":[]})).is_err());
        assert!(shell_snapshot(&json!({"foreground_processes":[{"pid":2,"name":"zsh"}]})).is_err());
        assert!(
            shell_snapshot(
                &json!({"shell_pid":1,"foreground_processes":[{"pid":2,"name":"codex"}]})
            )
            .is_err()
        );
        assert_ne!(
            shell_snapshot(&json!({"shell_pid":1,"foreground_processes":[{"pid":2,"name":"zsh"}]}))
                .unwrap(),
            shell_snapshot(&json!({"shell_pid":1,"foreground_processes":[{"pid":3,"name":"zsh"}]}))
                .unwrap()
        );
    }
}
