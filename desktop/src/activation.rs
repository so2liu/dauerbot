//! Private GUI readiness channel. Drafts cross a local socket, never the durable receipt.
use crate::backend::Reply;
use anyhow::{Context, Result, bail, ensure};
use herdr_feishu_bridge::{
    config::Config,
    update::{self, activation::Transaction},
};
use serde_json::{Value, json};
use std::os::unix::net::{UnixListener, UnixStream};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub struct Candidate {
    pub snapshot: Value,
    pub serving: Arc<AtomicBool>,
    pub updates: Arc<AtomicBool>,
    pub menu_ready: Arc<AtomicBool>,
    pub backend_ready: Arc<AtomicBool>,
    pub visible: Arc<AtomicBool>,
    pub ready: mpsc::Sender<()>,
    pub refresh: mpsc::Receiver<(Value, mpsc::Sender<bool>)>,
}
fn socket(data: &Path, id: &str) -> PathBuf {
    let _ = data;
    std::env::temp_dir().join(format!("dauerbot-ui-{id}.sock"))
}
fn send(stream: &mut UnixStream, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}
fn read(stream: &UnixStream) -> Result<Value> {
    let mut bytes = String::new();
    BufReader::new(stream)
        .take(1024 * 1024)
        .read_line(&mut bytes)?;
    ensure!(!bytes.is_empty(), "The update readiness channel closed");
    Ok(serde_json::from_str(&bytes)?)
}
impl Candidate {
    pub fn connect(data: &Path, id: &str) -> Result<Self> {
        let journal = update::activation::load(data)?.context("Missing activation receipt")?;
        ensure!(
            journal.id == id
                && journal.version == update::CURRENT
                && matches!(
                    journal.phase.as_str(),
                    "prepared" | "committed" | "publication_pending"
                ),
            "The candidate does not match its update receipt"
        );
        let mut stream = UnixStream::connect(socket(data, id))?;
        stream.set_read_timeout(Some(Duration::from_secs(150)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        send(&mut stream, &json!({"id":id,"pid":std::process::id()}))?;
        let snapshot = read(&stream)?;
        let serving = Arc::new(AtomicBool::new(false));
        let updates = Arc::new(AtomicBool::new(false));
        let menu_ready = Arc::new(AtomicBool::new(false));
        let backend_ready = Arc::new(AtomicBool::new(false));
        let (ready, ready_rx) = mpsc::channel();
        let (refresh_tx, refresh) = mpsc::channel();
        let result = Self {
            visible: Arc::new(AtomicBool::new(
                snapshot["visible"].as_bool().unwrap_or(false),
            )),
            snapshot,
            serving: serving.clone(),
            updates: updates.clone(),
            menu_ready: menu_ready.clone(),
            backend_ready: backend_ready.clone(),
            ready,
            refresh,
        };
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                ready_rx.recv_timeout(Duration::from_secs(20))?;
                send(&mut stream, &json!({"phase":"ready"}))?;
                let mut command = read(&stream)?;
                if command["phase"] == "refresh" {
                    let (ack, done) = mpsc::channel();
                    refresh_tx.send((command["snapshot"].take(), ack))?;
                    ensure!(
                        done.recv_timeout(Duration::from_secs(5))?,
                        "The updated window did not preserve its latest draft state"
                    );
                    send(&mut stream, &json!({"phase":"ready"}))?;
                    command = read(&stream)?;
                }
                ensure!(
                    command["phase"] == "publish",
                    "Candidate publication was cancelled"
                );
                serving.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(30);
                while !menu_ready.load(Ordering::Acquire) || !backend_ready.load(Ordering::Acquire)
                {
                    ensure!(
                        Instant::now() < deadline,
                        "The new app did not finish starting"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
                send(&mut stream, &json!({"phase":"published"}))?;
                ensure!(
                    read(&stream)?["phase"] == "complete",
                    "Activation confirmation was lost"
                );
                updates.store(true, Ordering::Release);
                Ok(())
            })();
            if result.is_err() && !serving.load(Ordering::Acquire) {
                std::process::exit(1);
            }
            // After ownership changes, stay alive even if the old GUI's final ACK is lost.
            if serving.load(Ordering::Acquire) {
                updates.store(true, Ordering::Release);
            }
        });
        Ok(result)
    }
}

pub fn snapshot_restored(expected: &Value, actual: &Value) -> bool {
    fn includes(expected: &Value, actual: &Value) -> bool {
        match expected {
            Value::Object(fields) => fields
                .iter()
                .all(|(key, value)| includes(value, &actual[key])),
            _ => expected == actual,
        }
    }
    [
        "setup",
        "selected",
        "picked_path",
        "name",
        "path",
        "app_id",
        "secret",
        "agent",
        "credentials_project",
        "credentials_app_id",
        "pending_secret",
        "credentials_checked",
    ]
    .iter()
    .all(|key| {
        expected
            .get(*key)
            .is_none_or(|value| includes(value, &actual[*key]))
    })
}

fn snapshot(replies: &mpsc::Sender<Reply>, freeze: bool) -> Result<Value> {
    let (reply, receiver) = mpsc::channel();
    replies.send(Reply::ActivationFreeze { reply, freeze })?;
    receiver.recv_timeout(Duration::from_secs(5))?
}

pub async fn activate(
    transaction: Transaction,
    data: &Path,
    replies: &mpsc::Sender<Reply>,
) -> Result<()> {
    activate_with_launcher(transaction, data, replies, |_| {}).await
}
async fn activate_with_launcher(
    mut transaction: Transaction,
    data: &Path,
    replies: &mpsc::Sender<Reply>,
    configure: impl Fn(&mut Command),
) -> Result<()> {
    let cfg: Config = serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(100))
        .build()?;
    let id = transaction.journal.id.clone();
    let state = match api(&http, &cfg, "GET", "/state", None).await {
        Ok(state) => state,
        Err(error) => {
            if !transaction.journal.owner_committed && transaction.journal.gui_attempts == 0 {
                transaction.rollback(&format!("{error:#}"))?;
            }
            return Err(error);
        }
    };
    if state["service_activation"]["capability"] != 1 {
        if !transaction.journal.owner_committed && transaction.journal.gui_attempts == 0 {
            transaction.rollback("The current app cannot prepare automatic handover")?;
        }
        bail!("The update is waiting for a compatible app. Your projects keep running.");
    }
    let status_path = format!("/service/activation/status?activation_id={id}");
    match api(&http, &cfg, "GET", &status_path, None).await {
        Ok(receipt) if receipt["owner_committed"] == true => transaction.committed()?,
        Err(error)
            if transaction.journal.gui_attempts > 0 && !transaction.journal.owner_committed =>
        {
            return Err(error).context("Waiting to confirm the previous activation attempt");
        }
        _ => {}
    }
    if !transaction.journal.owner_committed {
        let prepared = api(
            &http,
            &cfg,
            "POST",
            "/service/activation/prepare",
            Some(json!({"activation_id":id,
            "candidate_executable":transaction.journal.app.join("Contents/MacOS/hfb")})),
        )
        .await;
        let prepared = prepared.and_then(|receipt| {
            ensure!(
                receipt["phase"] == "prepared"
                    && receipt["transport_ready"] == true
                    && receipt["owner_committed"] == false,
                "The updated app did not finish its readiness checks"
            );
            Ok(receipt)
        });
        if let Err(error) = prepared {
            let _ = api(
                &http,
                &cfg,
                "POST",
                "/service/activation/abort",
                Some(json!({"activation_id":id})),
            )
            .await;
            transaction.rollback(&format!("{error:#}"))?;
            return Err(error);
        }
    }

    loop {
        let path = socket(data, &id);
        let mut child: Option<Child> = None;
        let result = async {
            transaction.begin_gui()?;
            let _ = std::fs::remove_file(&path);
            let listener=UnixListener::bind(&path)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let initial = snapshot(replies, false)?;
            let mut launch = Command::new(transaction.journal.app.join("Contents/MacOS/HerdrBridge"));
            launch.arg("--data-dir").arg(data).arg("--activation").arg(&id)
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            configure(&mut launch);
            child = Some(launch.spawn()?);
            transaction.gui_started(child.as_ref().context("Missing candidate process")?.id())?;
            let deadline = Instant::now() + Duration::from_secs(25);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        ensure!(Instant::now() < deadline, "The new app did not become ready");
                        if let Some(child) = &mut child { ensure!(child.try_wait()?.is_none(), "The new app closed before activation"); }
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            // Darwin inherits the listener's nonblocking flag on accept.
            // Readiness frames use bounded blocking reads on this worker.
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(35)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let hello = read(&stream)?;
            ensure!(hello["id"] == id && hello["pid"].as_u64() == child.as_ref().map(|c|u64::from(c.id())), "Unexpected candidate readiness identity");
            send(&mut stream, &initial)?;
            ensure!(read(&stream)?["phase"] == "ready", "The new app failed its window readiness check");
            let final_snapshot = snapshot(replies, true)?;
            send(&mut stream, &json!({"phase":"refresh","snapshot":final_snapshot}))?;
            ensure!(read(&stream)?["phase"] == "ready", "The new app did not accept the latest window state");
            if !transaction.journal.owner_committed {
                let commit = api(&http, &cfg, "POST", "/service/activation/commit", Some(json!({"activation_id":id}))).await;
                let receipt = match commit {
                    Ok(receipt) => receipt,
                    Err(_) => api(&http, &cfg, "GET", &status_path, None).await
                        .context("Ownership confirmation is unavailable; keeping the prepared app for recovery")?,
                };
                if receipt["owner_committed"] == true { transaction.committed()?; }
                ensure!(transaction.journal.owner_committed && receipt["phase"] == "serving", "The new service did not confirm healthy ownership");
            } else {
                let receipt = api(&http,&cfg,"GET",&status_path,None).await?;
                ensure!(receipt["owner_committed"] == true && receipt["phase"] == "serving", "Waiting for the updated service to finish starting");
            }
            let deadline = Instant::now()+Duration::from_secs(15);
            loop {
                let owner = api(&http,&cfg,"GET",&status_path,None).await?;
                if owner["owner_committed"]==true && owner["phase"]=="serving"
                    && owner["owner_pid"]==owner["candidate_pid"] && owner["owner_pid"].is_u64()
                    && owner["candidate_version"]==transaction.journal.version { break; }
                ensure!(Instant::now()<deadline,"Waiting for the updated app to take over its connection");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            send(&mut stream, &json!({"phase":"publish"}))?;
            ensure!(read(&stream)?["phase"] == "published", "The new app did not confirm publication");
            #[cfg(test)]
            if LOSE_PUBLICATION_ACK.swap(false,Ordering::SeqCst) { bail!("Injected lost publication acknowledgement"); }
            Ok::<_, anyhow::Error>(stream)
        }.await;
        let _ = std::fs::remove_file(path);
        match result {
            Ok(mut stream) => {
                if let Err(error) = transaction.finish() {
                    let _ = replies.send(Reply::ActivationResume);
                    return Err(error);
                }
                let _ = send(&mut stream, &json!({"phase":"complete"}));
                let _ = replies.send(Reply::Restart);
                return Ok(());
            }
            Err(error) => {
                let receipt = api(
                    &http,
                    &cfg,
                    "POST",
                    "/service/activation/abort",
                    Some(json!({"activation_id":id})),
                )
                .await;
                if receipt.as_ref().is_ok_and(|r| r["owner_committed"] == true) {
                    transaction.committed()?;
                }
                let _ = replies.send(Reply::ActivationResume);
                if transaction.journal.owner_committed {
                    if let Some(child) = &mut child {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    transaction.publication_pending(&format!("{error:#}"))?;
                    if transaction.journal.gui_attempts < 3 {
                        continue;
                    }
                } else if receipt.as_ref().is_ok_and(|r| {
                    r["owner_committed"] == false
                        && matches!(r["phase"].as_str(), Some("aborted" | "failed"))
                }) {
                    if let Some(child) = &mut child {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    transaction.rollback(&format!("{error:#}"))?;
                }
                return Err(error);
            }
        }
    }
}

async fn api(
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
    let response = req.send().await?;
    let status = response.status();
    let value: Value = response.json().await?;
    if !status.is_success() {
        bail!(
            "{}",
            value["error"]
                .as_str()
                .unwrap_or("Update activation failed")
        );
    }
    Ok(value)
}

#[cfg(test)]
static LOSE_PUBLICATION_ACK: AtomicBool = AtomicBool::new(false);

#[cfg(all(test, target_os = "macos"))]
mod native_tests {
    use super::*;
    use std::{ffi::c_void, fs, os::unix::fs::MetadataExt};

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(options: u32, relative: u32) -> *const c_void;
        static kCGWindowOwnerPID: *const c_void;
        static kCGWindowLayer: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut i32) -> bool;
        fn CFRelease(value: *const c_void);
    }
    fn visible_windows(pid: u32) -> usize {
        // Metadata only, filtered to our owned PID. No screenshots or accessibility access.
        unsafe {
            let array = CGWindowListCopyWindowInfo(1, 0);
            assert!(!array.is_null());
            let count = (0..CFArrayGetCount(array))
                .filter(|index| {
                    let row = CFArrayGetValueAtIndex(array, *index);
                    let mut owner = 0;
                    let mut layer = -1;
                    CFNumberGetValue(CFDictionaryGetValue(row, kCGWindowOwnerPID), 9, &mut owner)
                        && CFNumberGetValue(
                            CFDictionaryGetValue(row, kCGWindowLayer),
                            9,
                            &mut layer,
                        )
                        && owner == pid as i32
                        && layer == 0
                })
                .count();
            CFRelease(array);
            count
        }
    }
    fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "native activation fixture timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn owned_kill(pid: u32, app: &Path) {
        let output = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&output.stdout).contains(app.to_str().unwrap()) {
            let _ = Command::new("/bin/kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
    fn isolate(command: &mut Command, root: &Path) {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_")
                || key.to_string_lossy().starts_with("HC_")
            {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("xdg"));
    }
    struct Cleanup {
        app: PathBuf,
        data: PathBuf,
        old: Child,
        root: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            // Retained historical binaries live outside the canonical app. Match
            // our private data path as well, so panic cleanup includes rescues.
            if let Ok(output) = Command::new("/bin/ps")
                .args(["-axo", "pid=,command="])
                .output()
            {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if line.contains(self.data.to_str().unwrap())
                        && line.contains("hfb")
                        && let Some(pid) = line
                            .split_whitespace()
                            .next()
                            .and_then(|v| v.parse::<u32>().ok())
                    {
                        owned_kill(pid, &self.data);
                    }
                }
            }
            if let Ok(Some(journal)) = update::activation::load(&self.data)
                && let Some(pid) = journal.gui_pid
            {
                owned_kill(pid, &self.app);
            }
            if let Ok(bytes) = fs::read(self.data.join("service-activation.json"))
                && let Ok(receipt) = serde_json::from_slice::<Value>(&bytes)
                && let Some(pid) = receipt["candidate_pid"].as_u64()
            {
                owned_kill(pid as u32, &self.app);
            }
            let _ = self.old.kill();
            let _ = self.old.wait();
            if let Ok(bytes) = fs::read(self.data.join("config.json"))
                && let Ok(cfg) = serde_json::from_slice::<Config>(&bytes)
            {
                let mut command = Command::new(&cfg.herdr);
                isolate(&mut command, &self.root);
                let _ = command
                    .args(["--session", &cfg.herdr_session, "server", "stop"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
    /// Run explicitly after building dist/Dauerbot.app. Uses actual native GPUI
    /// candidates and real capable daemon processes; the old GUI side is a harness.
    #[test]
    #[ignore = "requires the packaged native app and a logged-in macOS GUI session"]
    fn packaged_candidate_and_real_service_recover_publication_without_recommitting() {
        let package = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("dist/Dauerbot.app");
        assert!(package.join("Contents/MacOS/HerdrBridge").is_file());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut evidence = Vec::new();
        for fault in ["none", "candidate_death", "lost_ack"] {
            let run = tempfile::Builder::new()
                .prefix("db-act-")
                .tempdir_in("/private/tmp")
                .unwrap();
            let app = run.path().join("Dauerbot.app");
            assert!(
                Command::new("/usr/bin/ditto")
                    .arg(&package)
                    .arg(&app)
                    .status()
                    .unwrap()
                    .success()
            );
            let data = run.path().join("data");
            fs::create_dir_all(data.join("updates")).unwrap();
            fs::write(
                data.join("updates/preferences.json"),
                r#"{"automatic":false,"channel":"stable"}"#,
            )
            .unwrap();
            let port = std::net::TcpListener::bind(("127.0.0.1", 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let log = fs::File::create(run.path().join("service.log")).unwrap();
            let mut launch = Command::new(app.join("Contents/MacOS/hfb"));
            launch
                .arg("--data-dir")
                .arg(&data)
                .arg("serve")
                .arg("--port")
                .arg(port.to_string())
                .arg("--herdr-session")
                .arg(format!("activation-{}", std::process::id()))
                .env_remove("HERDR_PANE_ID")
                .env_remove("HERDR_SESSION")
                .env("HOME", run.path().join("home"))
                .env("XDG_CONFIG_HOME", run.path().join("xdg"))
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log);
            isolate(&mut launch, run.path());
            let old = launch.spawn().unwrap();
            let mut cleanup = Cleanup {
                app: app.clone(),
                data: data.clone(),
                old,
                root: run.path().to_owned(),
            };
            let cfg: Config = eventually(|| {
                fs::read(data.join("config.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Config>(&bytes).ok())
                    .filter(|cfg| cfg.port == port)
            });
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            eventually(|| {
                runtime
                    .block_on(api(&http, &cfg, "GET", "/state", None))
                    .ok()
                    .filter(|state| state["service_activation"]["capability"] == 1)
            });
            let backup = run.path().join(".dauerbot-update-fixture/old");
            fs::create_dir_all(&backup).unwrap();
            let metadata = fs::metadata(&backup).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let journal = json!({"id":id,"version":update::CURRENT,"app":app,"backup":backup,
                "phase":"prepared","owner_committed":false,"previous_record":null,"error":null,
                "original_identity":[metadata.dev(),metadata.ino()],"gui_attempts":0,"gui_pid":null});
            fs::write(data.join("updates/activation.json"), journal.to_string()).unwrap();
            let transaction = Transaction::recover(&data).unwrap().unwrap();
            let (replies, receiver) = mpsc::channel();
            let driver_data = data.clone();
            let driver_app = app.clone();
            let driver = std::thread::spawn(move || {
                let mut snapshots = Vec::new();
                while let Ok(reply) = receiver.recv_timeout(Duration::from_secs(150)) {
                    match reply {
                        Reply::ActivationFreeze { reply, freeze } => {
                            if freeze {
                                let journal =
                                    update::activation::load(&driver_data).unwrap().unwrap();
                                assert_eq!(
                                    visible_windows(journal.gui_pid.unwrap()),
                                    0,
                                    "candidate window visible before publication"
                                );
                            }
                            snapshots.push(freeze);
                            // Initial hidden preference deliberately changes during preparation.
                            let _=reply.send(Ok(json!({"setup":{"page":6,"language":"en","terminal_hidden":true},
                                "selected":"","name":if freeze{"latest draft"}else{"initial draft"},"path":"",
                                "app_id":"","secret":"private ephemeral draft","visible":freeze})));
                        }
                        Reply::ActivationResume => {}
                        Reply::Restart => {
                            let journal = update::activation::load(&driver_data).unwrap().unwrap();
                            assert_eq!(
                                journal.phase, "complete",
                                "old GUI told to exit before durable publication"
                            );
                            eventually(|| {
                                (visible_windows(journal.gui_pid.unwrap()) > 0).then_some(())
                            });
                            assert!(
                                Command::new("/bin/ps")
                                    .args([
                                        "-p",
                                        &journal.gui_pid.unwrap().to_string(),
                                        "-o",
                                        "command="
                                    ])
                                    .output()
                                    .unwrap()
                                    .stdout
                                    .windows(driver_app.to_str().unwrap().len())
                                    .any(|w| w == driver_app.to_str().unwrap().as_bytes())
                            );
                            return snapshots;
                        }
                        _ => {}
                    }
                }
                panic!("old GUI harness did not receive publication completion");
            });
            let monitor = if fault == "candidate_death" {
                let data = data.clone();
                let app = app.clone();
                Some(std::thread::spawn(move || {
                    let pid = eventually(|| {
                        update::activation::load(&data)
                            .ok()
                            .flatten()
                            .filter(|receipt| receipt.owner_committed)
                            .and_then(|receipt| receipt.gui_pid)
                    });
                    owned_kill(pid, &app);
                }))
            } else {
                None
            };
            LOSE_PUBLICATION_ACK.store(fault == "lost_ack", Ordering::SeqCst);
            let result = runtime.block_on(activate_with_launcher(
                transaction,
                &data,
                &replies,
                |command| isolate(command, run.path()),
            ));
            eprintln!("activation fixture {fault}: {result:?}");
            drop(replies);
            if let Some(monitor) = monitor {
                monitor.join().unwrap();
            }
            let snapshot_events = driver.join().unwrap();
            let journal = update::activation::load(&data).unwrap().unwrap();
            let receipt = runtime
                .block_on(api(
                    &http,
                    &cfg,
                    "GET",
                    &format!("/service/activation/status?activation_id={id}"),
                    None,
                ))
                .unwrap();
            let state = runtime
                .block_on(api(&http, &cfg, "GET", "/state", None))
                .unwrap();
            evidence.push(json!({"fault":fault,"result":result.as_ref().err().map(|e|format!("{e:#}")),
                "phase":journal.phase,"gui_attempts":journal.gui_attempts,"owner_committed":journal.owner_committed,
                "service_receipt":receipt,"snapshots":snapshot_events,"version":state["version"]}));
            owned_kill(journal.gui_pid.unwrap(), &app);
            let _ = runtime.block_on(crate::backend::request(
                &http,
                &cfg,
                "POST",
                "/service/shutdown",
                Some(json!({})),
            ));
            eventually(|| cleanup.old.try_wait().ok().flatten());
            assert!(result.is_ok(), "{fault}: {result:?}");
            assert_eq!(journal.gui_attempts, if fault == "none" { 1 } else { 2 });
            assert_eq!(receipt["owner_committed"], true);
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test-results/automatic-activation-native.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
    #[test]
    #[ignore = "requires packaged native app and the verified published 0.4.7 fixture"]
    fn signed_legacy_bootstrap_recovers_spawn_and_shutdown_ack_failures() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let package = workspace.join("dist/Dauerbot.app");
        let published = workspace
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".dauerbot/public-v0.4.7/Dauerbot.app");
        assert!(published.join("Contents/MacOS/hfb").is_file());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut evidence = Vec::new();
        for fault in ["spawn_failure", "lost_shutdown_ack", "none"] {
            let run = tempfile::Builder::new()
                .prefix("db-boot-")
                .tempdir()
                .unwrap();
            let app = run.path().join("Dauerbot.app");
            let backup = run.path().join(".dauerbot-update-fixture/old");
            for (source, destination) in [(&package, &app), (&published, &backup)] {
                assert!(
                    Command::new("/usr/bin/ditto")
                        .arg(source)
                        .arg(destination)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            let data = run.path().join("data");
            fs::create_dir_all(data.join("updates")).unwrap();
            fs::write(
                data.join("updates/installed.json"),
                json!({"app":app,"backup":backup,"version":update::CURRENT}).to_string(),
            )
            .unwrap();
            let port = std::net::TcpListener::bind(("127.0.0.1", 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let log = fs::File::create(run.path().join("old.log")).unwrap();
            let mut command = Command::new(backup.join("Contents/MacOS/hfb"));
            isolate(&mut command, run.path());
            let old = command
                .arg("--data-dir")
                .arg(&data)
                .arg("serve")
                .arg("--port")
                .arg(port.to_string())
                .arg("--herdr-session")
                .arg(format!("bootstrap-{}-{fault}", std::process::id()))
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap();
            let _cleanup = Cleanup {
                app: app.clone(),
                data: data.clone(),
                old,
                root: run.path().to_owned(),
            };
            let cfg: Config = eventually(|| {
                fs::read(data.join("config.json"))
                    .ok()
                    .and_then(|v| serde_json::from_slice::<Config>(&v).ok())
                    .filter(|cfg| cfg.port == port)
            });
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let old_state = eventually(|| {
                runtime
                    .block_on(api(&http, &cfg, "GET", "/state", None))
                    .ok()
            });
            assert_ne!(old_state["service_activation"]["capability"], 1);
            crate::backend::FAIL_BOOTSTRAP_SPAWN.store(fault == "spawn_failure", Ordering::SeqCst);
            crate::backend::LOSE_BOOTSTRAP_SHUTDOWN_ACK
                .store(fault == "lost_shutdown_ack", Ordering::SeqCst);
            let result = runtime.block_on(crate::backend::bootstrap_service_configured(
                &data,
                &cfg,
                &http,
                &old_state,
                &app.join("Contents/MacOS/hfb"),
                |command| isolate(command, run.path()),
            ));
            let state = eventually(|| {
                runtime
                    .block_on(api(&http, &cfg, "GET", "/state", None))
                    .ok()
            });
            let receipt = update::activation::Bootstrap::load(&data).unwrap().unwrap();
            evidence.push(json!({"fault":fault,"error":result.as_ref().err().map(|e|format!("{e:#}")),
                "owner_committed":receipt.owner_committed,"phase":receipt.phase,
                "service_capability":state["service_activation"]["capability"],"version":state["version"]}));
            if fault == "none" {
                assert!(result.is_ok(), "{result:?}");
                assert!(receipt.owner_committed);
                assert_eq!(receipt.phase, "complete");
                assert!(crate::backend::service_healthy(&state));
            } else {
                assert!(result.is_err());
                assert!(!receipt.owner_committed);
                assert_eq!(receipt.phase, "failed");
                assert_ne!(state["service_activation"]["capability"], 1);
                assert_eq!(state["version"], "0.4.7");
                // A normal automatic retry is suppressed. Existing explicit
                // update intent authorizes exactly this target/receipt once.
                let automatic = runtime.block_on(crate::backend::bootstrap_service_configured(
                    &data,
                    &cfg,
                    &http,
                    &state,
                    &app.join("Contents/MacOS/hfb"),
                    |command| isolate(command, run.path()),
                ));
                assert!(automatic.is_err());
                assert!(eventually(|| crate::backend::authorize_bootstrap_retry(
                    &data,
                    &app.join("Contents/MacOS/hfb")
                )
                .ok()));
                runtime
                    .block_on(crate::backend::bootstrap_service_configured(
                        &data,
                        &cfg,
                        &http,
                        &state,
                        &app.join("Contents/MacOS/hfb"),
                        |command| isolate(command, run.path()),
                    ))
                    .unwrap();
                let retried = update::activation::Bootstrap::load(&data).unwrap().unwrap();
                assert_eq!(retried.id, receipt.id);
                assert!(retried.owner_committed);
                assert_eq!(retried.phase, "complete");
                evidence
                    .push(json!({"fault":fault,"explicit_retry":"complete","same_receipt":true}));
            }
        }
        let path = workspace.join("test-results/automatic-bootstrap-native.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
}
