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
    Terminal {
        project: String,
    },
    Takeover {
        project: String,
        generation: String,
        active: bool,
    },
    ReleaseBeforeExit {
        project: String,
        generation: String,
        done: mpsc::Sender<()>,
    },
    Work {
        project: String,
    },
    Folder(PathBuf),
    Database(PathBuf),
    Environment,
    RestartService,
    UpdatePreferences {
        automatic: bool,
        channel: String,
    },
}
pub enum Reply {
    State(Value),
    Terminal {
        project: String,
        value: Value,
        error: String,
    },
    Takeover {
        project: String,
        generation: String,
        active: bool,
    },
    Work {
        project: String,
        timers: Value,
    },
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
    /// Preserve ordering with an in-flight acquisition, while keeping shutdown bounded.
    pub fn release_before_exit(&self, project: String, generation: String) -> flume::Receiver<()> {
        let (done, receiver) = flume::bounded(1);
        let (acknowledge, acknowledged) = mpsc::channel();
        let _ = self.tx.send(CommandRequest::ReleaseBeforeExit {
            project,
            generation,
            done: acknowledge,
        });
        std::thread::spawn(move || {
            let _ = acknowledged.recv_timeout(Duration::from_secs(3));
            let _ = done.send(());
        });
        receiver
    }
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
                ensure_service(&data, &cfg, &probe, false).await?;
                let mut connection = ConnectionState::default();
                let mut last_restart = std::time::Instant::now() - Duration::from_secs(10);
                let mut last_state = Value::Null;
                let mut takeover: Option<(String, String)> = None;
                let mut lease_checked = std::time::Instant::now();
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
                        Ok(CommandRequest::Terminal { project }) => {
                            let response = request(&probe, &cfg, "GET", &format!("/projects/{project}/terminal"), None).await;
                            let (value, error) = match response { Ok(value) => (value, String::new()), Err(error) => (Value::Null, error.to_string()) };
                            let _ = replies.send(Reply::Terminal { project, value, error });
                        }
                        Ok(CommandRequest::Takeover { project, generation, active }) => {
                            match request(&probe, &cfg, "POST", &format!("/projects/{project}/takeover"), Some(json!({"generation":generation,"active":active}))).await {
                                Ok(_) => {
                                    takeover = active.then(|| (project.clone(), generation.clone()));
                                    lease_checked = std::time::Instant::now();
                                    let _ = replies.send(Reply::Takeover { project, generation, active });
                                }
                                Err(error) => { let _ = replies.send(Reply::Error(error.to_string())); }
                            }
                        }
                        Ok(CommandRequest::ReleaseBeforeExit { project, generation, done }) => {
                            let _ = request(&probe, &cfg, "POST", &format!("/projects/{project}/takeover"), Some(json!({"generation":generation,"active":false}))).await;
                            if takeover.as_ref().is_some_and(|(p, g)| p == &project && g == &generation) {
                                takeover = None;
                            }
                            let _ = done.send(());
                        }
                        Ok(CommandRequest::Work { project }) => {
                            let timers = request(&probe, &cfg, "GET", &format!("/projects/{project}/timers"), None).await.unwrap_or(Value::Null);
                            let _ = replies.send(Reply::Work { project, timers });
                        }
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
                        Ok(CommandRequest::RestartService) => {
                            match ensure_service(&data, &cfg, &probe, true).await {
                                Ok(()) => {
                                    last_state = Value::Null;
                                    let _ = replies.send(Reply::Done {
                                        path: "/service/restart".into(),
                                        value: Value::Null,
                                        message: "Background service updated. Agent sessions were preserved.".into(),
                                    });
                                }
                                Err(error) => { let _ = replies.send(Reply::Error(format!("{error:#}"))); }
                            }
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
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            if let Some((project, generation)) = takeover.take() {
                                let _ = request(&probe, &cfg, "POST", &format!("/projects/{project}/takeover"), Some(json!({"generation":generation,"active":false}))).await;
                            }
                            break;
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if lease_checked.elapsed() >= Duration::from_secs(20) {
                        if let Some((project, generation)) = takeover.clone() {
                            match request(&probe, &cfg, "POST", &format!("/projects/{project}/takeover"), Some(json!({"generation":generation,"active":true,"heartbeat":true}))).await {
                                Ok(lease) if lease["active"].as_bool() == Some(false) => {
                                    takeover = None;
                                    let _ = replies.send(Reply::Takeover { project, generation, active: false });
                                }
                                Ok(_) => {}
                                Err(_) => {
                                    let _ = replies.send(Reply::Error("Terminal control needs to reconnect. Release control before continuing.".into()));
                                }
                            }
                        }
                        lease_checked = std::time::Instant::now();
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
// Reopening the desktop must not interrupt an authenticated, compatible daemon.
// An explicit service update opts into a bounded stop/start and its reconnection gap.
async fn ensure_service(
    data: &std::path::Path,
    cfg: &Config,
    probe: &reqwest::Client,
    restart: bool,
) -> Result<()> {
    match request(probe, cfg, "GET", "/state", None).await {
        Ok(state) => {
            if !restart
                && state["api_protocol"].as_u64() == Some(herdr_feishu_bridge::api::PROTOCOL)
            {
                return Ok(());
            }
            if let Some(version) = state["version"].as_str() {
                anyhow::ensure!(
                    !herdr_feishu_bridge::update::is_newer(
                        version,
                        herdr_feishu_bridge::update::CURRENT
                    )?,
                    "The running background service is newer than this app. Use the newer app instead of downgrading its service."
                );
                if version == herdr_feishu_bridge::update::CURRENT
                    && state["api_protocol"].as_u64() == Some(herdr_feishu_bridge::api::PROTOCOL)
                {
                    return Ok(());
                }
            }
            let signed = herdr_feishu_bridge::update::bundle().is_some_and(|bundle| {
                herdr_feishu_bridge::update::verify_bundle(&bundle, None).is_ok()
            });
            if !signed {
                bail!(
                    "Updating the background service requires a trusted installed Dauerbot app. Quit an incompatible daemon before starting this build, or use --data-dir for an independent instance."
                );
            }
            herdr_feishu_bridge::update::stop_service(data, cfg).await?;
        }
        Err(error) if connection_refused(&error) => {}
        Err(error) => return Err(error),
    }
    let mut child = spawn_service(data)?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(state) = request(probe, cfg, "GET", "/state", None).await {
                anyhow::ensure!(
                    state["api_protocol"].as_u64() == Some(herdr_feishu_bridge::api::PROTOCOL),
                    "The local daemon uses an incompatible API protocol"
                );
                return Ok::<_, anyhow::Error>(());
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
    .context("Local daemon startup timed out. Check desktop-service.log.")?
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
fn environment(_cfg: &Config, dir: &std::path::Path) -> Vec<(String, String)> {
    let mut result = vec![];
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    for (name, executable) in [
        ("HERDR", "herdr"),
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
    fn service_fixture(status: &str, state: Value) -> (Config, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let config = Config {
            port: listener.local_addr().unwrap().port(),
            token: "private-test-token".into(),
            ..Config::default()
        };
        let status = status.to_owned();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /api/state HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer private-test-token")
            );
            let body = state.to_string();
            write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        (config, worker)
    }

    #[tokio::test]
    async fn reopening_a_compatible_daemon_never_restarts_it_for_a_version_difference() {
        let data = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for version in ["0.0.1", "99.0.0"] {
            let (config, worker) = service_fixture(
                "200 OK",
                json!({
                    "version": version, "api_protocol": herdr_feishu_bridge::api::PROTOCOL
                }),
            );
            // No bundled executable, daemon lock, or install exists here. Any
            // attempted stop/start would fail instead of silently passing.
            ensure_service(data.path(), &config, &client, false)
                .await
                .unwrap();
            worker.join().unwrap();
        }
    }

    #[tokio::test]
    async fn service_replacements_never_downgrade_a_newer_daemon() {
        let data = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (restart, protocol) in [
            (true, herdr_feishu_bridge::api::PROTOCOL),
            (false, herdr_feishu_bridge::api::PROTOCOL + 1),
        ] {
            let (config, worker) = service_fixture(
                "200 OK",
                json!({"version": "99.0.0", "api_protocol": protocol}),
            );
            let error = ensure_service(data.path(), &config, &client, restart)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("newer than this app"));
            worker.join().unwrap();
        }
    }

    #[tokio::test]
    async fn authentication_failure_does_not_replace_the_running_service() {
        let (config, worker) = service_fixture("401 Unauthorized", json!({"error":"Unauthorized"}));
        let data = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = ensure_service(data.path(), &config, &client, false)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Unauthorized");
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn explicit_updates_and_incompatible_protocols_keep_the_trusted_install_gate() {
        let data = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (restart, protocol) in [
            (true, Some(herdr_feishu_bridge::api::PROTOCOL)),
            (false, None),
        ] {
            let (config, worker) = service_fixture("200 OK", json!({"api_protocol":protocol}));
            let error = ensure_service(data.path(), &config, &client, restart)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("trusted installed Dauerbot app"));
            worker.join().unwrap();
        }
    }

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
