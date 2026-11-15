//! The configured bot owns one project. Chats describe message sources, never routes.
use crate::db::{Db, now};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS bot_project (
            slot INTEGER PRIMARY KEY CHECK(slot=1),
            project_id TEXT NOT NULL UNIQUE REFERENCES projects(id) ON DELETE CASCADE);
         PRAGMA user_version=3;",
    )?;
    ensure_owner(c)
}

pub fn ensure_owner(c: &Connection) -> Result<()> {
    // Upgrade the existing one-project installation and bind the first newly created project.
    // Adding another local project never changes the configured bot's owner.
    c.execute(
        "INSERT OR IGNORE INTO bot_project(slot,project_id)
         SELECT 1,MIN(id) FROM projects HAVING COUNT(*)=1",
        [],
    )?;
    if let Some(project) = owner(c)? {
        c.execute("UPDATE conversations SET project_id=?", [&project])?;
        c.execute(
            "INSERT OR IGNORE INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at)
             SELECT lower(hex(randomblob(16))),?,'in',u.source_id,u.content,'queued',u.attachment,u.created_at,?
             FROM unrouted_messages u
             WHERE NOT EXISTS(SELECT 1 FROM messages m WHERE m.direction='in' AND m.source_id=u.source_id)
             ORDER BY u.created_at,u.rowid",
            params![project,now()],
        )?;
        c.execute("DELETE FROM unrouted_messages", [])?;
    }
    Ok(())
}

pub fn owner(c: &Connection) -> Result<Option<String>> {
    Ok(
        c.query_row("SELECT project_id FROM bot_project WHERE slot=1", [], |r| {
            r.get(0)
        })
        .optional()?,
    )
}

impl Db {
    pub async fn bot_project(&self) -> Result<Option<String>> {
        self.call(owner).await
    }
}
