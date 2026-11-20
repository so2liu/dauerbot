//! Pinned native lifecycle metadata, never conversation bodies or credentials.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path, time::Duration};

// Archived history remains in the native graph. It must not exhaust the much
// smaller visible sidebar budget or hide a subsequently spawned open child.
pub(crate) const MAX_RECORDS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Open,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub id: String,
    pub parent: String,
    pub name: String,
    pub state: State,
    pub updated_at_ms: Option<i64>,
    pub archived: bool,
    pub queued_count: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub root: String,
    pub records: Vec<Record>,
}

pub(crate) fn verified(version: &str) -> bool {
    // Official rust-v0.169.1 c376cf7396eff83ce4c6ac61b9553df142e7a0be
    // and rust-v0.171.0 ccf9be8cc458bd6b8336fe84d68d8893f2691a5e:
    // state migration 0021 and threads.updated_at_ms have this exact shape.
    matches!(version, "codex-cli 0.169.1" | "codex-cli 0.171.0")
}

pub(crate) fn read(home: &Path, root: &str, version: &str) -> Result<Snapshot> {
    read_at(home, root, version, false)
}

pub(crate) fn read_with_queue(home: &Path, root: &str, version: &str) -> Result<Snapshot> {
    read_at(home, root, version, true)
}

fn read_at(home: &Path, root: &str, version: &str, with_queue: bool) -> Result<Snapshot> {
    ensure!(
        verified(version),
        "Native lifecycle schema version is not verified"
    );
    uuid::Uuid::parse_str(root)?;
    let path = home.join("state_5.sqlite");
    ensure!(
        std::fs::symlink_metadata(&path)?.file_type().is_file(),
        "Native lifecycle store is not a regular file"
    );
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(Duration::from_millis(100))?;
    connection.pragma_update(None, "query_only", true)?;
    if with_queue {
        let queue = home.join("queue_1.sqlite");
        ensure!(
            std::fs::symlink_metadata(&queue)?.file_type().is_file(),
            "Native queue is not a regular file"
        );
        let mut uri = reqwest::Url::from_file_path(&queue)
            .map_err(|_| anyhow::anyhow!("Invalid native queue path"))?;
        uri.query_pairs_mut().append_pair("mode", "ro");
        connection.execute("ATTACH DATABASE ? AS native_queue", [uri.as_str()])?;
    }
    let transaction = connection.transaction()?;
    // Preparing this allowlist fails closed for any missing table or column.
    // In particular, do not fall back to title, preview, first_user_message or
    // rollout contents when native lifecycle metadata is unavailable.
    let mut children = transaction.prepare(
        "SELECT e.child_thread_id,e.status,t.id,t.agent_nickname,t.agent_role,t.updated_at_ms,t.archived \
         FROM thread_spawn_edges e LEFT JOIN threads t ON t.id=e.child_thread_id \
         WHERE e.parent_thread_id=? ORDER BY e.child_thread_id LIMIT 4097",
    )?;
    let root_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)",
        [root],
        |row| row.get(0),
    )?;
    ensure!(root_exists, "Native lifecycle root is not recorded");
    let mut queued = if with_queue {
        Some(
            transaction
                .prepare("SELECT count(*) FROM native_queue.queued_items WHERE thread_id=?")?,
        )
    } else {
        None
    };
    let mut seen = HashSet::from([root.to_owned()]);
    let mut pending = std::collections::VecDeque::from([(root.to_owned(), 0usize)]);
    let mut records = Vec::new();
    while let Some((parent, depth)) = pending.pop_front() {
        let values = children
            .query_map([&parent], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<bool>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, status, thread, nickname, role, updated_at_ms, archived) in values {
            uuid::Uuid::parse_str(&id)?;
            ensure!(
                thread.as_deref() == Some(&id)
                    && depth < 8
                    && seen.insert(id.clone())
                    && records.len() < MAX_RECORDS,
                "Native lifecycle ancestry is incomplete, cyclic or over bound"
            );
            let state = match status.as_str() {
                "open" => State::Open,
                "closed" => State::Closed,
                _ => anyhow::bail!("Unknown native lifecycle state"),
            };
            let name = nickname.or(role).unwrap_or_else(|| id.clone());
            ensure!(
                !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
                "Invalid native lifecycle name"
            );
            ensure!(
                updated_at_ms.is_none_or(|value| value >= 0),
                "Invalid native lifecycle timestamp"
            );
            pending.push_back((id.clone(), depth + 1));
            let queued_count = queued
                .as_mut()
                .map(|statement| statement.query_row([&id], |row| row.get::<_, i64>(0)))
                .transpose()?
                .map(u64::try_from)
                .transpose()?;
            records.push(Record {
                id,
                parent: parent.clone(),
                name,
                state,
                updated_at_ms,
                archived: archived.context("Missing native archive state")?,
                queued_count,
            });
        }
    }
    drop(children);
    drop(queued);
    transaction
        .commit()
        .context("Native lifecycle snapshot could not finish")?;
    Ok(Snapshot {
        root: root.to_owned(),
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROOT: &str = "00000000-0000-4000-8000-000000000001";
    const CHILD: &str = "00000000-0000-4000-8000-000000000002";
    const GRAND: &str = "00000000-0000-4000-8000-000000000003";

    fn fixture(home: &Path) -> Connection {
        let db = Connection::open(home.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,agent_nickname TEXT,agent_role TEXT,updated_at_ms INTEGER,archived INTEGER NOT NULL,preview TEXT); CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT PRIMARY KEY,status TEXT);").unwrap();
        for id in [ROOT, CHILD, GRAND] {
            db.execute(
                "INSERT INTO threads VALUES(?, 'worker',NULL,1234,0,'PRIVATE BODY')",
                [id],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO thread_spawn_edges VALUES(?,?,'closed')",
            [ROOT, CHILD],
        )
        .unwrap();
        db.execute(
            "INSERT INTO thread_spawn_edges VALUES(?,?,'open')",
            [CHILD, GRAND],
        )
        .unwrap();
        db
    }

    #[test]
    fn preserves_archived_rows_beyond_the_sidebar_limit() {
        let home = tempfile::tempdir().unwrap();
        let db = fixture(home.path());
        for suffix in 10..150 {
            let id = format!("00000000-0000-4000-8000-{suffix:012}");
            db.execute(
                "INSERT INTO threads VALUES(?,'worker',NULL,1234,1,'')",
                [&id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO thread_spawn_edges VALUES(?,?,'closed')",
                [ROOT, &id],
            )
            .unwrap();
        }
        let snapshot = read(home.path(), ROOT, "codex-cli 0.171.0").unwrap();
        assert_eq!(snapshot.records.len(), 142);
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(|record| record.archived)
                .count(),
            140
        );
    }

    #[test]
    fn verified_metadata_distinguishes_closed_from_open_without_reading_bodies() {
        let home = tempfile::tempdir().unwrap();
        let db = fixture(home.path());
        db.execute("UPDATE threads SET archived=1 WHERE id=?", [CHILD])
            .unwrap();
        for version in ["codex-cli 0.169.1", "codex-cli 0.171.0"] {
            let snapshot = read(home.path(), ROOT, version).unwrap();
            assert_eq!(snapshot.records.len(), 2);
            assert_eq!(snapshot.records[0].state, State::Closed);
            assert_eq!(snapshot.records[1].state, State::Open);
            assert_eq!(snapshot.records[1].parent, CHILD);
            assert_eq!(snapshot.records[0].updated_at_ms, Some(1234));
            assert!(snapshot.records[0].archived);
            assert!(snapshot.records[0].queued_count.is_none());
            assert!(
                !serde_json::to_string(&snapshot)
                    .unwrap()
                    .contains("PRIVATE")
            );
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }

    #[test]
    fn missing_schema_and_unverified_versions_never_infer_lifecycle_state() {
        let home = tempfile::tempdir().unwrap();
        let db = fixture(home.path());
        assert!(read(home.path(), ROOT, "codex-cli 0.172.0").is_err());
        db.execute("ALTER TABLE threads DROP COLUMN updated_at_ms", [])
            .unwrap();
        assert!(
            read(home.path(), ROOT, "codex-cli 0.171.0")
                .unwrap_err()
                .to_string()
                .contains("updated_at_ms")
        );
        db.execute("DROP TABLE thread_spawn_edges", []).unwrap();
        assert!(
            read(home.path(), ROOT, "codex-cli 0.171.0")
                .unwrap_err()
                .to_string()
                .contains("thread_spawn_edges")
        );
    }

    #[test]
    fn cleanup_snapshot_includes_native_queue_without_payload_and_refuses_missing_schema() {
        let home = tempfile::tempdir().unwrap();
        fixture(home.path());
        assert!(read_with_queue(home.path(), ROOT, "codex-cli 0.171.0").is_err());
        let queue = Connection::open(home.path().join("queue_1.sqlite")).unwrap();
        queue.execute_batch("CREATE TABLE queued_items(thread_id TEXT, payload_json TEXT); INSERT INTO queued_items VALUES('00000000-0000-4000-8000-000000000003','PRIVATE QUEUED BODY');").unwrap();
        let snapshot = read_with_queue(home.path(), ROOT, "codex-cli 0.171.0").unwrap();
        assert_eq!(snapshot.records[0].queued_count, Some(0));
        assert_eq!(snapshot.records[1].queued_count, Some(1));
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("PRIVATE")
        );
        queue
            .execute(
                "ALTER TABLE queued_items RENAME COLUMN thread_id TO invalid_column",
                [],
            )
            .unwrap();
        assert!(read_with_queue(home.path(), ROOT, "codex-cli 0.171.0").is_err());
    }

    #[test]
    fn malformed_edges_and_partial_native_records_fail_closed() {
        let home = tempfile::tempdir().unwrap();
        let db = fixture(home.path());
        db.execute("UPDATE thread_spawn_edges SET status='idle'", [])
            .unwrap();
        assert!(read(home.path(), ROOT, "codex-cli 0.171.0").is_err());
        db.execute("UPDATE thread_spawn_edges SET status='closed'", [])
            .unwrap();
        db.execute("DELETE FROM threads WHERE id=?", [GRAND])
            .unwrap();
        assert!(read(home.path(), ROOT, "codex-cli 0.171.0").is_err());
    }
}
