//! Shared, read-only eligibility checks for reversible native child-thread archival.
//!
//! A native spawn edge marked `open` is not proof of live work; conversely an
//! observer's `notLoaded` is not proof of absence. The caller supplies a verified
//! cross-process writer observation and invokes only the native archive RPC.
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriterObservation {
    Live,
    Absent,
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct BridgeEvidence {
    pub protected_root: bool,
    pub active_execution: bool,
    pub pending: bool,
    pub uncertain: bool,
    pub schedule_reference: bool,
    pub manager_reference: bool,
    pub management_blocked: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Evidence {
    pub metadata_verified: bool,
    pub writer: WriterObservation,
    pub native_pending: u64,
    pub updated_at_ms: Option<i64>,
    pub bridge: BridgeEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetainReason {
    UnverifiedMetadata,
    ProtectedRoot,
    LiveWriter,
    UnknownWriter,
    ActiveExecution,
    PendingInput,
    UncertainEffect,
    ScheduleReference,
    ManagerReference,
    ManagementBlocked,
    ActivityUnknownOrRecent,
    IncompleteSubtree,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", content = "reasons", rename_all = "snake_case")]
pub(crate) enum Decision {
    Archive,
    Retain(Vec<RetainReason>),
}

pub(crate) fn decision(e: &Evidence, now_ms: i64) -> Decision {
    let mut reasons = Vec::new();
    if !e.metadata_verified {
        reasons.push(RetainReason::UnverifiedMetadata);
    }
    if e.bridge.protected_root {
        reasons.push(RetainReason::ProtectedRoot);
    }
    match e.writer {
        WriterObservation::Live => reasons.push(RetainReason::LiveWriter),
        WriterObservation::Unknown => reasons.push(RetainReason::UnknownWriter),
        WriterObservation::Absent => {}
    }
    if e.bridge.active_execution {
        reasons.push(RetainReason::ActiveExecution);
    }
    if e.native_pending != 0 || e.bridge.pending {
        reasons.push(RetainReason::PendingInput);
    }
    if e.bridge.uncertain {
        reasons.push(RetainReason::UncertainEffect);
    }
    if e.bridge.schedule_reference {
        reasons.push(RetainReason::ScheduleReference);
    }
    if e.bridge.manager_reference {
        reasons.push(RetainReason::ManagerReference);
    }
    if e.bridge.management_blocked {
        reasons.push(RetainReason::ManagementBlocked);
    }
    if !e.updated_at_ms.is_some_and(|updated| {
        updated > 0
            && now_ms
                .checked_sub(updated)
                .is_some_and(|age| age >= crate::work::WARM_MS)
    }) {
        reasons.push(RetainReason::ActivityUnknownOrRecent);
    }
    if reasons.is_empty() {
        Decision::Archive
    } else {
        Decision::Retain(reasons)
    }
}

/// Archival is recursive in Codex: one protected descendant vetoes the entire
/// operation. An incomplete bounded scan must never authorize a partial tree.
pub(crate) fn subtree_decision(members: &[Evidence], complete: bool, now_ms: i64) -> Decision {
    if !complete || members.is_empty() {
        return Decision::Retain(vec![RetainReason::IncompleteSubtree]);
    }
    let mut reasons = Vec::new();
    for member in members {
        if let Decision::Retain(found) = decision(member, now_ms) {
            for reason in found {
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
            }
        }
    }
    if reasons.is_empty() {
        Decision::Archive
    } else {
        Decision::Retain(reasons)
    }
}

/// All current and historical product main sessions remain protected, including
/// roots belonging to another project. No native record is adopted by this scan.
pub(crate) fn protected_roots(c: &Connection) -> Result<Vec<String>> {
    Ok(c.prepare(
        "SELECT native_session FROM projects WHERE native_session!='' UNION SELECT native_session FROM work_executions WHERE native_session!=''",
    )?
    .query_map([], |r| r.get(0))?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Read only booleans. Unrelated project-level input is addressed to its root,
/// not automatically to every child. A literal thread ID or native alias in a
/// pending record conservatively protects the corresponding child. Aliases are
/// metadata supplied by the scoped native snapshot, never model-generated text.
pub(crate) fn bridge_evidence(
    c: &Connection,
    project: &str,
    thread: &str,
    aliases: &[&str],
) -> Result<BridgeEvidence> {
    uuid::Uuid::parse_str(thread)?;
    ensure!(aliases.len() <= 4, "Too many native thread aliases");
    let protected_root = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE native_session=?1) OR EXISTS(SELECT 1 FROM work_executions WHERE native_session=?1)",
        [thread],
        |r| r.get(0),
    )?;
    let active_execution = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_runs r JOIN work_executions e ON e.project_id=r.project_id AND e.generation=r.generation WHERE r.project_id=?1 AND e.native_session=?2 AND (r.finished_at IS NULL OR r.status NOT IN ('settled','completed','cancelled','interrupted','failed'))) OR EXISTS(SELECT 1 FROM work_leases l JOIN work_executions e ON e.project_id=l.project_id AND e.generation=l.generation WHERE l.project_id=?1 AND l.active=1 AND e.native_session=?2)",
        params![project, thread], |r| r.get(0),
    )?;
    let management_blocked = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects p WHERE p.id=?1 AND (p.paused=1 OR EXISTS(SELECT 1 FROM work_leases l WHERE l.project_id=p.id AND l.generation=p.generation AND l.active=1))) OR EXISTS(SELECT 1 FROM cli_switch_requests WHERE project_id=?1 AND holds_input=1)",
        [project], |r| r.get(0),
    )?;
    let mut evidence = BridgeEvidence {
        protected_root,
        active_execution,
        management_blocked,
        ..BridgeEvidence::default()
    };
    let mut references = vec![thread];
    for alias in aliases {
        ensure!(
            !alias.is_empty() && alias.len() <= 256 && !alias.chars().any(char::is_control),
            "Invalid native thread alias"
        );
        if !references.contains(alias) {
            references.push(alias);
        }
    }
    for reference in references {
        evidence.active_execution |= c.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_runs r JOIN work_events e ON e.id=r.event_id WHERE r.project_id=?1 AND (r.finished_at IS NULL OR r.status NOT IN ('settled','completed','cancelled','interrupted','failed')) AND instr(e.payload,?2)>0)",
            params![project, reference], |r| r.get::<_, bool>(0),
        )?;
        evidence.pending |= c.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_inputs WHERE project_id=?1 AND status IN ('queued','processing') AND instr(payload,?2)>0) OR EXISTS(SELECT 1 FROM work_events WHERE project_id=?1 AND status IN ('queued','submitting','submitted') AND instr(payload,?2)>0) OR EXISTS(SELECT 1 FROM messages WHERE project_id=?1 AND ((direction='in' AND status='queued') OR (direction='out' AND status IN ('queued','pending','sending','submitting'))) AND (instr(content,?2)>0 OR instr(attachment,?2)>0)) OR EXISTS(SELECT 1 FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id WHERE r.project_id=?1 AND e.status IN ('pending','submitting','checking') AND instr(e.arguments,?2)>0)",
            params![project, reference], |r| r.get::<_, bool>(0),
        )?;
        evidence.uncertain |= c.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_events WHERE project_id=?1 AND status='uncertain' AND instr(payload,?2)>0) OR EXISTS(SELECT 1 FROM work_runs r JOIN work_events e ON e.id=r.event_id WHERE r.project_id=?1 AND r.status='uncertain' AND instr(e.payload,?2)>0) OR EXISTS(SELECT 1 FROM messages WHERE project_id=?1 AND status='uncertain' AND (instr(content,?2)>0 OR instr(attachment,?2)>0)) OR EXISTS(SELECT 1 FROM manager_effects e JOIN manager_runs r ON r.id=e.run_id WHERE r.project_id=?1 AND e.status='uncertain' AND (instr(e.arguments,?2)>0 OR instr(e.result,?2)>0))",
            params![project, reference], |r| r.get::<_, bool>(0),
        )?;
        evidence.schedule_reference |= c.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_timers WHERE project_id=?1 AND (instr(prompt,?2)>0 OR instr(request_payload,?2)>0)) OR EXISTS(SELECT 1 FROM work_tasks WHERE project_id=?1 AND status NOT IN ('completed','cancelled','failed') AND instr(instructions,?2)>0)",
            params![project, reference], |r| r.get::<_, bool>(0),
        )?;
        // Manager credentials/config have no child-thread field. Only a live
        // checkpoint's explicit child reference can retain a child here; provider
        // endpoint/model/key values are not selected or searched.
        evidence.manager_reference |= c.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_runs WHERE project_id=?1 AND status='working' AND json_valid(checkpoint) AND (json_extract(checkpoint,'$.thread_id')=?2 OR json_extract(checkpoint,'$.target_thread_id')=?2 OR json_extract(checkpoint,'$.native_session')=?2))",
            params![project, reference], |r| r.get::<_, bool>(0),
        )?;
    }
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    const THREAD: &str = "00000000-0000-4000-8000-000000000002";
    const OTHER: &str = "00000000-0000-4000-8000-000000000003";

    fn stale() -> Evidence {
        Evidence {
            metadata_verified: true,
            writer: WriterObservation::Absent,
            native_pending: 0,
            updated_at_ms: Some(1),
            bridge: BridgeEvidence::default(),
        }
    }

    #[test]
    fn ended_subtree_archives_but_pending_live_references_and_unknown_never_do() {
        let now = crate::work::WARM_MS + 1;
        // The callback represents the only admitted native mutation; do not
        // authorize it for a partially safe recursive subtree.
        let mut archived = false;
        let execute = |members: &[Evidence], archived: &mut bool| {
            if subtree_decision(members, true, now) == Decision::Archive {
                *archived = true;
            }
        };
        execute(&[stale(), stale()], &mut archived);
        assert!(archived, "An ended, unreferenced subtree reaches archival");
        let mut blocked = Vec::new();
        for writer in [WriterObservation::Live, WriterObservation::Unknown] {
            let mut e = stale();
            e.writer = writer;
            blocked.push(e);
        }
        let mut e = stale();
        e.native_pending = 1;
        blocked.push(e);
        for mutate in [
            |e: &mut Evidence| e.bridge.pending = true,
            |e: &mut Evidence| e.bridge.uncertain = true,
            |e: &mut Evidence| e.bridge.schedule_reference = true,
            |e: &mut Evidence| e.bridge.manager_reference = true,
            |e: &mut Evidence| e.bridge.protected_root = true,
            |e: &mut Evidence| e.bridge.active_execution = true,
            |e: &mut Evidence| e.bridge.management_blocked = true,
            |e: &mut Evidence| e.metadata_verified = false,
            |e: &mut Evidence| e.updated_at_ms = None,
            |e: &mut Evidence| e.updated_at_ms = Some(i64::MAX),
        ] {
            let mut e = stale();
            mutate(&mut e);
            blocked.push(e);
        }
        for descendant in blocked {
            archived = false;
            execute(&[stale(), descendant.clone()], &mut archived);
            assert!(!archived, "Protected descendant admitted: {descendant:?}");
        }
        assert_eq!(
            subtree_decision(&[stale()], false, now),
            Decision::Retain(vec![RetainReason::IncompleteSubtree])
        );
        assert!(matches!(
            subtree_decision(&[], true, now),
            Decision::Retain(_)
        ));
        let mut recent = stale();
        recent.updated_at_ms = Some(now - crate::work::WARM_MS + 1);
        assert!(matches!(decision(&recent, now), Decision::Retain(_)));
    }

    fn fixture() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE projects(id TEXT, native_session TEXT, paused INTEGER, generation TEXT);
             CREATE TABLE work_executions(project_id TEXT,native_session TEXT,generation TEXT);
             CREATE TABLE work_runs(project_id TEXT,generation TEXT,event_id TEXT,finished_at INTEGER,status TEXT);
             CREATE TABLE work_leases(project_id TEXT,generation TEXT,active INTEGER);
             CREATE TABLE cli_switch_requests(project_id TEXT,holds_input INTEGER);
             CREATE TABLE manager_inputs(project_id TEXT,status TEXT,payload TEXT);
             CREATE TABLE work_events(id TEXT,project_id TEXT,status TEXT,payload TEXT);
             CREATE TABLE messages(project_id TEXT,direction TEXT,status TEXT,content TEXT,attachment TEXT);
             CREATE TABLE manager_runs(id TEXT,project_id TEXT,status TEXT,checkpoint TEXT);
             CREATE TABLE manager_effects(run_id TEXT,status TEXT,arguments TEXT,result TEXT);
             CREATE TABLE work_timers(project_id TEXT,prompt TEXT,request_payload TEXT);
             CREATE TABLE work_tasks(project_id TEXT,status TEXT,instructions TEXT);
             INSERT INTO projects VALUES('p','00000000-0000-4000-8000-000000000001',0,'g');",
        )
        .unwrap();
        c
    }

    #[test]
    fn scoped_bridge_guards_keep_attributed_pending_uncertainty_and_references() {
        let c = fixture();
        c.execute(
            "INSERT INTO manager_inputs VALUES('p','queued',?1)",
            [OTHER],
        )
        .unwrap();
        c.execute(
            "INSERT INTO messages VALUES('p','out','uncertain',?1,'{}')",
            [OTHER],
        )
        .unwrap();
        let clean = bridge_evidence(&c, "p", THREAD, &[]).unwrap();
        assert_eq!(
            clean,
            BridgeEvidence::default(),
            "Unrelated input belongs to its target/root, not every child"
        );
        c.execute(
            "INSERT INTO manager_inputs VALUES('p','queued',?1)",
            [THREAD],
        )
        .unwrap();
        c.execute(
            "INSERT INTO manager_runs VALUES('r','p','working',?1)",
            [format!("{{\"thread_id\":\"{THREAD}\"}}")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO manager_effects VALUES('r','uncertain',?1,'null')",
            [THREAD],
        )
        .unwrap();
        c.execute("INSERT INTO work_timers VALUES('p',?1,'{}')", [THREAD])
            .unwrap();
        let held = bridge_evidence(&c, "p", THREAD, &[]).unwrap();
        assert!(held.pending);
        assert!(held.uncertain);
        assert!(held.schedule_reference);
        assert!(held.manager_reference);
        assert!(!held.protected_root);
        c.execute(
            "INSERT INTO work_events VALUES('delivered','p','delivered',?1)",
            [THREAD],
        )
        .unwrap();
        c.execute(
            "INSERT INTO work_runs VALUES('p','g','delivered',NULL,'submitted')",
            [],
        )
        .unwrap();
        assert!(
            bridge_evidence(&c, "p", THREAD, &[])
                .unwrap()
                .active_execution,
            "An acknowledged event does not end its still-running assigned worker"
        );
        let roots = protected_roots(&c).unwrap();
        assert_eq!(roots, ["00000000-0000-4000-8000-000000000001"]);
        assert!(
            bridge_evidence(&c, "p", &roots[0], &[])
                .unwrap()
                .protected_root
        );
        c.execute(
            "INSERT INTO manager_inputs VALUES('p','queued','Use Gauss for the pending review')",
            [],
        )
        .unwrap();
        assert!(bridge_evidence(&c, "p", OTHER, &["Gauss"]).unwrap().pending);
        assert!(bridge_evidence(&c, "p", THREAD, &[""]).is_err());
        assert!(bridge_evidence(&c, "p", "not-a-thread", &[]).is_err());
    }
}
