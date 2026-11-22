//! Scoped manager controls for the current owned native terminal, never GUI keys.
use crate::{App, db, db::Project, herdr, manager, work};
use anyhow::{Result, ensure};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendArgs {
    #[serde(default)]
    pub generation: String,
    pub segments: Vec<herdr::terminal_control::Segment>,
    #[serde(skip)]
    pub task_run: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewArgs {
    #[serde(default)]
    pub generation: String,
}

fn stopped(reason: &str, message: &str) -> Value {
    json!({"status":"observation_stopped","reason":reason,"stop_polling":true,"message":message})
}

async fn has_uncertain_input(
    app: &Arc<App>,
    authority: &manager::Context,
    effect: &str,
    generation: &str,
) -> Result<bool> {
    let authority = authority.clone();
    let effect = effect.to_owned();
    let generation = generation.to_owned();
    app.db
        .call(move |c| {
            manager::valid(c, &authority)?;
            Ok(c.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM manager_effects
                    WHERE run_id=? AND id!=? AND name IN ('cli_send_keys','steer_worker')
                      AND status IN ('submitting','uncertain')
                      AND COALESCE(NULLIF(json_extract(result,'$.generation'),''),
                                   NULLIF(json_extract(arguments,'$.generation'),''),?)=?
                )",
                rusqlite::params![authority.run, effect, generation, generation],
                |row| row.get(0),
            )?)
        })
        .await
}

fn current(c: &Connection, authority: &manager::Context, generation: &str) -> Result<Project> {
    let project = manager::valid(c, authority)?;
    let project = if generation.is_empty() && project.agent == "auto" {
        crate::manager_workers::ready_worker(c, &project)?
    } else if generation.is_empty() || project.generation == generation {
        project
    } else {
        crate::manager_workers::authorize(c, &project, generation, true)?.project
    };
    ensure!(
        project.ready && project.terminal.is_some() && !project.generation.is_empty(),
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
                !input || !input_held(c, &project.id, &project.generation)?,
                "CLI input is held by a transition; wait for supervised recovery"
            );
            Ok(project)
        })
        .await
}

fn input_held(c: &Connection, project: &str, generation: &str) -> Result<bool> {
    let primary = db::project_by_key(c, project)?.generation == generation;
    Ok(crate::cli_switch::latest(c, project)?
        .as_ref()
        .is_some_and(|request| {
            request.holds_input || (primary && crate::cli_switch::blocks_worker(request))
        }))
}

fn with_generation(mut value: Value, generation: &str) -> Value {
    value["generation"] = json!(generation);
    value
}

/// The caller releases its broad project lock before invoking this operation.
/// Existing effect receipts prevent replay after a partial or ambiguous submit.
pub(crate) async fn send(
    app: &Arc<App>,
    authority: &manager::Context,
    mut args: SendArgs,
    effect: &str,
) -> Result<Value> {
    if args.generation.is_empty() {
        args.generation = project(app, authority, "", false).await?.generation;
    }
    let denial = |reason, message| with_generation(stopped(reason, message), &args.generation);
    if has_uncertain_input(app, authority, effect, &args.generation).await? {
        return Ok(denial(
            "terminal_input_uncertain",
            "Previous terminal input may already have been submitted. Inspect existing evidence and do not replay it.",
        ));
    }
    let project_id = authority.project.clone();
    let target = args.generation.clone();
    let blocked = app
        .db
        .call(move |c| input_held(c, &project_id, &target))
        .await?;
    if blocked {
        return Ok(denial(
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
        if let Some(run) = &args.task_run {
            let project_id = fresh.id.clone();
            let generation = fresh.generation.clone();
            let run = run.clone();
            let active = app
                .db
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM work_runs WHERE project_id=?
                    AND generation=? AND id=? AND status IN ('submitted','running'))",
                        rusqlite::params![project_id, generation, run],
                        |row| row.get::<_, bool>(0),
                    )?)
                })
                .await?;
            if !active {
                if confirmed == 0 {
                    anyhow::bail!(
                        "The selected worker task finished before steering was submitted"
                    );
                }
                return Ok(uncertain(
                    &initial,
                    confirmed,
                    "The selected worker task changed after part of the correction",
                ));
            }
        }
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
    mut args: ViewArgs,
) -> Result<Value> {
    let config = app.config.read().await.clone();
    if args.generation.is_empty() {
        args.generation = project(app, authority, "", false).await?.generation;
    }
    let initial = project(app, authority, &args.generation, false).await?;
    let screen = tokio::time::timeout(
        Duration::from_secs(3),
        herdr::terminal_control::screen(&config, &initial),
    )
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
    Ok(with_generation(screen, &args.generation))
}

#[cfg(test)]
mod tests {
    use super::current;
    use crate::{
        App,
        config::Config,
        db::{self, ProjectInput},
        manager, work,
    };

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
                task_run: None,
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
        for control in ["pause", "takeover", "stop", "settle"] {
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
            let scoped_run = if control == "settle" {
                Some(
                    app.db
                        .sync(|c| {
                            manager::test_delegate(c, &p.id, "scoped", None)?;
                            let run = work::claim_next(c, &p.id, "g", db::now())?.unwrap();
                            work::delivery(c, &run.id, "submitted", "", db::now())?;
                            Ok(run.id)
                        })
                        .unwrap(),
                )
            } else {
                None
            };
            let task_run = scoped_run.clone();
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
                        task_run,
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
                    "settle" => {
                        c.execute(
                            "UPDATE work_runs SET status='settled' WHERE id=?",
                            [scoped_run.as_deref().unwrap()],
                        )?;
                        Ok(())
                    }
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
            c.execute("INSERT INTO manager_workers(generation,project_id,agent,state,created_at,native_session,execution,token)
                VALUES('task',?,'pi','ready',0,'native-task',json_object('terminal_id','task-pane'),'')", [&project.id])?;
            let worker = current(c,&ctx,"task")?;
            assert_eq!(worker.agent, "pi");
            assert_eq!(worker.native_session, "native-task");
            assert_eq!(current(c,&ctx,"")?.generation, "g");

            c.execute("UPDATE projects SET paused=1 WHERE id=?", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            c.execute("UPDATE projects SET paused=0,ready=0 WHERE id=?", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            c.execute("UPDATE projects SET ready=1 WHERE id=?", [&project.id])?;
            crate::work::set_takeover(c,&project.id,"g",true,crate::db::now())?;
            assert!(current(c,&ctx,"g").is_err());
            assert!(current(c,&ctx,"task").is_err());
            crate::work::set_takeover(c,&project.id,"g",false,crate::db::now())?;
            c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,holds_input) VALUES('switch',?,'codex','recovery_pending','g',0,1)", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            assert!(current(c,&ctx,"task").is_err());
            c.execute("UPDATE cli_switch_requests SET holds_input=0 WHERE id='switch'", [])?;
            assert!(current(c,&ctx,"g").is_ok());
            c.execute("UPDATE manager_config SET input_revision=1 WHERE project_id=?", [&project.id])?;
            assert!(current(c,&ctx,"g").is_err());
            Ok(())
        }).unwrap();
    }
}
