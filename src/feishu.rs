use crate::{App, bot::Binding, config::Config, db};
use anyhow::{Context, Result, bail};
use open_lark::ws_client::{EventDispatcherHandler, EventHandler, LarkWsClient};
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
    let (kind, content) = if let Some(path) = m.attachment["path"].as_str() {
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

pub async fn download(app: &App, m: &db::Message) -> Result<Option<PathBuf>> {
    let Some(key) = m.attachment["file_key"].as_str() else {
        return Ok(None);
    };
    let kind = m.attachment["kind"].as_str().unwrap_or("file");
    let cfg = app.config.read().await.clone();
    let binding = app.db.bot(&m.project_id).await?;
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
    if response.content_length().is_some_and(|n| n > MAX_FILE) {
        bail!("Incoming file exceeds 100 MB");
    }
    let name = m.attachment["file_name"]
        .as_str()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or(if kind == "image" {
            "image.png"
        } else {
            "attachment"
        });
    let target = app.dir.join("files").join(format!("{}-{name}", m.id));
    let temp = target.with_extension("part");
    let mut file = tokio::fs::File::create(&temp).await?;
    let result = async {
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await? {
            size += chunk.len() as u64;
            if size > MAX_FILE {
                bail!("Incoming file exceeds 100 MB");
            }
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
impl EventHandler for Inbound {
    fn handle(&self, payload: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let v: Value = serde_json::from_slice(payload)?;
        if let Some((chat, source, text, attachment)) = normalize(&v)? {
            if v["header"]["app_id"]
                .as_str()
                .is_some_and(|id| id != self.binding.app_id)
            {
                return Err(anyhow::anyhow!("The Feishu event belongs to a different bot").into());
            }
            self.app.db.ingest_bot(
                &self.binding.project_id,
                &self.binding.app_id,
                &chat,
                &source,
                &text,
                &attachment,
            )?;
            if let Ok(mut runtime) = self.app.runtime.try_write() {
                runtime.feishu = "receiving".into();
                runtime.feishu_error.clear();
                if let Some(state) = runtime.bots.get_mut(&self.binding.project_id) {
                    state["status"] = json!("receiving");
                    state["error"] = json!("");
                }
            }
            self.app.notify.notify_one();
        }
        Ok(())
    }
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
    if kind == "post" {
        collect_text(&content, &mut text);
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
    attachment["source"] = json!({"chat_id":chat,"chat_type":msg["chat_type"],"message_type":kind,
        "sender":event["sender"]["sender_id"],"mentions":msg["mentions"],"thread_id":msg["thread_id"],"root_id":msg["root_id"],"parent_id":msg["parent_id"],"create_time":msg["create_time"]});
    Ok(Some((chat, source, text, attachment)))
}
fn collect_text(v: &Value, out: &mut String) {
    match v {
        Value::Array(xs) => {
            for x in xs {
                collect_text(x, out)
            }
            out.push('\n');
        }
        Value::Object(o) => {
            if let Some(Value::String(s)) = o.get("text") {
                out.push_str(s);
            } else {
                for (_, x) in o {
                    collect_text(x, out)
                }
            }
        }
        _ => {}
    }
}

pub async fn websocket(app: Arc<App>) {
    let mut revision = app.settings_tx.subscribe();
    let mut workers: std::collections::HashMap<String, (Binding, tokio::task::JoinHandle<()>)> =
        std::collections::HashMap::new();
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
            .build();
        let config = open_lark::Config::builder()
            .app_id(&binding.app_id)
            .app_secret(&binding.app_secret)
            .base_url(&cfg.feishu_base)
            .max_response_size(2 * 1024 * 1024)
            .build();
        let started = Instant::now();
        connection_state(&app, &binding, "awaiting_events", "").await;
        tokio::select! {
            _ = app.shutdown.cancelled() => return,
            result = LarkWsClient::open(Arc::new(config), handler) => {
                let reason = if result.is_ok() { "Connection ended" } else {
                    "Connection lost or authentication failed; check this project's bot credentials and event subscriptions"
                };
                connection_state(&app, &binding, "reconnecting", reason).await;
            }
        }
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
