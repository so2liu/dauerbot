//! File contracts for terminal workers. Each delivery attempt has its own files.
use crate::{App, config::Config, db, db::Project, herdr, manager, manager_workers, work};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_TASK: usize = 2 * 1024 * 1024;
const MAX_RESULT: u64 = 32 * 1024;
const RESULT_WAIT_MS: i64 = 30 * 60 * 1_000;
const TERMINAL_TIMEOUT: Duration = Duration::from_secs(3);

fn component(name: &str) -> Result<CString> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "Invalid worker task path component"
    );
    Ok(CString::new(name)?)
}

pub(crate) fn result_path(
    data: &Path,
    project: &str,
    generation: &str,
    run: &str,
) -> Result<PathBuf> {
    for name in [project, generation, run] {
        component(name)?;
    }
    Ok(data
        .join(".dauerbot")
        .join("workers")
        .join(generation)
        .join(run)
        .join("RESULT.md"))
}

fn at(parent: &File, name: &CString, flags: libc::c_int, mode: libc::mode_t) -> io::Result<File> {
    // The directory descriptor anchors the operation even if the directory is renamed.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_int,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // openat returned a fresh owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_task_dir(
    data: &Path,
    project: &str,
    generation: &str,
    run: &str,
    create: bool,
) -> Result<File> {
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(data)?;
    for name in [project, generation, run] {
        component(name)?;
    }
    for name in [".dauerbot", "workers", generation, run] {
        let name = CString::new(name)?;
        if create {
            // Existing directories must still pass the no-follow open below.
            let made = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) };
            if made != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        dir = at(&dir, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    Ok(dir)
}

fn task_file(
    data: &Path,
    worker: &Project,
    run: &str,
    instructions: &str,
) -> Result<(PathBuf, PathBuf)> {
    let result = result_path(data, &worker.id, &worker.generation, run)?;
    let task = result.with_file_name("TASK.md");
    let text = format!(
        "# Worker assignment\n\nWork within the assigned project scope. Return a local result for the Manager. The Manager owns external messages and schedules.\n\nWrite a UTF-8 Markdown file no larger than {MAX_RESULT} bytes to the adjacent RESULT.md. Write a temporary sibling first and rename it into place only after the result is complete. Use this format:\n\nStatus: success | failed | blocked (choose one)\n\n## Summary\nWhat happened\n\n## Changes\nChanged files or None\n\n## Evidence\nLocal proof paths or None\n\n## Assignment\n{instructions}\n"
    );
    ensure!(text.len() <= MAX_TASK, "Worker task exceeds its size limit");
    let dir = open_task_dir(data, &worker.id, &worker.generation, run, true)?;
    let mut file = at(
        &dir,
        &CString::new("TASK.md")?,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok((task, result))
}

fn prompt(task: &Path, result: &Path) -> Result<String> {
    let task = task.to_str().context("Worker task path is not UTF-8")?;
    let result = result.to_str().context("Worker result path is not UTF-8")?;
    ensure!(
        !task.chars().any(char::is_control),
        "Worker path contains control characters"
    );
    Ok(format!(
        "Read {} and carry out the assignment. Save the complete result as {} using the format in TASK.md.",
        json!(task),
        json!(result)
    ))
}

fn visible_blocker(screen: &str) -> bool {
    const KNOWN: [&str; 11] = [
        "login required",
        "please log in",
        "please sign in",
        "sign in to continue",
        "log in to continue",
        "authentication required",
        "choose a login method",
        "quota exceeded",
        "usage limit reached",
        "trust this folder",
        "trust this workspace",
    ];
    screen
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(8)
        .any(|line| {
            KNOWN
                .iter()
                .any(|indicator| line.to_lowercase().contains(indicator))
        })
}

#[derive(Debug)]
struct VisibleBlocker;

impl std::fmt::Display for VisibleBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The terminal shows a native login, trust, or quota prompt")
    }
}

impl std::error::Error for VisibleBlocker {}

pub(crate) async fn dispatch(
    app: &App,
    cfg: &Config,
    worker: &Project,
    run: &work::Run,
) -> Result<bool> {
    let assignment: Result<_> = async {
        let project = worker.id.clone();
        let id = run.event_id.clone();
        let event = app.db.call(move |c| work::event(c, &project, &id)).await?;
        let instructions = manager::body(app, worker, &event).await?;
        task_file(Path::new(&worker.path), worker, &run.id, &instructions)
    }
    .await;
    let preflight: Result<_> = async {
        let (task, result) = assignment?;
        let text = prompt(&task, &result)?;
        let observed = tokio::time::timeout(TERMINAL_TIMEOUT, async {
            let mut record = herdr::observe(cfg, worker).await?;
            manager_workers::check_plain_target(worker, &record)?;
            if matches!(
                record["status"]["state"].as_str(),
                None | Some("" | "unknown")
            ) {
                if visible_blocker(&herdr::screen(cfg, worker, 12).await?) {
                    return Err(VisibleBlocker.into());
                }
                record = herdr::observe(cfg, worker).await?;
                manager_workers::check_plain_target(worker, &record)?;
            }
            ensure!(
                !matches!(
                    record["status"]["state"].as_str(),
                    Some("blocked" | "error" | "working")
                ),
                "The worker is not ready for another assignment"
            );
            Ok::<_, anyhow::Error>(record)
        })
        .await??;
        let pane = herdr::ExecutionRef::from_project(worker)?.pane_id;
        Ok((pane, text, observed["state_change_seq"].as_i64()))
    }
    .await;
    let (status, reason, seq) = match preflight {
        Ok((pane, text, seq)) => {
            // Submit a single literal text-and-Enter operation. The caller owns
            // the project lock; no polling or undisclosed input retries occur.
            match tokio::time::timeout(
                TERMINAL_TIMEOUT,
                herdr::rpc(
                    cfg,
                    "pane.send_input",
                    json!({"pane_id":pane,"text":text,"keys":["Enter"]}),
                ),
            )
            .await
            {
                Ok(Ok(_)) => ("submitted", "", seq),
                _ => (
                    "uncertain",
                    "Terminal task input may have been submitted; inspect this attempt before retrying",
                    seq,
                ),
            }
        }
        Err(error) if error.is::<VisibleBlocker>() => (
            "failed",
            "The terminal shows a native login, trust, or quota prompt",
            None,
        ),
        Err(_) => (
            "failed",
            "Could not prepare the task file or verify terminal readiness",
            None,
        ),
    };
    let id = run.id.clone();
    app.db
        .call(move |c| {
            if status == "submitted" {
                work::submission_sequence(c, &id, seq)?;
            }
            work::delivery(c, &id, status, reason, db::now())
        })
        .await?;
    Ok(status == "submitted")
}

fn parse_result(text: &str) -> Option<Value> {
    let (header, rest) = text.split_once("## Summary\n")?;
    let status = header
        .lines()
        .find_map(|line| line.strip_prefix("Status: "))?
        .trim();
    if !matches!(status, "success" | "failed" | "blocked") {
        return None;
    }
    let (summary, tail) = rest.split_once("## Changes\n")?;
    let (changes, evidence) = tail.split_once("## Evidence\n")?;
    let (summary, changes, evidence) = (summary.trim(), changes.trim(), evidence.trim());
    if [summary, changes, evidence].contains(&"") {
        return None;
    }
    Some(
        json!({"status":"reported","reported_status":status,"summary":summary,
        "changes":changes,"evidence":evidence}),
    )
}

fn read_result(data: &Path, worker: &Project, run: &str) -> Option<Value> {
    let dir = open_task_dir(data, &worker.id, &worker.generation, run, false).ok()?;
    let file = at(
        &dir,
        &CString::new("RESULT.md").ok()?,
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    )
    .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_RESULT {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_RESULT + 1).read_to_string(&mut text).ok()?;
    if text.len() as u64 > MAX_RESULT {
        return None;
    }
    parse_result(&text.replace("\r\n", "\n"))
}

#[derive(Debug)]
struct Candidate {
    run: String,
    event: String,
    status: String,
    updated_at: i64,
    seq: Option<i64>,
    missing_report: bool,
    row: i64,
}

fn pending_results(c: &Connection, worker: &Project) -> Result<Vec<Candidate>> {
    use rusqlite::OptionalExtension;
    let cursor: i64 = c
        .query_row(
            "SELECT COALESCE(json_extract(execution,'$.result_cursor'),0)
         FROM manager_workers WHERE project_id=? AND generation=?",
            params![worker.id, worker.generation],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let mut statement = c.prepare(
        "SELECT r.id,r.event_id,r.status,r.updated_at,r.native_seq,
                COALESCE(json_extract(e.payload,'$.worker_result.run_id')=r.id AND
                json_extract(e.payload,'$.worker_result.status')='result_missing',0), r.rowid FROM work_runs r
         JOIN work_events e ON e.id=r.event_id
         WHERE r.project_id=? AND r.generation=? AND r.rowid>? AND e.source='manager_worker'
           AND r.status IN ('submitted','running','settled','failed','uncertain')
           AND (json_extract(e.payload,'$.worker_result.run_id') IS NOT r.id
                OR json_extract(e.payload,'$.worker_result.status')='result_missing')
           AND NOT EXISTS(SELECT 1 FROM work_runs newer WHERE newer.event_id=e.id AND newer.rowid>r.rowid)
         ORDER BY r.rowid LIMIT 12",
    )?;
    let mut page = |after| {
        statement
            .query_map(params![worker.id, worker.generation, after], |r| {
                Ok(Candidate {
                    run: r.get(0)?,
                    event: r.get(1)?,
                    status: r.get(2)?,
                    updated_at: r.get(3)?,
                    seq: r.get(4)?,
                    missing_report: r.get(5)?,
                    row: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
    };
    let mut candidates = page(cursor)?;
    if candidates.is_empty() && cursor != 0 {
        candidates = page(0)?;
    }
    let next = candidates.last().map_or(0, |candidate| candidate.row);
    if next != cursor {
        c.execute(
            "UPDATE manager_workers SET execution=json_set(execution,'$.result_cursor',?)
            WHERE project_id=? AND generation=?",
            params![next, worker.id, worker.generation],
        )?;
    }
    Ok(candidates)
}

fn can_release(observed: Option<&Value>, seq: Option<i64>, reported: bool) -> bool {
    let Some(record) = observed else {
        return false;
    };
    match record["status"]["state"].as_str() {
        Some("idle" | "done") => match seq {
            Some(old) => record["state_change_seq"]
                .as_i64()
                .is_some_and(|new| old < new),
            None => reported,
        },
        // A plain CLI without integration hooks can complete through its file
        // contract. A known working or blocked process never gains this route.
        None | Some("unknown") => seq.is_none() && reported,
        _ => false,
    }
}

pub(crate) async fn collect(
    app: &App,
    cfg: &Config,
    worker: &Project,
    observed: Option<&Value>,
) -> Result<()> {
    let observed =
        observed.filter(|record| manager_workers::check_plain_target(worker, record).is_ok());
    let worker_copy = worker.clone();
    let candidates = app
        .db
        .call(move |c| pending_results(c, &worker_copy))
        .await?;
    let mut screen = None;
    let mut inspected = false;
    for candidate in candidates {
        let now = db::now();
        let report = read_result(Path::new(&worker.path), worker, &candidate.run);
        if candidate.missing_report && report.is_none() {
            continue;
        }
        let released = now - candidate.updated_at >= 2_000
            && can_release(observed, candidate.seq, report.is_some());
        let awaiting = matches!(candidate.status.as_str(), "submitted" | "running");
        let timed_out = awaiting && !released && now - candidate.updated_at >= RESULT_WAIT_MS;
        if awaiting && !released && !timed_out {
            continue;
        }
        if report.is_none() && candidate.status == "settled" && now - candidate.updated_at < 2_000 {
            continue;
        }
        if !inspected && observed.is_some() {
            inspected = true;
            let capture = async {
                let raw = herdr::screen(cfg, worker, 16).await?;
                manager_workers::check_plain_target(worker, &herdr::observe(cfg, worker).await?)?;
                Ok::<_, anyhow::Error>(raw)
            };
            screen = tokio::time::timeout(TERMINAL_TIMEOUT, capture)
                .await
                .ok()
                .and_then(Result::ok)
                .map(|raw| {
                    herdr::terminal_control::present_screen(&raw)
                        .0
                        .chars()
                        .take(1500)
                        .collect::<String>()
                });
        }
        let mut result = report.unwrap_or_else(|| json!({
            "status":"result_missing","reason":"No bounded, complete RESULT.md was available for this attempt"
        }));
        result["run_id"] = json!(candidate.run);
        result["path"] = json!(result_path(
            Path::new(&worker.path),
            &worker.id,
            &worker.generation,
            &candidate.run
        )?);
        result["terminal_excerpt"] = json!(screen);
        result["semantic_task_success"] = json!("unconfirmed");
        let project = worker.id.clone();
        app.db.call(move |c| {
            let tx = c.unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE work_events SET payload=json_set(payload,'$.worker_result',json(?))
                 WHERE project_id=? AND id=? AND (json_extract(payload,'$.worker_result.run_id') IS NOT ?
                     OR json_extract(payload,'$.worker_result.status')='result_missing')
                   AND (SELECT id FROM work_runs WHERE event_id=work_events.id ORDER BY rowid DESC LIMIT 1)=?",
                params![result.to_string(),project,candidate.event,candidate.run,candidate.run],
            )?;
            if changed == 1 && timed_out {
                work::delivery(&tx,&candidate.run,"uncertain",
                    "Timed out waiting for reliable terminal or file completion evidence; verify this attempt before retrying",now)?;
            } else if changed == 1 && released && awaiting {
                tx.execute("UPDATE work_runs SET status='settled',finished_at=?,updated_at=?
                    WHERE id=? AND status IN ('submitted','running')", params![now,now,candidate.run])?;
                tx.execute("UPDATE work_events SET status='delivered',updated_at=? WHERE id=? AND status='submitted'",
                    params![now,candidate.event])?;
            }
            if changed == 1 {
                manager::worker_changed(&tx, &project)?;
            }
            tx.commit()?;
            Ok(())
        }).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicI64, Ordering},
        },
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    #[tokio::test]
    async fn non_mcp_cli_receives_task_and_returns_result_through_owned_terminal() {
        let fixture = tempfile::tempdir_in("/tmp").unwrap();
        let workspace = fixture.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let cfg = Config {
            herdr_session: fixture.path().to_string_lossy().into_owned(),
            ..Config::default()
        };
        let app = App::new(fixture.path().join("store"), cfg.clone()).unwrap();
        let mut worker = app
            .db
            .create(db::ProjectInput {
                name: "Plain worker".into(),
                agent: "pi".into(),
                chat_id: String::new(),
                path: workspace.to_string_lossy().into_owned(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        worker.generation = "fixture-worker".into();
        worker.mcp_token.clear();
        worker.terminal = Some("fixture-terminal".into());
        worker.terminal_pid = Some(8420);
        worker.ready = true;
        worker.execution = json!({"backend":"legacy","workspace_id":"w1","pane_id":"w1:p2",
            "terminal_id":"fixture-terminal","managed_name":"fixture-managed","generation":worker.generation,
            "shell_pid":8420,"launch_permit":fixture.path().join("permit"),"worker_program":"pi"});
        let run = app.db.sync(|c| {
            c.execute("INSERT INTO work_tasks(id,project_id,title,instructions,status,created_at,updated_at)
                VALUES('task',?,'fixture','fixture','running',0,0)", [&worker.id])?;
            c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at)
                VALUES('event',?,'task','manager_worker','fixture',?,'submitting',0,0)", params![worker.id,
                    json!({"target_generation":worker.generation,"expected_agent":"pi","instructions":"Write the fixture result"}).to_string()])?;
            c.execute("INSERT INTO work_runs(id,project_id,task_id,event_id,generation,status,started_at,updated_at)
                VALUES('run',?,'task','event',?,'submitting',0,0)", params![worker.id,worker.generation])?;
            work::run(c,&worker.id,"run")
        }).unwrap();
        let result = result_path(
            Path::new(&worker.path),
            &worker.id,
            &worker.generation,
            &run.id,
        )
        .unwrap();
        let task = result.with_file_name("TASK.md");
        let mock = fixture.path().join("pi-mock");
        fs::write(&mock, r#"#!/bin/sh
test "$#" -eq 2 || exit 21
grep -q 'Write the fixture result' "$1" || exit 22
printf 'Status: success\n\n## Summary\nBuilt the fixture\n\n## Changes\nNone\n\n## Evidence\nNone\n' > "$2.new"
mv "$2.new" "$2"
"#).unwrap();
        let seq = Arc::new(AtomicI64::new(5));
        let server_seq = seq.clone();
        let tokens = json!({"hfb_owner":"dauerbot","hfb_project":worker.id,"hfb_generation":worker.generation,"hfb_execution":"fixture-managed"});
        let listener = UnixListener::bind(herdr::socket_path(&cfg).unwrap()).unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut io = BufReader::new(stream);
                let mut text = String::new();
                if io.read_line(&mut text).await.unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&text).unwrap();
                let args = &request["params"];
                let response = match request["method"].as_str().unwrap() {
                    "pane.get" => {
                        json!({"pane":{"pane_id":"w1:p2","terminal_id":"fixture-terminal","workspace_id":"w1","tokens":tokens}})
                    }
                    "agent.get" => {
                        json!({"agent":{"agent":"pi","agent_status":"idle","state_change_seq":server_seq.load(Ordering::SeqCst)}})
                    }
                    "pane.process_info" => {
                        json!({"process_info":{"shell_pid":8420,"foreground_processes":[{"name":"pi","argv":["pi"]}]}})
                    }
                    "pane.read" => json!({"read":{"text":"Fixture result written"}}),
                    "pane.send_input" => {
                        assert_eq!(args["pane_id"], "w1:p2");
                        assert_eq!(args["keys"], json!(["Enter"]));
                        assert_eq!(args["text"], prompt(&task, &result).unwrap());
                        assert!(
                            std::process::Command::new("/bin/sh")
                                .args([mock.as_os_str(), task.as_os_str(), result.as_os_str()])
                                .env_clear()
                                .env("PATH", "/usr/bin:/bin")
                                .status()
                                .unwrap()
                                .success()
                        );
                        json!({})
                    }
                    other => panic!("Unexpected fixture RPC {other}"),
                };
                io.get_mut()
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":response})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        assert!(dispatch(&app, &cfg, &worker, &run).await.unwrap());
        app.db
            .sync(|c| {
                c.execute(
                    "UPDATE work_runs SET updated_at=? WHERE id='run'",
                    [db::now() - 3000],
                )?;
                Ok(())
            })
            .unwrap();
        let observed = herdr::observe(&cfg, &worker).await.unwrap();
        collect(&app, &cfg, &worker, Some(&observed)).await.unwrap();
        assert_eq!(
            app.db
                .sync(|c| work::run(c, &worker.id, "run"))
                .unwrap()
                .status,
            "submitted"
        );
        let result = result_path(
            Path::new(&worker.path),
            &worker.id,
            &worker.generation,
            "run",
        )
        .unwrap();
        let late = result.with_file_name("result-fixture-delayed.md");
        std::fs::rename(&result, &late).unwrap();
        seq.store(6, Ordering::SeqCst);
        let observed = herdr::observe(&cfg, &worker).await.unwrap();
        collect(&app, &cfg, &worker, Some(&observed)).await.unwrap();
        let event = app
            .db
            .sync(|c| work::event(c, &worker.id, "event"))
            .unwrap();
        assert_eq!(event.payload["worker_result"]["status"], "result_missing");
        std::fs::rename(&late, &result).unwrap();
        collect(&app, &cfg, &worker, Some(&observed)).await.unwrap();
        let event = app
            .db
            .sync(|c| work::event(c, &worker.id, "event"))
            .unwrap();
        assert_eq!(event.status, "delivered");
        let reported = &event.payload["worker_result"];
        assert_eq!(reported["reported_status"], "success");
        assert_eq!(reported["semantic_task_success"], "unconfirmed");
        assert_eq!(reported["terminal_excerpt"], "Fixture result written");
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,created_at,execution)
                VALUES(?,?,'pi','','unavailable',0,'{}')", params![worker.generation,worker.id])?;
            for index in 0..13 {
                let event = format!("late-event-{index}");
                let run = format!("late-run-{index}");
                c.execute("INSERT INTO work_events(id,project_id,task_id,source,source_id,payload,status,created_at,updated_at)
                    SELECT ?,project_id,task_id,'manager_worker',?,?,'uncertain',0,0 FROM work_events WHERE id='event'",
                    params![event,event,json!({"worker_result":{"run_id":run,"status":"result_missing"}}).to_string()])?;
                c.execute("INSERT INTO work_runs(id,project_id,task_id,event_id,generation,status,started_at,updated_at)
                    SELECT ?,project_id,task_id,id,?,'uncertain',0,0 FROM work_events WHERE id=?",
                    params![run,worker.generation,event])?;
            }
            assert_eq!(pending_results(c,&worker)?.len(),12);
            assert_eq!(pending_results(c,&worker)?[0].run,"late-run-12");
            assert_eq!(pending_results(c,&worker)?[0].run,"late-run-0");
            Ok(())
        }).unwrap();
        let oldest = result_path(
            Path::new(&worker.path),
            &worker.id,
            &worker.generation,
            "late-run-0",
        )
        .unwrap();
        fs::create_dir_all(oldest.parent().unwrap()).unwrap();
        fs::copy(&result, &oldest).unwrap();
        let parent = app.db.project(&worker.id).await.unwrap();
        manager_workers::reconcile(&app, &parent).await.unwrap();
        manager_workers::reconcile(&app, &parent).await.unwrap();
        app.db
            .sync(|c| {
                let oldest = work::event(c, &worker.id, "late-event-0")?;
                assert_eq!(oldest.status, "uncertain");
                assert_eq!(oldest.payload["worker_result"]["status"], "reported");
                assert!(manager_workers::authorize(c, &parent, &worker.generation, true).is_err());
                Ok(())
            })
            .unwrap();
        server.abort();
    }

    #[test]
    fn task_directories_reject_symlinks_and_result_release_requires_fresh_sequence() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join(".dauerbot")).unwrap();
        assert!(open_task_dir(dir.path(), "project", "worker", "attempt", true).is_err());
        assert!(parse_result("Status: success\n").is_none());
        assert!(!can_release(
            Some(&json!({"status":{"state":"idle"}})),
            None,
            false
        ));
        assert!(!can_release(
            Some(&json!({"status":{"state":"idle"},"state_change_seq":4})),
            Some(4),
            true
        ));
        assert!(!can_release(
            Some(&json!({"status":{"state":"working"}})),
            None,
            true
        ));
        assert!(can_release(
            Some(&json!({"status":{"state":"unknown"}})),
            None,
            true
        ));
    }
}
