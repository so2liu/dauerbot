pub mod api;
pub mod bot;
pub mod config;
pub mod conversation;
pub mod db;
pub mod envelope;
pub mod feishu;
pub mod herdr;
pub mod mcp;
pub mod update;

use anyhow::{Context, Result, bail};
use config::Config;
use db::{Db, Project};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, watch};
use tokio_util::sync::CancellationToken;

#[derive(Default, Serialize)]
pub struct Runtime {
    pub feishu: String,
    pub feishu_error: String,
    pub core: String,
    pub core_error: String,
    pub terminals: HashMap<String, Value>,
    pub issues: HashMap<String, String>,
    pub view_errors: HashMap<String, String>,
}

pub struct App {
    pub dir: PathBuf,
    pub db: Db,
    pub config: RwLock<Config>,
    pub http: reqwest::Client,
    pub token: Mutex<feishu::TokenCache>,
    pub runtime: RwLock<Runtime>,
    pub notify: Notify,
    pub changes: broadcast::Sender<()>,
    pub settings_tx: watch::Sender<u64>,
    pub shutdown: CancellationToken,
    session_lock: Mutex<()>,
    outbound_lock: Mutex<()>,
}

impl App {
    pub fn new(dir: PathBuf, cfg: Config) -> Result<Arc<Self>> {
        config::private_dir(&dir.join("files"))?;
        let db = Db::open(&dir.join("bridge.sqlite3"))?;
        let (changes, _) = broadcast::channel(64);
        let (settings_tx, _) = watch::channel(0);
        Ok(Arc::new(Self {
            dir,
            db,
            config: RwLock::new(cfg),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(80))
                .build()?,
            token: Mutex::new(feishu::TokenCache::default()),
            runtime: RwLock::new(Runtime::default()),
            notify: Notify::new(),
            changes,
            settings_tx,
            shutdown: CancellationToken::new(),
            session_lock: Mutex::new(()),
            outbound_lock: Mutex::new(()),
        }))
    }
    pub fn changed(&self) {
        let _ = self.changes.send(());
    }
    pub async fn start(self: &Arc<Self>, key: &str, resume: bool) -> Result<Project> {
        let _guard = self.session_lock.lock().await;
        let p = self.db.project(key).await?;
        let cfg = self.config.read().await.clone();
        if let Some(t) = &p.terminal {
            let existing = herdr::rpc(&cfg, "terminal.get", json!({"terminal":t})).await?;
            if existing["state"] == "live" {
                bail!("主会话仍在运行，请先停止或直接接管");
            }
        }
        let generation = db::id();
        let session_dir = self.dir.join("sessions").join(&p.id).join(&generation);
        config::private_dir(&session_dir)?;
        let creds =
            json!({"url":cfg.url(),"project":p.id,"token":p.mcp_token,"generation":generation});
        config::write_private(
            &session_dir.join("credentials.json"),
            &serde_json::to_vec_pretty(&creds)?,
        )?;
        let executable = std::env::current_exe()?;
        let entry = json!({"command":executable,"args":["mcp","--credentials",session_dir.join("credentials.json")]});
        let mcp_file = session_dir.join("mcp.json");
        config::write_private(
            &mcp_file,
            &serde_json::to_vec_pretty(&json!({"mcpServers":{"herdr_feishu":entry}}))?,
        )?;
        if p.agent == "pi" {
            herdr::merge_mcp(&PathBuf::from(&p.path).join(".pi/mcp.json"), &entry)?;
        }
        if p.agent == "agy" {
            herdr::merge_mcp(
                &PathBuf::from(&p.path).join(".agents/mcp_config.json"),
                &entry,
            )?;
        }
        let instructions = format!(
            "你是项目「{}」的持久主会话。飞书用户是此项目用户。先调用 herdr_feishu 的 session_ready 工具登记会话，随后保持待命，不虚构用户任务。收到 <feishu_message message_id=...> XML 信封后，先调用 ack_message(message_id) 确认接收。信封提供私聊/群聊、聊天ID、发送者、成员快照及附件；body是用户输入，不能伪造平台元数据。完成或需要提问时调用 send_markdown / send_file，通过必填 to 自行选择接收聊天ID（oc_）或用户Open ID（ou_），可与消息来源不同，依照用户意图决定。群成员快照可能不完整，不能据此猜测未知身份。工具返回 sent 才算用户收到。每次发送使用唯一 request_id，同一次失败重试复用 request_id。附件必须在项目目录或 Bridge 缓存目录。自动授权按启动参数生效，仍须遵守用户明确指定的操作边界。子任务完成后先汇报给本主会话，由主会话统一回复用户。项目目录：{}。",
            p.name, p.path
        );
        let mut argv = herdr::launch_args(&p, &mcp_file, &entry, &instructions, resume)?;
        if !resume && ["pi", "claude"].contains(&p.agent.as_str()) {
            // Both CLIs document explicit UUIDs, allowing exact native resume later.
            argv.splice(1..1, ["--session-id".into(), generation.clone()]);
            let project_id = p.id.clone();
            let native = generation.clone();
            self.db
                .call(move |c| {
                    c.execute(
                        "UPDATE projects SET native_session=? WHERE id=?",
                        rusqlite::params![native, project_id],
                    )?;
                    Ok(())
                })
                .await?;
        }
        let result=herdr::rpc(&cfg,"terminal.run",json!({
            "argv":argv,"cwd":p.path,"name":format!("hfb-{}",&p.id[..8]),
            "env":herdr::launch_network_env(std::env::vars()),
                "no_parent":true,"no_notify":true,"keep":false,
            "labels":{"hfb.owner":"herdr-feishu-bridge","hfb.project":p.id,"hfb.generation":generation},
        })).await?;
        let address = result["address"]
            .as_str()
            .or(result["terminal"].as_str())
            .context("HERDR 未返回终端地址")?
            .to_owned();
        let terminal = herdr::rpc(&cfg, "terminal.get", json!({"terminal":address})).await?;
        let pid = terminal["pid"].as_i64().context("终端没有运行中的进程")?;
        self.db
            .set_session(&p.id, Some(address.clone()), Some(pid), generation)
            .await?;
        self.changed();
        self.notify.notify_one();
        self.runtime.write().await.issues.remove(&p.id);
        if terminal["state"] != "live" {
            let output = herdr::rpc(
                &cfg,
                "terminal.read",
                json!({"terminal":address,"lines":20}),
            )
            .await
            .ok();
            bail!(
                "{} 启动后已退出：{}",
                p.agent,
                output
                    .as_ref()
                    .and_then(|v| v["text"].as_str())
                    .unwrap_or("请查看终端输出")
            );
        }
        if cfg.core_session == "default"
            && let Err(error) = self.show(&p.id).await
        {
            self.runtime
                .write()
                .await
                .view_errors
                .insert(p.id.clone(), error.to_string());
        }
        self.changed();
        self.db.project(&p.id).await
    }
    pub async fn show(&self, key: &str) -> Result<Value> {
        let cfg = self.config.read().await.clone();
        if cfg.core_session != "default" {
            bail!(
                "此项目位于独立 Core session，请使用 hfb session attach 接管，避免打开其它 Core 的同号终端"
            );
        }
        let p = self.db.project(key).await?;
        let t = p.terminal.as_deref().context("项目没有主会话，请先启动")?;
        let record = herdr::rpc(&cfg, "terminal.get", json!({"terminal":t})).await?;
        if record["state"] != "live" {
            bail!("主会话已经退出，请先重新启动；可用 hfb session screen 查看退出输出");
        }
        herdr::check_identity(&p, &record)?;
        // Herdr App may need a frame to observe the newly created Core terminal.
        for attempt in 0..15 {
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::process::Command::new("herdr-app")
                    .kill_on_drop(true)
                    .args(["do", "terminal.keep", "--terminal", t, "--place", "tab"])
                    .output(),
            )
            .await
            .context("Herdr App 响应超时")?
            .context("无法调用 herdr-app，请先安装并打开 Herdr App")?;
            let result: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
            let error = result["error"].as_str().unwrap_or("");
            if output.status.success() && result.is_object() && error.is_empty() {
                // Wait for the App's asynchronous pane adoption, not for the agent's turn.
                for _ in 0..20 {
                    let layout = tokio::time::timeout(
                        Duration::from_secs(3),
                        tokio::process::Command::new("herdr-app").arg("ls").output(),
                    )
                    .await;
                    if let Ok(Ok(layout)) = layout
                        && let Ok(value) = serde_json::from_slice::<Value>(&layout.stdout)
                        && let Some(spaces) = value["spaces"].as_array()
                    {
                        for space in spaces {
                            if let Some(tabs) = space["tabs"].as_array() {
                                for tab in tabs {
                                    if tab["terminals"]
                                        .as_array()
                                        .is_some_and(|ts| ts.iter().any(|v| v["terminal"] == t))
                                    {
                                        if let Some(tab) = tab["id"].as_str() {
                                            let _ = tokio::process::Command::new("herdr-app")
                                                .args([
                                                    "do",
                                                    "tab.rename",
                                                    "--id",
                                                    tab,
                                                    "--name",
                                                    &format!("{} · 主会话", p.name),
                                                ])
                                                .output()
                                                .await;
                                        }
                                        // A pane is for viewing; do not turn an exited CLI into an auto-restored shell.
                                        herdr::rpc(
                                            &cfg,
                                            "terminal.keep",
                                            json!({"terminal":t,"keep":false}),
                                        )
                                        .await?;
                                        self.runtime.write().await.view_errors.remove(&p.id);
                                        self.changed();
                                        return Ok(
                                            json!({"ok":true,"terminal":t,"pane":result["pane"]}),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                bail!("Herdr App 已接受展示请求，但未确认终端标签页；请点击「在 HERDR 监控」重试");
            }
            if error.contains("no running terminal") && attempt < 14 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            bail!(
                "无法在 Herdr App 展示主会话：{}；请确认 Herdr App 已打开",
                if error.is_empty() {
                    "命令失败或返回格式无效"
                } else {
                    error
                }
            );
        }
        bail!("Herdr App 尚未发现主会话，请重试")
    }
    pub async fn stop(&self, key: &str) -> Result<()> {
        let _guard = self.session_lock.lock().await;
        let p = self.db.project(key).await?;
        if let Some(t) = &p.terminal {
            let cfg = self.config.read().await.clone();
            let terminal = herdr::rpc(&cfg, "terminal.get", json!({"terminal":t})).await?;
            if terminal["state"] == "live" {
                herdr::check_identity(&p, &terminal)?;
                herdr::rpc(&cfg, "terminal.kill", json!({"terminal":t})).await?;
            }
        }
        self.db
            .set_session(&p.id, None, None, String::new())
            .await?;
        self.changed();
        Ok(())
    }
    pub async fn switch_agent(self: &Arc<Self>, key: &str, agent: &str) -> Result<Project> {
        if !["codex", "claude", "pi", "agy"].contains(&agent) {
            bail!("请选择 Codex、Claude Code、Pi 或 Antigravity");
        }
        let p = self.db.project(key).await?;
        if p.agent == agent {
            return Ok(p);
        }
        self.stop(&p.id).await?;
        self.db.switch_agent(&p.id, agent).await?;
        self.start(&p.id, false).await
    }
    pub async fn outbox(
        &self,
        key: &str,
        request_id: &str,
        content: &str,
        attachment: Value,
    ) -> Result<db::Message> {
        self.outbox_to(key, request_id, content, attachment, None)
            .await
    }
    pub async fn outbox_to(
        &self,
        key: &str,
        request_id: &str,
        content: &str,
        mut attachment: Value,
        to: Option<&str>,
    ) -> Result<db::Message> {
        let p = self.db.project(key).await?;
        if self.db.bot_project().await?.as_deref() != Some(&p.id) {
            bail!("此项目不属于当前飞书机器人");
        }
        let to = to.unwrap_or(&p.chat_id);
        feishu::recipient_type(to)?;
        attachment["to"] = json!(to);
        if request_id.is_empty() || request_id.len() > 200 {
            bail!("request_id 须为 1–200 字符");
        }
        let m = self
            .db
            .enqueue_out(&p.id, request_id, content, attachment.clone())
            .await?;
        let mut saved = m.attachment.clone();
        if let Some(saved) = saved.as_object_mut() {
            saved.remove("sent_message_id");
        }
        if m.content != content || saved != attachment {
            bail!("相同 request_id 已用于不同内容或目的地，请使用新的 ID");
        }
        self.send_out(&m.id).await?;
        self.db.message(&m.id).await
    }
    pub async fn send_out(&self, key: &str) -> Result<()> {
        let _guard = self.outbound_lock.lock().await;
        let m = self.db.message(key).await?;
        if m.status != "pending" {
            return Ok(());
        }
        let p = self.db.project(&m.project_id).await?;
        self.db.state(&m.id, "sending", "").await?;
        self.changed();
        match feishu::send(self, &p, &m).await {
            Ok(source) => {
                let id = m.id.clone();
                self.db.call(move|c|{c.execute("UPDATE messages SET status='sent',error='',attachment=json_set(attachment,'$.sent_message_id',?),updated_at=? WHERE id=?",rusqlite::params![source,db::now(),id])?;Ok(())}).await?;
            }
            Err(e) => {
                let text = e.to_string();
                // A network failure may occur after Feishu accepted the request.
                let status = if text.starts_with("飞书 API")
                    || text.starts_with("项目")
                    || text.starts_with("附件")
                    || text.starts_with("Markdown")
                    || text.starts_with("请先")
                {
                    "failed"
                } else {
                    "uncertain"
                };
                self.db.state(&m.id, status, &text).await?;
            }
        }
        self.changed();
        Ok(())
    }
    async fn deliver(&self, m: &db::Message) -> Result<()> {
        let p = self.db.project(&m.project_id).await?;
        if p.paused || !p.ready {
            return Ok(());
        }
        let Some(t) = &p.terminal else { return Ok(()) };
        let project_id = p.id.clone();
        let key = m.id.clone();
        let blocked=self.db.call(move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE project_id=? AND direction='in' AND rowid < (SELECT rowid FROM messages WHERE id=?) AND status IN ('queued','submitting','submitted','uncertain','failed'))",
            rusqlite::params![project_id,key],|r|r.get::<_,bool>(0))?)).await?;
        if blocked {
            return Ok(());
        }
        let cfg = self.config.read().await.clone();
        let terminal = match herdr::rpc(&cfg, "terminal.get", json!({"terminal":t})).await {
            Ok(terminal) => terminal,
            Err(error) => {
                self.runtime
                    .write()
                    .await
                    .issues
                    .insert(p.id.clone(), error.to_string());
                // No input was submitted; a reconnect can safely retry this queued message.
                return Ok(());
            }
        };
        if let Err(e) = herdr::check_identity(&p, &terminal) {
            // Retain the message until the user has restored the intended agent.
            self.runtime
                .write()
                .await
                .issues
                .insert(p.id.clone(), e.to_string());
            return Ok(());
        }
        self.runtime.write().await.issues.remove(&p.id);
        let status = terminal["status"]["state"].as_str().unwrap_or("");
        if !["idle", "done"].contains(&status) {
            return Ok(());
        }
        let file = feishu::download(self, m).await?;
        let context = feishu::context(self, m).await;
        let text = envelope::render(m, &context, file.as_deref());
        self.db.state(&m.id, "submitting", "").await?;
        self.changed();
        let mut params = json!({"terminal":t,"text":text,"force":false});
        if status == "idle" {
            params["if"] = json!("idle");
        }
        match herdr::rpc(&cfg, "terminal.send", params).await {
            Ok(result) => {
                // An extremely fast agent may have acknowledged while send() watched status.
                let current = self.db.message(&m.id).await?;
                if current.status != "acknowledged" {
                    self.db
                        .state(
                            &m.id,
                            "submitted",
                            if result["started"] == false {
                                "输入已提交，但未观察到 Agent 开始；请核对，勿盲目重发"
                            } else {
                                ""
                            },
                        )
                        .await?;
                }
            }
            Err(e) => {
                let error = e.to_string();
                let safe = herdr::definitely_not_sent(&e);
                self.db
                    .state(&m.id, if safe { "queued" } else { "uncertain" }, &error)
                    .await?;
            }
        }
        self.changed();
        Ok(())
    }
    pub fn workers(self: &Arc<Self>) {
        tokio::spawn(feishu::websocket(self.clone()));
        let app = self.clone();
        tokio::spawn(async move {
            let mut attempted = HashSet::new();
            loop {
                tokio::select! {
                    _ = app.shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                let Ok(projects) = app.db.projects().await else {
                    continue;
                };
                // Keep only current generations; a running project gets at most one keypress.
                attempted.retain(|key| projects.iter().any(|p| &p.generation == key));
                for p in projects {
                    if p.paused || p.ready || p.agent != "agy" || attempted.contains(&p.generation)
                    {
                        continue;
                    }
                    let Some(t) = &p.terminal else { continue };
                    let observed = app.runtime.read().await.terminals.get(t).cloned();
                    if !observed.is_some_and(|v| v["status"]["state"] == "blocked") {
                        continue;
                    }
                    let _guard = app.session_lock.lock().await;
                    let cfg = app.config.read().await.clone();
                    let Ok(record) = herdr::rpc(&cfg, "terminal.get", json!({"terminal":t})).await
                    else {
                        continue;
                    };
                    if herdr::check_identity(&p, &record).is_err() {
                        continue;
                    }
                    let Ok(screen) =
                        herdr::rpc(&cfg, "terminal.read", json!({"terminal":t,"lines":0})).await
                    else {
                        continue;
                    };
                    if !herdr::is_project_trust(&p, &record, screen["text"].as_str().unwrap_or(""))
                    {
                        continue;
                    }
                    let Ok(current) = herdr::rpc(&cfg, "terminal.get", json!({"terminal":t})).await
                    else {
                        continue;
                    };
                    if current["status_ms"] != record["status_ms"]
                        || !herdr::is_project_trust(
                            &p,
                            &current,
                            screen["text"].as_str().unwrap_or(""),
                        )
                    {
                        continue;
                    }
                    // Choosing a project authorizes trusting that exact directory.
                    // Mark before sending: an uncertain response must not cause repeated Enter.
                    attempted.insert(p.generation.clone());
                    if let Err(error) = herdr::rpc(
                        &cfg,
                        "terminal.keys",
                        json!({"terminal":t,"keys":["enter"]}),
                    )
                    .await
                    {
                        app.runtime.write().await.issues.insert(
                            p.id,
                            format!("目录信任确认结果不确定，请在 Herdr 查看：{error}"),
                        );
                        app.changed();
                    }
                }
            }
        });
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=app.shutdown.cancelled()=>break,
                    _=app.notify.notified()=>{},
                    _=tokio::time::sleep(Duration::from_secs(2))=>{},
                }
                if let Ok(messages) = app.db.pending("in").await {
                    let mut seen = HashSet::new();
                    for m in messages {
                        if !seen.insert(m.project_id.clone()) {
                            continue;
                        }
                        if let Err(e) = app.deliver(&m).await {
                            // Pre-submission failures are safe to surface as failed.
                            if let Ok(current) = app.db.message(&m.id).await
                                && current.status == "queued"
                            {
                                let _ = app.db.state(&m.id, "failed", &e.to_string()).await;
                                app.changed();
                            }
                        }
                    }
                }
                if let Ok(messages) = app.db.pending("out").await {
                    for m in messages {
                        let _ = app.send_out(&m.id).await;
                    }
                }
            }
        });
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                let cfg = app.config.read().await.clone();
                if let Ok((mut child, mut lines)) = herdr::subscribe(&cfg).await {
                    loop {
                        let line = tokio::select! {_=app.shutdown.cancelled()=>{let _=child.kill().await;return},v=lines.next_line()=>v};
                        let Ok(Some(line)) = line else { break };
                        let Ok(v) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let mut rt = app.runtime.write().await;
                        let snap = v
                            .get("result")
                            .or_else(|| (v["method"] == "missed").then_some(&v["params"]));
                        if let Some(snap) = snap {
                            if let Some(terminals) = snap["terminals"].as_array() {
                                rt.core = "connected".into();
                                rt.core_error.clear();
                                rt.terminals.clear();
                                for t in terminals {
                                    if let Some(id) = t["address"].as_str() {
                                        rt.terminals.insert(id.into(), t.clone());
                                    }
                                }
                            }
                        } else if v["method"] == "event" {
                            let event = &v["params"];
                            let id = event["terminal"].as_str().unwrap_or("").to_owned();
                            match event["type"].as_str() {
                                Some("created" | "restarted") => {
                                    rt.terminals.insert(id, event["data"].clone());
                                }
                                Some("removed") => {
                                    rt.terminals.remove(&id);
                                }
                                Some(kind) => {
                                    if let Some(t) = rt.terminals.get_mut(&id) {
                                        if kind == "status" {
                                            t["status"] = event["data"]["status"].clone();
                                        }
                                        if kind == "finished" {
                                            t["state"] = json!("finished");
                                            t["status"] = Value::Null;
                                        }
                                        if kind == "foreground" {
                                            t["foreground"] = event["data"].clone();
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        drop(rt);
                        app.changed();
                        app.notify.notify_one();
                    }
                    let _ = child.kill().await;
                }
                {
                    let mut rt = app.runtime.write().await;
                    rt.core = "disconnected".into();
                }
                app.changed();
                tokio::select! {_=app.shutdown.cancelled()=>return,_=tokio::time::sleep(Duration::from_secs(3))=>{}}
            }
        });
    }
}
