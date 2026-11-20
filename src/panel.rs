//! Read-only activity views over durable inbox, outbox and execution evidence.
use anyhow::{Context, Result};
use chrono::{FixedOffset, Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, Row, ToSql, named_params, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

const ANALYSIS: &str = include_str!("panel/analysis.sql");
const HOUR: i64 = 3_600_000;

#[derive(Clone, Default, Deserialize)]
pub(crate) struct Query {
    project_id: Option<String>,
    chat_id: Option<String>,
    range: Option<String>,
    tz_offset_minutes: Option<i32>,
    filter: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
    before_ms: Option<i64>,
    before_id: Option<String>,
    bucket_start_ms: Option<i64>,
    bucket_end_ms: Option<i64>,
}

struct Window {
    start: i64,
    width: i64,
    count: usize,
    offset: FixedOffset,
}

impl Window {
    fn new(query: &Query, now: i64) -> Result<Self> {
        let (count, width) = match query.range.as_deref().unwrap_or("24h") {
            "24h" => (24, HOUR),
            "7d" => (7, 24 * HOUR),
            "30d" => (30, 24 * HOUR),
            _ => anyhow::bail!("Activity range must be 24h, 7d, or 30d"),
        };
        let minutes = query
            .tz_offset_minutes
            .unwrap_or_else(|| Local::now().offset().local_minus_utc() / 60);
        let offset = minutes
            .checked_mul(60)
            .and_then(FixedOffset::east_opt)
            .context("Invalid local time zone offset")?;
        let offset_ms = i64::from(offset.local_minus_utc()) * 1000;
        let aligned = (now + offset_ms).div_euclid(width) * width - offset_ms;
        Ok(Self {
            start: aligned - (count as i64 - 1) * width,
            width,
            count,
            offset,
        })
    }

    fn bucket(&self, ts: i64) -> Option<usize> {
        usize::try_from(ts.saturating_sub(self.start).div_euclid(self.width))
            .ok()
            .filter(|idx| *idx < self.count)
    }

    fn series(&self) -> Vec<Bucket> {
        (0..self.count)
            .map(|idx| {
                let start = self.start + idx as i64 * self.width;
                let format = if self.width == HOUR { "%H:%M" } else { "%m-%d" };
                let label = self
                    .offset
                    .timestamp_millis_opt(start)
                    .single()
                    .map(|time| time.format(format).to_string())
                    .unwrap_or_default();
                Bucket {
                    bucket_start_ms: start,
                    bucket_end_ms: start + self.width,
                    label,
                    ..Bucket::default()
                }
            })
            .collect()
    }
}

#[derive(Serialize, Default)]
struct Bucket {
    bucket_start_ms: i64,
    bucket_end_ms: i64,
    label: String,
    user_in: usize,
    out_reply: usize,
    out_receipt_or_notice: usize,
    anomaly_count: usize,
    median_first_reply_ms: Option<i64>,
    #[serde(skip)]
    first_replies: Vec<i64>,
}

#[derive(Serialize, Default)]
struct Breakdown {
    stuck_resource: usize,
    held_switch_or_pause: usize,
    failed_or_uncertain: usize,
    unanswered_closed: usize,
    routing_pending: usize,
    stuck_queue: usize,
}

#[derive(Serialize, Default)]
struct Kpis {
    user_in: usize,
    group_in: usize,
    p2p_in: usize,
    needs_action: usize,
    needs_action_breakdown: Breakdown,
    bot_out_sent: usize,
    bot_out_reply: usize,
    bot_out_receipt_or_notice: usize,
    median_first_reply_ms: Option<i64>,
    p90_first_reply_ms: Option<i64>,
    median_worker_queue_ms: Option<i64>,
}

fn median(samples: &mut [i64]) -> Option<i64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let middle = samples.len() / 2;
    let hi = samples[middle];
    Some(if samples.len().is_multiple_of(2) {
        let lo = samples[middle - 1];
        lo + (hi - lo) / 2
    } else {
        hi
    })
}

// Every surface uses the same SQL classifier. Only list/detail load previews or
// content, and a detail request never needs historical manager checkpoints.
struct Read<'a> {
    query: &'a Query,
    now: i64,
    start: Option<i64>,
    id: Option<&'a str>,
    include_out: bool,
}

impl Read<'_> {
    fn run(
        &self,
        connection: &Connection,
        suffix: &str,
        extra: &[(&str, &dyn ToSql)],
        mut visit: impl FnMut(&Row<'_>) -> Result<()>,
    ) -> Result<()> {
        let sql = format!("{ANALYSIS}{suffix}");
        let mut statement = connection.prepare(&sql)?;
        let core: &[(&str, &dyn ToSql)] = &[
            (":project", &self.query.project_id),
            (":chat", &self.query.chat_id),
            (":now", &self.now),
            (":start", &self.start),
            (":id", &self.id),
            (":include_out", &self.include_out),
        ];
        for (key, value) in core.iter().chain(extra) {
            statement.raw_bind_parameter(
                statement
                    .parameter_index(key)?
                    .context("Missing activity query parameter")?,
                value,
            )?;
        }
        let mut rows = statement.raw_query();
        while let Some(row) = rows.next()? {
            visit(row)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ChatStats {
    messages: usize,
    pending: usize,
}

pub(crate) fn overview(
    c: &Connection,
    runtime: &HashMap<String, Value>,
    query: &Query,
    now: i64,
) -> Result<Value> {
    let window = Window::new(query, now)?;
    let mut series = window.series();
    let mut kpis = Kpis::default();
    let mut project_needs_action = BTreeMap::<String, usize>::new();
    let mut chat_stats = HashMap::<String, ChatStats>::new();
    let mut replies = Vec::new();
    let mut queues = Vec::new();
    let read = Read {
        query,
        now,
        start: None,
        id: None,
        include_out: true,
    };
    read.run(
        c,
        "
        SELECT project_id,chat_id,chat_kind,direction,status,outbound_kind,activity_at,
               closure_state,closure_reason,needs_action,first_reply_ms,queue_ms
        FROM panel_rows
        WHERE needs_action OR activity_at>=:now-2678400000",
        &[],
        |row| {
            let direction = row.get_ref("direction")?.as_str()?;
            let status = row.get_ref("status")?.as_str()?;
            let chat_id = row.get_ref("chat_id")?.as_str()?;
            let stats = chat_stats.entry(chat_id.to_owned()).or_default();
            let needs_action: bool = row.get("needs_action")?;
            if needs_action {
                let project: String = row.get("project_id")?;
                *project_needs_action.entry(project).or_default() += 1;
                kpis.needs_action += 1;
                stats.pending += 1;
                let breakdown = &mut kpis.needs_action_breakdown;
                match row.get_ref("closure_reason")?.as_str()? {
                    "resource_timeout" => breakdown.stuck_resource += 1,
                    "routing_timeout" => breakdown.routing_pending += 1,
                    "manager_queue_timeout" | "native_queue_timeout" => breakdown.stuck_queue += 1,
                    "no_reply" | "receipt_only" => breakdown.unanswered_closed += 1,
                    "cli_switch" | "paused" | "takeover" => breakdown.held_switch_or_pause += 1,
                    _ => breakdown.failed_or_uncertain += 1,
                }
            }
            let at: i64 = row.get("activity_at")?;
            let Some(idx) = window.bucket(at).filter(|_| at <= now) else {
                return Ok(());
            };
            let bucket = &mut series[idx];
            if direction == "in" {
                stats.messages += 1;
                kpis.user_in += 1;
                bucket.user_in += 1;
                match row.get_ref("chat_kind")?.as_str()? {
                    "group" => kpis.group_in += 1,
                    "p2p" => kpis.p2p_in += 1,
                    _ => {}
                }
                if matches!(
                    row.get_ref("closure_state")?.as_str()?,
                    "failed_or_uncertain" | "stuck"
                ) {
                    bucket.anomaly_count += 1;
                }
                if let Some(ms) = row.get::<_, Option<i64>>("first_reply_ms")? {
                    replies.push(ms);
                    bucket.first_replies.push(ms);
                }
                if let Some(ms) = row.get::<_, Option<i64>>("queue_ms")? {
                    queues.push(ms);
                }
            } else if status == "sent" {
                stats.messages += 1;
                kpis.bot_out_sent += 1;
                if row.get_ref("outbound_kind")?.as_str()? == "reply" {
                    kpis.bot_out_reply += 1;
                    bucket.out_reply += 1;
                } else {
                    kpis.bot_out_receipt_or_notice += 1;
                    bucket.out_receipt_or_notice += 1;
                }
            }
            Ok(())
        },
    )?;
    kpis.median_first_reply_ms = median(&mut replies);
    kpis.p90_first_reply_ms = replies
        .get((replies.len() * 9).div_ceil(10).saturating_sub(1))
        .copied();
    kpis.median_worker_queue_ms = median(&mut queues);
    for bucket in &mut series {
        bucket.median_first_reply_ms = median(&mut bucket.first_replies);
    }
    let chats = chats(c, runtime, query, &chat_stats)?;
    Ok(
        json!({"kpis": kpis, "series": series, "chats": chats, "project_needs_action": project_needs_action}),
    )
}

#[derive(Serialize)]
struct Bot {
    project_id: String,
    project_name: String,
    agent: String,
    app_id: String,
    bound: bool,
    ws_status: String,
    ws_error: Option<&'static str>,
    identity_status: &'static str,
    source_kind: String,
}

#[derive(Serialize)]
struct Chat {
    chat_id: String,
    kind: String,
    display_name: String,
    name_missing: bool,
    last_seen_at: i64,
    message_count_in_range: usize,
    unhandled_count: usize,
    members_status: String,
    bots: Vec<Bot>,
}

fn chats(
    c: &Connection,
    runtime: &HashMap<String, Value>,
    query: &Query,
    stats: &HashMap<String, ChatStats>,
) -> Result<Vec<Chat>> {
    let mut statement = c.prepare(include_str!("panel/chats.sql"))?;
    let mut rows =
        statement.query(named_params! {":project":query.project_id,":chat":query.chat_id})?;
    let mut by_id = BTreeMap::<String, Chat>::new();
    while let Some(row) = rows.next()? {
        let chat_id: String = row.get("chat_id")?;
        // Keep direct-user destinations distinct from observed chats unless the
        // transport recorded their canonical chat ID in a successful receipt.
        let chat = match by_id.entry(chat_id.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let name: Option<String> = row.get("name")?;
                let counts = stats.get(&chat_id);
                entry.insert(Chat {
                    chat_id: chat_id.clone(),
                    kind: row.get("kind")?,
                    name_missing: name.is_none(),
                    display_name: name.unwrap_or(chat_id),
                    last_seen_at: 0,
                    message_count_in_range: counts.map_or(0, |stats| stats.messages),
                    unhandled_count: counts.map_or(0, |stats| stats.pending),
                    members_status: row.get("members_status")?,
                    bots: Vec::new(),
                })
            }
        };
        chat.last_seen_at = chat.last_seen_at.max(row.get("last_seen_at")?);
        let project_id: String = row.get("project_id")?;
        let app_id: Option<String> = row.get("app_id")?;
        let bound = app_id.is_some();
        let app_id = app_id.unwrap_or_default();
        let state = runtime
            .get(&project_id)
            .filter(|state| state["app_id"] == app_id);
        let ws_status = if bound {
            state
                .and_then(|state| state["status"].as_str())
                .unwrap_or("disconnected")
        } else {
            "binding_missing"
        };
        // Keep diagnostics to stable codes instead of copying transport errors.
        let ws_error = state
            .and_then(|state| state["error"].as_str())
            .filter(|error| !error.is_empty())
            .map(|_| "connection_error");
        let confirmed: bool = row.get("identity_confirmed")?;
        let failed: bool = row.get("identity_failed")?;
        chat.bots.push(Bot {
            project_id,
            project_name: row.get("project_name")?,
            agent: row.get("agent")?,
            app_id,
            bound,
            ws_status: ws_status.to_owned(),
            ws_error,
            identity_status: if !bound {
                "unbound"
            } else if confirmed {
                "confirmed"
            } else if failed {
                "failed"
            } else {
                "pending"
            },
            source_kind: row.get("source_kind")?,
        });
    }
    let mut chats: Vec<_> = by_id.into_values().collect();
    chats.sort_by(|a, b| {
        (b.unhandled_count > 0)
            .cmp(&(a.unhandled_count > 0))
            .then_with(|| b.last_seen_at.cmp(&a.last_seen_at))
            .then_with(|| a.chat_id.cmp(&b.chat_id))
    });
    Ok(chats)
}

const FIELDS: &str = include_str!("panel/fields.sql");

pub(crate) fn messages(c: &Connection, query: &Query, now: i64) -> Result<Value> {
    let window = Window::new(query, now)?;
    let (needs, include_out) = match query.filter.as_deref().unwrap_or("all_user") {
        "all_user" => (false, false),
        "needs_action" => (true, false),
        "with_outbound" => (false, true),
        _ => anyhow::bail!("Invalid activity filter"),
    };
    let limit = query.limit.unwrap_or(80).clamp(1, 200);
    // The current needs-action KPI includes older unresolved messages.
    let start = query.bucket_start_ms.or((!needs).then_some(window.start));
    let read = Read {
        query,
        now,
        start,
        id: None,
        include_out,
    };
    let suffix = format!("{} {FIELDS}", include_str!("panel/feed.sql"));
    let search = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_ascii_lowercase);
    let row_limit = limit as i64;
    let mut items = Vec::new();
    let mut tail = None;
    read.run(
        c,
        &suffix,
        &[
            (":needs", &needs),
            (":bucket_end", &query.bucket_end_ms),
            (":before", &query.before_ms),
            (":before_id", &query.before_id),
            (":search", &search),
            (":limit", &row_limit),
        ],
        |row| {
            items.push(feed_row(row)?);
            tail = Some((
                row.get::<_, i64>("activity_at")?,
                row.get::<_, String>("id")?,
            ));
            Ok(())
        },
    )?;
    let next = (items.len() == limit).then_some(tail).flatten();
    Ok(
        json!({"items":items, "next_before_ms":next.as_ref().map(|(ts,_)|ts), "next_before_id":next.as_ref().map(|(_,id)|id)}),
    )
}

/// Proposal evidence uses the same destination scope and closure classifier as Activity.
pub(crate) fn proposal_context(
    c: &Connection,
    project: &str,
    chat: &str,
    now: i64,
) -> Result<Value> {
    let select = |filter: &str, limit| -> Result<Vec<Value>> {
        let rows = messages(
            c,
            &Query {
                project_id: Some(project.to_owned()),
                chat_id: Some(chat.to_owned()),
                range: Some("30d".into()),
                filter: Some(filter.to_owned()),
                limit: Some(limit),
                ..Query::default()
            },
            now,
        )?;
        Ok(rows["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| {
                json!({
                    "preview": row["preview"],
                    "created_at": row["created_at"],
                    "closure_state": row["closure_state"],
                    "closure_reason": row["closure_reason"],
                })
            })
            .collect())
    };
    Ok(json!({"recent_input":select("all_user",24)?,"unresolved":select("needs_action",12)?}))
}

fn preview(content: &str, mentions: &Value) -> String {
    let mut result = content.to_owned();
    if let Some(mentions) = mentions.as_array() {
        for mention in mentions {
            if let Some(key) = mention["key"].as_str().filter(|key| {
                key.starts_with("@_user_") && key[7..].bytes().all(|ch| ch.is_ascii_digit())
            }) {
                result = result.replace(key, "");
            }
        }
    }
    // Older cards did not preserve their mention manifest.
    result
        .split_whitespace()
        .filter(|word| {
            !word.strip_prefix("@_user_").is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|ch| ch.is_ascii_digit())
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn parse_fields(row: &Row<'_>, name: &str) -> Result<Value> {
    Ok(serde_json::from_str(row.get_ref(name)?.as_str()?).unwrap_or_default())
}

fn feed_row(row: &Row<'_>) -> Result<Value> {
    let id = row.get_ref("id")?.as_str()?;
    let chat_id = row.get_ref("chat_id")?.as_str()?;
    let chat_name: Option<String> = row.get("chat_name")?;
    let resources = parse_fields(row, "resources")?;
    let resources = resources.as_array().map_or(&[][..], Vec::as_slice);
    let image_count = resources
        .iter()
        .filter(|entry| entry["kind"] == "image")
        .count();
    let file_names: Vec<_> = resources
        .iter()
        .filter(|entry| entry["kind"] != "image")
        .filter_map(|entry| entry["file_name"].as_str())
        .map(|name| name.chars().take(240).collect::<String>())
        .collect();
    let message_type = row.get_ref("message_type")?.as_str()?;
    let content = row.get_ref("preview")?.as_str()?;
    let reply_preview: Option<String> = row.get("reply_preview")?;
    Ok(json!({
        "id":id,"project_id":row.get_ref("project_id")?.as_str()?,
        "project_name":row.get_ref("project_name")?.as_str()?,
        "direction":row.get_ref("direction")?.as_str()?,
        "source_id":row.get_ref("source_id")?.as_str()?,
        "chat_id":chat_id,"chat_name":chat_name.as_deref().unwrap_or(chat_id),
        "chat_kind":row.get_ref("chat_kind")?.as_str()?,
        "created_at":row.get::<_,i64>("created_at")?,"updated_at":row.get::<_,i64>("updated_at")?,
        "activity_at":row.get::<_,i64>("activity_at")?,
        "status":row.get_ref("status")?.as_str()?,
        "outbound_kind":row.get_ref("outbound_kind")?.as_str()?,
        "preview":preview(content,&parse_fields(row,"mentions")?),
        "closure_state":row.get::<_,Option<String>>("closure_state")?,
        "closure_reason":row.get_ref("closure_reason")?.as_str()?,
        "needs_action":row.get::<_,bool>("needs_action")?,
        "badges":{
            "image_count":image_count,"file_names":file_names,
            "resource_failed":resources.iter().any(|entry| entry["error"].as_str().is_some_and(|v| !v.is_empty())),
            "topic_reply":row.get::<_,bool>("topic_reply")?,
            "mention_all":row.get::<_,bool>("mention_all")? || content.split_whitespace().any(|word| word=="@_all"),
            "card":matches!(message_type,"card"|"interactive"),
            "control":row.get::<_,bool>("is_control")?,
        },
        "reply_summary":{
            "count":row.get::<_,i64>("reply_count")?,
            "receipt_count":row.get::<_,i64>("receipt_count")?,
            "first_reply_ms":row.get::<_,Option<i64>>("first_reply_ms")?,
            "preview":reply_preview.map(|body| preview(&body,&Value::Null)),
        }
    }))
}

pub(crate) fn message_detail(c: &Connection, id: &str, now: i64) -> Result<Option<Value>> {
    let project = if id.starts_with("intake:") {
        c.query_row("SELECT project_id FROM feishu_group_intake WHERE 'intake:'||project_id||':'||app_id||':'||source_id=? AND status='pending'",
            [id], |row| row.get::<_, String>(0)).optional()?
    } else {
        c.query_row("SELECT project_id FROM messages WHERE id=?", [id], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
    };
    let Some(project) = project else {
        return Ok(None);
    };
    let query = Query {
        project_id: Some(project),
        ..Query::default()
    };
    let read = Read {
        query: &query,
        now,
        start: None,
        id: Some(id),
        include_out: true,
    };
    let suffix = format!(", chosen AS (SELECT * FROM panel_rows) {FIELDS}");
    let mut detail = None;
    read.run(c, &suffix, &[], |row| {
        let mut item = feed_row(row)?;
        let direction = row.get_ref("direction")?.as_str()?;
        let status = row.get_ref("status")?.as_str()?;
        let event_id: Option<String> = row.get("event_id")?;
        let event_status: Option<String> = row.get("event_status")?;
        let routing: bool = row.get("routing")?;
        let event_recoverable = matches!(event_status.as_deref(), Some("failed" | "uncertain"));
        let message_recoverable = !routing
            && matches!(status, "failed" | "uncertain")
            && (direction == "out" || event_id.is_none() || event_recoverable);
        let (content, source) = message_content(c, row, routing)?;
        let resources = parse_fields(row, "resources")?;
        let resources = resources.as_array().map_or(&[][..], Vec::as_slice);
        let resources: Vec<_> = resources.iter().map(resource_metadata).collect();
        let supports_images: bool = c
            .query_row(
                "SELECT supports_images!=0 FROM manager_provider_config WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        item["content"] = json!(content);
        item["source"] = source;
        item["resources"] = json!(resources);
        item["resources_ready"] = json!(!row.get::<_, bool>("waiting_resource")? && !routing);
        item["supports_images"] = json!(supports_images);
        item["control_phase"] = json!(row.get::<_, Option<String>>("command_phase")?);
        item["recovery"] = json!({
            "message_id":message_recoverable.then_some(id),
            "event_id":event_id.filter(|_|event_recoverable),
            "can_retry":event_recoverable || message_recoverable,
            "can_dismiss":event_recoverable || message_recoverable,
        });
        detail = Some(item);
        Ok(())
    })?;
    if let Some(item) = &mut detail {
        let mut timeline = Vec::new();
        // The base classifier and the detail timeline share the same source,
        // delegated-event and reply attribution rules.
        read.run(c, include_str!("panel/timeline.sql"), &[], |row| {
            timeline.push(json!({"kind":row.get_ref("kind")?.as_str()?,
                "id":row.get_ref("id")?.as_str()?,"status":row.get_ref("status")?.as_str()?,
                "at_ms":row.get::<_,i64>("at_ms")?}));
            Ok(())
        })?;
        item["timeline"] = json!(timeline);
    }
    Ok(detail)
}

fn message_content(c: &Connection, row: &Row<'_>, routing: bool) -> Result<(String, Value)> {
    let id = row.get_ref("id")?.as_str()?;
    let (content, raw): (String, String) = if routing {
        c.query_row(
            "SELECT content,COALESCE(json_extract(attachment,'$.source'),'{}')
            FROM feishu_group_intake WHERE 'intake:'||project_id||':'||app_id||':'||source_id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
    } else {
        c.query_row("SELECT content,COALESCE(json_extract(attachment,'$.source'),'{}') FROM messages WHERE id=?", [id],
            |row| Ok((row.get(0)?, row.get(1)?)))?
    };
    let source: Value = serde_json::from_str(&raw).unwrap_or_default();
    let message_id = row.get_ref("source_id")?.as_str()?;
    let direction = row.get_ref("direction")?.as_str()?;
    let mut fields = json!({
        "chat_id":row.get_ref("chat_id")?.as_str()?,
        "bot_app_id":row.get_ref("app_id")?.as_str()?,
        "message_id":(direction=="in").then_some(message_id),
        "thread_id":source["thread_id"].as_str(),
        "root_id":source["root_id"].as_str(),
        "parent_id":source["parent_id"].as_str(),
        "group_admission":source["group_admission"].as_str(),
        "sender":{
            "open_id":source["sender"]["open_id"].as_str(),
            "user_id":source["sender"]["user_id"].as_str(),
            "union_id":source["sender"]["union_id"].as_str(),
            "name":source["sender"]["name"].as_str(),
        }
    });
    if direction == "out" {
        let (message_id, thread_id, root_id, parent_id) = c.query_row(
            "SELECT json_extract(attachment,'$.sent_message_id'),json_extract(attachment,'$.sent_thread_id'),
              json_extract(attachment,'$.sent_root_id'),COALESCE(json_extract(attachment,'$.resolved_reply_to_message_id'),
              json_extract(attachment,'$.reply_to_message_id')) FROM messages WHERE id=?", params![id],
              |row| Ok((row.get::<_,Option<String>>(0)?, row.get::<_,Option<String>>(1)?,
                      row.get::<_,Option<String>>(2)?, row.get::<_,Option<String>>(3)?)),
        )?;
        fields["message_id"] = json!(message_id);
        fields["thread_id"] = json!(thread_id);
        fields["root_id"] = json!(root_id);
        fields["parent_id"] = json!(parent_id);
    }
    Ok((content, fields))
}

fn resource_metadata(resource: &Value) -> Value {
    let mut metadata = serde_json::Map::new();
    for field in [
        "file_key",
        "resource_key",
        "file_name",
        "kind",
        "local_path",
        "sha256",
        "media_type",
    ] {
        if let Some(value) = resource[field].as_str() {
            metadata.insert(field.to_owned(), json!(value));
        }
    }
    for field in ["bytes", "width", "height"] {
        if let Some(value) = resource[field].as_u64() {
            metadata.insert(field.to_owned(), json!(value));
        }
    }
    if resource["error"]
        .as_str()
        .is_some_and(|error| !error.is_empty())
    {
        metadata.insert("error".to_owned(), json!("resource_download_failed"));
    }
    Value::Object(metadata)
}

#[cfg(test)]
mod tests;
