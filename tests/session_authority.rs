use herdr_feishu_bridge::{
    App, api,
    config::Config,
    db::{self, Db, ProjectInput},
    work::{self, TaskInput},
};
use serde_json::json;
use std::{future::IntoFuture, sync::Arc, time::Duration};

fn input(path: &std::path::Path) -> ProjectInput {
    ProjectInput {
        name: "Authority fixture".into(),
        path: path.to_string_lossy().into(),
        agent: "codex".into(),
        chat_id: String::new(),
        native_session: "previous-native".into(),
        extra_args: vec![],
    }
}

#[tokio::test]
async fn upgrade_removes_the_stopped_project_gate_and_preserves_explicit_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite3");
    let db = Db::open(&path).unwrap();
    let stopped = db.create(input(dir.path())).await.unwrap();
    let mut paused_input = input(dir.path());
    paused_input.name = "Explicitly paused".into();
    let paused = db.create(paused_input).await.unwrap();
    db.pause(&paused.id, true).await.unwrap();
    db.sync(|c| {
        herdr_feishu_bridge::bot::save(c, &stopped.id, "cli_preserved", "saved-secret", false)?;
        c.execute_batch(
            "ALTER TABLE projects ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
             UPDATE projects SET enabled=0;
             PRAGMA user_version=5;",
        )?;
        Ok(())
    })
    .unwrap();
    drop(db);

    let upgraded = Db::open(&path).unwrap();
    assert!(!upgraded.project(&stopped.id).await.unwrap().paused);
    assert!(upgraded.project(&paused.id).await.unwrap().paused);
    assert_eq!(
        upgraded.project(&stopped.id).await.unwrap().native_session,
        "previous-native"
    );
    assert_eq!(
        upgraded.bot(&stopped.id).await.unwrap().app_secret,
        "saved-secret"
    );
    upgraded
        .sync(|c| {
            let obsolete: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name='enabled')",
                [],
                |row| row.get(0),
            )?;
            assert!(!obsolete);
            assert_eq!(
                c.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
                6
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn admitted_old_generation_cannot_finish_after_execution_changes() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(dir.path().into(), Config::default()).unwrap();
    let project = app.db.create(input(dir.path())).await.unwrap();
    app.db
        .save_execution(
            &project.id,
            json!({"terminal_id":"terminal","shell_pid":1}),
            "generation-one",
        )
        .await
        .unwrap();
    let id = project.id.clone();
    let run = app
        .db
        .call(move |c| {
            work::begin_execution(c, &id, "generation-one", 0)?;
            work::submit_task(
                c,
                &id,
                TaskInput {
                    title: "Existing task".into(),
                    instructions: "Keep its effects uncertain".into(),
                    to: None,
                    request_id: None,
                },
                "task",
                "one",
                1,
            )?;
            Ok(work::claim_next(c, &id, "generation-one", 2)?.unwrap())
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/projects/{}/finish",
        listener.local_addr().unwrap(),
        project.id
    );
    let server = tokio::spawn(axum::serve(listener, api::router(app.clone())).into_future());

    // Admission succeeds first; the handler then waits on the lifecycle mutex.
    // Counting the handler's Arc avoids a timing-dependent sleep.
    let lifecycle = app.project_lock(&project.id).await;
    let held = lifecycle.lock().await;
    let token = project.mcp_token.clone();
    let run_id = run.id.clone();
    let request = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .bearer_auth(token)
            .header("x-hfb-generation", "generation-one")
            .json(&json!({"run_id":run_id,"outcome":"completed","summary":"Stale result"}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::strong_count(&lifecycle) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("The authenticated finish handler did not reach the lifecycle lock");
    app.db
        .set_session(
            &project.id,
            Some("replacement".into()),
            Some(2),
            "generation-two".into(),
        )
        .await
        .unwrap();
    drop(held);
    let response = request.await.unwrap();
    assert!(!response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("execution changed")
    );
    let id = project.id;
    let stored = app
        .db
        .call(move |c| work::run(c, &id, &run.id))
        .await
        .unwrap();
    assert_eq!(stored.status, "submitting");
    assert!(stored.finished_at.is_none());
    server.abort();
}

#[tokio::test]
async fn inherited_native_pointer_is_not_confirmation_for_a_new_execution() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
    let project = db.create(input(dir.path())).await.unwrap();
    db.save_execution(
        &project.id,
        json!({"terminal_id":"terminal","shell_pid":1}),
        "new-generation",
    )
    .await
    .unwrap();
    assert_ne!(
        db.project(&project.id).await.unwrap().execution["native_confirmed"],
        true
    );
    db.native_session(&project.id, "confirmed-new-history")
        .await
        .unwrap();
    let confirmed = db.project(&project.id).await.unwrap();
    assert_eq!(confirmed.native_session, "confirmed-new-history");
    assert_eq!(confirmed.execution["native_confirmed"], true);
}

#[tokio::test]
async fn pre_refactor_store_is_refused_without_mutating_its_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite3");
    let db = Db::open(&path).unwrap();
    let project = db.create(input(dir.path())).await.unwrap();
    db.set_session(
        &project.id,
        Some("old-core-terminal".into()),
        Some(10),
        "old-generation".into(),
    )
    .await
    .unwrap();
    db.sync(|c| {
        c.execute_batch(
            "ALTER TABLE projects DROP COLUMN execution;
             PRAGMA user_version=3;",
        )?;
        Ok(())
    })
    .unwrap();
    drop(db);
    assert!(Db::open(&path).is_err());
    let c = rusqlite::Connection::open(&path).unwrap();
    let columns: Vec<String> = c
        .prepare("PRAGMA table_info(projects)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!columns.iter().any(|column| column == "execution"));
    assert!(!columns.iter().any(|column| column == "enabled"));
    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 3);
    let terminal: String = c
        .query_row(
            "SELECT terminal FROM projects WHERE id=?",
            [&project.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(terminal, "old-core-terminal");
    let native: String = c
        .query_row(
            "SELECT native_session FROM projects WHERE id=?",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(native, "previous-native");
}

#[tokio::test]
async fn failed_child_outcome_survives_api_and_wakeup_event() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::default();
    let app = App::new(dir.path().into(), cfg.clone()).unwrap();
    let project = app.db.create(input(dir.path())).await.unwrap();
    app.db
        .save_execution(&project.id, json!({"terminal_id":"t","shell_pid":1}), "g")
        .await
        .unwrap();
    let id = project.id.clone();
    let job = app
        .db
        .call(move |c| {
            work::submit_task(
                c,
                &id,
                TaskInput {
                    title: "Parent".into(),
                    instructions: "Wait for a child".into(),
                    to: None,
                    request_id: None,
                },
                "task",
                "one",
                db::now(),
            )?;
            let run = work::claim_next(c, &id, "g", db::now())?.unwrap();
            work::register_job(c, &id, "g", &run.id, "Child", db::now())
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/projects/{}/job-complete",
        listener.local_addr().unwrap(),
        project.id
    );
    let server = tokio::spawn(axum::serve(listener, api::router(app.clone())).into_future());
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .bearer_auth(&project.mcp_token)
        .header("x-hfb-generation", "g")
        .json(&json!({"job_id":job.id,"status":"failed","summary":"Command failed"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let event: serde_json::Value = response.json().await.unwrap();
    assert_eq!(event["payload"]["status"], "failed");
    let state = app
        .db
        .call(move |c| work::snapshot(c, &project.id))
        .await
        .unwrap();
    assert_eq!(state.jobs[0].status, "failed");
    server.abort();
}

#[tokio::test]
async fn answer_before_deferred_finish_does_not_strand_the_active_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
    let project = db.create(input(dir.path())).await.unwrap();
    db.call(move |c| {
        work::submit_task(
            c,
            &project.id,
            TaskInput {
                title: "Question".into(),
                instructions: "Wait for the user".into(),
                to: None,
                request_id: None,
            },
            "task",
            "one",
            0,
        )?;
        let run = work::claim_next(c, &project.id, "g", 1)?.unwrap();
        let question =
            work::request_decision(c, &project.id, "g", &run.id, "Which one?", "", json!([]), 2)?;
        let answered = work::answer_decision(c, &project.id, &question.id, "The first", 3)?;
        let retry =
            work::request_decision(c, &project.id, "g", &run.id, "Which one?", "", json!([]), 4)?;
        assert_eq!(question.id, retry.id);
        work::finish_run(
            c,
            &project.id,
            "g",
            &run.id,
            "deferred",
            "Answer arrived while finishing",
            5,
        )?;
        assert!(work::active_run(c, &project.id)?.is_none());
        assert_eq!(work::task(c, &project.id, &run.task_id)?.status, "queued");
        let followup = work::claim_next(c, &project.id, "g", 6)?.unwrap();
        assert_eq!(followup.event_id, answered.id);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn project_removal_cleans_all_durable_foreign_keys_and_preserves_other_work() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
    let project = db.create(input(dir.path())).await.unwrap();
    let mut another = input(dir.path());
    another.name = "Other project".into();
    let other = db.create(another).await.unwrap();
    db.sync(|c| herdr_feishu_bridge::bot::save(c, &project.id, "cli_fixture", "secret", false))
        .unwrap();
    db.ingest_bot(
        &project.id,
        "cli_fixture",
        "oc_source",
        "im_original",
        "Existing Feishu input",
        &json!({"source":{"chat_id":"oc_source"}}),
    )
    .unwrap();
    let id = project.id.clone();
    let other_id = other.id.clone();
    db.call(move |c| {
        work::sync_inbound(c)?;
        let run = work::claim_next(c, &id, "g", 1)?.unwrap();
        work::request_decision(c, &id, "g", &run.id, "Question", "", json!([]), 2)?;
        let job = work::register_job(c, &id, "g", &run.id, "Child", 3)?;
        work::complete_job(c, &id, "g", &job.id, "Result", 4)?;
        work::finish_run(c, &id, "g", &run.id, "deferred", "Waiting", 5)?;
        work::create_timer(
            c,
            &id,
            work::TimerInput {
                name: "Schedule".into(),
                prompt: "Check".into(),
                kind: "once".into(),
                at: Some(json!(10)),
                interval_seconds: None,
                cron: None,
                timezone: "UTC".into(),
                to: None,
                request_id: None,
            },
            6,
        )?;
        work::fire_due(c, 10)?;
        work::set_takeover(c, &id, "g", true, 11)?;
        work::upsert_attention(c, &id, "g", "fixture", "Needs attention", "", 12)?;
        work::submit_task(
            c,
            &other_id,
            TaskInput {
                title: "Keep".into(),
                instructions: "Keep this task".into(),
                to: None,
                request_id: None,
            },
            "task",
            "other",
            13,
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.remove_project(&project.id).await.unwrap();
    db.call(move |c| {
        let state = work::snapshot(c, &project.id)?;
        assert!(state.tasks.is_empty() && state.events.is_empty() && state.runs.is_empty());
        assert!(
            state.timers.is_empty()
                && state.decisions.is_empty()
                && state.jobs.is_empty()
                && state.attention.is_empty()
        );
        assert_eq!(work::tasks(c, &other.id)?.len(), 1);
        assert!(
            c.prepare("PRAGMA foreign_key_check")?
                .query([])?
                .next()?
                .is_none()
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn terminal_heartbeat_cannot_reactivate_a_released_control_pin() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::default();
    let token = cfg.token.clone();
    let app = App::new(dir.path().into(), cfg).unwrap();
    let project = app.db.create(input(dir.path())).await.unwrap();
    app.db
        .save_execution(&project.id, json!({"terminal_id":"t","shell_pid":1}), "g")
        .await
        .unwrap();
    let id = project.id.clone();
    app.db
        .call(move |c| {
            work::set_takeover(c, &id, "g", false, 0)?;
            Ok(())
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/projects/{}/takeover",
        listener.local_addr().unwrap(),
        project.id
    );
    let server = tokio::spawn(axum::serve(listener, api::router(app.clone())).into_future());
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .bearer_auth(token)
        .json(&json!({"generation":"g","active":true,"heartbeat":true}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["active"], false);
    let id = project.id;
    assert!(
        !app.db
            .call(move |c| work::has_takeover(c, &id, "g"))
            .await
            .unwrap()
    );
    server.abort();
}

#[tokio::test]
async fn message_ack_receives_the_linked_run_without_completing_it() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(dir.path().into(), Config::default()).unwrap();
    let project = app.db.create(input(dir.path())).await.unwrap();
    app.db
        .save_execution(
            &project.id,
            json!({"terminal_id":"terminal","shell_pid":1}),
            "generation-one",
        )
        .await
        .unwrap();
    let id = project.id.clone();
    let run = app.db.call(move |c| {
        work::begin_execution(c, &id, "generation-one", 0)?;
        c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('receipt-fixture',?,'in','im_receipt','Hello','queued',1,1)", [&id])?;
        work::sync_inbound(c)?;
        Ok(work::claim_next(c, &id, "generation-one", 2)?.unwrap())
    }).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/projects/{}/ack",
        listener.local_addr().unwrap(),
        project.id
    );
    let server = tokio::spawn(axum::serve(listener, api::router(app.clone())).into_future());
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .bearer_auth(&project.mcp_token)
        .header("x-hfb-generation", "generation-one")
        .json(&json!({"message_id":"im_receipt"}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert!(status.is_success(), "{}", response.text().await.unwrap());
    let id = project.id;
    app.db
        .call(move |c| {
            let stored = work::run(c, &id, &run.id)?;
            assert_eq!(stored.status, "running");
            assert!(stored.finished_at.is_none());
            assert_eq!(work::event(c, &id, &run.event_id)?.status, "acknowledged");
            assert_eq!(work::task(c, &id, &run.task_id)?.status, "running");
            Ok(())
        })
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn compatibility_recovery_requires_reviewed_children_and_keeps_other_attention() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(dir.path().into(), Config::default()).unwrap();
    let project = app.db.create(input(dir.path())).await.unwrap();
    let id = project.id.clone();
    let (run, job) = app.db.call(move|c| {
        work::begin_execution(c,&id,"interrupted",0)?;
        for (message,source,status) in [("recovery-one","im_one","queued"),("recovery-two","im_two","uncertain")] {
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES(?,?,'in',?,'Review effects',?,1,1)",rusqlite::params![message,id,source,status])?;
        }
        work::sync_inbound(c)?;
        // Resolve the second uncertainty temporarily so the first run can be claimed.
        c.execute("UPDATE work_events SET status='dismissed' WHERE id='recovery-two'",[])?;
        let run=work::claim_next(c,&id,"interrupted",2)?.unwrap();
        let job=work::register_job(c,&id,"interrupted",&run.id,"Interrupted child",3)?;
        work::finish_execution(c,&id,"interrupted","exited","Lost process",4)?;
        c.execute("UPDATE work_events SET status='uncertain' WHERE id='recovery-two'",[])?;
        work::upsert_attention(c,&id,"interrupted","execution_lost","Review both results","",5)?;
        Ok((run,job))
    }).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api", listener.local_addr().unwrap());
    let server = tokio::spawn(axum::serve(listener, api::router(app.clone())).into_future());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let token = app.config.read().await.token.clone();
    let response = client
        .post(format!("{base}/messages/recovery-one/retry"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    let response = client
        .post(format!("{base}/jobs/{}/abandon", job.id))
        .bearer_auth(&token)
        .json(&json!({"reason":"Reviewed the child's partial output; discard it"}))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    let response = client
        .post(format!("{base}/messages/recovery-one/retry"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    let id = project.id.clone();
    app.db
        .call(move |c| {
            assert_eq!(work::event(c, &id, &run.event_id)?.status, "queued");
            assert_eq!(work::run(c, &id, &run.id)?.status, "interrupted");
            assert_eq!(
                work::snapshot(c, &id)?
                    .attention
                    .iter()
                    .filter(|a| a.status == "open")
                    .count(),
                1
            );
            assert!(
                work::complete_job(c, &id, "interrupted", &job.id, "Late success", 10).is_err()
            );
            Ok(())
        })
        .await
        .unwrap();
    let response = client
        .post(format!("{base}/messages/recovery-two/dismiss"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    let id = project.id;
    app.db
        .call(move |c| {
            let state = work::snapshot(c, &id)?;
            assert!(state.attention.iter().all(|a| a.status == "resolved"));
            assert_eq!(state.jobs[0].status, "abandoned");
            assert!(state.events.iter().all(|e| e.source != "child"));
            Ok(())
        })
        .await
        .unwrap();
    server.abort();
}
