use crate::{db, multimodal::ImageRef};
use anyhow::Result;
use rig_core::{
    message::{Message, UserContent},
    transcript::validate_canonical,
};
use rusqlite::{Connection, OptionalExtension, params};

pub(super) fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS manager_channels(
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            channel_id TEXT NOT NULL, seen_memory_sha256 TEXT,
            head_memory TEXT, compaction_seq INTEGER NOT NULL DEFAULT 0,
            compaction_summary TEXT, last_input_tokens INTEGER, last_estimate INTEGER, retry_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(project_id,channel_id)
        );
        CREATE TABLE IF NOT EXISTS manager_session_messages(
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id TEXT NOT NULL, channel_id TEXT NOT NULL,
            message_json TEXT NOT NULL, images_json TEXT NOT NULL, created_at INTEGER NOT NULL, memory_snapshot TEXT,
            FOREIGN KEY(project_id,channel_id) REFERENCES manager_channels(project_id,channel_id) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS manager_ingested_inputs(
            input_id TEXT PRIMARY KEY REFERENCES manager_inputs(id) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS manager_detached_effects(
            effect_id TEXT PRIMARY KEY REFERENCES manager_effects(id) ON DELETE CASCADE,
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            channel_id TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS manager_session_channel ON manager_session_messages(project_id,channel_id,seq);",
    )?;
    let has_message_seq: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('manager_ingested_inputs') WHERE name='message_seq')",
        [],
        |row| row.get(0),
    )?;
    if !has_message_seq {
        c.execute("ALTER TABLE manager_ingested_inputs ADD COLUMN message_seq INTEGER", [])?;
    }
    Ok(())
}

pub(super) fn open(c: &Connection, project: &str, channel: &str) -> Result<()> {
    c.execute(
        "INSERT OR IGNORE INTO manager_channels(project_id,channel_id) VALUES(?,?)",
        params![project, channel],
    )?;
    Ok(())
}

pub(super) fn append(
    c: &Connection,
    project: &str,
    channel: &str,
    message: &Message,
    images: &[ImageRef],
) -> Result<()> {
    c.execute(
        "INSERT INTO manager_session_messages(project_id,channel_id,message_json,images_json,created_at)
         VALUES(?,?,?,?,?)",
        params![project, channel, serde_json::to_string(message)?, serde_json::to_string(images)?, db::now()],
    )?;
    Ok(())
}

/// The caller commits the marker and its message in the same transaction.
pub(super) fn sync_memory(
    c: &Connection,
    project: &str,
    channel: &str,
    digest: &str,
    snapshot: &str,
) -> Result<()> {
    let previous: Option<String> = c.query_row(
        "SELECT seen_memory_sha256 FROM manager_channels WHERE project_id=? AND channel_id=?",
        params![project, channel],
        |row| row.get(0),
    )?;
    if previous.as_deref() == Some(digest) {
        return Ok(());
    }
    let has_messages = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM manager_session_messages WHERE project_id=? AND channel_id=?)",
        params![project, channel],
        |row| row.get::<_, bool>(0),
    )?;
    if has_messages {
        c.execute(
            "INSERT INTO manager_session_messages(project_id,channel_id,message_json,images_json,created_at,memory_snapshot)
             VALUES(?,?,?,'[]',?,?)",
            params![project,channel,serde_json::to_string(&Message::user(snapshot))?,db::now(),snapshot],
        )?;
    }
    c.execute(
        "UPDATE manager_channels
         SET seen_memory_sha256=?,
             head_memory=CASE WHEN ? THEN head_memory ELSE ? END
         WHERE project_id=? AND channel_id=?",
        params![digest, has_messages, snapshot, project, channel],
    )?;
    Ok(())
}

pub(super) struct Entry {
    pub seq: i64,
    pub message: Message,
    pub images: Vec<ImageRef>,
}

pub(super) fn load(c: &Connection, project: &str, channel: &str) -> Result<Vec<Entry>> {
    let (memory, summary, cutoff) = c
        .query_row(
            "SELECT head_memory,compaction_summary,compaction_seq FROM manager_channels WHERE project_id=? AND channel_id=?",
            params![project, channel],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, i64>(2)?)),
        )
        .optional()?
        .unwrap_or_default();
    let mut history = Vec::new();
    for text in memory.into_iter().chain(summary) {
        history.push(Entry {
            seq: cutoff,
            message: Message::user(text),
            images: Vec::new(),
        });
    }
    let mut stmt = c.prepare(
        "SELECT seq,message_json,images_json FROM manager_session_messages
         WHERE project_id=? AND channel_id=? AND seq>? ORDER BY seq",
    )?;
    let mut rows = stmt.query(params![project, channel, cutoff])?;
    while let Some(row) = rows.next()? {
        history.push(Entry {
            seq: row.get(0)?,
            message: serde_json::from_str(&row.get::<_, String>(1)?)?,
            images: serde_json::from_str(&row.get::<_, String>(2)?)?,
        });
    }
    Ok(history)
}

pub(super) async fn hydrate(history: Vec<Entry>, supports_images: bool) -> Result<Vec<Message>> {
    let mut remaining = 8 * crate::multimodal::MAX_IMAGE_BYTES;
    let mut messages = Vec::with_capacity(history.len());
    for Entry {
        mut message,
        images,
        ..
    } in history
    {
        if let Message::User { content } = &mut message {
            for image in images {
                let part = if !supports_images {
                    UserContent::text(
                        "[Image omitted: configured model has no declared image capability]",
                    )
                } else if image.bytes <= remaining {
                    remaining -= image.bytes;
                    image
                        .content()
                        .await
                        .unwrap_or_else(|_| UserContent::text("[Image unavailable]"))
                } else {
                    UserContent::text("[Image unavailable]")
                };
                content.push(part);
            }
        }
        messages.push(message);
    }
    validate_canonical(&messages)?;
    Ok(messages)
}

pub(super) fn migrate_inputs(c: &Connection) -> Result<()> {
    let exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('manager_inputs') WHERE name='channel_id')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        c.execute_batch(
            "ALTER TABLE manager_inputs ADD COLUMN channel_id TEXT NOT NULL DEFAULT 'local:default';
             ALTER TABLE manager_inputs ADD COLUMN signal TEXT NOT NULL DEFAULT 'wake';
             UPDATE manager_inputs SET signal='steer' WHERE kind='user';",
        )?;
        let work_exists: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='work_events')",
            [],
            |row| row.get(0),
        )?;
        if work_exists {
            let mut stmt = c.prepare(
                "SELECT i.id, i.project_id, e.source, e.payload FROM manager_inputs i
             JOIN work_events e ON e.id=i.origin_event_id AND e.project_id=i.project_id",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, project, source, raw) in rows {
                let payload = serde_json::from_str(&raw)?;
                if let Some(channel) = super::channel::for_event(c, &project, &source, &payload)? {
                    c.execute(
                        "UPDATE manager_inputs SET channel_id=? WHERE id=?",
                        params![channel, id],
                    )?;
                }
            }
        }
    }
    c.execute(
        "CREATE INDEX IF NOT EXISTS manager_inputs_channel ON manager_inputs(project_id,channel_id,status)",
        [],
    )?;
    Ok(())
}

pub(super) fn estimated_tokens(entry: &Entry) -> u64 {
    let bytes = serde_json::to_vec(&entry.message).map_or(0, |bytes| bytes.len() as u64);
    bytes.div_ceil(3) + 1024 * entry.images.len() as u64
}

fn is_tool_result(message: &Message) -> bool {
    matches!(
        message,
        Message::User { content }
            if content.iter().any(|part| matches!(part, UserContent::ToolResult(_)))
    )
}

pub(super) fn compaction_cut(history: &[Entry], cutoff: i64, keep: u64) -> Option<usize> {
    let mut retained = 0;
    let mut fallback = None;
    for index in (1..history.len()).rev() {
        retained += estimated_tokens(&history[index]);
        let next = &history[index];
        if retained >= keep
            && history[index - 1].seq > cutoff
            && history[index - 1].seq < next.seq
            && !is_tool_result(&next.message)
        {
            let prefix = history[..index]
                .iter()
                .map(|entry| entry.message.clone())
                .collect::<Vec<_>>();
            let suffix = std::iter::once(Message::user("summary"))
                .chain(history[index..].iter().map(|entry| entry.message.clone()))
                .collect::<Vec<_>>();
            if validate_canonical(&prefix).is_ok() && validate_canonical(&suffix).is_ok() {
                if matches!(next.message, Message::User { .. }) {
                    return Some(index);
                }
                fallback.get_or_insert(index);
            }
        }
    }
    fallback
}

pub(super) fn compact(
    c: &Connection,
    project: &str,
    channel: &str,
    cutoff: i64,
    summary: &str,
) -> Result<()> {
    c.execute(
        "UPDATE manager_channels SET compaction_seq=?,compaction_summary=?,
         head_memory=COALESCE((SELECT memory_snapshot FROM manager_session_messages
           WHERE project_id=? AND channel_id=? AND seq<=? AND memory_snapshot IS NOT NULL
           ORDER BY seq DESC LIMIT 1),head_memory),last_input_tokens=NULL,last_estimate=NULL
         WHERE project_id=? AND channel_id=? AND compaction_seq<?",
        params![
            cutoff,
            format!("Earlier conversation summary:\n{summary}"),
            project,
            channel,
            cutoff,
            project,
            channel,
            cutoff
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_never_keeps_a_tool_result_without_its_call() {
        use rig_core::message::{AssistantContent, AssistantMessage, ToolCall, ToolFunction};
        let id = rig_core::message::CallId::from_wire("call");
        let name = "read_file".try_into().unwrap();
        let call = ToolCall::new(id, ToolFunction::new(name, serde_json::json!({})));
        let id = call.id.clone();
        let name = call.function.name.clone();
        let history = [
            Message::user("first"),
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            Message::User {
                content: vec![rig_core::transcript::tool_result_message(
                    id,
                    name,
                    "done".into(),
                )],
            },
            Message::assistant("reply"),
            Message::user("follow-up"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, message)| Entry {
            seq: index as i64 + 1,
            message,
            images: Vec::new(),
        })
        .collect::<Vec<_>>();
        assert_eq!(compaction_cut(&history, 0, 1), Some(4));
        assert_eq!(compaction_cut(&history[..4], 0, 1), Some(3));
        assert_eq!(compaction_cut(&history[..3], 0, 1), Some(1));
    }
    #[test]
    fn compaction_cuts_between_completed_tool_exchanges_in_one_turn() {
        use rig_core::message::{AssistantContent, AssistantMessage, ToolCall, ToolFunction};
        let exchange = |wire: &str, result: &str| {
            let id = rig_core::message::CallId::from_wire(wire);
            let name: rig_core::message::ToolName = "read_file".try_into().unwrap();
            let call = ToolCall::new(id, ToolFunction::new(name, serde_json::json!({})));
            let id = call.id.clone();
            let name = call.function.name.clone();
            [
                Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                    call,
                )])),
                Message::User {
                    content: vec![rig_core::transcript::tool_result_message(
                        id,
                        name,
                        result.into(),
                    )],
                },
            ]
        };
        let mut messages = vec![Message::user("inspect multiple files")];
        messages.extend(exchange("c1", "first large result"));
        messages.extend(exchange("c2", "second large result"));
        let history = messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| Entry {
                seq: index as i64 + 1,
                message,
                images: Vec::new(),
            })
            .collect::<Vec<_>>();
        let keep = estimated_tokens(&history[3]) + estimated_tokens(&history[4]);
        assert_eq!(compaction_cut(&history, 0, keep), Some(3));
    }
    #[test]
    fn compaction_cannot_cut_only_the_memory_and_summary_prefix() {
        let history = [7, 7, 8, 9].map(|seq| Entry {
            seq,
            message: Message::user("entry"),
            images: Vec::new(),
        });
        assert_eq!(compaction_cut(&history[..3], 7, 0), None);
        assert_eq!(compaction_cut(&history, 7, 0), Some(3));
    }

    #[tokio::test]
    async fn stored_image_reference_hydrates_only_while_available_and_supported() {
        let (_dir, app, project) = super::super::tests::fixture().await;
        let resource = crate::multimodal::fixture_resource(&app.dir, &project.id, "image-message");
        let root = std::path::Path::new(resource["cache_root"].as_str().unwrap());
        let image = ImageRef::from_resource(&project.id, "image-message", &resource, root).unwrap();
        let key = super::super::channel::LOCAL;
        let load = || app.db.sync(|c| super::load(c, &project.id, key)).unwrap();
        app.db
            .sync(|c| {
                open(c, &project.id, key)?;
                append(
                    c,
                    &project.id,
                    key,
                    &Message::user("attachment"),
                    std::slice::from_ref(&image),
                )?;
                let (message, refs): (String, String) = c.query_row(
                    "SELECT message_json,images_json FROM manager_session_messages",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                assert!(!message.contains("base64"));
                assert!(!refs.contains("base64"));
                assert!(refs.contains(&image.sha256));
                Ok(())
            })
            .unwrap();
        let hydrated = hydrate(load(), true).await.unwrap();
        assert!(
            matches!(&hydrated[0], Message::User { content } if matches!(content.last(), Some(UserContent::Image(_))))
        );
        let unsupported = hydrate(load(), false).await.unwrap();
        std::fs::remove_file(&image.path).unwrap();
        let unavailable = hydrate(load(), true).await.unwrap();
        let text = |messages: &[Message]| serde_json::to_string(messages).unwrap();
        assert!(
            text(&unsupported)
                .contains("[Image omitted: configured model has no declared image capability]")
        );
        assert!(text(&unavailable).contains("[Image unavailable]"));
    }
}
