use crate::{App, bot::Binding, config::Config, db};
use anyhow::{Context, Result, bail};
use open_lark::ws_client::{
    CardActionTrigger, CardActionTriggerHandler, CardActionTriggerResponse, CardToast,
    EventDispatcherHandler, EventHandler, LarkWsClient,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

pub const MAX_FILE: u64 = 100_000_000;

#[derive(Default)]
pub struct TokenCache {
    key: String,
    token: String,
    until: Option<Instant>,
}

pub async fn token(app: &App, binding: &Binding) -> Result<String> {
    let cfg = app.config.read().await.clone();
    if binding.app_id.is_empty() || binding.app_secret.is_empty() {
        bail!("Configure Feishu app credentials first");
    }
    let key = format!(
        "{}:{}:{}",
        binding.app_id, binding.app_secret, cfg.feishu_base
    );
    {
        let tokens = app.token.lock().await;
        if let Some(cache) = tokens.get(&binding.project_id)
            && cache.key == key
            && cache.until.is_some_and(|t| t > Instant::now())
        {
            return Ok(cache.token.clone());
        }
    }
    let response = app
        .http
        .post(format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            cfg.feishu_base
        ))
        .json(&json!({"app_id":binding.app_id,"app_secret":binding.app_secret}))
        .send()
        .await?;
    let v = api_json(response).await?;
    let mut tokens = app.token.lock().await;
    let cache = tokens.entry(binding.project_id.clone()).or_default();
    cache.key = key;
    cache.token = v["tenant_access_token"]
        .as_str()
        .context("Feishu did not return tenant_access_token")?
        .into();
    cache.until = Some(
        Instant::now()
            + Duration::from_secs(
                v["expire"]
                    .as_u64()
                    .unwrap_or(3600)
                    .saturating_sub(120)
                    .max(1),
            ),
    );
    Ok(cache.token.clone())
}

pub async fn api_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let v: Value = response
        .json()
        .await
        .context("Feishu response is not JSON")?;
    if !status.is_success() || v["code"].as_i64() != Some(0) {
        bail!(
            "Feishu API {} / code {}: {}",
            status,
            v["code"],
            v["msg"].as_str().unwrap_or("Request failed")
        );
    }
    Ok(v)
}

#[derive(Debug)]
pub struct SendReceipt {
    pub message_id: String,
    pub chat_id: String,
    pub thread_id: String,
    pub root_id: String,
}
impl SendReceipt {
    fn from_response(value: &Value) -> Result<Self> {
        let data = &value["data"];
        Ok(Self {
            message_id: data["message_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("Missing message_id in the response")?
                .into(),
            chat_id: data["chat_id"].as_str().unwrap_or("").into(),
            thread_id: data["thread_id"].as_str().unwrap_or("").into(),
            root_id: data["root_id"].as_str().unwrap_or("").into(),
        })
    }
}

pub(crate) fn ensure_card(card: &Value) -> Result<()> {
    anyhow::ensure!(
        card["schema"] == "2.0" && card["body"]["elements"].is_array(),
        "Invalid interactive card"
    );
    anyhow::ensure!(
        serde_json::to_vec(card)?.len() <= 28_000,
        "Interactive card exceeds the supported size"
    );
    Ok(())
}

pub async fn send(app: &App, p: &db::Project, m: &db::Message) -> Result<SendReceipt> {
    let cfg = app.config.read().await.clone();
    let binding = app.db.bot(&m.project_id).await?;
    if m.attachment["bot_app_id"]
        .as_str()
        .is_some_and(|id| id != binding.app_id)
    {
        bail!(
            "This message belongs to the previous bot; review it before sending with a different bot"
        );
    }
    let token = token(app, &binding).await?;
    let to = m.attachment["to"].as_str().unwrap_or(&p.chat_id);
    let receive_id_type = recipient_type(to)?;
    let (kind, content) = if let Some(card) = m.attachment.get("interactive_card") {
        ensure_card(card)?;
        ("interactive", card.clone())
    } else if let Some(path) = m.attachment["path"].as_str() {
        let path = allowed_file(p, Path::new(path), &app.dir)?;
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.len() == 0 || metadata.len() > MAX_FILE {
            bail!("File size must be greater than 0 and at most 100 MB");
        }
        let file = tokio::fs::File::open(&path).await?;
        let filename = path
            .file_name()
            .context("File name is invalid")?
            .to_string_lossy()
            .into_owned();
        let part = reqwest::multipart::Part::stream_with_length(
            reqwest::Body::wrap_stream(ReaderStream::new(file)),
            metadata.len(),
        )
        .file_name(filename.clone());
        let form = reqwest::multipart::Form::new()
            .text("file_type", "stream")
            .text("file_name", filename)
            .part("file", part);
        let v = api_json(
            app.http
                .post(format!("{}/open-apis/im/v1/files", cfg.feishu_base))
                .bearer_auth(&token)
                .multipart(form)
                .send()
                .await?,
        )
        .await?;
        (
            "file",
            json!({"file_key":v["data"]["file_key"].as_str().context("Missing file_key in the response")?}),
        )
    } else {
        if m.content.len() > 24_000 {
            bail!("Markdown exceeds 24000 bytes; send it as a file");
        }
        let mut paragraph = Vec::new();
        if let Some(mention) = m.attachment["discussion_mention"].as_str()
            && mention.starts_with("ou_")
        {
            paragraph.push(json!({"tag":"at","user_id":mention}));
        }
        paragraph.push(json!({"tag":"md","text":m.content}));
        ("post", json!({"zh_cn":{"title":"","content":[paragraph]}}))
    };
    let Some(anchor) = m.attachment["reply_to_message_id"].as_str() else {
        let body =
            json!({"receive_id":to,"msg_type":kind,"content":content.to_string(),"uuid":m.id});
        let value = api_json(
            app.http
                .post(format!("{}/open-apis/im/v1/messages", cfg.feishu_base))
                .query(&[("receive_id_type", receive_id_type)])
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await?,
        )
        .await?;
        return SendReceipt::from_response(&value);
    };
    let body =
        json!({"msg_type":kind,"content":content.to_string(),"uuid":m.id,"reply_in_thread":true});
    let v = api_json(
        app.http
            .post(format!(
                "{}/open-apis/im/v1/messages/{anchor}/reply",
                cfg.feishu_base
            ))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    let receipt = SendReceipt::from_response(&v)?;
    if receipt.thread_id.is_empty() {
        // Acceptance without a topic is a platform/protocol mismatch, not a
        // safe retry or an invitation to send an ordinary message.
        let id = m.id.clone();
        let sent = receipt.message_id.clone();
        app.db.call(move |c| {
            c.execute("UPDATE messages SET attachment=json_set(attachment,'$.sent_message_id',?),updated_at=? WHERE id=?",
                rusqlite::params![sent,db::now(),id])?;
            Ok(())
        }).await?;
        bail!(
            "Topic reply was accepted without thread_id; verify the recorded message before retrying"
        );
    }
    Ok(receipt)
}

fn debug_reply_body(text: &str) -> Value {
    json!({"msg_type":"text","content":json!({"text":text}).to_string(),"reply_in_thread":true})
}

fn debug_reply_parts(prompt: &str) -> Vec<Value> {
    const BODY_LIMIT: usize = 60_000;
    // Leave room for both JSON wrappers and the part number.
    const TEXT_BUDGET: usize = BODY_LIMIT - 256;
    let single = debug_reply_body(prompt);
    if single.to_string().len() <= BODY_LIMIT {
        return vec![single];
    }

    let mut chunks = Vec::new();
    let mut remaining = prompt;
    while !remaining.is_empty() {
        let mut bytes = 0;
        let mut end = remaining.len();
        let mut newline = 0;
        for (index, ch) in remaining.char_indices() {
            bytes += match ch {
                '"' | '\\' => 4,
                ch if ch.is_ascii_control() => 7,
                ch => ch.len_utf8(),
            };
            if bytes > TEXT_BUDGET {
                end = if newline > 0 { newline } else { index };
                break;
            }
            if ch == '\n' {
                newline = index + 1;
            }
        }
        chunks.push(&remaining[..end]);
        remaining = &remaining[end..];
    }
    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, text)| debug_reply_body(&format!("[debug {}/{}]\n{text}", index + 1, total)))
        .collect()
}

/// Echo a prompt without writing its message, outbox or worker observation to the store.
/// Feishu clients may still render markup present in the original plain text.
pub async fn send_debug_reply(
    app: &App,
    project: &str,
    bot_app_id: &str,
    to: &str,
    anchor: &str,
    prompt: &str,
) -> Result<()> {
    recipient_type(to)?;
    anyhow::ensure!(
        anchor.starts_with("om_")
            && anchor.len() <= 200
            && anchor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
        "Invalid debug reply anchor"
    );
    let parts = debug_reply_parts(prompt);
    let total = parts.len();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let authorization = tokio::time::timeout_at(deadline, async {
        let binding = app.db.bot(project).await?;
        anyhow::ensure!(binding.app_id == bot_app_id, "Feishu bot changed");
        let base = app.config.read().await.feishu_base.clone();
        let token = token(app, &binding).await?;
        Ok::<_, anyhow::Error>((base, token))
    })
    .await;
    let Ok(Ok((base, token))) = authorization else {
        bail!("Debug reply stopped after 0/{total} parts: Feishu authorization unavailable");
    };

    for (sent, body) in parts.into_iter().enumerate() {
        let response = tokio::time::timeout_at(deadline, async {
            let response = app
                .http
                .post(format!("{base}/open-apis/im/v1/messages/{anchor}/reply"))
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Feishu transport failed"))?;
            let status = response.status();
            let data = api_json(response)
                .await
                .map_err(|_| anyhow::anyhow!("Feishu API rejected the reply (HTTP {status})"))?;
            let receipt = SendReceipt::from_response(&data)?;
            anyhow::ensure!(
                !receipt.thread_id.is_empty(),
                "Feishu did not confirm a topic reply"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await;
        match response {
            Ok(Ok(())) => {}
            Ok(Err(error)) => bail!("Debug reply stopped after {sent}/{total} parts: {error}"),
            Err(_) => bail!("Debug reply stopped after {sent}/{total} parts: timed out"),
        }
    }
    Ok(())
}

pub fn allowed_file(p: &db::Project, path: &Path, data_dir: &Path) -> Result<PathBuf> {
    let absolute = std::fs::canonicalize(if path.is_absolute() {
        path.to_owned()
    } else {
        Path::new(&p.path).join(path)
    })?;
    let project = std::fs::canonicalize(&p.path)?;
    let cache = std::fs::canonicalize(data_dir.join("files"))?;
    if !absolute.is_file() || (!absolute.starts_with(project) && !absolute.starts_with(cache)) {
        bail!("File must resolve inside the project directory or Dauerbot cache");
    }
    Ok(absolute)
}

pub fn recover_resource_preparation(c: &rusqlite::Connection, recover: bool) -> Result<()> {
    if recover {
        c.execute("UPDATE messages SET attachment=json_remove(attachment,'$.resource_preparation') WHERE direction='in' AND json_extract(attachment,'$.resource_preparation') IS NOT NULL AND COALESCE(json_extract(attachment,'$.resources_ready'),0)!=1",[])?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct ResourcePreparation {
    pub token: String,
    pub message_id: String,
    pub source_id: String,
    pub bot_app_id: String,
    quoted_resource_message_id: Option<String>,
}

// Acknowledged messages still need resources while a live manager input
// or an outstanding delegation owns their original inbound work event.
const RESOURCE_ELIGIBLE: &str = "EXISTS(
    SELECT 1 FROM projects p WHERE p.id=m.project_id AND p.paused=0
) AND (
    m.status IN ('pending','queued') OR
    (m.status='acknowledged' AND EXISTS(
        SELECT 1 FROM work_events e
        WHERE e.message_id=m.id AND e.project_id=m.project_id AND e.source='feishu'
        AND (
            (e.status='managed' AND EXISTS(
                SELECT 1 FROM manager_inputs i
                WHERE i.project_id=e.project_id AND i.origin_event_id=e.id
                AND i.status IN ('queued','processing')
            )) OR EXISTS(
                SELECT 1 FROM work_events d
                WHERE d.project_id=e.project_id AND d.source IN ('management','manager_worker')
                AND d.status IN ('queued','worker_waiting') AND json_extract(d.payload,'$.origin_event_id')=e.id
            )
        )
    ))
)";

// Both direct and verified quoted references own their respective Feishu message IDs.
const HAS_RESOURCES: &str = "(json_extract(m.attachment,'$.file_key') IS NOT NULL
    OR json_array_length(m.attachment,'$.resources')>0
    OR json_array_length(m.attachment,'$.quoted_message.resources')>0)";

// Give only the reply waiting for a direct-parent lookup a short head start.
// Unrelated steer items remain eligible; a backed-up quote queue never holds
// its own reply indefinitely.
pub(crate) fn preparation_pending() -> String {
    let quote_cutoff = db::now().saturating_sub(2_000);
    format!("(({HAS_RESOURCES} AND COALESCE(json_extract(m.attachment,'$.resources_ready'),0)!=1)
        OR (json_extract(m.attachment,'$.quote_pending')=1 AND m.created_at>{quote_cutoff}))")
}

fn quote_resource_owner(attachment: &Value) -> Option<&str> {
    let quote = &attachment["quoted_message"];
    quote["resources"].as_array().filter(|items| !items.is_empty())?;
    quote["message_id"].as_str().filter(|id| {
        quote["status"] == "ok" && attachment["source"]["parent_id"] == *id
    })
}

fn direct_resources(attachment: &Value) -> Vec<Value> {
    let mut resources: Vec<_> = attachment["resources"].as_array().into_iter().flatten()
        .filter(|resource| resource["quoted"] != true && resource["frame_seconds"].is_null())
        .cloned().collect();
    if resources.is_empty() && attachment["file_key"].is_string() {
        resources.push(json!({"file_key":attachment["file_key"],"file_name":attachment["file_name"],
            "kind":attachment["kind"],"declared_duration_seconds":attachment["declared_duration_seconds"]}));
    }
    resources
}

pub(crate) fn message_resources(m: &db::Message) -> Vec<Value> {
    attachment_resources(&m.attachment, &m.source_id)
}

pub(crate) fn attachment_resources(attachment: &Value, source_id: &str) -> Vec<Value> {
    if attachment["resources_ready"] == true
        && let Some(resources) = attachment["resources"].as_array()
    {
        return resources.iter().cloned().map(|mut resource| {
            if resource["message_id"].is_null() {
                resource["message_id"] = json!(source_id);
            }
            if resource["chat_id"].is_null() {
                resource["chat_id"] = attachment["source"]["chat_id"].clone();
            }
            resource
        }).collect();
    }
    let mut resources = direct_resources(attachment);
    for resource in &mut resources {
        resource["message_id"] = json!(source_id);
        resource["chat_id"] = attachment["source"]["chat_id"].clone();
    }
    if let Some(parent) = quote_resource_owner(attachment) {
        resources.extend(attachment["quoted_message"]["resources"].as_array().unwrap().iter().map(|resource| {
            let mut resource = resource.clone();
            resource["message_id"] = json!(parent);
            resource["chat_id"] = attachment["source"]["chat_id"].clone();
            resource["quoted"] = json!(true);
            resource
        }));
    }
    resources
}

pub fn begin_resource_preparation(
    c: &rusqlite::Connection,
    message: &db::Message,
) -> Result<Option<ResourcePreparation>> {
    let preparation = ResourcePreparation {
        token: db::id(),
        message_id: message.id.clone(),
        source_id: message.source_id.clone(),
        bot_app_id: message.attachment["bot_app_id"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
        quoted_resource_message_id: quote_resource_owner(&message.attachment).map(str::to_owned),
    };
    let changed = c.execute(&format!("UPDATE messages AS m SET attachment=json_set(attachment,'$.resource_preparation',?) WHERE id=? AND source_id=? AND COALESCE(json_extract(attachment,'$.bot_app_id'),'')=? AND COALESCE(json_extract(attachment,'$.resources_ready'),0)!=1 AND json_extract(attachment,'$.resource_preparation') IS NULL AND {RESOURCE_ELIGIBLE}"),rusqlite::params![preparation.token,preparation.message_id,preparation.source_id,preparation.bot_app_id])?;
    Ok((changed == 1).then_some(preparation))
}

pub fn finish_resource_preparation(
    c: &rusqlite::Connection,
    preparation: &ResourcePreparation,
    resources: &[Value],
    context: &Value,
) -> Result<bool> {
    let mut resources = resources.to_vec();
    let current: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM messages m JOIN project_bots b ON b.project_id=m.project_id WHERE m.id=? AND b.app_id=?)",rusqlite::params![preparation.message_id,preparation.bot_app_id],|r|r.get(0))?;
    if !current {
        for resource in &mut resources {
            if let Some(o) = resource.as_object_mut() {
                o.remove("local_path");
            }
            resource["error"] = json!("The source bot binding changed during resource preparation");
        }
    }
    let context = if current {
        context.clone()
    } else {
        json!({"members_status":"unavailable"})
    };
    let changed=c.execute(&format!("UPDATE messages AS m SET attachment=json_remove(json_set(attachment,'$.resources',json(?),'$.resources_ready',json('true'),'$.resource_context',json(?)),'$.resource_preparation') WHERE id=? AND source_id=? AND COALESCE(json_extract(attachment,'$.bot_app_id'),'')=? AND json_extract(attachment,'$.resource_preparation')=? AND COALESCE(json_extract(attachment,'$.resources_ready'),0)!=1 AND (CASE WHEN json_array_length(attachment,'$.quoted_message.resources')>0
         THEN json_extract(attachment,'$.quoted_message.message_id') ELSE NULL END) IS ?7 AND {RESOURCE_ELIGIBLE}"),rusqlite::params![serde_json::to_string(&resources)?,context.to_string(),preparation.message_id,preparation.source_id,preparation.bot_app_id,preparation.token,preparation.quoted_resource_message_id])?;
    Ok(changed == 1)
}

/// Only the original message ID and keys are downloaded. Preparation never
/// holds the project delivery lock; resource names and captions remain user data.
pub async fn download_resources(app: &App, m: &db::Message) -> Result<Vec<Value>> {
    prepare_resources(app, m, Duration::from_secs(80), crate::video_preview::decoder).await
}

pub(crate) async fn prepare_resources(
    app: &App,
    m: &db::Message,
    timeout: Duration,
    resolve_decoder: fn(&str) -> Option<PathBuf>,
) -> Result<Vec<Value>> {
    let message = m.clone();
    let preparation = app
        .db
        .call(move |c| begin_resource_preparation(c, &message))
        .await?
        .context("Resources are already prepared or owned by another attempt")?;
    let mut resources = message_resources(m);
    let mut frames = Vec::new();
    let mut remaining = MAX_FILE;
    let deadline = tokio::time::Instant::now() + timeout;
    let timeout_reason = format!("Message resource preparation timed out ({} ms total)", timeout.as_millis());
    for (index, resource) in resources.iter_mut().enumerate() {
        if let Some(o) = resource.as_object_mut() {
            o.remove("local_path");
            o.remove("error");
            o.remove("frame_extraction_error");
        }
        if tokio::time::Instant::now() >= deadline {
            resource["error"] = json!(timeout_reason);
            continue;
        }
        if index >= 16 || remaining == 0 {
            resource["error"] =
                json!("Message resource download limit reached (16 resources / 100 MB)");
            continue;
        }
        let downloaded = tokio::time::timeout_at(
            deadline,
            download_resource(app, m, resource, index, &preparation.token, &mut remaining),
        )
        .await;
        match downloaded {
            Ok(Ok(Some(download))) => {
                resource.as_object_mut().unwrap().extend(download.metadata);
                if let Some(video_file) = download.video_file {
                    match tokio::time::timeout_at(deadline,
                        prepare_frame(app, m, resource, video_file, index, &preparation.token, resolve_decoder)).await
                    {
                        Ok(Ok((duration, frame))) => {
                            resource["duration_seconds"] = json!(duration);
                            frames.push(frame);
                        }
                        Ok(Err(error)) => resource["frame_extraction_error"] =
                            json!(error.to_string()),
                        Err(_) => resource["frame_extraction_error"] =
                            json!("Message preparation deadline reached"),
                    }
                }
            }
            Ok(Ok(None)) => resource["error"] = json!("Resource key is missing"),
            Ok(Err(error)) => resource["error"] = json!(error.to_string()),
            Err(_) => {
                resource["error"] =
                    json!(timeout_reason)
            }
        }
    }
    resources.extend(frames);
    let context = tokio::time::timeout_at(deadline, context(app, m))
        .await
        .unwrap_or_else(|_| json!({"members_status":"unavailable"}));
    let prepared = preparation.clone();
    let result = resources.clone();
    let finished = app
        .db
        .call(move |c| finish_resource_preparation(c, &prepared, &result, &context))
        .await;
    if !matches!(finished, Ok(true)) {
        app.db.call(move |c| {
            c.execute("UPDATE messages SET attachment=json_remove(attachment,'$.resource_preparation') WHERE id=? AND json_extract(attachment,'$.resource_preparation')=?",
                rusqlite::params![preparation.message_id, preparation.token])?;
            Ok(())
        }).await?;
        finished?;
        bail!("Resource preparation no longer owns the original source message");
    }
    // Read the committed, binding-fenced result rather than an obsolete local copy.
    Ok(app.db.message(&m.id).await?.attachment["resources"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// A single gated worker owns network preparation. Durable tokens fence restart
/// and handover; unfinished downloads never claim or submit an Agent run.
pub async fn resource_worker(app: Arc<App>) {
    loop {
        if app.shutdown.is_cancelled() {
            break;
        }
        let Some(permit) = app.service.gate.enter() else {
            tokio::select! { _=app.shutdown.cancelled()=>break, _=tokio::time::sleep(Duration::from_millis(250))=>{} }
            continue;
        };
        let next = app.db.call(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(&format!("SELECT m.id FROM messages m WHERE m.direction='in' AND {RESOURCE_ELIGIBLE} AND {HAS_RESOURCES} AND COALESCE(json_extract(m.attachment,'$.resources_ready'),0)!=1 AND json_extract(m.attachment,'$.resource_preparation') IS NULL ORDER BY m.rowid LIMIT 1"),[],|r|r.get::<_,String>(0)).optional()?)
        }).await;
        match next {
            Ok(Some(id)) => {
                let result = async {
                    let message = app.db.message(&id).await?;
                    download_resources(&app, &message).await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    tracing::warn!("Incoming resource preparation: {error}");
                }
                app.notify.notify_one();
            }
            Ok(None) => {}
            Err(error) => tracing::warn!("Incoming resource queue: {error}"),
        }
        drop(permit);
        tokio::select! { _ = app.shutdown.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
    }
}

struct DownloadedResource {
    metadata: serde_json::Map<String, Value>,
    video_file: Option<std::fs::File>,
}

async fn download_resource(
    app: &App,
    m: &db::Message,
    resource: &Value,
    index: usize,
    attempt: &str,
    remaining: &mut u64,
) -> Result<Option<DownloadedResource>> {
    let Some(key) = resource["file_key"].as_str() else {
        return Ok(None);
    };
    let kind = resource["kind"].as_str().unwrap_or("file");
    let source_id = if resource["quoted"] == true {
        quote_resource_owner(&m.attachment).context("Quoted resource owner is unavailable")?
    } else {
        &m.source_id
    };
    let cfg = app.config.read().await.clone();
    let binding = app.db.bot(&m.project_id).await?;
    anyhow::ensure!(
        m.attachment["bot_app_id"] == binding.app_id,
        "The source bot binding changed before resource download"
    );
    let token = token(app, &binding).await?;
    let mut url = reqwest::Url::parse(&format!("{}/open-apis/im/v1/messages", cfg.feishu_base))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid URL"))?
        .push(source_id)
        .push("resources")
        .push(key);
    url.query_pairs_mut()
        .append_pair("type", if kind == "image" { "image" } else { "file" });
    let mut response = app
        .http
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| anyhow::anyhow!("Feishu resource request failed: {}", if err.is_timeout() { "timeout" } else { "transport error" }))?;
    anyhow::ensure!(response.status().is_success(), "Feishu resource download returned HTTP {}", response.status().as_u16());
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok()).and_then(|value| value.split(';').next())
        .map(str::trim).filter(|value| value.len() <= 127 && value.contains('/')).map(str::to_owned);
    if response.content_length().is_some_and(|n| n > *remaining) {
        bail!("Incoming resources exceed the 100 MB message limit");
    }
    let name = resource["file_name"]
        .as_str()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or(if kind == "image" {
            "image.png"
        } else {
            "attachment"
        });
    let key_hash = format!("{:x}", Sha256::digest(key.as_bytes()));
    let mut end = name.len().min(120);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let name = format!("{}-{index}-{}-{}", m.id, &key_hash[..16], &name[..end]);
    let temporary = format!(".{}-{attempt}-{index}.part", m.id);
    let (mut cache, file) =
        crate::multimodal::CacheWrite::create(&app.dir, &m.project_id, &name, &temporary)?;
    let mut file = tokio::fs::File::from_std(file);
    let mut hasher = Sha256::new();
    let mut head = Vec::new();
    let mut total = 0_u64;
    let result = async {
        while let Some(chunk) = response.chunk().await.map_err(|_| anyhow::anyhow!("Feishu resource body interrupted"))? {
            let bytes = chunk.len() as u64;
            if bytes > *remaining {
                *remaining = 0;
                bail!("Incoming resources exceed the 100 MB message limit");
            }
            *remaining -= bytes;
            total += bytes;
            hasher.update(&chunk);
            let head_bytes = (32_usize.saturating_sub(head.len())).min(chunk.len());
            head.extend_from_slice(&chunk[..head_bytes]);
            file.write_all(&chunk).await?;
        }
        anyhow::ensure!(kind != "image" || total > 0, "Incoming image is empty");
        anyhow::ensure!(
            kind != "image" || crate::multimodal::image_type(&head).is_some(),
            "Incoming image has an unsupported media type"
        );
        file.sync_all().await?;
        cache.publish()?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    result?;
    let detected_type = crate::multimodal::image_type(&head);
    let media_type = detected_type.or(content_type.as_deref());
    let metadata = json!({"local_path":cache.path,"cache_root":cache.root,"cache_project_id":m.project_id,
        "cache_message_id":m.id,"cache_source_id":source_id,"sha256":format!("{:x}",hasher.finalize()),
        "bytes":total,"media_type":media_type,"duration_seconds":resource["declared_duration_seconds"],
        "file_name":resource["file_name"].as_str().unwrap_or_else(|| if kind=="image" {"image.png"} else {"attachment"})});
    let video_file = if matches!(kind, "video" | "media") || media_type.is_some_and(|mime| mime.starts_with("video/")) {
        Some(file.into_std().await)
    } else {
        None
    };
    Ok(Some(DownloadedResource { metadata: metadata.as_object().unwrap().clone(), video_file }))
}

async fn prepare_frame(
    app: &App,
    message: &db::Message,
    resource: &Value,
    file: std::fs::File,
    index: usize,
    attempt: &str,
    resolve_decoder: fn(&str) -> Option<PathBuf>,
) -> Result<(f64, Value)> {
    let frame = crate::video_preview::extract(file, resolve_decoder).await?;
    let name = format!("{}-{index}-preview.png", message.id);
    let temporary = format!(".{}-{attempt}-{index}-preview.part", message.id);
    let (mut cache, target) = crate::multimodal::CacheWrite::create(&app.dir, &message.project_id, &name, &temporary)?;
    let mut target = tokio::fs::File::from_std(target);
    target.write_all(&frame.bytes).await?;
    target.sync_all().await?;
    cache.publish()?;
    Ok((frame.duration, json!({"kind":"image","file_name":"video-preview-20-percent.png",
        "file_key":resource["file_key"],"message_id":resource["message_id"],"chat_id":resource["chat_id"],
        "quoted":resource["quoted"],"frame_seconds":frame.timestamp,"duration_seconds":frame.duration,
        "local_path":cache.path,"cache_root":cache.root,"cache_project_id":message.project_id,
        "cache_message_id":message.id,"cache_source_id":resource["message_id"],
        "sha256":format!("{:x}",Sha256::digest(&frame.bytes)),"bytes":frame.bytes.len(),"media_type":"image/png"})))
}

pub struct Inbound {
    pub app: Arc<App>,
    pub binding: Binding,
}
fn updating_card_response() -> CardActionTriggerResponse {
    CardActionTriggerResponse {
        toast: Some(CardToast {
            r#type: "info".into(),
            title: None,
            content: Some(
                "Dauerbot is finishing an update. This action was not accepted; try again shortly."
                    .into(),
            ),
        }),
        card: None,
    }
}
impl CardActionTriggerHandler for Inbound {
    fn handle(
        &self,
        event: CardActionTrigger,
    ) -> Result<Option<CardActionTriggerResponse>, Box<dyn std::error::Error + Send + Sync>> {
        if self
            .app
            .service
            .relay
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(Some(updating_card_response()));
        }
        let Some(_permit) = self.app.service.gate.enter() else {
            return Ok(Some(updating_card_response()));
        };
        if let Some(response) = crate::autonomy_feishu::callback(&self.app, &self.binding, &event) {
            return Ok(Some(response));
        }
        Ok(Some(crate::model_control::ingest_callback(
            &self.app,
            &self.binding,
            &event,
        )?))
    }
}
impl EventHandler for Inbound {
    fn handle(&self, payload: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self
            .app
            .service
            .relay
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let cfg = self.app.config.blocking_read().clone();
            crate::service::relay(
                &cfg,
                &crate::service::Ingest {
                    project: self.binding.project_id.clone(),
                    app_id: self.binding.app_id.clone(),
                    payload: serde_json::from_slice(payload)?,
                },
            )?;
            return Ok(());
        }
        let _permit =
            self.app.service.gate.enter().ok_or_else(|| {
                anyhow::anyhow!("Service ownership is changing; retry this event")
            })?;
        ingest(&self.app, &self.binding, payload)?;
        Ok(())
    }
}
pub fn ingest(app: &Arc<App>, binding: &Binding, payload: &[u8]) -> Result<()> {
    let v: Value = serde_json::from_slice(payload)?;
    if let Some((chat, source, text, mut attachment)) = normalize(&v)? {
        if v["header"]["app_id"]
            .as_str()
            .is_some_and(|id| id != binding.app_id)
        {
            return Err(anyhow::anyhow!(
                "The Feishu event belongs to a different bot"
            ));
        }
        if v["header"]["app_id"].as_str() == Some(binding.app_id.as_str())
            && app.db.sync(|c| {
                crate::autonomy_feishu::text(c, binding, &source, &text, &attachment["source"], db::now())
            })?
        {
            app.notify.notify_one();
            return Ok(());
        }
        let model_command = attachment["source"]["message_type"] == "text"
            && crate::model_control::is_command(crate::cli_control::command_text(&text, &attachment["source"]));
        if model_command {
            attachment["daemon_control"] = json!("model");
        }
        let inserted = app.db.ingest_routed_bot(
            &binding.project_id,
            &binding.app_id,
            &chat,
            &source,
            &text,
            &attachment,
        )?;
        if let Ok(mut runtime) = app.runtime.try_write() {
            runtime.feishu = "receiving".into();
            runtime.feishu_error.clear();
            if let Some(state) = runtime.bots.get_mut(&binding.project_id) {
                state["status"] = json!("receiving");
                state["error"] = json!("");
            }
        }
        if inserted.is_some() && attachment["source"]["parent_id"].is_string() {
            app.quote_notify.notify_one();
        }
        app.notify.notify_one();
    }
    Ok(())
}

const QUOTE_LIMIT: usize = 3072;

fn quote(
    parent: &str,
    sender: Option<&str>,
    kind: Option<&str>,
    body: &str,
    reason: Option<&str>,
) -> Value {
    let mut end = body.len().min(QUOTE_LIMIT);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "message_id": parent,
        "sender": sender,
        "msg_type": kind,
        "body": &body[..end],
        "preview_truncated": end < body.len(),
        "status": if reason.is_some() { "unavailable" } else { "ok" },
        "reason": reason,
        "trust": "untrusted_quote",
    })
}

pub(crate) fn cached_quote(
    c: &rusqlite::Connection,
    project: &str,
    bot: &str,
    chat: &str,
    parent: &str,
) -> Result<Option<Value>> {
    use rusqlite::OptionalExtension;
    let row: Option<(String, String, String)> = c.query_row(
        "SELECT content,attachment,direction FROM messages
         WHERE project_id=?1 AND json_extract(attachment,'$.bot_app_id')=?3 AND (
           (direction='in' AND source_id=?2 AND json_extract(attachment,'$.source.chat_id')=?4)
           OR (direction='out' AND status='sent' AND json_extract(attachment,'$.sent_message_id')=?2
               AND json_extract(attachment,'$.sent_chat_id')=?4))
         ORDER BY direction LIMIT 1",
        rusqlite::params![project,parent,bot,chat],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional()?;
    Ok(row.map(|(body, raw, direction)| {
        let attachment: Value = serde_json::from_str(&raw).unwrap_or_default();
        if direction == "out" {
            let (kind, body) = if let Some(path) = attachment["path"].as_str() {
                ("file", format!("[file] {}", Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or_default()))
            } else if attachment["interactive_card"].is_object() {
                ("interactive", "[interactive card]".into())
            } else {
                ("post", body)
            };
            return quote(parent, Some(bot), Some(kind), &body, None);
        }
        let source = &attachment["source"];
        let mut result = quote(parent, source["sender"]["open_id"].as_str(),
            source["message_type"].as_str(), &body, None);
        result["resources"] = json!(direct_resources(&attachment).into_iter().map(|r| {
            resource_reference(&r, r["kind"].as_str().unwrap_or("file"), &r["file_key"])
        }).collect::<Vec<_>>());
        result
    }))
}

fn remote_quote(
    item: &Value,
    parent: &str,
    chat: &str,
) -> std::result::Result<Value, &'static str> {
    if item["message_id"] != parent || item["chat_id"] != chat {
        return Err("source_mismatch");
    }
    if item["deleted"] == true {
        return Err("deleted");
    }
    let sender = item["sender"]["id"].as_str().ok_or("invalid_response")?;
    let kind = item["msg_type"].as_str().ok_or("invalid_response")?;
    let content: Value =
        serde_json::from_str(item["body"]["content"].as_str().ok_or("invalid_response")?)
            .map_err(|_| "invalid_response")?;
    let mut resources = Vec::new();
    let body = match kind {
        "text" => content["text"]
            .as_str()
            .ok_or("invalid_response")?
            .to_owned(),
        "post" => {
            let post = post_document(&content);
            let mut body = post["title"].as_str().unwrap_or_default().to_owned();
            body.push('\n');
            collect_post(&post["content"], &mut body, &mut resources);
            body.trim().replace("[image]", "[image] image.png")
        }
        "image" => "[image] image.png".into(),
        "file" | "audio" | "video" | "media" | "sticker" => {
            format!(
                "[{kind}] {}",
                content["file_name"].as_str().unwrap_or_default()
            )
        }
        _ => return Err("unsupported_message_type"),
    };
    if let Some(key) = content["file_key"].as_str().or(content["image_key"].as_str()) {
        resources.push(resource_reference(&content, kind, &json!(key)));
    }
    let mut result = quote(parent, Some(sender), Some(kind), &body, None);
    result["resources"] = json!(resources);
    Ok(result)
}

async fn fetch_quote(
    app: &App,
    message: &db::Message,
    parent: &str,
    chat: &str,
    bot: &str,
) -> std::result::Result<Value, &'static str> {
    let binding = app
        .db
        .bot(&message.project_id)
        .await
        .map_err(|_| "binding_unavailable")?;
    if binding.app_id != bot {
        return Err("binding_unavailable");
    }
    let token = token(app, &binding)
        .await
        .map_err(|_| "authentication_failed")?;
    let base = app.config.read().await.feishu_base.clone();
    let mut url = reqwest::Url::parse(&format!("{base}/open-apis/im/v1/messages/"))
        .map_err(|_| "invalid_endpoint")?;
    url.path_segments_mut()
        .map_err(|_| "invalid_endpoint")?
        .pop_if_empty()
        .push(parent);
    let mut response = app
        .http
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "network_error")?;
    match response.status().as_u16() {
        401 | 403 => return Err("permission_denied"),
        404 | 410 => return Err("not_found"),
        _ if !response.status().is_success() => return Err("request_failed"),
        _ => {}
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "network_error")? {
        if bytes.len() + chunk.len() > 256 * 1024 {
            return Err("response_too_large");
        }
        bytes.extend_from_slice(&chunk);
    }
    let result: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid_response")?;
    if result["code"].as_i64() != Some(0) {
        return Err("api_rejected");
    }
    let items = result["data"]["items"]
        .as_array()
        .ok_or("invalid_response")?;
    match items.as_slice() {
        [item] => remote_quote(item, parent, chat),
        [] => Err("not_found"),
        _ => Err("invalid_response"),
    }
}

pub(crate) async fn prepare_quote(app: &App) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let id: Option<String> = app.db.call(|c| {
        Ok(c.query_row("SELECT id FROM messages WHERE direction='in' AND json_extract(attachment,'$.quote_pending')=1 ORDER BY rowid LIMIT 1", [], |r| r.get(0)).optional()?)
    }).await?;
    let Some(id) = id else { return Ok(false) };
    let message = app.db.message(&id).await?;
    let source = &message.attachment["source"];
    let parent = source["parent_id"].as_str().unwrap_or_default();
    let chat = source["chat_id"].as_str().unwrap_or_default();
    let bot = message.attachment["bot_app_id"]
        .as_str()
        .unwrap_or_default();
    let result =
        if !parent.starts_with("om_")
            || parent.len() > 128
            || !parent.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            || chat.is_empty()
            || bot.is_empty()
            || parent == message.source_id
        {
            Err("invalid_source")
        } else {
            tokio::time::timeout(
                Duration::from_secs(2),
                fetch_quote(app, &message, parent, chat, bot),
            )
            .await
            .unwrap_or(Err("timeout"))
        };
    let result = result.unwrap_or_else(|reason| quote(parent, None, None, "", Some(reason)));
    app.db.call(move |c| {
        c.execute("UPDATE messages SET attachment=json_remove(json_set(attachment,'$.quoted_message',json(?1)),
           '$.quote_pending', CASE WHEN json_array_length(?1,'$.resources')>0 THEN '$.resources_ready' ELSE '$.__unused' END) WHERE id=?2 AND json_extract(attachment,'$.quote_pending')=1", rusqlite::params![result.to_string(),id])?;
        Ok(())
    }).await?;
    app.notify.notify_one();
    Ok(true)
}

// Parent lookups do not share the attachment downloader.
pub async fn quote_worker(app: Arc<App>) {
    while !app.shutdown.is_cancelled() {
        if let Some(_permit) = app.service.gate.enter() {
            match prepare_quote(&app).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(_) => tracing::warn!("Incoming quote preparation failed"),
            }
        }
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = app.quote_notify.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

fn resource_reference(content: &Value, kind: &str, key: &Value) -> Value {
    let duration = content["duration"].as_f64().map(|milliseconds| milliseconds / 1000.0)
        .or_else(|| content["declared_duration_seconds"].as_f64());
    let fallback = if kind == "image" { "image.png" } else { "attachment" };
    let name = content["file_name"].as_str().unwrap_or(fallback);
    let name: String = name.chars().take(160).collect();
    json!({"kind":kind,"file_key":key,"file_name":name,"declared_duration_seconds":duration})
}

pub fn normalize(v: &Value) -> Result<Option<(String, String, String, Value)>> {
    if v["header"]["event_type"] != "im.message.receive_v1" {
        return Ok(None);
    }
    let event = &v["event"];
    if event["sender"]["sender_type"].as_str() != Some("user") {
        return Ok(None);
    }
    let msg = &event["message"];
    let chat = msg["chat_id"]
        .as_str()
        .context("Message is missing chat_id")?
        .to_owned();
    let source = msg["message_id"]
        .as_str()
        .context("Message is missing message_id")?
        .to_owned();
    let content: Value = serde_json::from_str(msg["content"].as_str().unwrap_or("{}"))?;
    let kind = msg["message_type"].as_str().unwrap_or("unknown");
    let mut text = content["text"].as_str().unwrap_or("").to_owned();
    let mut resources = Vec::new();
    if kind == "post" {
        let post = post_document(&content);
        text = post["title"].as_str().unwrap_or("").to_owned();
        if !text.is_empty() {
            text.push('\n');
        }
        collect_post(&post["content"], &mut text, &mut resources);
        text = text.trim().to_owned();
    }
    let mut attachment = if let Some(key) = content["file_key"]
        .as_str()
        .or(content["image_key"].as_str())
    {
        resource_reference(&content, kind, &json!(key))
    } else {
        json!({})
    };
    if text.is_empty() {
        text = format!(
            "[{kind} message] {}",
            content
                .get("file_name")
                .and_then(Value::as_str)
                .unwrap_or("")
        );
    }
    if !resources.is_empty() {
        attachment["resources"] = json!(resources);
    }
    attachment["source"] = json!({"chat_id":chat,"chat_type":msg["chat_type"],"message_type":kind,
        "sender":event["sender"]["sender_id"],"mentions":msg["mentions"],"mention_all":crate::group_ingress::mention_all(kind,&content,&msg["mentions"]),"thread_id":msg["thread_id"],"root_id":msg["root_id"],"parent_id":msg["parent_id"],"create_time":msg["create_time"]});
    Ok(Some((chat, source, text, attachment)))
}
// A localized post is one document, not a sequence of translations. Prefer the
// original Chinese variant, then English, then a deterministic available locale.
fn post_document(content: &Value) -> &Value {
    let content = content.get("post").unwrap_or(content);
    if content["content"].is_array() {
        return content;
    }
    for locale in ["zh_cn", "en_us"] {
        if content[locale]["content"].is_array() {
            return &content[locale];
        }
    }
    content
        .as_object()
        .and_then(|o| o.values().find(|v| v["content"].is_array()))
        .unwrap_or(content)
}

fn collect_post(v: &Value, text: &mut String, resources: &mut Vec<Value>) {
    match v {
        Value::Array(items) => {
            for item in items {
                collect_post(item, text, resources);
            }
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        Value::Object(_) => match v["tag"].as_str().unwrap_or("") {
            "img" => {
                if let Some(key) = v["image_key"].as_str().filter(|k| !k.is_empty()) {
                    if !resources
                        .iter()
                        .any(|r| r["kind"] == "image" && r["file_key"] == key)
                    {
                        resources
                            .push(json!({"kind":"image","file_key":key,"file_name":"image.png"}));
                    }
                    text.push_str("[image]");
                }
            }
            "text" | "a" | "code_block" | "md" => {
                if let Some(t) = v["text"].as_str() {
                    text.push_str(t);
                }
            }
            "at" => text.push_str(v["user_name"].as_str().unwrap_or("@mention")),
            "media" | "file" | "audio" | "video" => {
                if let Some(key) = v["file_key"].as_str().filter(|k| !k.is_empty()) {
                    if !resources.iter().any(|r| r["file_key"] == key) {
                        resources.push(resource_reference(v, v["tag"].as_str().unwrap(), &json!(key)));
                    }
                    text.push_str("[media]");
                }
            }
            _ => {}
        },
        _ => {}
    }
}

pub async fn websocket(app: Arc<App>) {
    websocket_with_warm(app, None).await
}
type Connections = std::collections::HashMap<String, (Binding, tokio::task::JoinHandle<()>)>;
pub struct WarmIngress {
    workers: Option<Connections>,
    target: Arc<std::sync::RwLock<Option<Arc<App>>>>,
}
impl WarmIngress {
    pub fn is_ready(&self) -> bool {
        self.workers
            .as_ref()
            .is_some_and(|workers| workers.values().all(|(_, task)| !task.is_finished()))
    }
    pub async fn activate(&mut self, app: Arc<App>) {
        *self.target.write().unwrap() = Some(app.clone());
        if let Some(workers) = &self.workers {
            for (binding, _) in workers.values() {
                connection_state(&app, binding, "awaiting_events", "").await;
            }
        }
    }
}
impl Drop for WarmIngress {
    fn drop(&mut self) {
        if let Some(workers) = self.workers.take() {
            for (_, (_, worker)) in workers {
                worker.abort();
            }
        }
    }
}
struct WarmingInbound {
    cfg: Config,
    binding: Binding,
    target: Arc<std::sync::RwLock<Option<Arc<App>>>>,
}
impl CardActionTriggerHandler for WarmingInbound {
    fn handle(
        &self,
        event: CardActionTrigger,
    ) -> Result<Option<CardActionTriggerResponse>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(app) = self.target.read().unwrap().clone() {
            CardActionTriggerHandler::handle(
                &Inbound {
                    app,
                    binding: self.binding.clone(),
                },
                event,
            )
        } else {
            // Never ACK acceptance on a standby store. The click can be retried
            // after ownership transfer; already accepted actions stay journaled.
            Ok(Some(updating_card_response()))
        }
    }
}
impl EventHandler for WarmingInbound {
    fn handle(&self, payload: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let app = self.target.read().unwrap().clone();
        if let Some(app) = app {
            EventHandler::handle(
                &Inbound {
                    app,
                    binding: self.binding.clone(),
                },
                payload,
            )
        } else {
            crate::service::relay(
                &self.cfg,
                &crate::service::Ingest {
                    project: self.binding.project_id.clone(),
                    app_id: self.binding.app_id.clone(),
                    payload: serde_json::from_slice(payload)?,
                },
            )?;
            Ok(())
        }
    }
}
/// Cluster transports may overlap, but all callbacks commit to one journal owner.
/// Actual SDK WebSocket upgrade completion, rather than task launch, is readiness.
pub async fn warm_ingress(cfg: Config, bindings: Vec<Binding>) -> Result<WarmIngress> {
    let mut warm = WarmIngress {
        workers: Some(Connections::new()),
        target: Arc::new(std::sync::RwLock::new(None)),
    };
    let mut ready = Vec::new();
    for binding in bindings {
        let handler = EventDispatcherHandler::builder()
            .register_raw(
                "im.message.receive_v1",
                WarmingInbound {
                    cfg: cfg.clone(),
                    binding: binding.clone(),
                    target: warm.target.clone(),
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?
            .register_card_action_trigger(WarmingInbound {
                cfg: cfg.clone(),
                binding: binding.clone(),
                target: warm.target.clone(),
            })
            .map_err(|e| anyhow::anyhow!(e))?
            .build();
        let config = open_lark::Config::builder()
            .app_id(&binding.app_id)
            .app_secret(&binding.app_secret)
            .base_url(&cfg.feishu_base)
            .max_response_size(2 * 1024 * 1024)
            .build();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let _ = LarkWsClient::open_with_ready(Arc::new(config), handler, tx).await;
        });
        warm.workers
            .as_mut()
            .unwrap()
            .insert(binding.project_id.clone(), (binding, worker));
        ready.push(rx);
    }
    tokio::time::timeout(Duration::from_secs(25), async {
        for ready in ready {
            ready
                .await
                .context("Candidate bot transport did not connect")?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    anyhow::ensure!(warm.is_ready(), "Candidate bot transport disconnected");
    Ok(warm)
}
pub async fn websocket_with_warm(app: Arc<App>, mut warm: Option<WarmIngress>) {
    let mut revision = app.settings_tx.subscribe();
    let mut workers: Connections = warm
        .as_mut()
        .and_then(|warm| warm.workers.take())
        .unwrap_or_default();
    loop {
        let bindings = match app.db.bots().await {
            Ok(bindings) => bindings,
            Err(_) => {
                let mut runtime = app.runtime.write().await;
                runtime.feishu_error = "Cannot read project bot connections".into();
                drop(runtime);
                tokio::select! {
                    _ = app.shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                }
            }
        };
        let replaced: Vec<String> = workers
            .iter()
            .filter(|(project, (current, handle))| {
                handle.is_finished()
                    || !bindings.iter().any(|next| {
                        next.project_id == **project
                            && next.app_id == current.app_id
                            && next.app_secret == current.app_secret
                    })
            })
            .map(|(project, _)| project.clone())
            .collect();
        for project in replaced {
            if let Some((_, worker)) = workers.remove(&project) {
                worker.abort();
                let _ = worker.await;
            }
            app.runtime.write().await.bots.remove(&project);
        }
        for binding in bindings {
            if !workers.contains_key(&binding.project_id) {
                let worker = tokio::spawn(websocket_for(app.clone(), binding.clone()));
                workers.insert(binding.project_id.clone(), (binding, worker));
            }
        }
        {
            let mut runtime = app.runtime.write().await;
            if workers.is_empty() {
                runtime.feishu = "unconfigured".into();
            }
            runtime.feishu_error.clear();
        }
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = revision.changed() => {},
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
    for (_, (_, worker)) in workers {
        worker.abort();
        let _ = worker.await;
    }
}

async fn connection_state(app: &App, binding: &Binding, status: &str, error: &str) {
    let mut runtime = app.runtime.write().await;
    runtime.bots.insert(
        binding.project_id.clone(),
        json!({"app_id":binding.app_id,"status":status,"error":error}),
    );
    runtime.feishu = if runtime
        .bots
        .values()
        .any(|b| b["status"] == "awaiting_events" || b["status"] == "receiving")
    {
        "connected"
    } else {
        status
    }
    .into();
}

async fn websocket_for(app: Arc<App>, binding: Binding) {
    let mut delay = 1;
    loop {
        let cfg = app.config.read().await.clone();
        connection_state(&app, &binding, "connecting", "").await;
        let handler = EventDispatcherHandler::builder()
            .register_raw(
                "im.message.receive_v1",
                Inbound {
                    app: app.clone(),
                    binding: binding.clone(),
                },
            )
            .expect("unique handler")
            .register_card_action_trigger(Inbound {
                app: app.clone(),
                binding: binding.clone(),
            })
            .expect("unique card handler")
            .build();
        let config = open_lark::Config::builder()
            .app_id(&binding.app_id)
            .app_secret(&binding.app_secret)
            .base_url(&cfg.feishu_base)
            .max_response_size(2 * 1024 * 1024)
            .build();
        let started = Instant::now();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let connection = LarkWsClient::open_with_ready(Arc::new(config), handler, ready_tx);
        tokio::pin!(connection);
        let result = tokio::select! {
            _ = app.shutdown.cancelled() => return,
            ready = ready_rx => {
                if ready.is_ok() {connection_state(&app,&binding,"awaiting_events","").await;}
                tokio::select! {_=app.shutdown.cancelled()=>return,result=&mut connection=>result}
            },
            result=&mut connection=>result,
        };
        let reason = if result.is_ok() {
            "Connection ended"
        } else {
            "Connection lost or authentication failed; check this project's bot credentials and event subscriptions"
        };
        connection_state(&app, &binding, "reconnecting", reason).await;
        if started.elapsed() > Duration::from_secs(60) {
            delay = 1;
        }
        tokio::select! {
            _ = app.shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(delay)) => delay = (delay * 2).min(60),
        }
    }
}

pub fn public_config(cfg: &Config) -> Value {
    json!({"port":cfg.port})
}

/// Both private/group chat IDs and direct user Open IDs are valid destinations.
pub fn recipient_type(to: &str) -> Result<&'static str> {
    if to.len() > 200 || to.len() < 4 || !to.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("to must be a Feishu chat ID (oc_...) or user Open ID (ou_...)");
    }
    if to.starts_with("oc_") {
        Ok("chat_id")
    } else if to.starts_with("ou_") {
        Ok("open_id")
    } else {
        bail!("to must be a Feishu chat ID (oc_...) or user Open ID (ou_...)")
    }
}

fn scoped_group(attachment: &Value) -> Option<(&str, &str)> {
    let source = &attachment["source"];
    let chat = source["chat_id"].as_str()?;
    let app_id = attachment["bot_app_id"].as_str()?;
    (source["chat_type"] == "group" && !app_id.is_empty()
        && chat.starts_with("oc_") && recipient_type(chat).is_ok()).then_some((chat, app_id))
}

fn current_group_cache(value: &Value, chat: &str, app_id: &str) -> bool {
    value["chat_id"] == chat && value["app_id"] == app_id
        && value["fetched_at"].as_i64().is_some_and(|t| (0..300_000).contains(&db::now().saturating_sub(t)))
}

fn group_sender(mut value: Value, attachment: &Value) -> Value {
    let sender = &attachment["source"]["sender"]["open_id"];
    value["sender_name"] = value["members"].as_array()
        .and_then(|members| members.iter().find(|member| &member["member_id"] == sender))
        .map(|member| member["name"].clone()).unwrap_or(Value::Null);
    value
}

/// Read only a bot-scoped snapshot for this original inbound message.
pub(crate) fn cached_group_context(
    c: &rusqlite::Connection,
    project: &str,
    attachment: &Value,
) -> Result<Option<Value>> {
    use rusqlite::OptionalExtension;
    let Some((chat, app_id)) = scoped_group(attachment) else { return Ok(None) };
    let stored: Option<Option<String>> = c.query_row(
        "SELECT c.context FROM project_bots b LEFT JOIN conversations c ON c.project_id=b.project_id
         AND c.chat_id=?2 WHERE b.project_id=?1 AND b.app_id=?3",
        rusqlite::params![project, chat, app_id], |r| r.get(0),
    ).optional()?;
    let Some(stored) = stored else { return Ok(None) };
    let resource = &attachment["resource_context"];
    let snapshot = if current_group_cache(resource, chat, app_id) {
        Some(resource.clone())
    } else {
        stored.and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .filter(|value| current_group_cache(value, chat, app_id))
    };
    Ok(snapshot.map(|value| group_sender(value, attachment)))
}

/// Fetch at most one bounded page; missing scopes never invent members.
pub async fn context(app: &App, m: &db::Message) -> Value {
    let Some((chat, app_id)) = scoped_group(&m.attachment) else { return json!({}) };
    if let Ok(Some(value)) = app.db.sync(|c| cached_group_context(c, &m.project_id, &m.attachment)) {
        return value;
    }
    let fetch = async {
        let cfg = app.config.read().await.clone();
        let binding = app.db.bot(&m.project_id).await?;
        anyhow::ensure!(binding.app_id == app_id, "The source belongs to another bot");
        let token = token(app, &binding).await?;
        let base = format!("{}/open-apis/im/v1/chats/{chat}", cfg.feishu_base);
        let name = api_json(app.http.get(&base).bearer_auth(&token).send().await?)
            .await.ok().and_then(|v| v["data"]["name"].as_str().map(bounded_group_label))
            .unwrap_or_default();
        let response = api_json(
            app.http.get(format!("{base}/members"))
                .query(&[("member_id_type", "open_id"), ("page_size", "100")])
                .bearer_auth(&token).send().await?,
        ).await;
        Ok::<_, anyhow::Error>(match response {
            Ok(v) if v["data"]["items"].is_array() => {
                let items = v["data"]["items"].as_array().expect("checked members array");
                let members: Vec<_> = items.iter().take(100).filter_map(|item| {
                    let id = item["member_id"].as_str().filter(|id| id.starts_with("ou_") && recipient_type(id).is_ok())?;
                    Some(json!({"member_id":id,"name":bounded_group_label(item["name"].as_str().unwrap_or_default())}))
                }).collect();
                json!({"name":name,"members_status":"snapshot","members":members,
                    "truncated":v["data"]["has_more"]==true || items.len()>100})
            }
            _ => json!({"name":name,"members_status":"unavailable","members":[]}),
        })
    };
    let mut value = match tokio::time::timeout(Duration::from_secs(6), fetch).await {
        Ok(Ok(v)) => v,
        _ => json!({"members_status":"unavailable","members":[]}),
    };
    value["chat_id"] = json!(chat);
    value["app_id"] = json!(app_id);
    value["fetched_at"] = json!(db::now());
    let stored = app.db.sync(|c| Ok(c.execute(
        "UPDATE conversations SET context=?1,name=CASE WHEN ?2='' THEN name ELSE ?2 END
         WHERE project_id=?3 AND chat_id=?4 AND EXISTS(
           SELECT 1 FROM project_bots WHERE project_id=?3 AND app_id=?5)",
        rusqlite::params![value.to_string(),value["name"].as_str().unwrap_or_default(),m.project_id,chat,app_id],
    )? == 1)).unwrap_or(false);
    if !stored {
        return json!({"members_status":"unavailable"});
    }
    group_sender(value, &m.attachment)
}

fn bounded_group_label(value: &str) -> String {
    value.chars().take(120).collect()
}

pub(crate) enum ProgressDelivery {
    Sent(String),
    Uneditable,
    Rejected(String),
}

pub(crate) async fn progress_message(
    app: &App,
    token: &str,
    anchor: &str,
    message_id: &str,
    text: &str,
) -> Result<ProgressDelivery> {
    let base = app.config.read().await.feishu_base.clone();
    let content = json!({"zh_cn":{"title":"","content":[[{"tag":"md","text":text}]]}}).to_string();
    let editing = !message_id.is_empty();
    let request = if editing {
        app.http
            .put(format!("{base}/open-apis/im/v1/messages/{message_id}"))
            .json(&json!({"msg_type":"post","content":content}))
    } else {
        app.http.post(format!("{base}/open-apis/im/v1/messages/{anchor}/reply"))
            .json(&json!({"msg_type":"post","content":content,"reply_in_thread":true,"uuid":db::id()}))
    };
    let response = request.bearer_auth(token).send().await?;
    let status = response.status();
    let reply: Value = response
        .json()
        .await
        .context("Feishu progress response is not JSON")?;
    let code = reply["code"].as_i64();
    if status.is_server_error() || code.is_none() {
        bail!("Feishu progress delivery could not be confirmed ({status})");
    }
    if editing
        && matches!(
            code,
            Some(230011 | 230054 | 230071 | 230072 | 230075 | 230110)
        )
    {
        return Ok(ProgressDelivery::Uneditable);
    }
    if !status.is_success() || code != Some(0) {
        return Ok(ProgressDelivery::Rejected(format!(
            "Feishu progress request rejected ({status}, code {code:?})"
        )));
    }
    if editing {
        return Ok(ProgressDelivery::Sent(message_id.into()));
    }
    let receipt = SendReceipt::from_response(&reply)?;
    anyhow::ensure!(
        !receipt.thread_id.is_empty(),
        "Feishu progress reply has no topic receipt"
    );
    Ok(ProgressDelivery::Sent(receipt.message_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_parts_preserve_utf8_and_escaped_bytes_under_wire_budget() {
        let payload = "行\u{0001} \\ \" <at user_id=\"example\">text</at> &\n".repeat(4500);
        assert!(payload.len() > 150_000);
        let parts = debug_reply_parts(&payload);
        assert!(parts.len() > 1);
        let mut reconstructed = String::new();
        for (index, part) in parts.iter().enumerate() {
            assert!(serde_json::to_vec(part).unwrap().len() <= 60_000);
            let content: Value = serde_json::from_str(part["content"].as_str().unwrap()).unwrap();
            let header = format!("[debug {}/{}]\n", index + 1, parts.len());
            // This verifies API payload bytes. Feishu clients may render supported markup.
            reconstructed.push_str(
                content["text"]
                    .as_str()
                    .unwrap()
                    .strip_prefix(&header)
                    .unwrap(),
            );
        }
        assert_eq!(reconstructed, payload);
    }
}
