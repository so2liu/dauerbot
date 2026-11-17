use herdr_feishu_bridge::{
    db::{Db, Project, ProjectInput},
    handoff,
    native_children::{self, RetirementGate},
    work,
};
use serde_json::{Value, json};
use std::fs;

async fn fixture() -> (tempfile::TempDir, Db, Project) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db
        .create(ProjectInput {
            name: "children".into(),
            path: dir.path().to_string_lossy().into(),
            agent: "pi".into(),
            chat_id: String::new(),
            native_session: "parent".into(),
            extra_args: vec![],
        })
        .await
        .unwrap();
    p.generation = "generation".into();
    p.execution = json!({"child_adapter":dir.path()});
    db.sync(|c| {
        c.execute(
            "UPDATE projects SET generation='generation',terminal='owned',ready=1 WHERE id=?",
            [&p.id],
        )?;
        work::begin_execution(c, &p.id, &p.generation, 0)?;
        Ok(())
    })
    .unwrap();
    (dir, db, p)
}
fn evidence(dir: &std::path::Path, state: &str, revision: u64, now: i64) -> Value {
    let value = json!({"schema":1,"generation":"generation","session":"parent","sessionFile":"/native/parent.jsonl","epoch":"adapter-1","revision":revision,"state":state,"updated":now});
    fs::write(
        dir.join("children.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    value
}

#[tokio::test]
async fn missing_stale_malformed_and_wrong_generation_or_session_are_unknown() {
    let (dir, _db, mut p) = fixture().await;
    assert_eq!(native_children::observe(&p, 100)["state"], "unknown");
    let valid = evidence(dir.path(), "quiescent", 0, 100);
    assert_eq!(native_children::observe(&p, 100)["state"], "quiescent");
    p.native_session = "/native/parent.jsonl".into();
    assert_eq!(native_children::observe(&p, 100)["state"], "quiescent");
    assert_eq!(native_children::observe(&p, 10_101)["state"], "unknown");
    assert_eq!(native_children::observe(&p, 99)["state"], "unknown");
    for (key, value) in [
        ("generation", json!("old")),
        ("sessionFile", json!("other")),
        ("revision", json!(-1)),
        ("schema", json!(9)),
        ("state", json!("idle")),
    ] {
        let mut changed = valid.clone();
        changed[key] = value;
        fs::write(
            dir.path().join("children.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        assert_eq!(
            native_children::observe(&p, 100)["state"],
            "unknown",
            "{key}"
        );
    }
    fs::write(dir.path().join("children.json"), "{partial").unwrap();
    assert_eq!(native_children::observe(&p, 100)["state"], "unknown");
    evidence(dir.path(), "quiescent", 0, 100);
    for agent in ["codex", "claude", "agy", "custom"] {
        p.agent = agent.into();
        assert_eq!(
            native_children::observe(&p, 100)["state"],
            "unknown",
            "{agent} has no native child adapter"
        );
    }
}

#[tokio::test]
async fn child_revision_active_and_unknown_reset_warm_window_and_cancel_handoff() {
    let (_dir, db, p) = fixture().await;
    db.sync(|c| {
        let quiet = json!({"state":"quiescent","revision":"parent:adapter:0"});
        work::observe_children(c, &p.id, &p.generation, &quiet, 0)?;
        assert!(!work::execution_idle_observed(
            c,
            &p.id,
            &p.generation,
            5,
            0
        )?);
        assert!(work::execution_idle_observed(
            c,
            &p.id,
            &p.generation,
            5,
            work::WARM_MS
        )?);
        let request = handoff::begin(c, &p.id, &p.generation, "old", 5, work::WARM_MS)?.unwrap();
        // A full child invocation between observations is still visible in its revision.
        let changed = json!({"state":"quiescent","revision":"parent:adapter:2"});
        work::observe_children(c, &p.id, &p.generation, &changed, work::WARM_MS + 1)?;
        assert_eq!(
            handoff::latest(c, &p.id, &p.generation)?.unwrap().status,
            "cancelled"
        );
        assert!(!handoff::claim(c, &p.id, &p.generation, &request.id)?);
        assert!(!work::execution_idle_observed(
            c,
            &p.id,
            &p.generation,
            5,
            work::WARM_MS + 1
        )?);
        for state in ["active", "unknown"] {
            work::observe_children(
                c,
                &p.id,
                &p.generation,
                &json!({"state":state,"revision":"parent:adapter:3"}),
                work::WARM_MS * 2,
            )?;
            assert!(
                work::execution(c, &p.id, &p.generation)?
                    .unwrap()
                    .idle_since
                    .is_none()
            );
        }
        work::observe_children(c, &p.id, &p.generation, &quiet, work::WARM_MS * 3)?;
        assert!(!work::execution_idle_observed(
            c,
            &p.id,
            &p.generation,
            5,
            work::WARM_MS * 3
        )?);
        assert!(work::execution_idle_observed(
            c,
            &p.id,
            &p.generation,
            5,
            work::WARM_MS * 4
        )?);
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn native_dispatch_gate_rechecks_revision_and_keeps_crash_fences() {
    let (dir, _db, p) = fixture().await;
    evidence(dir.path(), "quiescent", 0, 100);
    let quiet = native_children::observe(&p, 100);
    fs::create_dir(dir.path().join("dispatch.lock")).unwrap();
    assert!(
        RetirementGate::acquire(&p, &quiet, 100).is_err(),
        "A concurrent native dispatch holds the same mkdir gate"
    );
    fs::remove_dir(dir.path().join("dispatch.lock")).unwrap();
    evidence(dir.path(), "active", 1, 100);
    assert!(RetirementGate::acquire(&p, &quiet, 100).is_err());
    assert!(!dir.path().join("dispatch.lock").exists());
    evidence(dir.path(), "quiescent", 2, 100);
    assert!(
        RetirementGate::acquire(&p, &quiet, 100).is_err(),
        "An invocation between observations invalidates the claim"
    );
    let quiet = native_children::observe(&p, 100);
    let gate = RetirementGate::acquire(&p, &quiet, 100).unwrap();
    assert!(dir.path().join("retiring").exists());
    assert!(RetirementGate::acquire(&p, &quiet, 100).is_err());
    drop(gate);
    assert!(
        !dir.path().join("retiring").exists(),
        "A pre-stop veto resumes native dispatch"
    );
    RetirementGate::acquire(&p, &quiet, 100).unwrap().commit();
    assert!(!dir.path().join("dispatch.lock").exists());
    assert!(
        dir.path().join("retiring").exists(),
        "Attempted native stop preserves the generation fence even if its response is uncertain"
    );
    assert!(RetirementGate::acquire(&p, &quiet, 100).is_err());
    assert!(
        dir.path().join("retiring").exists(),
        "Never erase a previous process's crash fence"
    );
}

#[tokio::test]
async fn quiescent_retirement_and_input_during_handoff_preserve_input_boundary() {
    let (dir, db, p) = fixture().await;
    evidence(dir.path(), "quiescent", 0, 100);
    let quiet = native_children::observe(&p, 100);
    db.sync(|c| {
        work::observe_children(c,&p.id,&p.generation,&quiet,0)?;
        assert!(!work::execution_idle_observed(c,&p.id,&p.generation,10,0)?);
        assert!(work::execution_idle_observed(c,&p.id,&p.generation,10,work::WARM_MS)?);
        let request=handoff::begin(c,&p.id,&p.generation,"",10,work::WARM_MS)?.unwrap();
        let gate=RetirementGate::acquire(&p,&quiet,100)?;
        c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('input',?,'in','input','steer','queued',1,1)",[&p.id])?;
        assert!(!handoff::claim(c,&p.id,&p.generation,&request.id)?);
        drop(gate);
        assert_eq!(handoff::latest(c,&p.id,&p.generation)?.unwrap().status,"cancelled");
        c.execute("UPDATE messages SET status='submitted' WHERE id='input'",[])?;
        let request=handoff::begin(c,&p.id,&p.generation,"",12,work::WARM_MS*2)?.unwrap();
        let gate=RetirementGate::acquire(&p,&quiet,100)?;
        assert!(handoff::claim(c,&p.id,&p.generation,&request.id)?);
        gate.commit();
        handoff::complete(c,&p.id,&p.generation,&request.id,work::WARM_MS*2)?;
        assert_eq!(work::execution(c,&p.id,&p.generation)?.unwrap().status,"sleeping");
        Ok(())
    }).unwrap();
}
