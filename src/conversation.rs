//! Conversation discovery and inbound routing, independent of outbound destinations.
use crate::db::{Db, now};
use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};

pub fn migrate(c: &rusqlite::Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS conversations (
        chat_id TEXT PRIMARY KEY, kind TEXT NOT NULL DEFAULT 'unknown', name TEXT NOT NULL DEFAULT '',
        project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
        context TEXT NOT NULL DEFAULT '{}', seen_at INTEGER NOT NULL DEFAULT 0, preview TEXT NOT NULL DEFAULT '');
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
    let scoped: bool = c.query_row(
        "SELECT pk>0 FROM pragma_table_info('conversations') WHERE name='project_id'",
        [],
        |r| r.get(0),
    )?;
    if !scoped {
        c.execute_batch(
            "ALTER TABLE conversations RENAME TO previous_conversations;
             CREATE TABLE conversations (
             project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
             chat_id TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'unknown',name TEXT NOT NULL DEFAULT '',
             context TEXT NOT NULL DEFAULT '{}',seen_at INTEGER NOT NULL DEFAULT 0,preview TEXT NOT NULL DEFAULT '',
             PRIMARY KEY(project_id,chat_id));
             INSERT INTO conversations SELECT project_id,chat_id,kind,name,context,seen_at,preview FROM previous_conversations;
             DROP TABLE previous_conversations;",
        )?;
    }
    c.execute_batch("CREATE INDEX IF NOT EXISTS message_source ON messages(source_id,direction); DROP INDEX IF EXISTS project_chat; DROP TABLE IF EXISTS routing_defaults;")?;
    Ok(())
}
pub(crate) fn primary(
    c: &rusqlite::Connection,
    project: &str,
    new: &str,
    _old: &str,
) -> Result<()> {
    if !new.is_empty() {
        c.execute(
            "INSERT OR IGNORE INTO conversations(chat_id,project_id,seen_at) VALUES(?,?,?)",
            params![new, project, now()],
        )?;
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
