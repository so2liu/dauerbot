use herdr_feishu_bridge::work::*;
use rusqlite::{Connection, params};
use serde_json::json;

fn store() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE projects(id TEXT PRIMARY KEY,agent TEXT NOT NULL,terminal TEXT,native_session TEXT NOT NULL DEFAULT '');
         CREATE TABLE messages(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,direction TEXT NOT NULL,source_id TEXT NOT NULL,
           content TEXT NOT NULL,status TEXT NOT NULL,attachment TEXT NOT NULL DEFAULT '{}',error TEXT NOT NULL DEFAULT '',
           attempts INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
         INSERT INTO projects(id,agent,terminal,native_session) VALUES('p','codex','t1','native-one'),('other','pi',NULL,'');",
    )
    .unwrap();
    migrate(&c).unwrap();
    c
}
fn input(instructions: &str) -> TaskInput {
    TaskInput {
        title: "Work".into(),
        instructions: instructions.into(),
        to: Some("ou_owner".into()),
        request_id: None,
    }
}
fn submit(c: &Connection, source: &str, now: i64) -> Event {
    submit_task(c, "p", input("Remember the task"), "task", source, now).unwrap()
}
fn interval(seconds: i64) -> TimerInput {
    TimerInput {
        name: "Periodic check".into(),
        prompt: "Inspect the project and report only changes".into(),
        kind: "interval".into(),
        at: None,
        interval_seconds: Some(seconds),
        cron: None,
        timezone: "Asia/Shanghai".into(),
        to: Some("ou_owner".into()),
        request_id: None,
    }
}
fn finish(c: &Connection, run: &Run, now: i64) {
    finish_run(c, "p", &run.generation, &run.id, "completed", "Done", now).unwrap();
}

#[test]
fn conversation_messages_steer_unfinished_work_without_replaying_uncertain_input() {
    let c = store();
    begin_execution(&c, "p", "g", 0).unwrap();
    submit(&c, "long-task", 1);
    let original = claim_next(&c, "p", "g", 2).unwrap().unwrap();
    delivery(&c, &original.id, "submitted", "", 3).unwrap();
    let scheduled = submit(&c, "later-task", 4);
    for id in ["correction", "stop", "clarification"] {
        c.execute(
            "INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at)
             VALUES(?,'p','in',?,'Latest user direction','queued',5,5)",
            params![id, id],
        ).unwrap();
    }
    sync_inbound(&c).unwrap();
    let first = claim_steering(&c, "p", "g", 6).unwrap().unwrap();
    assert_eq!(first.event_id, "correction");
    delivery(
        &c,
        &first.id,
        "uncertain",
        "Connection closed after write",
        7,
    )
    .unwrap();
    let second = claim_steering(&c, "p", "g", 8).unwrap().unwrap();
    assert_eq!(second.event_id, "stop");
    delivery(&c, &second.id, "submitted", "", 9).unwrap();
    let third = claim_next(&c, "p", "g", 10).unwrap().unwrap();
    assert_eq!(third.event_id, "clarification");
    ack_event(&c, "p", "g", &third.event_id, 11).unwrap();
    finish(&c, &third, 12);
    assert!(claim_steering(&c, "p", "g", 13).unwrap().is_none());
    assert!(claim_next(&c, "p", "g", 13).unwrap().is_none());
    assert_eq!(event_status(&c, &first.event_id), "uncertain");
    assert_eq!(event_status(&c, &scheduled.id), "queued");
    assert_eq!(run(&c, "p", &original.id).unwrap().status, "submitted");
    assert!(!execution_idle(&c, "p", "g", WARM_MS * 10).unwrap());

    c.execute(
        "INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at)
         VALUES('takeover','p','in','takeover','Keep the draft','queued',14,14)", [],
    ).unwrap();
    sync_inbound(&c).unwrap();
    set_takeover(&c, "p", "g", true, 15).unwrap();
    assert!(claim_steering(&c, "p", "g", 16).unwrap().is_none());
    set_takeover(&c, "p", "g", false, 17).unwrap();
    assert_eq!(
        claim_steering(&c, "p", "g", 18).unwrap().unwrap().event_id,
        "takeover"
    );
}

#[test]
fn a_large_backlog_cannot_hide_the_event_that_needs_recovery() {
    let c = store();
    let blocked = submit(&c, "blocked", 1);
    let run = claim_next(&c, "p", "g", 2).unwrap().unwrap();
    delivery(&c, &run.id, "uncertain", "Socket closed after input", 3).unwrap();
    for index in 0..205 {
        submit(&c, &format!("queued-{index}"), 10 + index);
    }
    let state = snapshot(&c, "p").unwrap();
    assert_eq!(state.events.len(), 200);
    assert_eq!(state.events[0].id, blocked.id);
    assert_eq!(state.tasks[0].id, blocked.task_id);
    assert_eq!(state.events[0].status, "uncertain");
}

#[test]
fn migration_preserves_receipt_and_uncertainty_without_replaying_old_work() {
    let c = store();
    for (index, status) in [
        "queued",
        "submitting",
        "submitted",
        "acknowledged",
        "uncertain",
        "failed",
        "dismissed",
    ]
    .iter()
    .enumerate()
    {
        c.execute(
            "INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at)
             VALUES(?,'p','in',?,'Original content',?,1,2)",
            params![format!("m{index}"), format!("im_{index}"), status],
        )
        .unwrap();
    }
    assert_eq!(sync_inbound(&c).unwrap(), 7);
    assert_eq!(sync_inbound(&c).unwrap(), 0);
    assert_eq!(tasks(&c, "p").unwrap().len(), 7);
    assert_eq!(event(&c, "p", "m1").unwrap().status, "uncertain");
    assert_eq!(event(&c, "p", "m3").unwrap().status, "acknowledged");
    assert_eq!(task(&c, "p", "m3").unwrap().status, "imported");
    assert!(active_run(&c, "p").unwrap().is_none());
    // An uncertain older input is never replayed, but must not block a new message.
    let fresh = claim_steering(&c, "p", "g", 5).unwrap().unwrap();
    assert_eq!(fresh.event_id, "m0");
    delivery(&c, &fresh.id, "submitted", "", 6).unwrap();
    let original: String = c
        .query_row("SELECT status FROM messages WHERE id='m1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(original, "submitting");

    c.execute(
        "UPDATE messages SET status='queued',updated_at=10 WHERE id IN ('m1','m2','m4')",
        [],
    )
    .unwrap();
    sync_inbound(&c).unwrap();
    assert_eq!(event(&c, "p", "m1").unwrap().status, "queued");
    resolve_event(&c, "p", "m5", false, 10).unwrap();
    assert!(claim_next(&c, "p", "g", 11).unwrap().is_some());
}

#[test]
fn receipt_and_late_delivery_do_not_complete_or_regress_a_run() {
    let c = store();
    begin_execution(&c, "p", "g", 0).unwrap();
    let event = submit(&c, "first", 1);
    let run = claim_next(&c, "p", "g", 2).unwrap().unwrap();
    ack_event(&c, "p", "g", &event.id, 3).unwrap();
    delivery(&c, &run.id, "submitted", "", 4).unwrap();
    assert_eq!(active_run(&c, "p").unwrap().unwrap().status, "running");
    assert!(!execution_idle(&c, "p", "g", WARM_MS * 10).unwrap());
    assert!(finish_run(&c, "p", "replacement", &run.id, "completed", "Done", 5).is_err());
    finish(&c, &run, 6);
    delivery(&c, &run.id, "uncertain", "A late send timeout", 7).unwrap();
    assert_eq!(
        herdr_feishu_bridge::work::run(&c, "p", &run.id)
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(event_status(&c, &event.id), "completed");
    assert!(!execution_idle(&c, "p", "g", 8).unwrap());
    assert!(!execution_idle(&c, "p", "g", 8 + WARM_MS - 1).unwrap());
    assert!(execution_idle(&c, "p", "g", 8 + WARM_MS).unwrap());
    submit(&c, "racing-message", 8 + WARM_MS);
    assert!(!execution_idle(&c, "p", "g", 8 + WARM_MS).unwrap());
}

#[test]
fn legacy_message_mirror_cannot_requeue_submitted_or_completed_effects() {
    let c = store();
    c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at)
      VALUES('message','p','in','im_one','Perform once','queued',1,1)",[]).unwrap();
    sync_inbound(&c).unwrap();
    let run = claim_next(&c, "p", "g", 2).unwrap().unwrap();
    sync_inbound(&c).unwrap();
    assert_eq!(event_status(&c, "message"), "submitting");
    delivery(&c, &run.id, "submitted", "", 3).unwrap();
    sync_inbound(&c).unwrap();
    assert_eq!(event_status(&c, "message"), "submitted");
    ack_event(&c, "p", "g", "message", 4).unwrap();
    delivery(&c, &run.id, "uncertain", "late timeout", 5).unwrap();
    sync_inbound(&c).unwrap();
    assert_eq!(active_run(&c, "p").unwrap().unwrap().status, "running");
    assert_eq!(event_status(&c, "message"), "acknowledged");
    finish_run(
        &c,
        "p",
        "g",
        &run.id,
        "failed",
        "Verified provider failure",
        6,
    )
    .unwrap();
    sync_inbound(&c).unwrap();
    assert_eq!(event_status(&c, "message"), "failed");
    assert!(active_run(&c, "p").unwrap().is_none());
    assert!(claim_next(&c, "p", "g", 7).unwrap().is_none());
    let status: String = c
        .query_row("SELECT status FROM messages WHERE id='message'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(status, "acknowledged");
    submit(&c, "next-independent-task", 8);
    assert!(claim_next(&c, "p", "g", 9).unwrap().is_some());
}
fn event_status(c: &Connection, id: &str) -> String {
    event(c, "p", id).unwrap().status
}

#[test]
fn takeover_is_a_persistent_pin_and_release_restarts_the_warm_period() {
    let c = store();
    begin_execution(&c, "p", "g", 0).unwrap();
    set_takeover(&c, "p", "g", true, 1).unwrap();
    assert!(has_takeover(&c, "p", "g").unwrap());
    assert!(!execution_idle(&c, "p", "g", WARM_MS * 100).unwrap());
    submit(&c, "human-is-typing", 2);
    assert!(claim_next(&c, "p", "g", WARM_MS * 100).unwrap().is_none());
    set_takeover(&c, "p", "g", false, WARM_MS * 100).unwrap();
    let run = claim_next(&c, "p", "g", WARM_MS * 100).unwrap().unwrap();
    finish(&c, &run, WARM_MS * 100 + 1);
    assert!(!execution_idle(&c, "p", "g", WARM_MS * 100 + 2).unwrap());
    assert!(execution_idle(&c, "p", "g", WARM_MS * 101 + 2).unwrap());
}

#[test]
fn interval_downtime_coalesces_and_pending_occurrences_do_not_overlap() {
    let c = store();
    let timer = create_timer(&c, "p", interval(60), 0).unwrap();
    let fired = fire_due(&c, 600_000).unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].source, "timer");
    assert_eq!(fired[0].payload["scheduled_at"], 60_000);
    assert_eq!(fired[0].payload["to"], "ou_owner");
    assert_eq!(
        herdr_feishu_bridge::work::timer(&c, "p", &timer.id)
            .unwrap()
            .next_fire,
        Some(660_000)
    );
    assert!(fire_due(&c, 600_000).unwrap().is_empty());
    assert!(fire_due(&c, 660_000).unwrap().is_empty());
    let run = claim_next(&c, "p", "g", 660_001).unwrap().unwrap();
    finish(&c, &run, 660_002);
    assert_eq!(fire_due(&c, 720_000).unwrap().len(), 1);
    let count: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM work_occurrences WHERE timer_id=?",
            [&timer.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn editing_a_timer_cancels_only_unsubmitted_occurrences() {
    let c = store();
    let timer = create_timer(&c, "p", interval(1), 0).unwrap();
    let first = fire_due(&c, 1_000).unwrap().remove(0);
    let edited = update_timer(&c, "p", &timer.id, interval(2), 1_001).unwrap();
    assert_eq!(edited.revision, 2);
    assert_eq!(event_status(&c, &first.id), "cancelled");
    let second = fire_due(&c, 3_001).unwrap().remove(0);
    let run = claim_next(&c, "p", "g", 3_002).unwrap().unwrap();
    delete_timer(&c, "p", &timer.id, 3_003).unwrap();
    assert_eq!(event_status(&c, &second.id), "submitting");
    assert!(fire_due(&c, i64::MAX / 2).unwrap().is_empty());
    finish(&c, &run, 3_004);
}

#[test]
fn one_shot_survives_downtime_and_cron_respects_iana_timezone() {
    let c = store();
    let mut once = interval(60);
    once.kind = "once".into();
    once.at = Some(json!("2026-11-16T12:00:00+08:00"));
    let created = create_timer(&c, "p", once, 0).unwrap();
    assert_eq!(created.next_fire, Some(1_794_801_600_000));
    assert!(
        fire_due(&c, created.next_fire.unwrap() - 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fire_due(&c, created.next_fire.unwrap() + 86_400_000)
            .unwrap()
            .len(),
        1
    );
    assert!(
        !herdr_feishu_bridge::work::timer(&c, "p", &created.id)
            .unwrap()
            .enabled
    );

    let mut cron = interval(60);
    cron.kind = "cron".into();
    cron.cron = Some("30 9 * * *".into());
    cron.timezone = "Asia/Shanghai".into();
    let midnight = chrono::DateTime::parse_from_rfc3339("2026-11-16T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    let timer = create_timer(&c, "p", cron.clone(), midnight).unwrap();
    let expected = chrono::DateTime::parse_from_rfc3339("2026-11-16T01:30:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(timer.next_fire, Some(expected));
    cron.timezone = "Not/AZone".into();
    assert!(create_timer(&c, "p", cron, midnight).is_err());
}

#[test]
fn five_field_cron_uses_unix_weekdays_day_union_and_dst_gaps() {
    let c = store();
    let mut input = interval(60);
    input.kind = "cron".into();
    input.timezone = "UTC".into();
    input.cron = Some("0 9 * * 1-5".into());
    let saturday = chrono::DateTime::parse_from_rfc3339("2026-11-14T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    let monday = chrono::DateTime::parse_from_rfc3339("2026-11-16T09:00:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        create_timer(&c, "p", input.clone(), saturday)
            .unwrap()
            .next_fire,
        Some(monday)
    );
    input.cron = Some("0 9 1 * 1".into());
    assert_eq!(
        create_timer(&c, "p", input.clone(), saturday)
            .unwrap()
            .next_fire,
        Some(monday)
    );
    input.cron = Some("0 9 * * 0".into());
    let sunday = chrono::DateTime::parse_from_rfc3339("2026-11-15T09:00:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        create_timer(&c, "p", input.clone(), saturday)
            .unwrap()
            .next_fire,
        Some(sunday)
    );
    input.cron = Some("30 2 * * *".into());
    input.timezone = "America/New_York".into();
    let before_gap = chrono::DateTime::parse_from_rfc3339("2026-03-08T00:00:00-05:00")
        .unwrap()
        .timestamp_millis();
    let after_gap = chrono::DateTime::parse_from_rfc3339("2026-03-09T02:30:00-04:00")
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        create_timer(&c, "p", input, before_gap).unwrap().next_fire,
        Some(after_gap)
    );
}

#[test]
fn decision_answer_and_child_result_are_durable_wakeup_events() {
    let c = store();
    begin_execution(&c, "p", "g", 0).unwrap();
    let original = submit(&c, "decision-task", 1);
    let run = claim_next(&c, "p", "g", 2).unwrap().unwrap();
    let decision = request_decision(
        &c,
        "p",
        "g",
        &run.id,
        "Which option?",
        "ou_owner",
        json!(["A", "B"]),
        3,
    )
    .unwrap();
    finish_run(
        &c,
        "p",
        "g",
        &run.id,
        "deferred",
        "Waiting for an answer",
        4,
    )
    .unwrap();
    finish_execution(&c, "p", "g", "sleeping", "", 5).unwrap();
    let answer = answer_decision(&c, "p", &decision.id, "B", 6).unwrap();
    assert_eq!(answer.source, "decision");
    assert_eq!(answer.task_id, original.task_id);
    assert_eq!(
        answer_decision(&c, "p", &decision.id, "B", 7).unwrap().id,
        answer.id
    );
    assert!(answer_decision(&c, "p", &decision.id, "A", 7).is_err());

    begin_execution(&c, "p", "g2", 8).unwrap();
    let resumed = claim_next(&c, "p", "g2", 9).unwrap().unwrap();
    let child = register_job(&c, "p", "g2", &resumed.id, "Research", 10).unwrap();
    assert!(finish_run(&c, "p", "g2", &resumed.id, "completed", "Done", 11).is_err());
    assert!(!execution_idle(&c, "p", "g2", WARM_MS * 2).unwrap());
    let result = complete_job(&c, "p", "g2", &child.id, "Found the answer", 12).unwrap();
    assert_eq!(result.source, "child");
    assert_eq!(
        complete_job(&c, "p", "g2", &child.id, "Found the answer", 13)
            .unwrap()
            .id,
        result.id
    );
    finish(&c, &resumed, 13);
    assert!(!execution_idle(&c, "p", "g2", WARM_MS * 2).unwrap());
    let followup = claim_next(&c, "p", "g2", 14).unwrap().unwrap();
    finish(&c, &followup, 15);
}

#[test]
fn interrupted_work_requires_explicit_resolution_and_is_never_blindly_replayed() {
    let c = store();
    begin_execution(&c, "p", "g", 0).unwrap();
    let event = submit(&c, "interrupted", 1);
    let run = claim_next(&c, "p", "g", 2).unwrap().unwrap();
    ack_event(&c, "p", "g", &event.id, 3).unwrap();
    finish_execution(&c, "p", "g", "exited", "Process disappeared", 4).unwrap();
    assert_eq!(active_run(&c, "p").unwrap().unwrap().status, "uncertain");
    assert!(claim_next(&c, "p", "replacement", 5).unwrap().is_none());
    assert!(cancel_task(&c, "p", &event.task_id, 6).is_err());
    resolve_event(&c, "p", &event.id, true, 7).unwrap();
    let retry = claim_next(&c, "p", "replacement", 8).unwrap().unwrap();
    assert_ne!(retry.id, run.id);
    assert_eq!(retry.event_id, event.id);
    assert_eq!(
        herdr_feishu_bridge::work::run(&c, "p", &run.id)
            .unwrap()
            .status,
        "interrupted"
    );
}

#[test]
fn event_identity_and_attention_dedup_do_not_hide_conflicting_work() {
    let c = store();
    let first = submit(&c, "idempotent", 0);
    assert_eq!(submit(&c, "idempotent", 1).id, first.id);
    assert!(submit_task(&c, "p", input("Different effects"), "task", "idempotent", 2).is_err());
    assert_eq!(tasks(&c, "p").unwrap().len(), 1);
    let one =
        upsert_attention(&c, "p", "g", "login", "Sign in to continue", "ou_owner", 3).unwrap();
    let two = upsert_attention(&c, "p", "g", "login", "Still waiting for sign-in", "", 4).unwrap();
    assert_eq!(one["id"], two["id"]);
    assert_eq!(one["created"], true);
    assert_eq!(two["created"], false);
    assert_eq!(two["to"], "ou_owner");
    assert_eq!(snapshot(&c, "p").unwrap().attention[0].status, "open");
    resolve_attention(&c, "p", "g", 5).unwrap();
    let again = upsert_attention(&c, "p", "g", "login", "Sign in again", "ou_owner", 6).unwrap();
    assert_ne!(again["id"], one["id"]);
}

#[test]
fn mcp_request_ids_deduplicate_tasks_and_schedules_across_network_retries() {
    let c = store();
    let mut task_input = input("One deliberate task");
    task_input.request_id = Some("tool-request".into());
    let one = submit_task(&c, "p", task_input.clone(), "task", "random-one", 0).unwrap();
    let two = submit_task(&c, "p", task_input, "task", "random-two", 1).unwrap();
    assert_eq!(one.id, two.id);
    let mut scheduled = interval(30);
    scheduled.request_id = Some("schedule-request".into());
    let one = create_timer(&c, "p", scheduled.clone(), 0).unwrap();
    let two = create_timer(&c, "p", scheduled.clone(), 9_000).unwrap();
    assert_eq!(one.id, two.id);
    assert_eq!(one.next_fire, two.next_fire);
    scheduled.prompt = "Different effects".into();
    assert!(create_timer(&c, "p", scheduled, 10_000).is_err());
    assert_eq!(snapshot(&c, "p").unwrap().timers.len(), 1);
}

#[test]
fn deleting_a_project_cascades_durable_work_without_affecting_other_projects() {
    let c = store();
    let event = submit(&c, "first", 1);
    create_timer(&c, "p", interval(5), 0).unwrap();
    claim_next(&c, "p", "g", 2).unwrap().unwrap();
    submit_task(&c, "other", input("Keep me"), "task", "other", 3).unwrap();
    c.execute("DELETE FROM projects WHERE id='p'", []).unwrap();
    assert!(task(&c, "p", &event.task_id).is_err());
    assert!(events(&c, "p").unwrap().is_empty());
    assert_eq!(tasks(&c, "other").unwrap().len(), 1);
}
