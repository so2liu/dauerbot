//! Explicit, cooperative CLI handoff. Automatic idle retirement is stricter.
use crate::{
    App,
    config::Config,
    db::{self, Project},
    handoff, herdr, native_children, work,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

const BUDGET_MS: i64 = 180_000;
const ATTEMPTS: i64 = 4;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub target: String,
    pub phase: String,
    pub error: String,
    pub generation: String,
    pub nonce: String,
    pub baseline: String,
    pub sequence: i64,
    pub revision: String,
    pub created_at: i64,
    pub attempts: i64,
}
pub fn migrate(c: &Connection, recover: bool) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS cli_switch_requests (
      id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
      target TEXT NOT NULL,phase TEXT NOT NULL,error TEXT NOT NULL DEFAULT '',generation TEXT NOT NULL,
      nonce TEXT NOT NULL DEFAULT '',baseline TEXT NOT NULL DEFAULT '',sequence INTEGER NOT NULL DEFAULT 0,
      revision TEXT NOT NULL DEFAULT '',created_at INTEGER NOT NULL,attempts INTEGER NOT NULL DEFAULT 0);
      CREATE INDEX IF NOT EXISTS cli_switch_project ON cli_switch_requests(project_id,created_at);")?;
    if recover {
        c.execute("UPDATE cli_switch_requests SET phase=CASE WHEN phase='switching' THEN 'uncertain' ELSE 'failed' END,error='The service restarted during the CLI handoff; the saved native conversation and input are preserved.' WHERE phase IN ('preparing','requested','switching')", [])?;
    }
    Ok(())
}
pub fn latest(c: &Connection, project: &str) -> Result<Option<Request>> {
    Ok(c.query_row("SELECT id,target,phase,error,generation,nonce,baseline,sequence,revision,created_at,attempts FROM cli_switch_requests WHERE project_id=? ORDER BY rowid DESC LIMIT 1",[project],|r|Ok(Request{id:r.get(0)?,target:r.get(1)?,phase:r.get(2)?,error:r.get(3)?,generation:r.get(4)?,nonce:r.get(5)?,baseline:r.get(6)?,sequence:r.get(7)?,revision:r.get(8)?,created_at:r.get(9)?,attempts:r.get(10)?})).optional()?)
}
pub fn active(r: &Request) -> bool {
    matches!(
        r.phase.as_str(),
        "preparing" | "requested" | "switching" | "starting" | "uncertain"
    )
}
pub fn revision(c: &Connection, project: &str) -> Result<String> {
    Ok(c.query_row("SELECT COALESCE((SELECT MAX(rowid) FROM messages WHERE project_id=?1 AND direction='in'),0)||':'||COALESCE((SELECT MAX(rowid) FROM work_events WHERE project_id=?1),0)",[project],|r|r.get(0))?)
}
fn pending_input(c: &Connection, project: &str) -> Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE project_id=?1 AND direction='in' AND status='queued') OR EXISTS(SELECT 1 FROM work_events WHERE project_id=?1 AND status='queued')",[project],|r|r.get(0))?)
}
pub fn cancel(c: &Connection, project: &str, error: &str) -> Result<()> {
    c.execute("UPDATE cli_switch_requests SET phase='failed',error=? WHERE project_id=? AND phase IN ('preparing','requested','switching','starting','uncertain')",params![error,project])?;
    Ok(())
}
fn phase(c: &Connection, id: &str, value: &str, error: &str) -> Result<()> {
    c.execute(
        "UPDATE cli_switch_requests SET phase=?,error=? WHERE id=?",
        params![value, error, id],
    )?;
    Ok(())
}
pub fn proof(r: &Request, text: Option<&str>, sequence: Option<i64>) -> bool {
    text.is_some_and(|t| {
        t.len() > 100
            && t.contains(&format!("<!-- dauerbot-switch-ready:{} -->", r.nonce))
            && handoff::fingerprint(Some(t)) != r.baseline
    }) && sequence.is_some_and(|s| s > r.sequence)
}
pub fn claim(c: &Connection, project: &str, r: &Request) -> Result<bool> {
    let tx = c.unchecked_transaction()?;
    if revision(&tx, project)? != r.revision
        || pending_input(&tx, project)?
        || work::has_takeover(&tx, project, &r.generation)?
    {
        return Ok(false);
    }
    let valid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=? AND generation=? AND paused=0)",
        params![project, r.generation],
        |v| v.get(0),
    )?;
    if !valid {
        return Ok(false);
    }
    let changed=tx.execute("UPDATE cli_switch_requests SET phase='switching' WHERE id=? AND phase='requested' AND nonce=?",params![r.id,r.nonce])?;
    tx.commit()?;
    Ok(changed == 1)
}
fn complete(c: &Connection, project: &str, r: &Request) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    ensure!(tx.execute("UPDATE cli_switch_requests SET phase='starting' WHERE id=? AND phase IN ('switching','uncertain')",[&r.id])?==1,"CLI transition changed");
    work::finish_execution(
        &tx,
        project,
        &r.generation,
        "stopped",
        "CLI handoff completed",
        db::now(),
    )?;
    ensure!(tx.execute("UPDATE projects SET agent=?,native_session='',extra_args='[]',terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null' WHERE id=? AND generation=?",params![r.target,project,r.generation])?==1,"Execution changed during CLI handoff");
    tx.commit()?;
    Ok(())
}
fn prompt(r: &Request) -> String {
    format!(
        "<dauerbot_cli_switch request_id=\"{}\">\nThe user requested switching this project to {} now. Finish or safely settle current work and JOIN/SETTLE every native subagent and external worker before acknowledging. Update HANDOFF.md with the current goal, completed work, decisions, files, pending issues and next actions for the new CLI. Preserve user content and AGENTS.md. ONLY when no child or external worker or ongoing side effect remains, include this exact line: <!-- dauerbot-switch-ready:{} -->. This line is your explicit ready-to-switch acknowledgement. If you cannot settle outstanding work, omit the marker and explain it in HANDOFF.md. Handle incoming user input immediately; it invalidates this request and a newer handoff request will follow. Return to idle after writing. Do not close your own terminal or send an automatic completion message.\n</dauerbot_cli_switch>",
        r.id, r.target, r.nonce
    )
}
async fn artifact(p: &Project) -> Result<Option<String>> {
    let path = PathBuf::from(&p.path);
    let content = tokio::task::spawn_blocking(move || {
        crate::project_documents::read(&path, crate::project_documents::Document::Handoff)
    })
    .await?;
    match content.status.as_str() {
        "available" => Ok(Some(content.text)),
        "missing" => Ok(None),
        _ => anyhow::bail!("{}", content.message),
    }
}
impl App {
    pub(crate) async fn cli_switch_bootstrap(&self, project: &str) -> Result<String> {
        let id = project.to_owned();
        let r = self.db.call(move |c| latest(c, &id)).await?;
        Ok(if r.is_some_and(|r| r.phase == "starting") {
            " The user explicitly switched this project's CLI now. Read this project's AGENTS.md and HANDOFF.md before continuing the current goal. If HANDOFF.md is absent, this is a fresh workspace: use the project instructions and await user input. Preserve completed effects and request IDs; do not replay already delivered messages or work merely because this is a fresh native conversation.".into()
        } else {
            String::new()
        })
    }
    pub async fn request_cli_switch(self: &Arc<Self>, key: &str, target: &str) -> Result<Project> {
        ensure!(
            ["codex", "claude", "pi", "agy"].contains(&target),
            "Choose Codex, Claude Code, Pi, or Antigravity"
        );
        let p = self.db.project(key).await?;
        let lock = self.project_lock(&p.id).await;
        let _guard = lock.lock().await;
        let p = self.db.project(&p.id).await?;
        ensure!(!p.paused, "Resume this project before changing its CLI");
        ensure!(
            !Self::update_required(&p),
            "Recover this project after its service update before changing the CLI"
        );
        let id = p.id.clone();
        let generation = p.generation.clone();
        self.db
            .call(move |c| {
                ensure!(
                    !work::has_takeover(c, &id, &generation)?,
                    "Release terminal input before changing the CLI"
                );
                Ok(())
            })
            .await?;
        let id = p.id.clone();
        let previous = self.db.call(move |c| latest(c, &id)).await?;
        if let Some(r) = previous.as_ref().filter(|r| active(r)) {
            ensure!(
                r.target == target && r.phase != "uncertain",
                "A CLI handoff is already in progress; review its result before starting another"
            );
            return Ok(p);
        }
        let retry = p.agent == target
            && p.terminal.is_none()
            && previous
                .as_ref()
                .is_some_and(|r| r.phase == "failed" && r.target == target);
        if p.agent == target && !retry {
            return Ok(p);
        }
        let initial_phase = if retry { "starting" } else { "preparing" };
        let id = p.id.clone();
        let target = target.to_owned();
        let generation = p.generation.clone();
        self.db.call(move|c|{c.execute("INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at) VALUES(?,?,?,?,?,?)",params![db::id(),id,target,initial_phase,generation,db::now()])?;Ok(())}).await?;
        self.notify.notify_one();
        Ok(p)
    }
    /// Called with the project lock. None handles terminal-free startup only;
    /// a live observation is processed after normal input steering.
    pub(crate) async fn advance_cli_switch(
        &self,
        p: &Project,
        cfg: Option<&Config>,
        record: Option<&Value>,
    ) -> Result<bool> {
        let id = p.id.clone();
        let Some(mut r) = self.db.call(move |c| latest(c, &id)).await?.filter(active) else {
            return Ok(false);
        };
        let id = p.id.clone();
        let generation = p.generation.clone();
        let takeover = self
            .db
            .call(move |c| work::has_takeover(c, &id, &generation))
            .await?;
        if p.paused || takeover {
            if matches!(r.phase.as_str(), "uncertain" | "switching" | "starting") {
                self.set_phase(
                    p,
                    if p.paused { "paused" } else { "taken_over" },
                    Value::Null,
                )
                .await;
                return Ok(true);
            }
            self.fail_cli_switch(&r,"CLI handoff cancelled because the project was paused or terminal input was taken over.").await?;
            return Ok(false);
        }
        if r.phase == "uncertain" {
            // A lost stop response or restart after the claim cannot be retried.
            // Only a verified closed original execution permits target launch.
            let cfg = self.config.read().await.clone();
            if p.generation == r.generation
                && p.terminal.is_some()
                && let Ok(observed) = herdr::observe(&cfg, p).await
                && observed["state"] == "finished"
            {
                let id = p.id.clone();
                let rr = r.clone();
                self.db.call(move |c| complete(c, &id, &rr)).await?;
                self.start_cli_target(p, &r).await?;
                self.notify.notify_one();
                return Ok(true);
            }
            self.set_phase(p, "needs_attention", Value::Null).await;
            return Ok(true);
        }
        if r.phase == "starting" && db::now() - r.created_at > BUDGET_MS {
            self.fail_cli_switch(&r,"The selected CLI started but is not ready. Check its login or permissions in Terminal.").await?;
            return Ok(false);
        }
        if r.phase != "starting" && db::now() - r.created_at > BUDGET_MS {
            self.fail_cli_switch(&r,"The Agent did not finish a safe handoff in time. The current CLI and native conversation were kept.").await?;
            return Ok(false);
        }
        if p.terminal.is_none() {
            if r.phase == "starting" {
                self.start_cli_target(p, &r).await?;
                return Ok(true);
            }
            if !p.native_session.is_empty() {
                match self.start_inner(&p.id, true).await {
                    Ok(p) => {
                        let id = r.id.clone();
                        let generation = p.generation;
                        self.db
                            .call(move |c| {
                                c.execute(
                                    "UPDATE cli_switch_requests SET generation=? WHERE id=?",
                                    params![generation, id],
                                )?;
                                Ok(())
                            })
                            .await?;
                    }
                    Err(e) => {
                        self.fail_cli_switch(
                            &r,
                            &format!("Cannot resume the current CLI to prepare its handoff: {e}"),
                        )
                        .await?
                    }
                }
            } else {
                // No live or resumable native state exists. The selected CLI reads
                // existing project files, or starts fresh when HANDOFF is absent.
                let id = p.id.clone();
                let target = r.target.clone();
                let request = r.id.clone();
                self.db.call(move|c|{let tx=c.unchecked_transaction()?;ensure!(tx.execute("UPDATE projects SET agent=?,extra_args='[]',native_session='',generation='',ready=0,execution='null' WHERE id=? AND terminal IS NULL",params![target,id])?==1,"Project woke during CLI selection");phase(&tx,&request,"starting","")?;tx.commit()?;Ok(())}).await?;
                r.phase = "starting".into();
                self.start_cli_target(p, &r).await?;
            }
            self.notify.notify_one();
            return Ok(true);
        }
        let Some(record) = record else {
            return Ok(false);
        };
        let cfg = cfg.expect("live observation requires config");
        if r.phase == "starting" {
            if p.agent != r.target {
                self.fail_cli_switch(&r, "The project CLI changed during startup.")
                    .await?;
                return Ok(false);
            }
            if p.ready
                && matches!(
                    record["status"]["state"].as_str(),
                    Some("idle" | "done" | "working")
                )
            {
                let id = r.id;
                self.db.call(move |c| phase(c, &id, "ready", "")).await?;
                return Ok(false);
            }
            if db::now() - r.created_at > BUDGET_MS {
                self.fail_cli_switch(&r,"The selected CLI started but is not ready. Check its login or permissions in Terminal.").await?;
                return Ok(false);
            }
            self.set_phase(p, "switching_cli", record.clone()).await;
            return Ok(true);
        }
        if p.generation != r.generation {
            self.fail_cli_switch(
                &r,
                "The native execution changed during CLI handoff. Its continuity was preserved.",
            )
            .await?;
            return Ok(false);
        }
        let id = p.id.clone();
        let current = self.db.call(move |c| revision(c, &id)).await?;
        if r.phase == "preparing" || current != r.revision {
            if r.attempts >= ATTEMPTS {
                self.fail_cli_switch(&r,"New input kept changing the handoff. The current CLI was kept; select the new CLI again when ready.").await?;
                return Ok(false);
            }
            let text = artifact(p).await?;
            let baseline = handoff::fingerprint(text.as_deref());
            let nonce = db::id();
            let sequence = record["state_change_seq"].as_i64().ok_or_else(|| {
                anyhow::anyhow!("The CLI has no native activity sequence for handoff")
            })?;
            let id = p.id.clone();
            let rid = r.id.clone();
            let revision = current.clone();
            let baseline2 = baseline.clone();
            let nonce2 = nonce.clone();
            let saved=self.db.call(move|c|{if self::revision(c,&id)?!=revision{return Ok(false)};c.execute("UPDATE cli_switch_requests SET phase='requested',nonce=?,baseline=?,sequence=?,revision=?,attempts=attempts+1 WHERE id=?",params![nonce2,baseline2,sequence,revision,rid])?;Ok(true)}).await?;
            if !saved {
                return Ok(true);
            }
            r.nonce = nonce;
            r.baseline = baseline;
            r.sequence = sequence;
            r.revision = current;
            r.phase = "requested".into();
            match herdr::send(cfg,p,&prompt(&r)).await {Ok(v)=>{let id=r.id.clone();let seq=v["native_seq"].as_i64().unwrap_or(sequence);self.db.call(move|c|{c.execute("UPDATE cli_switch_requests SET sequence=? WHERE id=?",params![seq,id])?;Ok(())}).await?;},Err(e)=>self.fail_cli_switch(&r,&format!("The handoff request could not be confirmed: {e}. The current CLI was kept.")).await?}
            self.set_phase(p, "preparing_handoff", record.clone()).await;
            return Ok(true);
        }
        self.set_phase(p, "preparing_handoff", record.clone()).await;
        if !herdr::foreground_quiescent(p, record) || record["children"]["state"] == "active" {
            return Ok(true);
        }
        let id = p.id.clone();
        let generation = p.generation.clone();
        let seq = record["state_change_seq"].as_i64();
        self.db
            .call(move |c| work::observe_idle(c, &id, &generation, seq, db::now()))
            .await?;
        let text = artifact(p).await?;
        if !proof(&r, text.as_deref(), record["state_change_seq"].as_i64()) {
            return Ok(true);
        }
        let check = herdr::observe(cfg, p).await?;
        if !herdr::foreground_quiescent(p, &check)
            || check["children"]["state"] == "active"
            || check["children"] != record["children"]
            || !proof(&r, text.as_deref(), check["state_change_seq"].as_i64())
        {
            return Ok(true);
        }
        // Managed Pi provides a real dispatch fence. Other CLIs have no complete
        // registry: explicit user intent + the Agent's nonce acknowledgement is
        // cooperative evidence, not a claim of native child coverage.
        let gate = if p.agent == "pi" && p.execution["child_adapter"].is_string() {
            match native_children::RetirementGate::acquire(p, &check["children"], db::now()) {
                Ok(g) => Some(g),
                Err(_) => return Ok(true),
            }
        } else {
            None
        };
        let final_check = herdr::observe(cfg, p).await?;
        if !herdr::foreground_quiescent(p, &final_check)
            || final_check["children"]["state"] == "active"
            || final_check["children"] != check["children"]
        {
            return Ok(true);
        }
        let id = p.id.clone();
        let rr = r.clone();
        if !self.db.call(move |c| claim(c, &id, &rr)).await? {
            return Ok(true);
        }
        if let Err(e) = herdr::stop(cfg, p).await {
            if let Some(g) = gate {
                g.commit()
            };
            let id = r.id.clone();
            let error = format!(
                "Native stop has an uncertain result: {e}. Review this execution before stopping or starting it explicitly."
            );
            self.db
                .call(move |c| phase(c, &id, "uncertain", &error))
                .await?;
            return Ok(true);
        }
        if let Some(g) = gate {
            g.commit()
        };
        let id = p.id.clone();
        let rr = r.clone();
        self.db.call(move |c| complete(c, &id, &rr)).await?;
        self.start_cli_target(p, &r).await?;
        self.notify.notify_one();
        Ok(true)
    }
    async fn fail_cli_switch(&self, r: &Request, error: &str) -> Result<()> {
        let id = r.id.clone();
        let error = error.to_owned();
        self.db.call(move |c| phase(c, &id, "failed", &error)).await
    }
    async fn start_cli_target(&self, p: &Project, r: &Request) -> Result<()> {
        let current = self.db.project(&p.id).await?;
        let id = p.id.clone();
        let generation = current.generation.clone();
        let takeover = self
            .db
            .call(move |c| work::has_takeover(c, &id, &generation))
            .await?;
        if current.paused || takeover {
            return Ok(());
        }
        if let Err(e) = self.start_inner(&p.id, false).await {
            self.fail_cli_switch(r,&format!("The CLI selection was saved, but startup failed: {e}. Workspace files and saved input are preserved.")).await?;
            self.attention(p, "startup", &e.to_string()).await?;
        }
        self.set_phase(p, "switching_cli", Value::Null).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::ProjectInput};
    async fn fixture() -> (tempfile::TempDir, Arc<App>, Project) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().into(), Config::default()).unwrap();
        let p = app
            .db
            .create(ProjectInput {
                name: "Switch fixture".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: "old-native".into(),
                extra_args: vec!["--model".into(), "preserve".into()],
            })
            .await
            .unwrap();
        app.db.sync(|c|{c.execute("UPDATE projects SET generation='old-gen',terminal='old-pane',ready=1,execution='{\"api_protocol\":7,\"mcp_contract\":1}' WHERE id=?",[&p.id])?;work::begin_execution(c,&p.id,"old-gen",0)?;Ok(())}).unwrap();
        (dir, app.clone(), app.db.project(&p.id).await.unwrap())
    }
    fn requested(c: &Connection, p: &Project) -> Result<Request> {
        c.execute("UPDATE cli_switch_requests SET phase='requested',nonce='fresh',baseline='old',sequence=10,revision=? WHERE project_id=?",params![revision(c,&p.id)?,p.id])?;
        Ok(latest(c, &p.id)?.unwrap())
    }
    #[tokio::test]
    async fn explicit_request_is_idempotent_and_never_stops_active_agent() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        assert!(app.request_cli_switch(&p.id, "claude").await.is_err());
        assert_eq!(app.db.project(&p.id).await.unwrap().terminal, p.terminal);
        assert_eq!(
            app.db
                .sync(|c| Ok(
                    c.query_row("SELECT count(*) FROM cli_switch_requests", [], |r| r
                        .get::<_, i64>(0))?
                ))
                .unwrap(),
            1
        );
        app.db.pause(&p.id, true).await.unwrap();
        assert!(app.request_cli_switch(&p.id, "pi").await.is_err());
        assert!(
            !app.advance_cli_switch(&app.db.project(&p.id).await.unwrap(), None, None)
                .await
                .unwrap()
        );
        assert_eq!(app.db.project(&p.id).await.unwrap().agent, "codex");
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "failed"
        );
    }
    #[tokio::test]
    async fn inbound_revision_fences_claim_and_later_input_survives_atomic_cli_flip() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db.sync(|c|{
            let stale=requested(c,&p)?;
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('before',?,'in','before','new input','queued',1,1)",[&p.id])?;
            assert!(!claim(c,&p.id,&stale)?);
            c.execute("UPDATE messages SET status='delivered' WHERE id='before'",[])?;
            let fresh=requested(c,&p)?;
            assert!(claim(c,&p.id,&fresh)?);
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('after',?,'in','after','new input','queued',2,2)",[&p.id])?;
            complete(c,&p.id,&fresh)?;
            assert_eq!(c.query_row("SELECT status FROM messages WHERE id='after'",[],|r|r.get::<_,String>(0))?,"queued");
            Ok(())
        }).unwrap();
        let changed = app.db.project(&p.id).await.unwrap();
        assert_eq!(changed.agent, "pi");
        assert!(
            changed.terminal.is_none()
                && changed.native_session.is_empty()
                && changed.extra_args.is_empty()
        );
        assert_eq!(changed.path, p.path);
    }
    #[tokio::test]
    async fn pause_after_claim_keeps_uncertain_execution_fenced_without_startup() {
        let (_dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db
            .sync(|c| {
                let r = requested(c, &p)?;
                assert!(claim(c, &p.id, &r)?);
                phase(c, &r.id, "uncertain", "Lost stop response")?;
                Ok(())
            })
            .unwrap();
        app.db.pause(&p.id, true).await.unwrap();
        let paused = app.db.project(&p.id).await.unwrap();
        assert!(app.advance_cli_switch(&paused, None, None).await.unwrap());
        assert_eq!(
            app.db
                .sync(|c| Ok(latest(c, &p.id)?.unwrap().phase))
                .unwrap(),
            "uncertain"
        );
        let preserved = app.db.project(&p.id).await.unwrap();
        assert_eq!(preserved.agent, "codex");
        assert_eq!(preserved.terminal, p.terminal);
        assert_eq!(preserved.native_session, p.native_session);
    }
    #[tokio::test]
    async fn nonce_native_transition_takeover_and_restart_cannot_authorize_stale_stop() {
        let (dir, app, p) = fixture().await;
        app.request_cli_switch(&p.id, "pi").await.unwrap();
        app.db.sync(|c|{
            let r=requested(c,&p)?;let text=format!("# Handoff\n{}\n<!-- dauerbot-switch-ready:fresh -->","Settled all children and external work; continue the current user goal without replaying completed effects. ".repeat(2));
            assert!(!proof(&r,Some(&text),Some(10)));assert!(proof(&r,Some(&text),Some(11)));
            assert!(!proof(&r,Some(&text.replace("fresh","stale")),Some(11)));
            work::set_takeover(c,&p.id,&p.generation,true,1)?;assert!(!claim(c,&p.id,&r)?);work::set_takeover(c,&p.id,&p.generation,false,2)?;
            assert!(claim(c,&p.id,&r)?);Ok(())
        }).unwrap();
        drop(app);
        let reopened = crate::db::Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
        reopened
            .sync(|c| {
                assert_eq!(latest(c, &p.id)?.unwrap().phase, "uncertain");
                Ok(())
            })
            .unwrap();
        let preserved = reopened.project(&p.id).await.unwrap();
        assert_eq!(preserved.agent, "codex");
        assert_eq!(preserved.native_session, "old-native");
        assert_eq!(preserved.terminal, p.terminal);
    }
}
