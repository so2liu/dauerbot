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
fn submit(c: &Connection, source: &str, now: i64) -> Event {
    c.execute("INSERT OR IGNORE INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at)
        VALUES(?,'p','in',?,'User direction','queued',?,?)",params![source,source,now,now]).unwrap();
    sync_inbound(c).unwrap();
    event(c, "p", source).unwrap()
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
    delivery(c, &run.id, "submitted", "", now).unwrap();
    observe_idle(c, "p", &run.generation, None, now).unwrap();
}

#[test]
fn native_idle_settles_submission_only_after_a_new_transition_and_never_resolves_uncertainty() {
    let c = store();
    begin_execution(&c, "p", "g", 0).unwrap();
    submit(&c, "original", 1);
    let original = claim_steering(&c, "p", "g", 2).unwrap().unwrap();
    submission_sequence(&c, &original.id, Some(10)).unwrap();
    delivery(&c, &original.id, "submitted", "", 3).unwrap();
    observe_idle(&c, "p", "g", Some(10), 4).unwrap();
    assert_eq!(run(&c, "p", &original.id).unwrap().status, "submitted");

    // A user steer is accepted while earlier delivery has not settled.
    submit(&c, "steer", 5);
    let steer = claim_steering(&c, "p", "g", 6).unwrap().unwrap();
    delivery(
        &c,
        &steer.id,
        "uncertain",
        "Lost response after writing input",
        7,
    )
    .unwrap();
    observe_idle(&c, "p", "g", Some(12), 8).unwrap();
    assert_eq!(run(&c, "p", &original.id).unwrap().status, "settled");
    assert_eq!(event(&c, "p", "original").unwrap().status, "delivered");
    assert_eq!(event(&c, "p", "steer").unwrap().status, "uncertain");
    sync_inbound(&c).unwrap();
    assert_eq!(event(&c, "p", "original").unwrap().status, "delivered");
    assert!(claim_steering(&c, "p", "g", 9).unwrap().is_none());

    // An old uncertain input neither pins the CLI nor blocks an independent timer.
    assert!(!execution_idle(&c, "p", "g", 10).unwrap());
    assert!(execution_idle(&c, "p", "g", 10 + WARM_MS).unwrap());
    create_timer(&c, "p", interval(1), 10 + WARM_MS).unwrap();
    assert_eq!(fire_due(&c, 1010 + WARM_MS).unwrap().len(), 1);
    assert_eq!(
        claim_next(&c, "p", "g", 1011 + WARM_MS)
            .unwrap()
            .unwrap()
            .event_id,
        events(&c, "p")
            .unwrap()
            .into_iter()
            .find(|e| e.source == "timer")
            .unwrap()
            .id
    );
}
#[test]
fn an_interrupted_old_execution_does_not_pin_new_work_or_its_idle_retirement() {
    let c = store();
    begin_execution(&c, "p", "old", 0).unwrap();
    let old_event = submit(&c, "interrupted", 1);
    let old = claim_next(&c, "p", "old", 2).unwrap().unwrap();
    finish_execution(&c, "p", "old", "stopped", "Stopped", 4).unwrap();
    begin_execution(&c, "p", "new", 5).unwrap();
    let fresh_event = submit(&c, "independent-timer", 6);
    let fresh = claim_next(&c, "p", "new", 7).unwrap().unwrap();
    assert_eq!(fresh.event_id, fresh_event.id);
    finish(&c, &fresh, 9);
    assert!(active_execution_run(&c, "p", "new").unwrap().is_none());
    assert!(!execution_idle(&c, "p", "new", 10).unwrap());
    assert!(execution_idle(&c, "p", "new", 10 + WARM_MS).unwrap());
    assert_eq!(run(&c, "p", &old.id).unwrap().status, "uncertain");
    assert_eq!(event_status(&c, &old_event.id), "uncertain");
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
    assert_eq!(event(&c, "p", "m1").unwrap().status, "uncertain");
    assert_eq!(event(&c, "p", "m3").unwrap().status, "acknowledged");
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
fn timer_pending_gate_clears_on_native_idle_with_or_without_transition_metadata() {
    for sequence in [Some(12), None] {
        let c = store();
        create_timer(&c, "p", interval(10), 0).unwrap();
        let event = fire_due(&c, 10_000).unwrap().remove(0);
        let run = claim_next(&c, "p", "g", 10_001).unwrap().unwrap();
        submission_sequence(&c, &run.id, Some(10)).unwrap();
        delivery(&c, &run.id, "submitted", "", 10_002).unwrap();
        observe_idle(&c, "p", "g", Some(10), 10_003).unwrap();
        assert_eq!(event_status(&c, &event.id), "submitted");
        observe_idle(&c, "p", "g", sequence, 12_003).unwrap();
        assert_eq!(event_status(&c, &event.id), "delivered");
        assert_eq!(fire_due(&c, 20_000).unwrap().len(), 1);
        assert!(claim_steering(&c, "p", "g", 20_001).unwrap().is_none());
    }
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
fn interrupted_work_requires_explicit_resolution_and_is_never_blindly_replayed() {
    let c = store();
    begin_execution(&c, "p", "g", 0).unwrap();
    let event = submit(&c, "interrupted", 1);
    let run = claim_next(&c, "p", "g", 2).unwrap().unwrap();
    finish_execution(&c, "p", "g", "exited", "Process disappeared", 4).unwrap();
    assert_eq!(active_run(&c, "p").unwrap().unwrap().status, "uncertain");
    assert!(claim_next(&c, "p", "replacement", 5).unwrap().is_none());
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
fn schedule_request_ids_deduplicate_across_network_retries() {
    let c = store();
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
