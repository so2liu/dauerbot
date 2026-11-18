use crate::{App, bot::Binding, config::Config, db};
use anyhow::{Context, Result, bail};
use open_lark::ws_client::{
    CardActionTrigger, CardActionTriggerHandler, CardActionTriggerResponse, CardToast,
    EventDispatcherHandler, EventHandler, LarkWsClient,
};
use serde_json::{Value, json};
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

fn ensure_card(card: &Value) -> Result<()> {
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
        (
            "post",
            json!({"zh_cn":{"title":"","content":[[{"tag":"md","text":m.content}]]}}),
        )
    };
    let Some(anchor) = m.attachment["reply_to_message_id"].as_str() else {
        // A message without an anchor starts a new discussion at the explicit
        // destination. The Agent chooses anchors; the daemon never guesses one.
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
    };
    let changed = c.execute("UPDATE messages SET attachment=json_set(attachment,'$.resource_preparation',?) WHERE id=? AND source_id=? AND COALESCE(json_extract(attachment,'$.bot_app_id'),'')=? AND COALESCE(json_extract(attachment,'$.resources_ready'),0)!=1 AND json_extract(attachment,'$.resource_preparation') IS NULL AND status IN ('pending','queued')",rusqlite::params![preparation.token,preparation.message_id,preparation.source_id,preparation.bot_app_id])?;
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
    let changed=c.execute("UPDATE messages SET attachment=json_set(attachment,'$.resources',json(?),'$.resources_ready',json('true'),'$.resource_context',json(?)) WHERE id=? AND source_id=? AND COALESCE(json_extract(attachment,'$.bot_app_id'),'')=? AND json_extract(attachment,'$.resource_preparation')=? AND COALESCE(json_extract(attachment,'$.resources_ready'),0)!=1 AND status IN ('pending','queued')",rusqlite::params![serde_json::to_string(&resources)?,context.to_string(),preparation.message_id,preparation.source_id,preparation.bot_app_id,preparation.token])?;
    Ok(changed == 1)
}

/// Only the original message ID and keys are downloaded. Preparation never
/// holds the project delivery lock; resource names and captions remain user data.
pub async fn download_resources(app: &App, m: &db::Message) -> Result<Vec<Value>> {
    let message = m.clone();
    let preparation = app
        .db
        .call(move |c| begin_resource_preparation(c, &message))
        .await?
        .context("Resources are already prepared or owned by another attempt")?;
    let mut resources = m.attachment["resources"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if resources.is_empty() && m.attachment["file_key"].is_string() {
        resources.push(json!({"file_key":m.attachment["file_key"],"file_name":m.attachment["file_name"],"kind":m.attachment["kind"]}));
    }
    let mut remaining = MAX_FILE;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(80);
    for (index, resource) in resources.iter_mut().enumerate() {
        if let Some(o) = resource.as_object_mut() {
            o.remove("local_path");
            o.remove("error");
        }
        if tokio::time::Instant::now() >= deadline {
            resource["error"] = json!("Message resource preparation timed out (80 seconds total)");
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
            Ok(Ok(Some(path))) => resource["local_path"] = json!(path),
            Ok(Ok(None)) => resource["error"] = json!("Resource key is missing"),
            Ok(Err(error)) => resource["error"] = json!(error.to_string()),
            Err(_) => {
                resource["error"] =
                    json!("Message resource preparation timed out (80 seconds total)")
            }
        }
    }
    let context = tokio::time::timeout_at(deadline, context(app, m))
        .await
        .unwrap_or_else(|_| json!({"members_status":"unavailable"}));
    let prepared = preparation.clone();
    let result = resources.clone();
    if !app
        .db
        .call(move |c| finish_resource_preparation(c, &prepared, &result, &context))
        .await?
    {
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
            Ok(c.query_row("SELECT id FROM messages WHERE direction='in' AND status IN ('pending','queued') AND (json_extract(attachment,'$.file_key') IS NOT NULL OR json_array_length(attachment,'$.resources')>0) AND COALESCE(json_extract(attachment,'$.resources_ready'),0)!=1 AND json_extract(attachment,'$.resource_preparation') IS NULL ORDER BY rowid LIMIT 1",[],|r|r.get::<_,String>(0)).optional()?)
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

struct PartialResource(PathBuf);
impl Drop for PartialResource {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn download_resource(
    app: &App,
    m: &db::Message,
    resource: &Value,
    index: usize,
    attempt: &str,
    remaining: &mut u64,
) -> Result<Option<PathBuf>> {
    let Some(key) = resource["file_key"].as_str() else {
        return Ok(None);
    };
    let kind = resource["kind"].as_str().unwrap_or("file");
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
        .push(&m.source_id)
        .push("resources")
        .push(key);
    url.query_pairs_mut()
        .append_pair("type", if kind == "image" { "image" } else { "file" });
    let mut response = app
        .http
        .get(url)
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
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
    let target = app
        .dir
        .join("files")
        .join(format!("{}-{attempt}-{index}-{name}", m.id));
    let temp = target.with_extension("part");
    let _cleanup = PartialResource(temp.clone());
    let mut file = tokio::fs::File::create(&temp).await?;
    let result = async {
        while let Some(chunk) = response.chunk().await? {
            let bytes = chunk.len() as u64;
            if bytes > *remaining {
                *remaining = 0;
                bail!("Incoming resources exceed the 100 MB message limit");
            }
            *remaining -= bytes;
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        tokio::fs::rename(&temp, &target).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result?;
    Ok(Some(target))
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
        let model_command = attachment["source"]["message_type"] == "text"
            && crate::model_control::is_command(&text);
        if model_command {
            attachment["daemon_control"] = json!("model");
        }
        app.db.ingest_bot(
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
        app.notify.notify_one();
    }
    Ok(())
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
        json!({"file_key":key,"file_name":content["file_name"],"kind":kind})
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
        "sender":event["sender"]["sender_id"],"mentions":msg["mentions"],"thread_id":msg["thread_id"],"root_id":msg["root_id"],"parent_id":msg["parent_id"],"create_time":msg["create_time"]});
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
            "media" => {
                if let Some(key) = v["file_key"].as_str().filter(|k| !k.is_empty()) {
                    if !resources.iter().any(|r| r["file_key"] == key) {
                        resources.push(json!({"kind":"file","file_key":key,"file_name":"video"}));
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

/// Cache a bounded, best-effort group snapshot. Missing scopes never invent members.
pub async fn context(app: &App, m: &db::Message) -> Value {
    let source = &m.attachment["source"];
    let chat = source["chat_id"].as_str().unwrap_or("");
    if source["chat_type"] != "group" {
        return json!({});
    }
    let cached = app
        .db
        .sync(|c| {
            Ok(c.query_row(
                "SELECT context FROM conversations WHERE project_id=? AND chat_id=?",
                rusqlite::params![m.project_id, chat],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "{}".into()))
        })
        .unwrap_or_default();
    let cached: Value = serde_json::from_str(&cached).unwrap_or_default();
    let mut result = if cached["fetched_at"]
        .as_i64()
        .is_some_and(|t| db::now() - t < 300_000)
    {
        cached
    } else {
        let fetch = async {
            let cfg = app.config.read().await.clone();
            let binding = app.db.bot(&m.project_id).await?;
            let token = token(app, &binding).await?;
            let base = format!("{}/open-apis/im/v1/chats/{chat}", cfg.feishu_base);
            let name = api_json(app.http.get(&base).bearer_auth(&token).send().await?)
                .await
                .ok()
                .and_then(|v| v["data"]["name"].as_str().map(str::to_owned))
                .unwrap_or_default();
            let response = api_json(
                app.http
                    .get(format!("{base}/members"))
                    .query(&[("member_id_type", "open_id"), ("page_size", "100")])
                    .bearer_auth(&token)
                    .send()
                    .await?,
            )
            .await;
            Ok::<_, anyhow::Error>(match response {
                Ok(v) => {
                    json!({"name":name,"members_status":"snapshot","members":v["data"]["items"],"truncated":v["data"]["has_more"]==true,"fetched_at":db::now()})
                }
                Err(_) => {
                    json!({"name":name,"members_status":"unavailable","members":[],"fetched_at":db::now()})
                }
            })
        };
        let value = match tokio::time::timeout(Duration::from_secs(6), fetch).await {
            Ok(Ok(v)) => v,
            _ => json!({"members_status":"unavailable","members":[],"fetched_at":db::now()}),
        };
        let _ = app.db.sync(|c| {c.execute("UPDATE conversations SET context=?,name=CASE WHEN ?='' THEN name ELSE ? END WHERE project_id=? AND chat_id=?",rusqlite::params![value.to_string(),value["name"].as_str().unwrap_or(""),value["name"].as_str().unwrap_or(""),m.project_id,chat])?;Ok(())});
        value
    };
    let sender = source["sender"]["open_id"].as_str().unwrap_or("");
    result["sender_name"] = result["members"]
        .as_array()
        .and_then(|ms| ms.iter().find(|v| v["member_id"] == sender))
        .map(|v| v["name"].clone())
        .unwrap_or(Value::Null);
    result
}
