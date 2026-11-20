//! Durable repair commands remain reachable while ordinary project input is held.
use crate::{App, cli_switch, db};
use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};
use std::sync::Arc;

/// Feishu replaces leading group mentions with generated placeholders. Strip
/// only those recorded in this event; retain the original message in storage.
pub(crate) fn command_text<'a>(text: &'a str, source: &Value) -> &'a str {
    let mut text = text.trim();
    if source["chat_type"] != "group" {
        return text;
    }
    while let Some((key, rest)) = text.split_once(char::is_whitespace) {
        if !key.strip_prefix("@_user_").is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit())
        }) || !source["mentions"].as_array().is_some_and(|mentions| {
            mentions.iter().any(|mention| mention["key"] == key)
        }) {
            break;
        }
        text = rest.trim_start();
    }
    text
}

fn instruction(text: &str) -> Option<&str> {
    let text = text.trim();
    let rest = text.strip_prefix("/repair")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

/// Classify before work::sync_inbound, in the same scheduler transaction.
/// The original source metadata and source ID remain in the message journal.
pub fn classify(c: &rusqlite::Connection) -> Result<()> {
    let rows = c.prepare("SELECT id,content,attachment FROM messages WHERE direction='in' AND status='queued' AND json_extract(attachment,'$.daemon_control') IS NULL AND NOT EXISTS(SELECT 1 FROM work_events WHERE message_id=messages.id) ORDER BY rowid LIMIT 128")?
        .query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, text, raw) in rows {
        let attachment: Value = serde_json::from_str(&raw)?;
        // Only actual text messages use the reserved command grammar. A post,
        // quoted snippet or attachment must not become a daemon operation.
        if attachment["source"]["message_type"] != "text"
            || !attachment["source"]["chat_id"].is_string()
            || instruction(command_text(&text, &attachment["source"])).is_none()
        {
            continue;
        }
        c.execute("UPDATE messages SET attachment=json_set(attachment,'$.daemon_control',json(?)),updated_at=? WHERE id=?",params![json!({"kind":"repair","status":"pending","next_attempt_at":0}).to_string(),db::now(),id])?;
    }
    Ok(())
}

pub async fn process(app: &Arc<App>) -> Result<()> {
    let commands = app.db.call(|c| {
        Ok(c.prepare("SELECT id,project_id,source_id,content,attachment FROM messages WHERE direction='in' AND json_extract(attachment,'$.daemon_control.kind')='repair' AND json_extract(attachment,'$.daemon_control.status')='pending' AND COALESCE(json_extract(attachment,'$.daemon_control.next_attempt_at'),0)<=? ORDER BY rowid LIMIT 16")?
            .query_map([db::now()], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }).await?;
    for (message, project, source_id, content, raw) in commands {
        let attachment: Value = serde_json::from_str(&raw)?;
        let control = json!({"content":instruction(command_text(&content, &attachment["source"])).unwrap_or_default(),"message_id":source_id,"source":attachment["source"]});
        let key = project.clone();
        let source = source_id.clone();
        let accepted = async {
            app.db
                .call(move |c| cli_switch::journal_repair_instruction(c, &key, &source, &control))
                .await?;
            app.request_cli_repair(&project).await
        }
        .await;
        match accepted {
            Ok(_) => {
                app.db.call(move |c| {
                    c.execute("UPDATE messages SET status='acknowledged',attachment=json_set(attachment,'$.daemon_control.status','accepted'),updated_at=? WHERE id=?",params![db::now(),message])?;
                    Ok(())
                }).await?;
                app.notify.notify_one();
            }
            Err(error) => {
                // A paused project or temporarily unavailable runtime must not
                // discard a repair request or turn it into ordinary work.
                let error = error.to_string();
                app.db.call(move |c| {
                    c.execute("UPDATE messages SET attachment=json_set(attachment,'$.daemon_control.next_attempt_at',?,'$.daemon_control.error',?),updated_at=? WHERE id=?",params![db::now()+30_000,error,db::now(),message])?;
                    Ok(())
                }).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repair_is_deduplicated_and_never_becomes_ordinary_held_work() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().into(), crate::config::Config::default()).unwrap();
        let p = app
            .db
            .create(crate::db::ProjectInput {
                name: "repair".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        app.db.sync(|c| {
            for (source,text,kind) in [("om_repair","/repair inspect the failed AGY startup","text"),("om_normal","please inspect /repair in this example","text"),("om_quoted","/repair","post"),("om_similar","/repairing","text")] {
                c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES(?,?,'in',?,?,'queued',?,1,1)",params![source,p.id,source,text,json!({"source":{"message_type":kind,"chat_id":"oc_fixture"}}).to_string()])?;
            }
            classify(c)?;
            classify(c)?;
            crate::work::sync_inbound(c)?;
            assert_eq!(c.query_row("SELECT count(*) FROM work_events",[],|r|r.get::<_,i64>(0))?,3);
            assert_eq!(c.query_row("SELECT count(*) FROM work_events WHERE source_id='om_repair'",[],|r|r.get::<_,i64>(0))?,0);
            assert_eq!(c.query_row("SELECT count(*) FROM messages WHERE json_extract(attachment,'$.daemon_control.kind')='repair'",[],|r|r.get::<_,i64>(0))?,1);
            Ok(())
        }).unwrap();
    }
}
