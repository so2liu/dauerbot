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
    pub generation: String,
    pub segments: Vec<herdr::terminal_control::Segment>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewArgs {
    pub generation: String,
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
) -> Result<Project> {
    let authority = authority.clone();
    let generation = generation.to_owned();
    app.db
        .call(move |c| current(c, &authority, &generation))
        .await
}

/// The caller releases its broad project lock before invoking this operation.
/// Existing effect receipts prevent replay after a partial or ambiguous submit.
pub(crate) async fn send(
    app: &Arc<App>,
    authority: &manager::Context,
    args: SendArgs,
) -> Result<Value> {
    herdr::terminal_control::validate(&args.segments)?;
    let initial = project(app, authority, &args.generation).await?;
    let config = app.config.read().await.clone();
    let mut confirmed = 0;
    for segment in &args.segments {
        // Do not queue a sequence behind Stop/takeover, or pin the project lock
        // while waiting for the terminal. Revalidate before every separate RPC.
        let fresh = match project(app, authority, &args.generation).await {
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
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            herdr::terminal_control::submit(&config, &fresh, segment),
        )
        .await;
        match result {
            Ok(Ok(())) => confirmed += 1,
            Ok(Err(_)) | Err(_) => {
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
) -> Result<Value> {
    let initial = project(app, authority, &args.generation).await?;
    let config = app.config.read().await.clone();
    let screen = tokio::time::timeout(
        Duration::from_secs(3),
        herdr::terminal_control::screen(&config, &initial),
    )
    .await
    .map_err(|_| anyhow::anyhow!("CLI screen inspection timed out"))?
    .map_err(|_| {
        anyhow::anyhow!("Current CLI screen identity or transport could not be verified")
    })?;
    let fresh = project(app, authority, &args.generation).await?;
    ensure!(
        fresh.terminal == initial.terminal && fresh.native_session == initial.native_session,
        "The bound terminal changed during inspection"
    );
    Ok(screen)
}

#[cfg(test)]
mod tests {
    use super::current;
    use crate::{App, config::Config, db::ProjectInput, manager};

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
            c.execute("INSERT INTO manager_config(project_id,mode) VALUES(?,'manager')", [&project.id])?;
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
