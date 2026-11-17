//! A bounded, durable retirement handshake; no Agent acknowledgement tool.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

pub const TIMEOUT_MS: i64 = 120_000;
#[derive(Clone, Debug)]
pub struct Request {
    pub id: String,
    pub status: String,
    pub baseline: String,
    pub sequence: i64,
    pub requested_at: i64,
}

pub fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS idle_handoffs(
        id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        generation TEXT NOT NULL, status TEXT NOT NULL, baseline TEXT NOT NULL,
        native_seq INTEGER NOT NULL, requested_at INTEGER NOT NULL, error TEXT NOT NULL DEFAULT '');
        CREATE INDEX IF NOT EXISTS idle_handoff_execution ON idle_handoffs(project_id,generation,requested_at);
        CREATE TRIGGER IF NOT EXISTS handoff_inbound AFTER INSERT ON messages WHEN NEW.direction='in' BEGIN
          UPDATE idle_handoffs SET status='cancelled',error='New input' WHERE project_id=NEW.project_id AND status='requested';
        END;
        CREATE TRIGGER IF NOT EXISTS handoff_event AFTER INSERT ON work_events WHEN NEW.status='queued' BEGIN
          UPDATE idle_handoffs SET status='cancelled',error='New work' WHERE project_id=NEW.project_id AND status='requested';
        END;
        CREATE TRIGGER IF NOT EXISTS handoff_pause AFTER UPDATE OF paused ON projects WHEN NEW.paused=1 BEGIN
          UPDATE idle_handoffs SET status='cancelled',error='Paused' WHERE project_id=NEW.id AND status='requested';
        END;
        CREATE TRIGGER IF NOT EXISTS handoff_takeover AFTER INSERT ON work_leases WHEN NEW.active=1 BEGIN
          UPDATE idle_handoffs SET status='cancelled',error='Terminal takeover' WHERE project_id=NEW.project_id AND generation=NEW.generation AND status='requested';
        END;
        CREATE TRIGGER IF NOT EXISTS handoff_takeover_update AFTER UPDATE OF active ON work_leases WHEN NEW.active=1 BEGIN
          UPDATE idle_handoffs SET status='cancelled',error='Terminal takeover' WHERE project_id=NEW.project_id AND generation=NEW.generation AND status='requested';
        END;
        CREATE TRIGGER IF NOT EXISTS handoff_finish AFTER UPDATE OF status ON work_executions WHEN NEW.status IN ('stopped','exited','error') BEGIN
          UPDATE idle_handoffs SET status='cancelled',error='Execution ended' WHERE project_id=NEW.project_id AND generation=NEW.generation AND status='requested';
        END;
        UPDATE idle_handoffs SET status='failed',error='Daemon restarted during handoff; native recovery preserved' WHERE status IN ('requested','retiring');")?;
    Ok(())
}

pub fn latest(c: &Connection, project: &str, generation: &str) -> Result<Option<Request>> {
    Ok(c.query_row("SELECT id,status,baseline,native_seq,requested_at FROM idle_handoffs WHERE project_id=? AND generation=? ORDER BY rowid DESC LIMIT 1", params![project,generation], |r| Ok(Request { id:r.get(0)?,status:r.get(1)?,baseline:r.get(2)?,sequence:r.get(3)?,requested_at:r.get(4)? })).optional()?)
}

pub fn eligible(c: &Connection, project: &str, generation: &str) -> Result<bool> {
    Ok(c.query_row("SELECT NOT EXISTS(SELECT 1 FROM messages WHERE project_id=?1 AND direction='in' AND status='queued')
        AND NOT EXISTS(SELECT 1 FROM work_events WHERE project_id=?1 AND status='queued')
        AND NOT EXISTS(SELECT 1 FROM work_runs WHERE project_id=?1 AND generation=?2 AND status IN ('submitting','submitted','running','uncertain'))
        AND NOT EXISTS(SELECT 1 FROM work_leases WHERE project_id=?1 AND generation=?2 AND active=1)
        AND EXISTS(SELECT 1 FROM projects WHERE id=?1 AND generation=?2 AND paused=0)",params![project,generation], |r| r.get(0))?)
}

pub fn begin(
    c: &Connection,
    project: &str,
    generation: &str,
    baseline: &str,
    sequence: i64,
    now: i64,
) -> Result<Option<Request>> {
    let tx = c.unchecked_transaction()?;
    if !eligible(&tx, project, generation)? {
        return Ok(None);
    }
    if let Some(last) = latest(&tx, project, generation)? {
        // Failures never retry automatically. A cancelled handshake may try again
        // after another full idle period, with an absolute per-generation budget.
        if last.status != "cancelled" {
            return Ok(None);
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM idle_handoffs WHERE generation=?",
            [generation],
            |r| r.get(0),
        )?;
        if count >= 3 || now - last.requested_at < crate::work::WARM_MS {
            return Ok(None);
        }
    }
    let request = Request {
        id: crate::db::id(),
        status: "requested".into(),
        baseline: baseline.into(),
        sequence,
        requested_at: now,
    };
    tx.execute("INSERT INTO idle_handoffs(id,project_id,generation,status,baseline,native_seq,requested_at) VALUES(?,?,?,'requested',?,?,?)",params![request.id,project,generation,baseline,sequence,now])?;
    tx.commit()?;
    Ok(Some(request))
}

pub fn fail(c: &Connection, id: &str, error: &str) -> Result<()> {
    c.execute("UPDATE idle_handoffs SET status='failed',error=? WHERE id=? AND status IN ('requested','retiring')",params![error,id])?;
    Ok(())
}

/// This transaction is the daemon input retirement boundary; the caller must also
/// hold the native managed-child dispatch gate. Later input stays queued for wakeup.
/// Until stop succeeds, keep the old native session available for crash recovery.
pub fn claim(c: &Connection, project: &str, generation: &str, id: &str) -> Result<bool> {
    let tx = c.unchecked_transaction()?;
    if !eligible(&tx, project, generation)? {
        return Ok(false);
    }
    let changed = tx.execute("UPDATE idle_handoffs SET status='retiring' WHERE id=? AND project_id=? AND generation=? AND status='requested'",params![id,project,generation])?;
    tx.commit()?;
    Ok(changed == 1)
}

pub fn complete(c: &Connection, project: &str, generation: &str, id: &str, now: i64) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    ensure!(
        tx.execute(
            "UPDATE idle_handoffs SET status='retired' WHERE id=? AND status='retiring'",
            [id]
        )? == 1,
        "Handoff retirement changed"
    );
    crate::work::finish_execution(&tx, project, generation, "sleeping", "", now)?;
    ensure!(tx.execute("UPDATE projects SET terminal=NULL,terminal_pid=NULL,generation='',ready=0,execution='null',native_session='' WHERE id=? AND generation=?",params![project,generation])? == 1,"Execution changed during retirement");
    tx.commit()?;
    Ok(())
}

pub fn prompt(request: &Request) -> String {
    format!(
        "<dauerbot_handoff request_id=\"{}\">\nYou have been natively idle for ten minutes. Update HANDOFF.md now for a fresh Agent session. Preserve useful user content. Include the current goal, completed work, key decisions, relevant files and commits, pending issues, and next actions. Include this exact line in the file: <!-- dauerbot-handoff:{} -->. Keep durable rules in AGENTS.md. If new user input arrives, handle it immediately; it cancels retirement. Do not start unrelated work or send a Feishu completion notice for this housekeeping. Finish writing and return to idle; do not close your own terminal.\n</dauerbot_handoff>",
        request.id, request.id
    )
}

/// Bounded regular-file reads; a symlink must never turn housekeeping into an
/// unsolicited read outside the selected project directory.
pub fn artifact(path: &Path) -> Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        meta.file_type().is_file(),
        "HANDOFF.md must be a regular file"
    );
    ensure!(meta.len() <= 1_000_000, "HANDOFF.md exceeds one megabyte");
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(1_000_001)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_000_000, "HANDOFF.md exceeds one megabyte");
    Ok(Some(
        String::from_utf8(bytes).context("HANDOFF.md must be UTF-8")?,
    ))
}
pub fn fingerprint(text: Option<&str>) -> String {
    text.map(|s| format!("{:x}", Sha256::digest(s.as_bytes())))
        .unwrap_or_default()
}
pub fn updated(request: &Request, text: Option<&str>, sequence: Option<i64>) -> bool {
    text.is_some_and(|text| {
        text.contains(&format!("<!-- dauerbot-handoff:{} -->", request.id))
            && text.len() > 100
            && fingerprint(Some(text)) != request.baseline
    }) && sequence.is_some_and(|seq| seq > request.sequence)
}
