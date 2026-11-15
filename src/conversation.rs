//! Conversation discovery and inbound routing, independent of outbound destinations.
use crate::db::{Db, now};
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub fn migrate(c: &rusqlite::Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS conversations (
        chat_id TEXT PRIMARY KEY, kind TEXT NOT NULL DEFAULT 'unknown', name TEXT NOT NULL DEFAULT '',
        project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
        context TEXT NOT NULL DEFAULT '{}', seen_at INTEGER NOT NULL DEFAULT 0, preview TEXT NOT NULL DEFAULT '');
        CREATE TABLE IF NOT EXISTS unrouted_messages (
        source_id TEXT PRIMARY KEY, chat_id TEXT NOT NULL, content TEXT NOT NULL,
        attachment TEXT NOT NULL, created_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS routing_defaults (
        slot TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE);
        INSERT OR IGNORE INTO conversations(chat_id,kind,project_id)
        SELECT chat_id,'unknown',id FROM projects WHERE chat_id <> '';")?;
    let columns = c
        .prepare("PRAGMA table_info(conversations)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|v| v == "preview") {
        c.execute(
            "ALTER TABLE conversations ADD COLUMN preview TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    c.execute_batch("CREATE INDEX IF NOT EXISTS message_source ON messages(source_id,direction); PRAGMA user_version=2;")?;
    Ok(())
}
pub(crate) fn primary(c: &rusqlite::Connection, project: &str, new: &str, old: &str) -> Result<()> {
    if !new.is_empty() {
        let owner: Option<String> = c
            .query_row(
                "SELECT project_id FROM conversations WHERE chat_id=?",
                [new],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        if owner.as_deref().is_some_and(|id| id != project) {
            bail!("This chat belongs to another project; unbind it first");
        }
    }
    if old != new && !old.is_empty() {
        c.execute(
            "UPDATE conversations SET project_id=NULL WHERE chat_id=? AND project_id=?",
            params![old, project],
        )?;
    }
    if !new.is_empty() {
        c.execute("INSERT INTO conversations(chat_id,project_id) VALUES(?,?) ON CONFLICT(chat_id) DO UPDATE SET project_id=excluded.project_id",params![new,project])?;
        c.execute("INSERT OR IGNORE INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) SELECT lower(hex(randomblob(16))),?,'in',source_id,content,'queued',attachment,created_at,? FROM unrouted_messages WHERE chat_id=?",params![project,now(),new])?;
        c.execute("DELETE FROM unrouted_messages WHERE chat_id=?", [new])?;
    }
    Ok(())
}
impl Db {
    pub async fn conversations(&self) -> Result<Value> {
        self.call(|c| {
            let mut query = c.prepare("SELECT chat_id,kind,name,project_id,seen_at,preview FROM conversations ORDER BY seen_at DESC,chat_id")?;
            let rows = query.query_map([], |r| Ok(json!({"chat_id":r.get::<_,String>(0)?,"kind":r.get::<_,String>(1)?,"name":r.get::<_,String>(2)?,"project_id":r.get::<_,Option<String>>(3)?,"seen_at":r.get::<_,i64>(4)?,"preview":r.get::<_,String>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(json!({"items":rows}))
        }).await
    }
}
