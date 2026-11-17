use herdr_feishu_bridge::{
    db::{Db, ProjectInput},
    handoff, work,
};
use rusqlite::params;

async fn store() -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let p = db
        .create(ProjectInput {
            name: "fixture".into(),
            path: dir.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: String::new(),
            native_session: "recover-me".into(),
            extra_args: vec![],
        })
        .await
        .unwrap();
    db.sync(|c| {
        c.execute(
            "UPDATE projects SET generation='g',terminal='owned-terminal',ready=1 WHERE id=?",
            [&p.id],
        )?;
        work::begin_execution(c, &p.id, "g", 0)?;
        Ok(())
    })
    .unwrap();
    (dir, db, p.id)
}

#[tokio::test]
async fn new_input_cancels_handoff_before_import_and_cannot_erase_recovery() {
    let (_dir, db, p) = store().await;
    db.sync(|c| {
        let r = handoff::begin(c,&p,"g","old",10,0)?.unwrap();
        c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('new',?,'in','local-new','steer','queued',1,1)",[&p])?;
        assert_eq!(handoff::latest(c,&p,"g")?.unwrap().status,"cancelled");
        assert!(!handoff::claim(c,&p,"g",&r.id)?);
        assert!(handoff::complete(c,&p,"g",&r.id,2).is_err());
        Ok(())
    }).unwrap();
    assert_eq!(db.project(&p).await.unwrap().native_session, "recover-me");
    assert!(db.project(&p).await.unwrap().terminal.is_some());
}

#[tokio::test]
async fn retirement_boundary_keeps_later_input_queued_and_clears_native_only_on_completion() {
    let (_dir, db, p) = store().await;
    db.sync(|c| {
        let r = handoff::begin(c,&p,"g","",10,0)?.unwrap();
        assert!(handoff::claim(c,&p,"g",&r.id)?);
        let native:String = c.query_row("SELECT native_session FROM projects WHERE id=?",[&p],|r|r.get(0))?;
        assert_eq!(native,"recover-me");
        c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('after',?,'in','local-after','new work','queued',1,1)",[&p])?;
        handoff::complete(c,&p,"g",&r.id,2)?;
        let state:String = c.query_row("SELECT status FROM messages WHERE id='after'",[],|r|r.get(0))?;
        assert_eq!(state,"queued");
        assert_eq!(handoff::latest(c,&p,"g")?.unwrap().status,"retired");
        Ok(())
    }).unwrap();
    let project = db.project(&p).await.unwrap();
    assert!(project.native_session.is_empty());
    assert!(project.terminal.is_none());
}

#[tokio::test]
async fn pause_takeover_restart_and_uncertain_work_preserve_native_recovery() {
    let (dir, db, p) = store().await;
    db.sync(|c| {
        let first = handoff::begin(c, &p, "g", "", 10, 0)?.unwrap();
        work::set_takeover(c, &p, "g", true, 1)?;
        assert!(!handoff::claim(c, &p, "g", &first.id)?);
        assert_eq!(handoff::latest(c, &p, "g")?.unwrap().status, "cancelled");
        work::set_takeover(c, &p, "g", false, 2)?;
        assert!(handoff::begin(c, &p, "g", "", 12, 3)?.is_none());
        let second = handoff::begin(c, &p, "g", "", 12, work::WARM_MS)?.unwrap();
        c.execute("UPDATE projects SET paused=1 WHERE id=?", [&p])?;
        assert!(!handoff::claim(c, &p, "g", &second.id)?);
        c.execute("UPDATE projects SET paused=0 WHERE id=?", [&p])?;
        handoff::begin(c, &p, "g", "", 14, work::WARM_MS * 2)?.unwrap();
        Ok(())
    })
    .unwrap();
    drop(db);
    let db = Db::open(&dir.path().join("db")).unwrap();
    db.sync(|c| {
        assert_eq!(handoff::latest(c, &p, "g")?.unwrap().status, "failed");
        assert!(handoff::begin(c, &p, "g", "", 16, work::WARM_MS * 3)?.is_none());
        Ok(())
    })
    .unwrap();
    assert_eq!(db.project(&p).await.unwrap().native_session, "recover-me");
}

#[tokio::test]
async fn failed_write_stale_artifact_and_idle_transition_never_qualify_for_retirement() {
    let (dir, db, p) = store().await;
    let request = db
        .sync(|c| Ok(handoff::begin(c, &p, "g", "old", 10, 0)?.unwrap()))
        .unwrap();
    let text = format!(
        "# Goal\nCompleted work, key decisions, files and commits, pending issues, and next actions for the following session.\n<!-- dauerbot-handoff:{} -->",
        request.id
    );
    assert!(!handoff::updated(&request, Some(&text), Some(10)));
    assert!(!handoff::updated(
        &request,
        Some("an old handoff"),
        Some(12)
    ));
    assert!(!handoff::updated(&request, Some(&text), None));
    assert!(handoff::updated(&request, Some(&text), Some(12)));
    let mut same = request.clone();
    same.baseline = handoff::fingerprint(Some(&text));
    assert!(!handoff::updated(&same, Some(&text), Some(12)));
    let file = dir.path().join("HANDOFF.md");
    std::fs::create_dir(&file).unwrap();
    assert!(handoff::artifact(&file).is_err());
    db.sync(|c| {
        handoff::fail(c, &request.id, "Write failed")?;
        assert!(handoff::begin(c, &p, "g", "", 12, work::WARM_MS)?.is_none());
        assert!(!work::execution_idle_observed(c, &p, "g", 10, 0)?);
        assert!(!work::execution_idle_observed(
            c,
            &p,
            "g",
            12,
            work::WARM_MS
        )?);
        assert!(work::execution_idle_observed(
            c,
            &p,
            "g",
            12,
            work::WARM_MS * 2
        )?);
        // A generation replacement can never complete the old handshake.
        c.execute(
            "UPDATE projects SET generation='new' WHERE id=?",
            params![p],
        )?;
        assert!(!handoff::claim(c, &p, "g", &request.id)?);
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn existing_project_instructions_are_never_seeded_or_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let rules = dir.path().join("AGENTS.md");
    std::fs::write(&rules, "用户规则\n").unwrap();
    db.create(ProjectInput {
        name: "existing".into(),
        path: dir.path().to_string_lossy().into(),
        agent: "codex".into(),
        chat_id: String::new(),
        native_session: String::new(),
        extra_args: vec![],
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(rules).unwrap(), "用户规则\n");
}
