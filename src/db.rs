use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub fn id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub agent: String,
    pub chat_id: String,
    pub paused: bool,
    pub terminal: Option<String>,
    pub terminal_pid: Option<i64>,
    pub generation: String,
    pub ready: bool,
    pub execution: serde_json::Value,
    pub native_session: String,
    pub extra_args: Vec<String>,
    pub created_at: i64,
    #[serde(skip_serializing)]
    pub mcp_token: String,
}

#[derive(Debug, Deserialize)]
pub struct ProjectInput {
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    pub native_session: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}
fn default_agent() -> String {
    "codex".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub project_id: String,
    pub direction: String,
    pub source_id: String,
    pub content: String,
    pub status: String,
    pub error: String,
    pub attempts: i64,
    pub attachment: serde_json::Value,
    pub created_at: i64,
    pub updated_at: i64,
}

fn project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        agent: row.get(3)?,
        chat_id: row.get(4)?,
        paused: row.get(5)?,
        terminal: row.get(6)?,
        terminal_pid: row.get(7)?,
        generation: row.get(8)?,
        ready: row.get(9)?,
        native_session: row.get(10)?,
        extra_args: serde_json::from_str(&row.get::<_, String>(11)?).unwrap_or_default(),
        created_at: row.get(12)?,
        mcp_token: row.get(13)?,
        execution: serde_json::from_str(&row.get::<_, String>(14)?).unwrap_or_default(),
    })
}
const PROJECT: &str = "id,name,path,agent,chat_id,paused,terminal,terminal_pid,generation,ready,native_session,extra_args,created_at,mcp_token,execution";
fn message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        project_id: row.get(1)?,
        direction: row.get(2)?,
        source_id: row.get(3)?,
        content: row.get(4)?,
        status: row.get(5)?,
        error: row.get(6)?,
        attempts: row.get(7)?,
        attachment: serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default(),
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}
const MESSAGE: &str = "id,project_id,direction,source_id,content,status,error,attempts,attachment,created_at,updated_at";

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let schema: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            schema <= 7,
            "This data directory requires a newer Dauerbot version"
        );
        anyhow::ensure!(
            schema == 0 || schema >= 4,
            "Migrate this pre-refactor data directory with Dauerbot 0.4.1 before upgrading"
        );
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let migration = conn.unchecked_transaction()?;
        migration.execute_batch("
            CREATE TABLE IF NOT EXISTS projects (
              id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, path TEXT NOT NULL, agent TEXT NOT NULL,
              chat_id TEXT NOT NULL DEFAULT '', paused INTEGER NOT NULL DEFAULT 0,
              terminal TEXT, terminal_pid INTEGER, generation TEXT NOT NULL DEFAULT '',
              ready INTEGER NOT NULL DEFAULT 0, native_session TEXT NOT NULL DEFAULT '',
              extra_args TEXT NOT NULL DEFAULT '[]', created_at INTEGER NOT NULL, mcp_token TEXT NOT NULL,
              execution TEXT NOT NULL DEFAULT 'null');
            CREATE TABLE IF NOT EXISTS messages (
              id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
              direction TEXT NOT NULL, source_id TEXT NOT NULL, content TEXT NOT NULL,
              status TEXT NOT NULL, error TEXT NOT NULL DEFAULT '', attempts INTEGER NOT NULL DEFAULT 0,
              attachment TEXT NOT NULL DEFAULT '{}', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
              UNIQUE(project_id,direction,source_id));
            CREATE INDEX IF NOT EXISTS message_pending ON messages(direction,status,created_at);

        ")?;
        if schema == 4 {
            migration.execute_batch("DROP TABLE IF EXISTS archived_core_executions;")?;
        }
        // A stopped execution must never disable future message-triggered wakeup.
        // Keep explicit pauses, credentials and native conversation references.
        let has_enabled: bool = migration.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name='enabled')",
            [],
            |row| row.get(0),
        )?;
        if has_enabled {
            migration.execute_batch("ALTER TABLE projects DROP COLUMN enabled;")?;
        }
        crate::conversation::migrate(&migration)?;
        crate::bot::migrate(&migration)?;
        migration.execute("UPDATE messages SET status='uncertain',error='The service exited during this operation; verify the result before retrying manually',updated_at=? WHERE status IN ('submitting','sending')", [now()])?;
        crate::work::migrate(&migration)?;
        crate::handoff::migrate(&migration)?;
        migration.pragma_update(None, "user_version", 7)?;
        migration.commit()?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }
    pub fn sync<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock is poisoned"))?;
        f(&conn)
    }
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.sync(f)).await?
    }
    pub async fn projects(&self) -> Result<Vec<Project>> {
        self.call(|c| {
            Ok(c.prepare(&format!(
                "SELECT {PROJECT} FROM projects ORDER BY created_at"
            ))?
            .query_map([], project)?
            .collect::<rusqlite::Result<_>>()?)
        })
        .await
    }
    pub async fn project(&self, key: &str) -> Result<Project> {
        let key = key.to_owned();
        self.call(move |c| {
            c.query_row(
                &format!("SELECT {PROJECT} FROM projects WHERE id=?1 OR name=?1"),
                [key],
                project,
            )
            .context("Project not found")
        })
        .await
    }
    pub async fn create(&self, input: ProjectInput) -> Result<Project> {
        validate(&input)?;
        let requested = input.path.clone();
        let name = input.name.clone();
        let path = tokio::task::spawn_blocking(move || -> Result<_> {
            if requested.trim().is_empty() {
                crate::workspace::create(&name)
            } else {
                std::fs::canonicalize(&requested).context("Project directory does not exist")
            }
        })
        .await??;
        if !path.is_dir() {
            bail!("Project path must be a directory");
        }
        let key = id();
        let out = key.clone();
        self.call(move |c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("INSERT INTO projects(id,name,path,agent,chat_id,native_session,extra_args,created_at,mcp_token) VALUES(?,?,?,?,?,?,?,?,?)",
            params![key,input.name.trim(),path.to_string_lossy(),input.agent,input.chat_id.trim(),input.native_session,serde_json::to_string(&input.extra_args)?,now(),id()])?;
            crate::conversation::primary(&tx, &key, input.chat_id.trim(), "")?;
            tx.commit()?;
            Ok(())
        }).await?;
        self.project(&out).await
    }
    pub async fn update(&self, key: &str, input: ProjectInput) -> Result<Project> {
        validate(&input)?;
        let p = self.project(key).await?;
        let path = std::fs::canonicalize(&input.path)?;
        if !path.is_dir() {
            bail!("Project path must be a directory");
        }
        if p.terminal.is_some()
            && (p.path != path.to_string_lossy()
                || p.agent != input.agent
                || p.extra_args != input.extra_args)
        {
            bail!("Stop the session before changing the directory, agent, or launch arguments");
        }
        let out = p.id.clone();
        self.call(move |c| {
            let tx = c.unchecked_transaction()?;
            crate::conversation::primary(&tx, &p.id, input.chat_id.trim(), &p.chat_id)?;
            tx.execute("UPDATE projects SET name=?,path=?,agent=?,chat_id=?,native_session=?,extra_args=? WHERE id=?",
            params![input.name.trim(),path.to_string_lossy(),input.agent,input.chat_id.trim(),input.native_session,serde_json::to_string(&input.extra_args)?,p.id])?;
            tx.commit()?; Ok(())
        }).await?;
        self.project(&out).await
    }
    pub async fn pause(&self, key: &str, value: bool) -> Result<()> {
        let key = self.project(key).await?.id;
        self.call(move |c| {
            c.execute(
                "UPDATE projects SET paused=? WHERE id=?",
                params![value, key],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn set_session(
        &self,
        key: &str,
        terminal: Option<String>,
        pid: Option<i64>,
        generation: String,
    ) -> Result<()> {
        let key = key.to_owned();
        self.call(move |c| {
            c.execute(
                "UPDATE projects SET terminal=?,terminal_pid=?,generation=?,ready=0,execution=CASE WHEN ?1 IS NULL THEN 'null' ELSE execution END WHERE id=?",
                params![terminal, pid, generation, key],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn save_execution(
        &self,
        key: &str,
        execution: serde_json::Value,
        generation: &str,
    ) -> Result<()> {
        let key = key.to_owned();
        let generation = generation.to_owned();
        self.call(move |c| {
            c.execute("UPDATE projects SET terminal=?,terminal_pid=?,generation=?,ready=0,execution=? WHERE id=?",
                params![execution["terminal_id"].as_str(),execution["shell_pid"].as_i64(),generation,execution.to_string(),key])?;
            Ok(())
        }).await
    }
    pub async fn native_session(&self, key: &str, native: &str) -> Result<()> {
        let key = key.to_owned();
        let native = native.to_owned();
        self.call(move |c| {
            c.execute(
                "UPDATE projects SET native_session=?,
                 execution=CASE WHEN terminal IS NOT NULL AND execution!='null'
                   THEN json_set(execution,'$.native_confirmed',json('true')) ELSE execution END
                 WHERE id=? AND ?<>''",
                params![native, key, native],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn switch_agent(&self, key: &str, agent: &str) -> Result<()> {
        anyhow::ensure!(
            ["codex", "claude", "pi", "agy"].contains(&agent),
            "Unsupported agent"
        );
        let key = self.project(key).await?.id;
        let agent = agent.to_owned();
        self.call(move |c| {
            let n = c.execute(
                "UPDATE projects SET agent=?,native_session='',extra_args='[]',generation='',ready=0
                 WHERE id=? AND terminal IS NULL",
                params![agent,key],
            )?;
            anyhow::ensure!(n == 1, "Stop the current agent before switching");
            Ok(())
        })
        .await
    }
    pub async fn ready(
        &self,
        key: &str,
        generation: &str,
        terminal: &str,
        native: &str,
    ) -> Result<()> {
        let (key, generation, terminal, native) = (
            key.to_owned(),
            generation.to_owned(),
            terminal.to_owned(),
            native.to_owned(),
        );
        self.call(move|c|{
            let n=c.execute("UPDATE projects SET ready=1,native_session=CASE WHEN ?1='' THEN native_session ELSE ?1 END,
                execution=CASE WHEN ?1!='' AND execution!='null' THEN json_set(execution,'$.native_confirmed',json('true')) ELSE execution END
                WHERE id=?2 AND generation=?3 AND terminal=?4",
                params![native,key,generation,terminal])?;
            if n!=1 {bail!("Session identity changed; reconnect MCP");} Ok(())
        }).await
    }
    /// The receiving Bot supplies the project; chat IDs are source metadata only.
    pub fn ingest_bot(
        &self,
        project: &str,
        app_id: &str,
        chat: &str,
        source: &str,
        content: &str,
        attachment: &serde_json::Value,
    ) -> Result<Option<String>> {
        self.sync(|c| {
            let tx = c.unchecked_transaction()?;
            let bound: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM project_bots WHERE project_id=? AND app_id=?)",
                params![project, app_id], |r| r.get(0),
            )?;
            anyhow::ensure!(bound, "The receiving bot is no longer bound to this project");
            let kind = attachment["source"]["chat_type"].as_str().unwrap_or("unknown");
            tx.execute(
                "INSERT INTO conversations(project_id,chat_id,kind,seen_at,preview) VALUES(?,?,?,?,?)
                 ON CONFLICT(project_id,chat_id) DO UPDATE SET seen_at=excluded.seen_at,preview=excluded.preview,
                 kind=CASE WHEN excluded.kind='unknown' THEN conversations.kind ELSE excluded.kind END",
                params![project, chat, kind, now(), content.chars().take(96).collect::<String>()],
            )?;
            let mut attachment = attachment.clone();
            attachment["bot_app_id"] = serde_json::json!(app_id);
            let key = id();
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at)
                 VALUES(?,?,'in',?,?,'queued',?,?,?)",
                params![key,project,source,content,attachment.to_string(),now(),now()],
            )?;
            tx.commit()?;
            Ok((inserted == 1).then_some(key))
        })
    }
    pub async fn enqueue_out(
        &self,
        key: &str,
        source: &str,
        content: &str,
        attachment: serde_json::Value,
    ) -> Result<Message> {
        let (key, source, content) = (key.to_owned(), source.to_owned(), content.to_owned());
        let (k, s) = (key.clone(), source.clone());
        self.call(move|c|{c.execute("INSERT OR IGNORE INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES(?,?,'out',?,?,'pending',?,?,?)",
            params![id(),key,source,content,serde_json::to_string(&attachment)?,now(),now()])?; Ok(())}).await?;
        self.call(move|c|Ok(c.query_row(&format!("SELECT {MESSAGE} FROM messages WHERE project_id=? AND direction='out' AND source_id=?"),params![k,s],message)?)).await
    }
    pub async fn messages(&self, key: &str) -> Result<Vec<Message>> {
        let key = key.to_owned();
        self.call(move|c|Ok(c.prepare(&format!("SELECT {MESSAGE} FROM messages WHERE project_id=? ORDER BY created_at DESC LIMIT 150"))?
            .query_map([key],message)?.collect::<rusqlite::Result<_>>()?)).await
    }
    pub async fn pending(&self, direction: &str) -> Result<Vec<Message>> {
        let direction = direction.to_owned();
        self.call(move|c|{
            // One head per project: a paused project's backlog must not starve other projects.
            let sql=if direction=="in"{
                format!("SELECT {MESSAGE} FROM messages WHERE rowid IN (SELECT MIN(rowid) FROM messages WHERE direction='in' AND status='queued' GROUP BY project_id) ORDER BY rowid")
            }else{
                format!("SELECT {MESSAGE} FROM messages WHERE direction='out' AND status='pending' ORDER BY rowid LIMIT 32")
            };
            Ok(c.prepare(&sql)?.query_map([],message)?.collect::<rusqlite::Result<_>>()?)
        }).await
    }
    pub async fn message(&self, key: &str) -> Result<Message> {
        let key = key.to_owned();
        self.call(move |c| {
            Ok(c.query_row(
                &format!("SELECT {MESSAGE} FROM messages WHERE id=?"),
                [key],
                message,
            )?)
        })
        .await
    }
    pub async fn state(&self, key: &str, status: &str, error: &str) -> Result<()> {
        let (key, status, error) = (key.to_owned(), status.to_owned(), error.to_owned());
        self.call(move|c|{c.execute("UPDATE messages SET status=?,error=?,updated_at=?,attempts=attempts+CASE WHEN ? IN ('submitting','sending') THEN 1 ELSE 0 END WHERE id=?",
            params![status,error,now(),status,key])?;Ok(())}).await
    }
    pub async fn remove_project(&self, key: &str) -> Result<()> {
        let p = self.project(key).await?;
        if p.terminal.is_some() {
            bail!("Stop the session before removing the project");
        }
        self.call(move |c| {
            c.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<()> {
                let running: bool = c.query_row(
                    "SELECT terminal IS NOT NULL FROM projects WHERE id=?",
                    [&p.id],
                    |r| r.get(0),
                )?;
                anyhow::ensure!(!running, "Stop the session before removing the project");
                c.execute("DELETE FROM messages WHERE project_id=?", [&p.id])?;
                c.execute("DELETE FROM projects WHERE id=?", [&p.id])?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    c.execute_batch("COMMIT")?;
                    Ok(())
                }
                Err(e) => {
                    let _ = c.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
        .await
    }
}

fn validate(p: &ProjectInput) -> Result<()> {
    if p.name.trim().is_empty() || p.name.len() > 120 {
        bail!("Project name must contain 1 to 120 characters");
    }
    if !["codex", "claude", "pi", "agy", "custom"].contains(&p.agent.as_str()) {
        bail!("Unsupported agent");
    }
    if p.agent == "custom" && p.extra_args.is_empty() {
        bail!("Custom agents require a complete argv");
    }
    if !p.chat_id.is_empty() && !p.chat_id.starts_with("oc_") {
        bail!("Enter a Feishu chat_id starting with oc_");
    }
    Ok(())
}
