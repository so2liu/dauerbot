use anyhow::{Context, Result, bail};
use herdr_feishu_bridge::config::{self, Config};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

pub enum CommandRequest {
    Api {
        method: &'static str,
        path: String,
        body: Option<Value>,
        success: String,
    },
    Folder(PathBuf),
    Database(PathBuf),
    Environment,
    Watch(String),
    UpdatePreferences {
        automatic: bool,
        channel: String,
    },
}
pub enum Reply {
    State(Value),
    Detail(Value),
    Done {
        path: String,
        value: Value,
        message: String,
    },
    Access(String),
    Environment(Vec<(String, String)>),
    Error(String),
    Connection(Option<String>),
    Update(Value),
    Restart,
}

pub struct Backend {
    pub tx: mpsc::Sender<CommandRequest>,
    pub rx: mpsc::Receiver<Reply>,
    pub dir: PathBuf,
    pub updates: mpsc::Sender<crate::updater::Action>,
}
impl Backend {
    pub fn start(dir: PathBuf) -> Self {
        let (tx, requests) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let updates = crate::updater::start(dir.clone(), replies.clone());
        let updater = updates.clone();
        let data = dir.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            let result = runtime.block_on(async {
                config::private_dir(&data)?;
                let first_launch = !data.join("config.json").exists();
                let mut cfg = Config::load(&data)?;
                if first_launch {
                    // Avoid conflicting with another Dauerbot instance.
                    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
                    cfg.port = listener.local_addr()?.port();
                    cfg.save(&data)?;
                }
                // Local bearer tokens and control traffic must never reach a proxy.
                let http = reqwest::Client::builder()
                    .no_proxy()
                    .connect_timeout(Duration::from_secs(2))
                    .timeout(Duration::from_secs(110))
                    .build()?;
                let probe = reqwest::Client::builder()
                    .no_proxy()
                    .connect_timeout(Duration::from_secs(1))
                    .timeout(Duration::from_secs(3))
                    .build()?;
                if let Ok(state) = request(&probe, &cfg, "GET", "/state", None).await
                    && state["version"]
                        .as_str()
                        .is_some_and(|v| v != herdr_feishu_bridge::update::CURRENT)
                    && let Some(bundle) = herdr_feishu_bridge::update::bundle()
                    && herdr_feishu_bridge::update::verify_bundle(&bundle, None).is_ok()
                {
                    herdr_feishu_bridge::update::stop_service(&data, &cfg).await?;
                }
                if let Err(e) = request(&probe, &cfg, "GET", "/state", None).await {
                    if !connection_refused(&e) {
                        return Err(e);
                    }
                    let mut child = spawn_service(&data)?;
                    tokio::time::timeout(Duration::from_secs(15), async {
                        loop {
                            if request(&probe, &cfg, "GET", "/state", None).await.is_ok() {
                                break Ok::<_, anyhow::Error>(());
                            }
                            if let Some(status) = child.try_wait()? {
                                bail!(
                                    "The local daemon exited ({status}). Check {}",
                                    data.join("desktop-service.log").display()
                                );
                            }
                            tokio::time::sleep(Duration::from_millis(150)).await;
                        }
                    })
                    .await
                    .context("Local daemon startup timed out. Check desktop-service.log.")??;
                }
                let mut connection = ConnectionState::default();
                let mut last_restart = std::time::Instant::now() - Duration::from_secs(10);
                let mut last_state = Value::Null;
                let mut watched = String::new();
                let mut last_detail = Value::Null;
                loop {
                    match requests.recv_timeout(Duration::from_millis(1200)) {
                        Ok(CommandRequest::Api {
                            method,
                            path,
                            body,
                            success,
                        }) => match request(&http, &cfg, method, &path, body).await {
                            Ok(value) => {
                                let _ = replies.send(Reply::Done {
                                    path,
                                    value,
                                    message: success,
                                });
                            }
                            Err(e) => {
                                let _ = replies.send(Reply::Error(e.to_string()));
                            }
                        },
                        Ok(CommandRequest::Folder(path)) => {
                            let result = crate::access::project_access(&path);
                            let _ = replies.send(Reply::Access(match result {
                                Ok(s) => s,
                                Err(e) => crate::access::explain(&e),
                            }));
                        }
                        Ok(CommandRequest::Database(path)) => {
                            let result = crate::access::database_access(&path);
                            let _ = replies.send(Reply::Access(match result {
                                Ok(s) => s,
                                Err(e) => crate::access::explain(&e),
                            }));
                        }
                        Ok(CommandRequest::Environment) => {
                            let _ = replies.send(Reply::Environment(environment(&cfg, &data)));
                        }
                        Ok(CommandRequest::Watch(id)) => {
                            watched = id;
                            last_detail = Value::Null;
                        }
                        Ok(CommandRequest::UpdatePreferences { automatic, channel }) => {
                            let result =
                                herdr_feishu_bridge::update::Preferences { automatic, channel }
                                    .save(&data);
                            if let Err(error) = result {
                                let _ = replies.send(Reply::Error(format!("{error:#}")));
                            } else {
                                let _ = updater.send(crate::updater::Action::PreferencesChanged);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    match request(&probe, &cfg, "GET", "/state", None).await {
                        Ok(state) => {
                            if let Some(reply) = connection.observe(None) {
                                let _ = replies.send(reply);
                            }
                            if state != last_state {
                                last_state = state.clone();
                                let _ = replies.send(Reply::State(state));
                            }
                        }
                        Err(e) => {
                            if let Some(reply) = connection.observe(Some(format!("{e:#}"))) {
                                let _ = replies.send(reply);
                            }
                            if connection_refused(&e)
                                && last_restart.elapsed() >= Duration::from_secs(10)
                            {
                                last_restart = std::time::Instant::now();
                                // A service restart may have changed its endpoint or token.
                                if let Ok(current) = Config::load(&data) {
                                    cfg = current;
                                }
                                if request(&probe, &cfg, "GET", "/state", None).await.is_err()
                                    && let Err(error) = spawn_service(&data)
                                    && let Some(reply) = connection
                                        .observe(Some(format!("Daemon restart failed: {error:#}")))
                                {
                                    let _ = replies.send(reply);
                                }
                            }
                        }
                    }
                    if !watched.is_empty()
                        && let Ok(detail) =
                            request(&http, &cfg, "GET", &format!("/projects/{watched}"), None).await
                        && detail != last_detail
                    {
                        last_detail = detail.clone();
                        let _ = replies.send(Reply::Detail(detail));
                    }
                }
                Ok::<_, anyhow::Error>(())
            });
            if let Err(e) = result {
                let _ = replies.send(Reply::Error(e.to_string()));
            }
        });
        Self {
            tx,
            rx,
            dir,
            updates,
        }
    }
}
#[derive(Default)]
struct ConnectionState {
    error: Option<String>,
}
impl ConnectionState {
    fn observe(&mut self, error: Option<String>) -> Option<Reply> {
        if self.error == error {
            return None;
        }
        self.error = error.clone();
        Some(Reply::Connection(error))
    }
}
fn connection_refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(reqwest::Error::is_connect)
}
fn spawn_service(data: &std::path::Path) -> Result<std::process::Child> {
    let binary = std::env::current_exe()?.with_file_name("hfb");
    anyhow::ensure!(
        binary.is_file(),
        "Bundled hfb executable missing. Reinstall the app."
    );
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("desktop-service.log"))?;
    let mut paths = vec![];
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(&home).join(".local/bin"));
        paths.push(PathBuf::from(home).join(".cargo/bin"));
    }
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    Command::new(binary)
        .arg("--data-dir")
        .arg(data)
        .arg("serve")
        .env("PATH", std::env::join_paths(paths)?)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("Cannot start the local daemon")
}

async fn request(
    http: &reqwest::Client,
    cfg: &Config,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let mut req = http
        .request(method.parse()?, format!("{}/api{path}", cfg.url()))
        .bearer_auth(&cfg.token);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let response = req
        .send()
        .await
        .context("Cannot connect to the local service")?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .context("The service returned invalid data")?;
    if !status.is_success() {
        bail!("{}", value["error"].as_str().unwrap_or("Request failed"));
    }
    Ok(value)
}
fn environment(cfg: &Config, dir: &std::path::Path) -> Vec<(String, String)> {
    let mut result = vec![];
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    for (name, executable) in [
        ("HERDR", cfg.hc.as_str()),
        ("Codex", "codex"),
        ("Claude Code", "claude"),
        ("Pi", "pi"),
        ("Antigravity", "agy"),
    ] {
        let mut candidates = vec![PathBuf::from(executable)];
        for root in [
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ] {
            candidates.push(root.join(executable));
        }
        candidates.extend(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join(executable)),
        );
        let resolved = candidates.into_iter().find(|p| p.is_file());
        result.push((
            name.into(),
            resolved
                .map(|p| format!("Found · {} (check sign-in in the CLI)", p.display()))
                .unwrap_or_else(|| "Not found · install it and check again".into()),
        ));
    }
    let database = crate::access::database_access(&dir.join("bridge.sqlite3"));
    result.push((
        "Application database".into(),
        if database.is_ok() {
            "Readable · Dauerbot's SQLite database does not need Full Disk Access".into()
        } else {
            "Unreadable · check data-directory permissions".into()
        },
    ));
    result
}

pub fn scopes(all_group_replies: bool) -> Value {
    let mut scopes = vec![
        "im:message:send_as_bot",
        "im:message.p2p_msg:readonly",
        "im:message.group_at_msg:readonly",
        "im:message:readonly",
        "im:resource",
        "im:chat.members:read",
    ];
    if all_group_replies {
        scopes.push("im:message.group_msg");
    }
    json!({"scopes":{"tenant":scopes,"user":[]}})
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    #[test]
    fn recovery_is_emitted_even_when_the_api_state_has_not_changed() {
        let mut state = ConnectionState::default();
        assert!(state.observe(None).is_none());
        assert!(matches!(
            state.observe(Some("connection refused".into())),
            Some(Reply::Connection(Some(_)))
        ));
        assert!(state.observe(Some("connection refused".into())).is_none());
        assert!(matches!(state.observe(None), Some(Reply::Connection(None))));
        assert!(state.observe(None).is_none());
    }
}
