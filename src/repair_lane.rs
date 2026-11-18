//! A forked native conversation may repair its cause, never own ordinary input.
use crate::{cli_switch, db, db::Project, herdr, work};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const OUTBOX_KEY: &str = "daemon_repair_authority";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lane {
    pub request_id: String,
    pub generation: String,
    pub expected_native: String,
    pub observed_native: String,
    pub mcp_registered: bool,
}
fn same_binding(a: &Lane, b: &Lane) -> bool {
    a.request_id == b.request_id
        && a.generation == b.generation
        && a.expected_native == b.expected_native
        && a.observed_native == b.observed_native
}
pub(crate) fn descriptor(p: &Project) -> Option<Lane> {
    serde_json::from_value(p.execution["repair_lane"].clone()).ok()
}
pub(crate) fn view(p: &Project, lane: &Lane) -> Project {
    let mut view = p.clone();
    view.native_session = lane.observed_native.clone();
    view.execution["intended_native_session"] = json!(lane.observed_native);
    view
}
/// Child snapshots belong to the observed repair conversation B, not saved A.
pub(crate) fn child_view(p: &Project) -> Project {
    match descriptor(p).filter(|lane| lane.generation == p.generation) {
        Some(lane) => view(p, &lane),
        None => p.clone(),
    }
}
pub(crate) fn check_identity(p: &Project, record: &Value) -> Result<()> {
    if let Some(lane) = descriptor(p) {
        ensure!(
            lane.generation == p.generation,
            "The repair generation changed"
        );
        herdr::check_identity(&view(p, &lane), record)
    } else {
        herdr::check_identity(p, record)
    }
}
pub(crate) fn foreground_quiescent(p: &Project, record: &Value) -> bool {
    if let Some(lane) = descriptor(p) {
        lane.generation == p.generation && herdr::foreground_quiescent(&view(p, &lane), record)
    } else {
        herdr::foreground_quiescent(p, record)
    }
}
pub(crate) fn effect_phase(c: &Connection, lane: &Lane) -> Result<()> {
    let phase: String = c.query_row(
        "SELECT phase FROM cli_switch_requests WHERE id=?",
        [&lane.request_id],
        |r| r.get(0),
    )?;
    ensure!(
        matches!(
            phase.as_str(),
            "recovery_pending" | "recovery_starting" | "repairing" | "requested"
        ),
        "Repair-only execution is retiring; no new effects may start"
    );
    Ok(())
}
/// Recheck the authoritative role in the same transaction as each effect.
pub(crate) fn current(c: &Connection, project: &str, registered: bool) -> Result<Option<Lane>> {
    let (generation,native,agent,terminal,pid,paused,ready,raw):(String,String,String,Option<String>,Option<i64>,bool,bool,String)=c.query_row(
        "SELECT generation,native_session,agent,terminal,terminal_pid,paused,ready,execution FROM projects WHERE id=?",[project],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)),
    )?;
    let execution: Value = serde_json::from_str(&raw)?;
    let Some(raw_lane) = execution.get("repair_lane") else {
        return Ok(None);
    };
    let lane: Lane =
        serde_json::from_value(raw_lane.clone()).context("Invalid repair-only authority")?;
    let request = cli_switch::latest(c, project)?.context("The repair request no longer exists")?;
    let source: String = c.query_row(
        "SELECT source FROM cli_switch_requests WHERE id=?",
        [&request.id],
        |r| r.get(0),
    )?;
    let source: Value = serde_json::from_str(&source)?;
    ensure!(
        !paused
            && !ready
            && terminal.is_some()
            && !generation.is_empty()
            && request.holds_input
            && request.id == lane.request_id
            && request.recovery_generation == generation
            && lane.generation == generation
            && execution["cli_recovery_request"] == request.id
            && execution["generation"] == generation
            && execution["terminal_id"].as_str() == terminal.as_deref()
            && execution["shell_pid"].as_i64() == pid
            && source["agent"] == agent
            && herdr::same_native_session(&agent, &lane.expected_native, &native)
            && execution["intended_native_session"]
                .as_str()
                .is_some_and(|v| herdr::same_native_session(&agent, v, &native))
            && source["native_session"]
                .as_str()
                .is_some_and(|v| herdr::same_native_session(&agent, v, &native))
            && !herdr::same_native_session(&agent, &lane.expected_native, &lane.observed_native)
            && !work::has_takeover(c, project, &generation)?
            && (!registered || lane.mcp_registered),
        "Repair-only authority changed, paused, or retired; reconnect before using its tools"
    );
    if registered {
        effect_phase(c, &lane)?;
    }
    Ok(Some(lane))
}
pub(crate) fn authorize(c: &Connection, p: &Project, registered: bool) -> Result<Option<Lane>> {
    let current = current(c, &p.id, registered)?;
    ensure!(
        current.is_some() == descriptor(p).is_some(),
        "The repair-only role was retired or replaced"
    );
    if let Some(lane) = current.as_ref() {
        ensure!(
            lane.generation == p.generation
                && descriptor(p)
                    .as_ref()
                    .is_some_and(|snapshot| same_binding(snapshot, lane)),
            "The repair-only execution changed"
        );
    }
    Ok(current)
}
/// Admit only an observation-derived owned fork; no user body supplies identity.
pub(crate) fn admit(c: &Connection, p: &Project, record: &Value) -> Result<Option<Lane>> {
    herdr::check_execution_identity(p, record)?;
    let intended = p.execution["intended_native_session"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("No intended native resume identity")?;
    let observed = record["native_session"]["value"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("The repair native identity is not observed")?;
    ensure!(
        herdr::same_native_session(&p.agent, intended, &p.native_session)
            && !herdr::same_native_session(&p.agent, intended, observed),
        "This is not an owned native fork"
    );
    let mut candidate = Lane {
        request_id: String::new(),
        generation: p.generation.clone(),
        expected_native: p.native_session.clone(),
        observed_native: observed.into(),
        mcp_registered: false,
    };
    herdr::check_identity(&view(p, &candidate), record)?;
    let tx = c.unchecked_transaction()?;
    let (generation, terminal, paused, raw): (String, Option<String>, bool, String) = tx
        .query_row(
            "SELECT generation,terminal,paused,execution FROM projects WHERE id=?",
            [&p.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let stored: Value = serde_json::from_str(&raw)?;
    ensure!(
        generation == p.generation
            && terminal == p.terminal
            && !paused
            && stored == p.execution
            && !work::has_takeover(&tx, &p.id, &generation)?,
        "The native execution changed before repair admission"
    );
    if stored.get("repair_lane").is_some() {
        let lane = current(&tx, &p.id, false)?.context("Repair lane vanished")?;
        tx.commit()?;
        return Ok(Some(lane));
    }
    let request = cli_switch::ensure_repair_request(&tx, &p.id)?;
    let source: String = tx.query_row(
        "SELECT source FROM cli_switch_requests WHERE id=?",
        [&request.id],
        |r| r.get(0),
    )?;
    let source: Value = serde_json::from_str(&source)?;
    if !request.holds_input
        || cli_switch::retiring_candidate(&request, &p.execution, &p.generation)
        || source["agent"] != p.agent
        || !source["native_session"]
            .as_str()
            .is_some_and(|v| herdr::same_native_session(&p.agent, v, &p.native_session))
    {
        // Restore the trusted source launch settings before electing its repair
        // Agent. A selected or superseded target may carry the broken model or
        // provider even when its CLI and saved native identity match the source.
        return Ok(None);
    }
    candidate.request_id = request.id.clone();
    let history = json!({"generation":p.generation,"expected_native":candidate.expected_native,"observed_native":candidate.observed_native,"created_at":db::now()});
    tx.execute("UPDATE cli_switch_requests SET phase='recovery_pending',phase_started_at=?,next_attempt_at=0,recovery_generation=?,source=json_insert(source,'$.repair_lanes',json(COALESCE(json_extract(source,'$.repair_lanes'),'[]'))) WHERE id=? AND holds_input=1",params![db::now(),p.generation,request.id])?;
    tx.execute("UPDATE cli_switch_requests SET source=json_insert(source,'$.repair_lanes[#]',json(?)) WHERE id=?",params![history.to_string(),request.id])?;
    ensure!(tx.execute("UPDATE projects SET ready=0,execution=json_remove(json_set(execution,'$.repair_lane',json(?),'$.cli_recovery_request',?),'$.native_confirmed','$.mcp_registered') WHERE id=? AND generation=? AND terminal=?",params![serde_json::to_string(&candidate)?,request.id,p.id,p.generation,p.terminal])?==1,"Repair execution changed before admission");
    tx.commit()?;
    Ok(Some(candidate))
}
pub(crate) fn validate_outbox(c: &Connection, project: &str, attachment: &Value) -> Result<()> {
    if let Some(binding) = attachment.get(OUTBOX_KEY) {
        let expected: Lane = serde_json::from_value(binding.clone())?;
        ensure!(
            current(c, project, true)?.as_ref() == Some(&expected),
            "This repair reply belongs to a retired or changed repair-only authority"
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{App, config::Config, db::ProjectInput};
    use std::sync::Arc;

    pub(crate) async fn fixture(agent: &str) -> (tempfile::TempDir, Arc<App>, Project, Value) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(
            dir.path().join("store"),
            Config {
                herdr_session: dir.path().to_string_lossy().into(),
                ..Config::default()
            },
        )
        .unwrap();
        let (original, fork) = if agent == "pi" {
            let original = dir.path().join("original.jsonl");
            let fork = dir.path().join("fork.jsonl");
            std::fs::write(&original, "original native").unwrap();
            std::fs::write(&fork, "repair native").unwrap();
            (
                original.to_string_lossy().into_owned(),
                fork.to_string_lossy().into_owned(),
            )
        } else {
            ("original-native".into(), "repair-native".into())
        };
        let p = app
            .db
            .create(ProjectInput {
                name: "Fork repair".into(),
                path: dir.path().to_string_lossy().into(),
                agent: agent.into(),
                chat_id: String::new(),
                native_session: original,
                extra_args: vec!["--model".into(), "original-model".into()],
            })
            .await
            .unwrap();
        let execution = json!({"api_protocol":7,"mcp_contract":1,"backend":"legacy","workspace_id":"w1","pane_id":"w1:p1","terminal_id":"owned-terminal","managed_name":"owned-managed","generation":"owned-generation","shell_pid":null,"launch_permit":dir.path().join("permit"),"intended_native_session":p.native_session,"native_confirmed":false});
        app.db.sync(|c|{c.execute("UPDATE projects SET terminal='owned-terminal',terminal_pid=NULL,generation='owned-generation',ready=0,execution=? WHERE id=?",params![execution.to_string(),p.id])?;work::begin_execution(c,&p.id,"owned-generation",db::now())?;Ok(())}).unwrap();
        let p = app.db.project(&p.id).await.unwrap();
        let record = json!({"state":"live","status":{"state":"idle"},"foreground":{"name":agent},"processes":[{"pid":100,"name":agent,"argv":[agent]}],"native_session":{"value":fork},"interactive_ready":true,"state_change_seq":12,"execution":p.execution,"pane_id":"w1:p1","terminal_id":"owned-terminal","workspace_id":"w1","agent":{"agent":agent,"pane_id":"w1:p1","terminal_id":"owned-terminal","workspace_id":"w1"},"tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":"owned-generation","hfb_execution":"owned-managed"},"children":{"state":"quiescent"}});
        app.db
            .sync(|c| {
                assert!(admit(c, &p, &record)?.is_some());
                Ok(())
            })
            .unwrap();
        let current = app.db.project(&p.id).await.unwrap();
        (dir, app, current, record)
    }
    #[tokio::test]
    async fn failed_model_targets_restore_source_settings_before_fork_repair_admission() {
        for role in ["selected-target", "superseded-target", "restored-source"] {
            let (_dir, app, initial, mut record) = fixture("codex").await;
            let lane = descriptor(&initial).unwrap();
            app.db.sync(|c| {
                let mut execution = initial.execution.clone();
                execution.as_object_mut().unwrap().remove("repair_lane");
                let tag = if role == "superseded-target" { "superseded-model-request" } else { &lane.request_id };
                execution["cli_switch_request"] = json!(tag);
                if role != "restored-source" {
                    execution.as_object_mut().unwrap().remove("cli_recovery_request");
                }
                let retired = if role == "superseded-target" {
                    json!([{"generation":initial.generation,"request_id":tag}])
                } else { json!([]) };
                c.execute("UPDATE projects SET execution=? WHERE id=?",params![execution.to_string(),initial.id])?;
                c.execute("UPDATE cli_switch_requests SET source=json_set(source,'$.intent','model','$.selection',json(?),'$.target_model',json(?),'$.retiring_candidates',json(?)) WHERE id=?",params![json!({"model":"working-source-model","provider":"working-provider"}).to_string(),json!({"model":"broken-target-model","provider":"broken-provider"}).to_string(),retired.to_string(),lane.request_id])?;
                Ok(())
            }).unwrap();
            let p = app.db.project(&initial.id).await.unwrap();
            record["execution"] = p.execution.clone();
            app.db
                .sync(|c| {
                    let source_before: String = c.query_row(
                        "SELECT source FROM cli_switch_requests WHERE id=?",
                        [&lane.request_id],
                        |r| r.get(0),
                    )?;
                    let admitted = admit(c, &p, &record)?;
                    assert_eq!(admitted.is_some(), role == "restored-source", "{role}");
                    let source_after: String = c.query_row(
                        "SELECT source FROM cli_switch_requests WHERE id=?",
                        [&lane.request_id],
                        |r| r.get(0),
                    )?;
                    if role != "restored-source" {
                        assert_eq!(
                            source_before, source_after,
                            "rejected target must not rewrite the source snapshot"
                        );
                        assert!(current(c, &p.id, false)?.is_none());
                    }
                    let source: Value = serde_json::from_str(&source_after)?;
                    assert_eq!(source["selection"]["model"], "working-source-model");
                    assert_eq!(source["target_model"]["model"], "broken-target-model");
                    assert_eq!(source["native_session"], p.native_session);
                    Ok(())
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn repair_native_and_effect_authority_never_become_original_readiness() {
        let (_dir, app, p, record) = fixture("codex").await;
        assert!(
            herdr::check_identity(&p, &record)
                .unwrap_err()
                .is::<herdr::NativeSessionChanged>()
        );
        assert!(check_identity(&p, &record).is_ok());
        assert!(!p.ready);
        assert_ne!(p.execution["mcp_registered"], true);
        assert_ne!(p.execution["native_confirmed"], true);
        app.db.sync(|c|{
            assert!(current(c,&p.id,true).is_err());
            let lane=current(c,&p.id,false)?.unwrap();assert_eq!(lane.expected_native,p.native_session);assert_ne!(lane.observed_native,p.native_session);
            c.execute("UPDATE projects SET execution=json_set(execution,'$.repair_lane.mcp_registered',json('true')) WHERE id=?",[&p.id])?;
            assert!(authorize(c,&p,false)?.is_some(),"normal MCP flag change does not replace immutable repair identity");
            let lane=current(c,&p.id,true)?.unwrap();let attachment=json!({(OUTBOX_KEY):lane});
            assert!(validate_outbox(c,&p.id,&attachment).is_ok());
            c.execute("UPDATE cli_switch_requests SET phase='switching' WHERE project_id=?",[&p.id])?;
            assert!(current(c,&p.id,false)?.is_some(),"the daemon may still retire its owned repair execution");
            assert!(current(c,&p.id,true).is_err());assert!(validate_outbox(c,&p.id,&attachment).is_err());
            let source:String=c.query_row("SELECT source FROM cli_switch_requests WHERE project_id=?",[&p.id],|r|r.get(0))?;
            assert_eq!(serde_json::from_str::<Value>(&source)?["native_session"],p.native_session);
            Ok(())
        }).unwrap();
        assert!(
            !app.db
                .observe_native(&p.id, &p.generation, "repair-native")
                .await
                .unwrap()
        );
        assert!(
            app.db
                .ready(
                    &p.id,
                    &p.generation,
                    p.terminal.as_deref().unwrap(),
                    "repair-native"
                )
                .await
                .is_err()
        );
        assert_eq!(
            app.db.project(&p.id).await.unwrap().native_session,
            p.native_session
        );
    }
    #[tokio::test]
    async fn pi_repair_child_fence_uses_b_without_overwriting_saved_a() {
        let (dir, app, mut p, _record) = fixture("pi").await;
        let adapter = dir.path().join("adapter");
        std::fs::create_dir(&adapter).unwrap();
        let lane = descriptor(&p).unwrap();
        p.execution["child_adapter"] = json!(adapter);
        let evidence = json!({"schema":1,"generation":p.generation,"session":lane.observed_native,"sessionFile":lane.observed_native,"epoch":"epoch","revision":1,"state":"active","updated":db::now()});
        std::fs::write(adapter.join("children.json"), evidence.to_string()).unwrap();
        assert_eq!(
            crate::native_children::observe(&p, db::now())["state"],
            "unknown"
        );
        assert_eq!(
            crate::native_children::observe(&child_view(&p), db::now())["state"],
            "active"
        );
        let mut evidence = evidence;
        evidence["state"] = json!("quiescent");
        std::fs::write(adapter.join("children.json"), evidence.to_string()).unwrap();
        let child = child_view(&p);
        let observed = crate::native_children::observe(&child, db::now());
        assert_eq!(observed["state"], "quiescent");
        let gate =
            crate::native_children::RetirementGate::acquire(&child, &observed, db::now()).unwrap();
        drop(gate);
        assert_eq!(
            app.db.project(&p.id).await.unwrap().native_session,
            lane.expected_native
        );
    }
    #[tokio::test]
    async fn queued_repair_reply_cannot_dispatch_after_retirement_and_uncertain_is_preserved() {
        let (_dir, app, p, _record) = fixture("codex").await;
        app.db.sync(|c|{
            c.execute("UPDATE projects SET execution=json_set(execution,'$.repair_lane.mcp_registered',json('true')) WHERE id=?",[&p.id])?;
            let lane=current(c,&p.id,true)?.unwrap();let attachment=json!({"to":"oc_fixture",(OUTBOX_KEY):lane}).to_string();
            for (id,status) in [("pending-repair","pending"),("uncertain-repair","uncertain")] {
                c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES(?,?,'out',?,'diagnostic',?,?,1,1)",params![id,p.id,id,status,attachment])?;
            }
            c.execute("UPDATE cli_switch_requests SET phase='switching' WHERE project_id=?",[&p.id])?;Ok(())
        }).unwrap();
        app.send_out("pending-repair").await.unwrap();
        app.send_out("uncertain-repair").await.unwrap();
        assert_eq!(
            app.db.message("pending-repair").await.unwrap().status,
            "failed"
        );
        assert_eq!(
            app.db.message("uncertain-repair").await.unwrap().status,
            "uncertain"
        );
        app.db
            .sync(|c| {
                assert_eq!(cli_switch::latest(c, &p.id)?.unwrap().phase, "switching");
                Ok(())
            })
            .unwrap();
    }
}
