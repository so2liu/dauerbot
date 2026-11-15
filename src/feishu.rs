use crate::{App, config::Config, db};
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

pub async fn token(app: &App) -> Result<String> {
    let cfg = app.config.read().await.clone();
    if cfg.app_id.is_empty() || cfg.app_secret.is_empty() {
        bail!("请先配置飞书应用凭证");
    }
    let mut cache = app.token.lock().await;
    let key = format!("{}:{}", cfg.app_id, cfg.feishu_base);
    if cache.key == key && cache.until.is_some_and(|t| t > Instant::now()) {
        return Ok(cache.token.clone());
    }
    let response = app
        .http
        .post(format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            cfg.feishu_base
        ))
        .json(&json!({"app_id":cfg.app_id,"app_secret":cfg.app_secret}))
        .send()
        .await?;
    let v = api_json(response).await?;
    cache.key = key;
    cache.token = v["tenant_access_token"]
        .as_str()
        .context("飞书未返回 tenant_access_token")?
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
    let v: Value = response.json().await.context("飞书响应不是 JSON")?;
    if !status.is_success() || v["code"].as_i64() != Some(0) {
        bail!(
            "飞书 API {} / code {}: {}",
            status,
            v["code"],
            v["msg"].as_str().unwrap_or("请求失败")
        );
    }
    Ok(v)
}

pub async fn send(app: &App, p: &db::Project, m: &db::Message) -> Result<String> {
    let cfg = app.config.read().await.clone();
    let token = token(app).await?;
    let to = m.attachment["to"].as_str().unwrap_or(&p.chat_id);
    let receive_id_type = recipient_type(to)?;
    let (kind, content) = if let Some(path) = m.attachment["path"].as_str() {
        let path = allowed_file(p, Path::new(path), &app.dir)?;
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.len() == 0 || metadata.len() > MAX_FILE {
            bail!("附件须大于 0 且不超过 100 MB");
        }
        let file = tokio::fs::File::open(&path).await?;
        let filename = path
            .file_name()
            .context("附件文件名无效")?
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
            json!({"file_key":v["data"]["file_key"].as_str().context("未返回 file_key")?}),
        )
    } else {
        if m.content.len() > 24_000 {
            bail!("Markdown 超过 24000 字节，请改发附件");
        }
        (
            "post",
            json!({"zh_cn":{"title":"","content":[[{"tag":"md","text":m.content}]]}}),
        )
    };
    let body = json!({"receive_id":to,"msg_type":kind,"content":content.to_string(),"uuid":m.id});
    let v = api_json(
        app.http
            .post(format!("{}/open-apis/im/v1/messages", cfg.feishu_base))
            .query(&[("receive_id_type", receive_id_type)])
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?,
    )
    .await?;
    Ok(v["data"]["message_id"]
        .as_str()
        .context("未返回 message_id")?
        .into())
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
        bail!("附件必须位于该项目目录或 Bridge 文件缓存内");
    }
    Ok(absolute)
}

pub async fn download(app: &App, m: &db::Message) -> Result<Option<PathBuf>> {
    let Some(key) = m.attachment["file_key"].as_str() else {
        return Ok(None);
    };
    let kind = m.attachment["kind"].as_str().unwrap_or("file");
    let cfg = app.config.read().await.clone();
    let token = token(app).await?;
    let mut url = reqwest::Url::parse(&format!("{}/open-apis/im/v1/messages", cfg.feishu_base))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("URL 无效"))?
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
        bail!("入站附件超过 100 MB");
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
                bail!("入站附件超过 100 MB");
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

pub struct Inbound(pub Arc<App>);
impl EventHandler for Inbound {
    fn handle(&self, payload: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let v: Value = serde_json::from_slice(payload)?;
        if let Some((chat, source, text, attachment)) = normalize(&v)? {
            self.0.db.ingest_bot(&chat, &source, &text, &attachment)?;
            if let Ok(mut runtime) = self.0.runtime.try_write() {
                runtime.feishu = "receiving".into();
                runtime.feishu_error.clear();
            }
            self.0.notify.notify_one();
            self.0.changed();
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
        .context("消息缺少 chat_id")?
        .to_owned();
    let source = msg["message_id"]
        .as_str()
        .context("消息缺少 message_id")?
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
            "[{kind} 消息] {}",
            content
                .get("file_name")
                .and_then(Value::as_str)
                .unwrap_or("")
        );
    }
    attachment["source"] = json!({"chat_id":chat,"chat_type":msg["chat_type"],"message_type":kind,
        "sender":event["sender"]["sender_id"],"mentions":msg["mentions"],"root_id":msg["root_id"],"parent_id":msg["parent_id"],"create_time":msg["create_time"]});
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
    let mut delay = 1;
    loop {
        let cfg = app.config.read().await.clone();
        if cfg.app_id.is_empty() || cfg.app_secret.is_empty() {
            app.runtime.write().await.feishu = "unconfigured".into();
            app.changed();
            if revision.changed().await.is_err() {
                return;
            }
            continue;
        }
        app.runtime.write().await.feishu = "connecting".into();
        app.changed();
        let handler = EventDispatcherHandler::builder()
            .register_raw("im.message.receive_v1", Inbound(app.clone()))
            .expect("unique handler")
            .build();
        let config = open_lark::Config::builder()
            .app_id(&cfg.app_id)
            .app_secret(&cfg.app_secret)
            .base_url(&cfg.feishu_base)
            .max_response_size(2 * 1024 * 1024)
            .build();
        // open() owns one session. This supervisor also cancels old credentials on change.
        let started = Instant::now();
        app.runtime.write().await.feishu = "awaiting_events".into();
        app.changed();
        tokio::select! {
            _=app.shutdown.cancelled()=>return,
            result=LarkWsClient::open(Arc::new(config),handler)=>{
                // SDK errors can contain a gateway URL; keep credentials out of UI/logs.
                let reason=if result.is_ok(){"连接结束"}else{"连接断开或认证失败；请检查应用凭证和事件订阅"};
                let mut rt=app.runtime.write().await;rt.feishu="reconnecting".into();rt.feishu_error=reason.into();
            }
            _=revision.changed()=>{delay=1;continue}
        }
        app.changed();
        if started.elapsed() > Duration::from_secs(60) {
            delay = 1;
        }
        tokio::select! {
            _=app.shutdown.cancelled()=>return,
            _=revision.changed()=>delay=1,
            _=tokio::time::sleep(Duration::from_secs(delay))=>delay=(delay*2).min(60),
        }
    }
}

pub fn reset_cache(cache: &mut TokenCache) {
    *cache = TokenCache::default();
}
pub fn public_config(cfg: &Config) -> Value {
    json!({"app_id":cfg.app_id,"has_secret":!cfg.app_secret.is_empty(),"core_session":cfg.core_session,"hc":cfg.hc,"port":cfg.port})
}

/// Both private/group chat IDs and direct user Open IDs are valid destinations.
pub fn recipient_type(to: &str) -> Result<&'static str> {
    if to.len() > 200 || to.len() < 4 || !to.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("to 须为飞书聊天 ID（oc_…）或用户 Open ID（ou_…）");
    }
    if to.starts_with("oc_") {
        Ok("chat_id")
    } else if to.starts_with("ou_") {
        Ok("open_id")
    } else {
        bail!("to 须为飞书聊天 ID（oc_…）或用户 Open ID（ou_…）")
    }
}

/// Cache a bounded, best-effort group snapshot. Missing scopes never invent members.
pub async fn context(app: &App, m: &db::Message) -> Value {
    let source = &m.attachment["source"];
    let chat = source["chat_id"].as_str().unwrap_or("");
    if source["chat_type"] != "group" {
        return json!({"members_status":"not_applicable","members":[]});
    }
    let cached = app
        .db
        .sync(|c| {
            Ok(c.query_row(
                "SELECT context FROM conversations WHERE chat_id=?",
                [chat],
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
            let token = token(app).await?;
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
        let _ = app.db.sync(|c| {c.execute("UPDATE conversations SET context=?,name=CASE WHEN ?='' THEN name ELSE ? END WHERE chat_id=?",rusqlite::params![value.to_string(),value["name"].as_str().unwrap_or(""),value["name"].as_str().unwrap_or(""),chat])?;Ok(())});
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
