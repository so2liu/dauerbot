//! Each project owns its Feishu application. Chats never select a destination project.
use crate::db::{Db, now};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Binding {
    pub project_id: String,
    pub app_id: String,
    #[serde(skip_serializing)]
    pub app_secret: String,
}

fn binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Binding> {
    Ok(Binding {
        project_id: row.get(0)?,
        app_id: row.get(1)?,
        app_secret: row.get(2)?,
    })
}

pub fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_bots (
         project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
         app_id TEXT NOT NULL, app_secret TEXT NOT NULL, updated_at INTEGER NOT NULL);
         CREATE UNIQUE INDEX IF NOT EXISTS bot_application_owner ON project_bots(app_id);",
    )?;
    let previous: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='bot_project')",
        [],
        |row| row.get(0),
    )?;
    if previous {
        c.execute_batch(
            "INSERT OR IGNORE INTO project_bots SELECT project_id,'','',0 FROM bot_project;
             DROP TABLE bot_project;",
        )?;
    }
    Ok(())
}

pub fn get(c: &Connection, project: &str) -> Result<Option<Binding>> {
    Ok(c.query_row(
        "SELECT project_id,app_id,app_secret FROM project_bots WHERE project_id=?",
        [project],
        binding_row,
    )
    .optional()?)
}

pub fn save(c: &Connection, project: &str, app_id: &str, secret: &str, clear: bool) -> Result<()> {
    ensure!(
        app_id.starts_with("cli_") && app_id.len() <= 200,
        "Enter an App ID starting with cli_"
    );
    let other: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_bots WHERE app_id=? AND project_id<>?)",
        params![app_id, project],
        |row| row.get(0),
    )?;
    ensure!(!other, "This bot is already connected to another project");
    let old = get(c, project)?;
    let changed = old.as_ref().is_some_and(|binding| binding.app_id != app_id);
    let secret = if !secret.is_empty() {
        secret.to_owned()
    } else if !clear {
        old.filter(|binding| binding.app_id == app_id)
            .map(|binding| binding.app_secret)
            .unwrap_or_default()
    } else {
        String::new()
    };
    ensure!(!secret.is_empty(), "Enter this bot's App Secret");
    c.execute(
        "INSERT INTO project_bots(project_id,app_id,app_secret,updated_at) VALUES(?,?,?,?)
         ON CONFLICT(project_id) DO UPDATE SET app_id=excluded.app_id,app_secret=excluded.app_secret,updated_at=excluded.updated_at",
        params![project,app_id,secret,now()],
    )?;
    if changed {
        c.execute(
            "UPDATE conversations SET context='{}' WHERE project_id=?",
            [project],
        )?;
    }
    Ok(())
}

impl Db {
    pub async fn bot(&self, project: &str) -> Result<Binding> {
        let project = project.to_owned();
        self.call(move |c| get(c, &project)?.context("Connect a Feishu bot to this project first"))
            .await
    }
    pub async fn bots(&self) -> Result<Vec<Binding>> {
        self.call(|c| {
            Ok(c.prepare(
                "SELECT project_id,app_id,app_secret FROM project_bots ORDER BY project_id",
            )?
            .query_map([], binding_row)?
            .collect::<rusqlite::Result<_>>()?)
        })
        .await
    }
    /// Import former global credentials into their existing owner's binding once.
    pub fn import_bot(&self, app_id: &str, secret: &str) -> Result<bool> {
        self.sync(|c| {
            let project: Option<String> = c
                .query_row(
                    "SELECT project_id FROM project_bots WHERE app_id='' LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(project) = project else {
                return Ok(!app_id.is_empty()
                    && c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM project_bots WHERE app_id=? AND app_secret=?)",
                        params![app_id, secret],
                        |row| row.get::<_, bool>(0),
                    )?);
            };
            let tx = c.unchecked_transaction()?;
            if !app_id.is_empty() && !secret.is_empty() {
                save(&tx, &project, app_id, secret, false)?;
            } else {
                tx.execute(
                    "DELETE FROM project_bots WHERE project_id=? AND app_id=''",
                    [&project],
                )?;
            }
            tx.commit()?;
            Ok(true)
        })
    }
}
