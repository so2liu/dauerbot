//! Scoped manager controls for the current owned native terminal, never GUI keys.
use crate::{App, db, db::Project, herdr, manager, work};
use anyhow::{Result, ensure};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendArgs {
    pub generation: String,
    pub segments: Vec<herdr::terminal_control::Segment>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewArgs {
    pub generation: String,
}

// Leave room in the Manager turn for decisions and reporting, even if the
// terminal keeps changing. This budget survives actor checkpoint recovery.
const MAX_TERMINAL_CALLS: i64 = 10;
const MAX_INPUT_CALLS: i64 = 4;
const MAX_UNCHANGED_POLLS: i64 = 3;

fn stopped(reason: &str, message: &str) -> Value {
    json!({"status":"observation_stopped","reason":reason,"stop_polling":true,"message":message})
}

struct TerminalHistory {
    calls: i64,
    inputs: i64,
    last_screen: Option<String>,
    unchanged_polls: i64,
    stopped: Option<Value>,
    turn: String,
    uncertain_input: bool,
}

impl TerminalHistory {
    fn load(c: &Connection, run: &str, effect: &str, generation: &str) -> Result<Self> {
        let occurrence: String = c.query_row(
            "SELECT occurrence FROM manager_effects WHERE run_id=? AND id=?",
            rusqlite::params![run, effect],
            |row| row.get(0),
        )?;
        let mut history = Self {
            calls: 0,
            inputs: 0,
            last_screen: None,
            unchanged_polls: 0,
            stopped: None,
            turn: model_turn(&occurrence).to_owned(),
            uncertain_input: false,
        };
        let mut statement = c.prepare(
            "SELECT name,status,json_extract(result,'$.status'),json_extract(result,'$.reason'),
                json_extract(result,'$.message'),json_extract(result,'$.screen_fingerprint'),
                json_extract(result,'$.unchanged_polls'),json_extract(result,'$.generation'),
                occurrence,json_extract(result,'$.ignored_attempt')
             FROM manager_effects WHERE run_id=? AND id!=? AND name IN ('cli_view','cli_send_keys')
             ORDER BY rowid DESC LIMIT ?",
        )?;
        let mut rows = statement.query(rusqlite::params![run, effect, MAX_TERMINAL_CALLS])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let status: String = row.get(1)?;
            history.calls += 1;
            if name == "cli_send_keys" {
                history.inputs += 1;
                history.uncertain_input |= matches!(status.as_str(), "submitting" | "uncertain");
            }
            if history.stopped.is_none()
                && row.get::<_, Option<String>>(2)?.as_deref() == Some("observation_stopped")
            {
                let reason: String = row.get(3)?;
                let message: String = row.get(4)?;
                let previous_occurrence: String = row.get(8)?;
                let ignored_attempt = row.get::<_, Option<i64>>(9)?.unwrap_or_default()
                    + i64::from(model_turn(&previous_occurrence) != history.turn);
                let mut denied = stopped(&reason, &message);
                denied["ignored_attempt"] = json!(ignored_attempt);
                denied["ignored_terminal_stop"] = json!(ignored_attempt >= 2);
                history.stopped = Some(denied);
            } else if history.last_screen.is_none()
                && name == "cli_view"
                && row.get::<_, Option<String>>(7)?.as_deref() == Some(generation)
            {
                history.last_screen = row.get(5)?;
                history.unchanged_polls = row.get::<_, Option<i64>>(6)?.unwrap_or(0);
            }
        }
        Ok(history)
    }

    fn stop(&self) -> Option<Value> {
        self.stopped.clone().or_else(|| {
            (self.calls >= MAX_TERMINAL_CALLS).then(|| {
                stopped("terminal_budget_exhausted",
                    "Terminal call budget exhausted for this turn. Stop polling and either report or switch strategy.")
            })
        })
    }

    fn observe(&self, mut screen: Value) -> Value {
        let text = screen["text"].as_str().unwrap_or_default();
        // Timestamps and screen geometry are outside the rendered text.
        // Ignore incidental whitespace while still observing textual changes.
        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let fingerprint = format!("{:x}", Sha256::digest(normalized.as_bytes()));
        let unchanged_polls = if self.last_screen.as_deref() == Some(fingerprint.as_str()) {
            self.unchanged_polls + 1
        } else {
            0
        };
        screen["screen_fingerprint"] = json!(fingerprint);
        screen["unchanged_polls"] = json!(unchanged_polls);
        if unchanged_polls >= MAX_UNCHANGED_POLLS {
            return stopped(
                "terminal_output_unchanged",
                &format!(
                    "Terminal output unchanged for {unchanged_polls} polls. Stop polling and either report or switch strategy."
                ),
            );
        }
        screen
    }
}

fn model_turn(occurrence: &str) -> &str {
    occurrence
        .rsplit_once(":call:")
        .map_or(occurrence, |(turn, _)| turn)
}

async fn history(
    app: &Arc<App>,
    authority: &manager::Context,
    effect: &str,
    generation: &str,
) -> Result<TerminalHistory> {
    let authority = authority.clone();
    let effect = effect.to_owned();
    let generation = generation.to_owned();
    app.db
        .call(move |c| {
            manager::valid(c, &authority)?;
            TerminalHistory::load(c, &authority.run, &effect, &generation)
        })
        .await
}

fn current(c: &Connection, authority: &manager::Context, generation: &str) -> Result<Project> {
    let project = manager::valid(c, authority)?;
    ensure!(
        !generation.is_empty() && project.generation == generation,
        "The native generation changed; inspect current worker status"
    );
    ensure!(
        project.ready && project.terminal.is_some(),
        "The current CLI is not ready"
    );
    ensure!(
        !crate::cli_switch::holds_input(c, &project.id)?,
        "CLI input is held by a transition; wait for supervised recovery"
    );
    ensure!(
        !work::has_takeover(c, &project.id, generation)?,
        "Native input is under explicit user control"
    );
    Ok(project)
}

async fn project(
    app: &Arc<App>,
    authority: &manager::Context,
    generation: &str,
    input: bool,
) -> Result<Project> {
    let authority = authority.clone();
    let generation = generation.to_owned();
    app.db
        .call(move |c| {
            let project = current(c, &authority, &generation)?;
            ensure!(
                !input || !input_held(c, &project.id)?,
                "CLI input is held by a transition; wait for supervised recovery"
            );
            Ok(project)
        })
        .await
}

fn input_held(c: &Connection, project: &str) -> Result<bool> {
    Ok(crate::cli_switch::latest(c, project)?
        .as_ref()
        .is_some_and(crate::cli_switch::blocks_worker))
}

/// The caller releases its broad project lock before invoking this operation.
/// Existing effect receipts prevent replay after a partial or ambiguous submit.
pub(crate) async fn send(
    app: &Arc<App>,
    authority: &manager::Context,
    args: SendArgs,
    effect: &str,
) -> Result<Value> {
    let history = history(app, authority, effect, &args.generation).await?;
    if let Some(denied) = history.stop() {
        return Ok(denied);
    }
    if history.uncertain_input {
        return Ok(stopped(
            "terminal_input_uncertain",
            "Previous terminal input may already have been submitted. Inspect existing evidence and do not replay it.",
        ));
    }
    if history.inputs >= MAX_INPUT_CALLS {
        return Ok(stopped(
            "terminal_input_budget_exhausted",
            "Terminal input budget exhausted for this turn. Use a different strategy.",
        ));
    }
    let project_id = authority.project.clone();
    let blocked = app.db.call(move |c| input_held(c, &project_id)).await?;
    if blocked {
        return Ok(stopped(
            "terminal_input_held",
            "Terminal input is held by a CLI transition. Use worker_status to inspect recovery instead of retrying keys.",
        ));
    }
    let config = app.config.read().await.clone();
    let inspect_config = config.clone();
    send_with(
        app,
        authority,
        args,
        move |project| {
            let config = inspect_config.clone();
            async move { herdr::terminal_control::inspect_input(&config, &project).await }
        },
        move |project, observed, segment| {
            let config = config.clone();
            async move {
                herdr::terminal_control::submit_observed(&config, &project, &observed, &segment)
                    .await
            }
        },
    )
    .await
}

async fn send_with<O, OF, S, SF>(
    app: &Arc<App>,
    authority: &manager::Context,
    args: SendArgs,
    mut inspect: O,
    mut submit: S,
) -> Result<Value>
where
    O: FnMut(Project) -> OF,
    OF: std::future::Future<Output = Result<Value>>,
    S: FnMut(Project, Value, herdr::terminal_control::Segment) -> SF,
    SF: std::future::Future<Output = Result<()>>,
{
    herdr::terminal_control::validate(&args.segments)?;
    let initial = project(app, authority, &args.generation, true).await?;
    let mut confirmed = 0;
    for segment in &args.segments {
        // Slow observation never holds the control lock. Recheck after it, so
        // completed pause/Stop/takeover operations revoke input admission.
        let before = match project(app, authority, &args.generation, true).await {
            Ok(project) => project,
            Err(error) if confirmed == 0 => return Err(error),
            Err(_) => {
                return Ok(uncertain(
                    &initial,
                    confirmed,
                    "Project control changed after part of the sequence",
                ));
            }
        };
        let observed = tokio::time::timeout(Duration::from_secs(3), inspect(before)).await;
        let observed = match observed {
            Ok(Ok(observed)) => observed,
            _ if confirmed == 0 => {
                anyhow::bail!("Current CLI identity could not be verified before input")
            }
            _ => {
                return Ok(uncertain(
                    &initial,
                    confirmed,
                    "CLI observation failed after part of the sequence",
                ));
            }
        };
        let lock = app.project_lock(&initial.id).await;
        let guard = lock.lock().await;
        let fresh = match project(app, authority, &args.generation, true).await {
            Ok(fresh) => fresh,
            Err(error) if confirmed == 0 => return Err(error),
            Err(_) => {
                return Ok(uncertain(
                    &initial,
                    confirmed,
                    "Project control changed after part of the sequence",
                ));
            }
        };
        if fresh.terminal != initial.terminal || fresh.native_session != initial.native_session {
            return Ok(uncertain(&initial, confirmed, "The bound terminal changed"));
        }
        // All public pause/Stop/takeover paths use this same project lock.
        // Serialize only the final RPC (at most 3s), not observation or the
        // complete sequence. Once admitted, a timeout remains uncertain.
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            submit(fresh, observed, segment.clone()),
        )
        .await;
        drop(guard);
        match result {
            Ok(Ok(())) => confirmed += 1,
            _ => {
                return Ok(uncertain(
                    &initial,
                    confirmed,
                    "Terminal submission was not confirmed; inspect the screen before any new input",
                ));
            }
        }
    }
    Ok(
        json!({"status":"submitted","project_id":initial.id,"generation":initial.generation,"segments_confirmed":confirmed,"submitted_at":db::now(),"semantic_task_success":"unconfirmed"}),
    )
}
fn uncertain(project: &Project, confirmed: usize, message: &str) -> Value {
    json!({"status":"uncertain","project_id":project.id,"generation":project.generation,"segments_confirmed":confirmed,"message":message,"do_not_replay":true})
}

pub(crate) async fn view(
    app: &Arc<App>,
    authority: &manager::Context,
    args: ViewArgs,
    effect: &str,
) -> Result<Value> {
    let config = app.config.read().await.clone();
    view_with(app, authority, args, effect, move |project| async move {
        herdr::terminal_control::screen(&config, &project).await
    })
    .await
}

async fn view_with<O, OF>(
    app: &Arc<App>,
    authority: &manager::Context,
    args: ViewArgs,
    effect: &str,
    observe: O,
) -> Result<Value>
where
    O: FnOnce(Project) -> OF,
    OF: std::future::Future<Output = Result<Value>>,
{
    let history = history(app, authority, effect, &args.generation).await?;
    if let Some(denied) = history.stop() {
        return Ok(denied);
    }
    let initial = project(app, authority, &args.generation, false).await?;
    let screen = tokio::time::timeout(Duration::from_secs(3), observe(initial.clone()))
        .await
        .map_err(|_| anyhow::anyhow!("CLI screen inspection timed out"))?
        .map_err(|_| {
            anyhow::anyhow!("Current CLI screen identity or transport could not be verified")
        })?;
    let fresh = project(app, authority, &args.generation, false).await?;
    ensure!(
        fresh.terminal == initial.terminal && fresh.native_session == initial.native_session,
        "The bound terminal changed during inspection"
    );
    Ok(history.observe(screen))
}

#[cfg(test)]
mod tests {
    use super::current;
    use crate::{App, config::Config, db::ProjectInput, manager};

    #[tokio::test]
    async fn repeated_unchanged_terminal_views_stop_before_using_the_turn_budget() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), Config::default()).unwrap();
        let project = app
            .db
            .create(ProjectInput {
                name: "unchanged terminal".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let authority = manager::Context {
            project: project.id.clone(),
            run: "run".into(),
            revision: 0,
            input_revision: 0,
        };
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at) VALUES('run',?,0,0,'{}','working',?,0)", rusqlite::params![project.id,manager::owner()])?;
            c.execute("UPDATE projects SET generation='g',terminal='term',ready=1 WHERE id=?", [&project.id])?;
            Ok(())
        }).unwrap();

        let mut actual_polls = 0;
        for turn in 0..7 {
            let effect = format!("effect-{turn}");
            let occurrence = format!("turn:{turn}:call:0");
            app.db.sync(|c| {
                c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES(?,'run',?,'cli_view','{}','submitting')", rusqlite::params![effect, occurrence])?;
                Ok(())
            }).unwrap();
            let value = super::view_with(
                &app,
                &authority,
                super::ViewArgs {
                    generation: "g".into(),
                },
                &effect,
                |_| {
                    actual_polls += 1;
                    let text = if turn % 2 == 0 {
                        "Stable screen"
                    } else {
                        "Stable  screen  "
                    };
                    async move { Ok(json!({"text": text, "generation": "g", "captured_at": turn})) }
                },
            )
            .await
            .unwrap();
            if turn < 3 {
                assert_eq!(value["unchanged_polls"], turn);
            } else {
                assert_eq!(value["status"], "observation_stopped");
                assert_eq!(value["reason"], "terminal_output_unchanged");
                assert_eq!(value["stop_polling"], true);
                assert_eq!(value["ignored_terminal_stop"] == true, turn >= 5);
            }
            app.db
                .sync(|c| {
                    c.execute(
                        "UPDATE manager_effects SET status='complete',result=? WHERE id=?",
                        rusqlite::params![value.to_string(), effect],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(
            actual_polls, 4,
            "calls after the stop marker never reach the backend"
        );
    }

    #[tokio::test]
    async fn exhausted_transition_returns_structured_input_refusal_before_terminal_io() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), Config::default()).unwrap();
        let project = app
            .db
            .create(ProjectInput {
                name: "exhausted transition".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let authority = manager::Context {
            project: project.id.clone(),
            run: "run".into(),
            revision: 0,
            input_revision: 0,
        };
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at) VALUES('run',?,0,0,'{}','working',?,0)", rusqlite::params![project.id,manager::owner()])?;
            c.execute("UPDATE projects SET generation='g',terminal='term',ready=1 WHERE id=?", [&project.id])?;
            c.execute("INSERT INTO manager_effects(id,run_id,occurrence,name,arguments,status) VALUES('effect','run','turn:1:call:0','cli_send_keys','{}','submitting')", [])?;
            c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input) VALUES('switch',?,'agy','recovery_exhausted','g',0,0)", [&project.id])?;
            Ok(())
        }).unwrap();
        let result = super::send(
            &app,
            &authority,
            super::SendArgs {
                generation: "g".into(),
                segments: vec![crate::herdr::terminal_control::Segment::Key {
                    key: "enter".into(),
                }],
            },
            "effect",
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "observation_stopped");
        assert_eq!(result["reason"], "terminal_input_held");
    }

    #[tokio::test]
    async fn completed_controls_during_observation_prevent_input_submission() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for control in ["pause", "takeover", "stop"] {
            let dir = tempfile::tempdir().unwrap();
            let app = App::new(dir.path().join("store"), Config::default()).unwrap();
            let p = app
                .db
                .create(ProjectInput {
                    name: "blocked observation".into(),
                    path: dir.path().to_string_lossy().into(),
                    agent: "codex".into(),
                    chat_id: String::new(),
                    native_session: String::new(),
                    extra_args: vec![],
                })
                .await
                .unwrap();
            let context = manager::Context {
                project: p.id.clone(),
                run: "run".into(),
                revision: 0,
                input_revision: 0,
            };
            app.db.sync(|c| {
                c.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at) VALUES('run',?,0,0,'{}','working',?,0)", rusqlite::params![p.id,manager::owner()])?;
                c.execute("UPDATE projects SET generation='g',terminal='term',ready=1 WHERE id=?", [&p.id])?;
                Ok(())
            }).unwrap();
            let (entered, waiting) = tokio::sync::oneshot::channel();
            let (resume, blocked) = tokio::sync::oneshot::channel();
            let calls = Arc::new(AtomicUsize::new(0));
            let submitted = calls.clone();
            let worker = app.clone();
            let mut entered = Some(entered);
            let mut blocked = Some(blocked);
            let task = tokio::spawn(async move {
                super::send_with(
                    &worker,
                    &context,
                    super::SendArgs {
                        generation: "g".into(),
                        segments: vec![crate::herdr::terminal_control::Segment::Key {
                            key: "enter".into(),
                        }],
                    },
                    move |_| {
                        let entered = entered.take().unwrap();
                        let blocked = blocked.take().unwrap();
                        async move {
                            entered.send(()).unwrap();
                            blocked.await.unwrap();
                            Ok(serde_json::Value::Null)
                        }
                    },
                    move |_, _, _| {
                        submitted.fetch_add(1, Ordering::SeqCst);
                        async { Ok(()) }
                    },
                )
                .await
            });
            waiting.await.unwrap();
            // Match the public control paths' project-lock serialization while
            // the preflight observer is deterministically blocked.
            let lock = app.project_lock(&p.id).await;
            let guard = tokio::time::timeout(std::time::Duration::from_secs(1), lock.lock())
                .await
                .expect("observation must not pin the control lock");
            app.db
                .sync(|c| match control {
                    "pause" => {
                        c.execute("UPDATE projects SET paused=1 WHERE id=?", [&p.id])?;
                        Ok(())
                    }
                    "takeover" => {
                        crate::work::set_takeover(c, &p.id, "g", true, crate::db::now()).map(|_| ())
                    }
                    "stop" => crate::cli_switch::stopped(c, &p.id, "g"),
                    _ => unreachable!(),
                })
                .unwrap();
            drop(guard);
            resume.send(()).unwrap();
            assert!(
                task.await.unwrap().is_err(),
                "{control} must revoke admission after observation"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "{control} must prevent the final input RPC from starting"
            );
        }
    }

    #[tokio::test]
    async fn native_terminal_authority_rejects_pause_takeover_generation_and_unready() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), Config::default()).unwrap();
        let project = app
            .db
            .create(ProjectInput {
                name: "terminal fixture".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let ctx = manager::Context {
            project: project.id.clone(),
            run: "run".into(),
            revision: 0,
            input_revision: 0,
        };
        app.db.sync(|c| {
            c.execute("INSERT INTO manager_runs(id,project_id,revision,input_revision,checkpoint,status,owner,created_at) VALUES('run',?,0,0,'{}','working',?,0)", rusqlite::params![project.id,manager::owner()])?;
            c.execute("UPDATE projects SET generation='g',terminal='term',ready=1 WHERE id=?", [&project.id])?;
            assert!(current(c,&ctx,"g").is_ok());
            assert!(current(c,&ctx,"other").is_err());
            c.execute("UPDATE projects SET paused=1 WHERE id=?", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            c.execute("UPDATE projects SET paused=0,ready=0 WHERE id=?", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            c.execute("UPDATE projects SET ready=1 WHERE id=?", [&project.id])?;
            crate::work::set_takeover(c,&project.id,"g",true,crate::db::now())?;
            assert!(current(c,&ctx,"g").is_err());
            crate::work::set_takeover(c,&project.id,"g",false,crate::db::now())?;
            c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input) VALUES('switch',?,'codex','recovery_pending','g',0,1)", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            c.execute("UPDATE cli_switch_requests SET holds_input=0 WHERE id='switch'", [])?;
            assert!(current(c,&ctx,"g").is_ok());
            c.execute("UPDATE manager_config SET input_revision=1 WHERE project_id=?", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            Ok(())
        }).unwrap();
    }
}
