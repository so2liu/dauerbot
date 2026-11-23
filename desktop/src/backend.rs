use anyhow::{Context, Result, bail};
use herdr_feishu_bridge::config::{self, Config};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

enum DesktopPreferences {
    Save(Vec<u8>),
    Flush(mpsc::Sender<()>),
}

fn apply_desktop_preferences(dir: &std::path::Path, request: DesktopPreferences) -> Result<()> {
    match request {
        DesktopPreferences::Save(bytes) => {
            config::private_dir(dir)?;
            config::write_private(&dir.join("desktop.json"), &bytes)
        }
        DesktopPreferences::Flush(done) => {
            let _ = done.send(());
            Ok(())
        }
    }
}

fn start_desktop_preferences(
    dir: PathBuf,
    replies: mpsc::Sender<Reply>,
) -> (
    mpsc::Sender<DesktopPreferences>,
    std::thread::JoinHandle<()>,
) {
    let (tx, requests) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        for request in requests {
            if let Err(error) = apply_desktop_preferences(&dir, request) {
                let _ = replies.send(Reply::Error(format!(
                    "Could not save desktop preferences: {error}"
                )));
            }
        }
    });
    (tx, worker)
}

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
    Update(crate::updater::Action),
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
    ActivationFreeze {
        reply: mpsc::Sender<Result<Value>>,
        freeze: bool,
    },
    ActivationResume,
}

pub struct Backend {
    preferences: mpsc::Sender<DesktopPreferences>,
    pub tx: mpsc::Sender<CommandRequest>,
    pub rx: mpsc::Receiver<Reply>,
    pub dir: PathBuf,
    pub activation_pause: Arc<AtomicBool>,
    pub frontend_ready: Arc<AtomicBool>,
    pub candidate_ready: Option<Arc<AtomicBool>>,
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
    pub fn save_preferences(&self, bytes: Vec<u8>) {
        let _ = self.preferences.send(DesktopPreferences::Save(bytes));
    }
    /// The independent FIFO is unaffected by slow daemon/network requests.
    pub fn flush_preferences_before_exit(&self) -> flume::Receiver<()> {
        let (done, receiver) = flume::bounded(1);
        let (acknowledge, acknowledged) = mpsc::channel();
        let _ = self
            .preferences
            .send(DesktopPreferences::Flush(acknowledge));
        std::thread::spawn(move || {
            let _ = acknowledged.recv_timeout(Duration::from_secs(3));
            let _ = done.send(());
        });
        receiver
    }
    pub fn start(dir: PathBuf) -> Self {
        Self::start_mode(dir, None)
    }
    pub fn start_mode(
        dir: PathBuf,
        candidate: Option<(Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicBool>)>,
    ) -> Self {
        let candidate_ready = candidate.as_ref().map(|candidate| candidate.2.clone());
        let (tx, requests) = mpsc::channel();
        let (replies, rx) = mpsc::channel();
        let (preferences, _) = start_desktop_preferences(dir.clone(), replies.clone());
        let startup_updates = Arc::new(AtomicBool::new(false));
        let update_ready = startup_updates.clone();
        let updates = crate::updater::start(
            dir.clone(),
            replies.clone(),
            Some(
                candidate
                    .as_ref()
                    .map(|c| c.1.clone())
                    .unwrap_or(startup_updates),
            ),
        );
        let updater = updates.clone();
        let activation_pause = Arc::new(AtomicBool::new(false));
        let pause = activation_pause.clone();
        let frontend_ready = Arc::new(AtomicBool::new(false));
        let ready = frontend_ready.clone();
        let data = dir.clone();
        std::thread::spawn(move || {
            if let Some((gate, _, _)) = &candidate {
                while !gate.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
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
                while !ready.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_millis(25)).await; }
                match ensure_service(&data, &cfg, &probe, false).await {
                    Ok(()) => update_ready.store(true, Ordering::Release),
                    Err(error) => {
                        // Keep observing: preparation, transport readiness or an uncertain
                        // control ACK must not permanently disable the desktop worker.
                        let _ = replies.send(Reply::Error(format!("{error:#}")));
                    }
                }
                let mut connection = ConnectionState::default();
                let mut last_restart = std::time::Instant::now() - Duration::from_secs(10);
                let mut last_state = Value::Null;
                let mut takeover: Option<(String, String)> = None;
                let mut lease_checked = std::time::Instant::now();
                loop {
                    match requests.recv_timeout(Duration::from_millis(1200)) {
                        Ok(CommandRequest::Update(action)) => {
                            let retry = async {
                                let executable = std::env::current_exe()?.with_file_name("hfb");
                                if authorize_bootstrap_retry(&data, &executable)? {
                                    ensure_service(&data, &cfg, &probe, false).await?;
                                    update_ready.store(true, Ordering::Release);
                                }
                                Ok::<(), anyhow::Error>(())
                            }.await;
                            match retry {
                                Ok(()) => { let _ = updater.send(action); }
                                Err(error) => { let _ = replies.send(Reply::Update(json!({"phase":"error","message":format!("{error:#}")}))); }
                            }
                        }
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
                            if service_healthy(&state) { update_ready.store(true, Ordering::Release); }
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
                            if !pause.load(Ordering::Acquire) && connection_refused(&e)
                                && last_restart.elapsed() >= Duration::from_secs(10)
                            {
                                last_restart = std::time::Instant::now();
                                // A service restart may have changed its endpoint or token.
                                if let Ok(current) = Config::load(&data) {
                                    cfg = current;
                                }
                                if request(&probe, &cfg, "GET", "/state", None).await.is_err()
                                    && let Err(error) = ensure_service(&data, &cfg, &probe, false).await
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
            preferences,
            tx,
            rx,
            dir,
            activation_pause,
            frontend_ready,
            candidate_ready,
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
// Source builds only attach. Trusted installed applications activate a prepared
// successor automatically; published services use the explicit bootstrap boundary.
async fn ensure_service(
    data: &std::path::Path,
    cfg: &Config,
    probe: &reqwest::Client,
    restart: bool,
) -> Result<()> {
    match request(probe, cfg, "GET", "/state", None).await {
        Ok(state) => {
            let signed = herdr_feishu_bridge::update::bundle().is_some_and(|bundle| {
                herdr_feishu_bridge::update::verify_bundle(&bundle, None).is_ok()
            });
            let protocol_matches =
                state["api_protocol"].as_u64() == Some(herdr_feishu_bridge::api::PROTOCOL);
            if !restart && protocol_matches && !signed {
                return Ok(());
            }
            let current = herdr_feishu_bridge::update::CURRENT;
            if let Some(version) = state["version"].as_str() {
                anyhow::ensure!(
                    !herdr_feishu_bridge::update::is_newer(version, current)?,
                    "The running background service is newer than this app. Use the newer app instead of downgrading its service."
                );
                if version == current
                    && protocol_matches
                    && state["service_activation"]["capability"] == 1
                {
                    return wait_for_service(data, cfg, probe, None).await;
                }
            }
            anyhow::ensure!(
                signed,
                "Updating the background service requires a trusted installed Dauerbot app. Use --data-dir for an independent development instance."
            );
            herdr_feishu_bridge::update::record_service(data, &state)?;
            if state["service_activation"]["capability"] == 1 {
                let id = uuid::Uuid::new_v4().to_string();
                let executable = std::env::current_exe()?.with_file_name("hfb");
                let control = reqwest::Client::builder()
                    .no_proxy()
                    .connect_timeout(Duration::from_secs(2))
                    .timeout(Duration::from_secs(110))
                    .build()?;
                request(
                    &control,
                    cfg,
                    "POST",
                    "/service/activation/prepare",
                    Some(json!({"activation_id":id,"candidate_executable":executable})),
                )
                .await?;
                let receipt = request(
                    &control,
                    cfg,
                    "POST",
                    "/service/activation/commit",
                    Some(json!({"activation_id":id})),
                )
                .await;
                let receipt = match receipt {
                    Ok(value) => value,
                    Err(_) => {
                        request(
                            probe,
                            cfg,
                            "GET",
                            &format!("/service/activation/status?activation_id={id}"),
                            None,
                        )
                        .await?
                    }
                };
                anyhow::ensure!(
                    receipt["owner_committed"] == true && receipt["phase"] == "serving",
                    "The update is still finishing. Your current projects are preserved."
                );
                return Ok(());
            }
            return bootstrap_service(data, cfg, probe, &state).await;
        }
        Err(error) if connection_refused(&error) => {}
        Err(error) => return Err(error),
    }
    let binary = std::env::current_exe()?.with_file_name("hfb");
    if let Some(receipt) = herdr_feishu_bridge::update::activation::Bootstrap::load(data)?
        && receipt.owner_committed
        && receipt.target_digest
            == herdr_feishu_bridge::update::activation::executable_digest(&binary)?
    {
        let mut child = spawn_service(data)?;
        return wait_for_service(data, cfg, probe, Some(&mut child)).await;
    }
    if let Some(journal) = herdr_feishu_bridge::update::activation::load(data)?
        && journal.version == herdr_feishu_bridge::update::CURRENT
        && !journal.owner_committed
        && !matches!(journal.phase.as_str(), "failed" | "complete")
    {
        // The old owner may have exited during disk preparation. Explicitly
        // preflight and commit the candidate; generic startup cannot assume ownership.
        herdr_feishu_bridge::update::verify_bundle(&journal.backup, None)?;
        let identity = Command::new(journal.backup.join("Contents/MacOS/hfb"))
            .arg("companion-identity")
            .output()?;
        anyhow::ensure!(
            identity.status.success(),
            "The prior update owner could not be verified"
        );
        let prior: Value = serde_json::from_slice(&identity.stdout)?;
        return bootstrap_service(data, cfg, probe, &prior).await;
    }
    let mut child = spawn_service(data)?;
    wait_for_service(data, cfg, probe, Some(&mut child)).await
}
pub(super) fn service_healthy(state: &Value) -> bool {
    state["api_protocol"].as_u64() == Some(herdr_feishu_bridge::api::PROTOCOL)
        && state["version"] == herdr_feishu_bridge::update::CURRENT
        && state["service_activation"]["capability"] == 1
        && state["mcp_contracts"]
            .as_array()
            .is_some_and(|contracts| contracts.iter().any(|contract| contract == 1))
        && state["projects"].as_array().is_some_and(|projects| {
            projects.iter().all(|p| {
                p["bot"].is_null()
                    || p["bot"]["app_id"].as_str().is_none_or(str::is_empty)
                    || matches!(
                        state["runtime"]["bots"][p["id"].as_str().unwrap_or("")]["status"].as_str(),
                        Some("awaiting_events" | "receiving")
                    )
            })
        })
}
async fn wait_for_service(
    data: &std::path::Path,
    cfg: &Config,
    probe: &reqwest::Client,
    mut child: Option<&mut std::process::Child>,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        if let Ok(state) = request(probe, cfg, "GET", "/state", None).await
            && service_healthy(&state)
        {
            if let Some(mut receipt) =
                herdr_feishu_bridge::update::activation::Bootstrap::load(data)?
                && receipt.owner_committed
            {
                receipt.phase = "complete".into();
                receipt.save(data)?;
            }
            return Ok(());
        }
        if let Some(child) = child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            bail!("The updated app could not start ({status}). Your saved work is preserved.");
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "The update is still connecting. Your projects and conversations are preserved."
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
// Only an explicit update intent grants a single retry of a failed bootstrap.
pub(super) fn authorize_bootstrap_retry(
    data: &std::path::Path,
    executable: &std::path::Path,
) -> Result<bool> {
    use herdr_feishu_bridge::update::activation;
    let _lock = activation::lock(data)?;
    let Some(mut receipt) = activation::Bootstrap::load(data)? else {
        return Ok(false);
    };
    if receipt.owner_committed || receipt.phase != "failed" {
        return Ok(false);
    }
    anyhow::ensure!(
        receipt.target_digest == activation::executable_digest(executable)?,
        "The prepared update changed; check again from the current app"
    );
    receipt.phase = "retry_authorized".into();
    receipt.save(data)?;
    Ok(true)
}
async fn bootstrap_service(
    data: &std::path::Path,
    cfg: &Config,
    probe: &reqwest::Client,
    state: &Value,
) -> Result<()> {
    let executable = std::env::current_exe()?.with_file_name("hfb");
    bootstrap_service_configured(data, cfg, probe, state, &executable, |_| {}).await
}
pub(super) async fn bootstrap_service_configured(
    data: &std::path::Path,
    cfg: &Config,
    probe: &reqwest::Client,
    state: &Value,
    executable: &std::path::Path,
    configure: impl Fn(&mut Command),
) -> Result<()> {
    use herdr_feishu_bridge::update::{self, activation::Bootstrap};
    let digest = update::activation::executable_digest(executable)?;
    if let Some(previous) = Bootstrap::load(data)? {
        anyhow::ensure!(
            previous.target_digest != digest
                || matches!(previous.phase.as_str(), "complete" | "retry_authorized"),
            "The update is waiting to finish safely. Your existing projects keep their saved state."
        );
    }
    let preflight = Command::new(executable)
        .arg("--data-dir")
        .arg(data)
        .arg("service-preflight")
        .stdin(Stdio::null())
        .output()
        .context("The update could not be prepared")?;
    anyhow::ensure!(
        preflight.status.success(),
        "The update could not be prepared; the current app keeps running"
    );
    let ready: Value = serde_json::from_slice(&preflight.stdout)?;
    anyhow::ensure!(
        ready["schema_compatible"] == true
            && ready["store_readable"] == true
            && ready["mcp_compatible"] == true
            && ready["version"] == update::CURRENT
            && ready["api_protocol"].as_u64() == Some(herdr_feishu_bridge::api::PROTOCOL),
        "The update cannot safely take over the current work yet"
    );
    let old_version = state["version"]
        .as_str()
        .context("The current app version is unknown")?;
    let old_protocol = state["api_protocol"]
        .as_u64()
        .context("The current app contract is unknown")?;
    let previous = update::historical_companion(data, old_version, old_protocol)?;
    let _update_lock = update::activation::lock(data)?;
    anyhow::ensure!(
        update::activation::executable_digest(executable)? == digest,
        "The prepared app changed; keeping the current app running"
    );
    let retry_id = Bootstrap::load(data)?
        .filter(|receipt| receipt.phase == "retry_authorized" && receipt.target_digest == digest)
        .map(|receipt| receipt.id);
    let mut receipt = Bootstrap {
        id: retry_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        target_version: update::CURRENT.into(),
        target_digest: digest,
        previous_executable: previous.path().to_owned(),
        previous_version: old_version.into(),
        owner_committed: false,
        phase: "prepared".into(),
    };
    // No model/pane restart: API7 and the three-tool contract stay unchanged.
    receipt.save(data)?;
    let mut child = None;
    let mut shutdown_attempted = false;
    let mut old_released = false;
    let result = async {
        match request(probe, cfg, "GET", "/state", None).await {
            Ok(_) => {
                shutdown_attempted = true;
                update::stop_service(data, cfg).await?;
                #[cfg(test)]
                if LOSE_BOOTSTRAP_SHUTDOWN_ACK.swap(false, Ordering::SeqCst) {
                    bail!("Injected lost shutdown ACK");
                }
            }
            Err(error) if connection_refused(&error) => update::wait_service_exit(data).await?,
            Err(error) => return Err(error),
        }
        old_released = true;
        receipt.phase = "starting".into();
        receipt.save(data)?;
        #[cfg(test)]
        if FAIL_BOOTSTRAP_SPAWN.swap(false, Ordering::SeqCst) {
            bail!("Injected successor spawn failure");
        }
        child = Some(spawn_service_configured(data, executable, &configure)?);
        let mut result = wait_for_service(data, cfg, probe, child.as_mut()).await;
        if result.is_err()
            && Bootstrap::load(data)?.is_some_and(|r| r.owner_committed)
            && child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            child = Some(spawn_service_configured(data, executable, &configure)?);
            result = wait_for_service(data, cfg, probe, child.as_mut()).await;
        }
        result
    }
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let committed = Bootstrap::load(data)?.is_some_and(|r| r.owner_committed);
            if !committed {
                if let Some(child) = &mut child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                if shutdown_attempted || old_released {
                    // Even a service that answers once may already be draining after
                    // a lost shutdown ACK. Its released lock is the recovery fence.
                    update::wait_service_exit(data).await?;
                    let mut rescue = spawn_service_configured(data, previous.path(), &configure)?;
                    let deadline = std::time::Instant::now() + Duration::from_secs(15);
                    loop {
                        if request(probe, cfg, "GET", "/state", None)
                            .await
                            .is_ok_and(|s| {
                                s["version"] == old_version
                                    && s["api_protocol"].as_u64() == Some(old_protocol)
                            })
                        {
                            break;
                        }
                        anyhow::ensure!(
                            std::time::Instant::now() < deadline && rescue.try_wait()?.is_none(),
                            "The previous app is still reconnecting. Saved work is preserved."
                        );
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                }
                receipt.phase = "failed".into();
                receipt.save(data)?;
            }
            // After the startup receipt, only the successor may reopen the latest
            // journal. Never restore an old database or incompatible Agent contract.
            Err(error)
        }
    }
}

fn spawn_service(data: &std::path::Path) -> Result<std::process::Child> {
    spawn_service_at(data, &std::env::current_exe()?.with_file_name("hfb"))
}
fn spawn_service_at(
    data: &std::path::Path,
    binary: &std::path::Path,
) -> Result<std::process::Child> {
    spawn_service_configured(data, binary, |_| {})
}
fn spawn_service_configured(
    data: &std::path::Path,
    binary: &std::path::Path,
    configure: impl Fn(&mut Command),
) -> Result<std::process::Child> {
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
    let mut command = Command::new(binary);
    command
        .arg("--data-dir")
        .arg(data)
        .arg("serve")
        .env("PATH", std::env::join_paths(paths)?)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    configure(&mut command);
    command.spawn().context("Cannot start the local daemon")
}

pub(super) async fn request(
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
        ("HERDR", "herdr"),
        ("Codex", "codex"),
        ("Claude Code", "claude"),
        ("Pi", "pi"),
        ("Antigravity", "agy"),
    ] {
        let mut candidates = if name == "HERDR" {
            vec![PathBuf::from(&cfg.herdr)]
        } else {
            vec![PathBuf::from(executable)]
        };
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
        let resolved = if name == "HERDR" {
            PathBuf::from(&cfg.herdr)
                .is_file()
                .then(|| PathBuf::from(&cfg.herdr))
        } else {
            candidates.into_iter().find(|p| p.is_file())
        };
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
pub(super) static FAIL_BOOTSTRAP_SPAWN: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
pub(super) static LOSE_BOOTSTRAP_SHUTDOWN_ACK: AtomicBool = AtomicBool::new(false);

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
    async fn older_services_cannot_satisfy_workspace_and_native_child_contract() {
        let data = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (version, protocol) in [("0.4.5", 5), ("0.4.6", 6)] {
            let (config, worker) = service_fixture(
                "200 OK",
                json!({"version":version, "api_protocol":protocol}),
            );
            let error = ensure_service(data.path(), &config, &client, false)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("trusted installed Dauerbot app"));
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

#[cfg(test)]
mod desktop_preference_tests {
    use super::{DesktopPreferences, start_desktop_preferences};
    use std::{os::unix::fs::PermissionsExt, sync::mpsc, time::Duration};

    #[test]
    fn preference_write_failure_is_reported_without_blocking_the_flush() {
        let data = tempfile::tempdir().unwrap();
        let blocked = data.path().join("not-a-directory");
        std::fs::write(&blocked, b"fixture").unwrap();
        let (replies, errors) = mpsc::channel();
        let (tx, worker) = start_desktop_preferences(blocked, replies);
        tx.send(DesktopPreferences::Save(b"{}".to_vec())).unwrap();
        let (done, acknowledged) = mpsc::channel();
        tx.send(DesktopPreferences::Flush(done)).unwrap();
        acknowledged.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            matches!(errors.recv_timeout(Duration::from_secs(3)).unwrap(), super::Reply::Error(message) if message.starts_with("Could not save desktop preferences:"))
        );
        drop(tx);
        worker.join().unwrap();
    }

    #[test]
    fn queued_preferences_are_ordered_private_and_flushed_before_exit() {
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().to_owned();
        let (replies, errors) = mpsc::channel();
        let (tx, worker) = start_desktop_preferences(dir, replies);
        tx.send(DesktopPreferences::Save(
            br#"{"sidebar_width":224}"#.to_vec(),
        ))
        .unwrap();
        tx.send(DesktopPreferences::Save(
            br#"{"sidebar_width":280}"#.to_vec(),
        ))
        .unwrap();
        let (done, acknowledged) = mpsc::channel();
        tx.send(DesktopPreferences::Flush(done)).unwrap();
        acknowledged.recv_timeout(Duration::from_secs(3)).unwrap();
        let path = data.path().join("desktop.json");
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"sidebar_width":280}"#);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(errors.try_recv().is_err());
        drop(tx);
        worker.join().unwrap();
    }
}
