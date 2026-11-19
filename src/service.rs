//! Single-writer service activation. Standby processes never open the live store.
use crate::{
    App, api,
    config::{self, Config},
    db::Db,
    feishu,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex as AsyncMutex, Notify},
};

pub const CAPABILITY: u64 = 1;

/// Outer entry-point permits are deliberately non-reentrant. Nested App methods
/// do not acquire another permit, so closing admission cannot deadlock readers.
#[derive(Default)]
pub struct Gate {
    closed: AtomicBool,
    active: AtomicUsize,
    changed: Notify,
}
pub struct Permit(Arc<Gate>);
impl Gate {
    pub fn enter(self: &Arc<Self>) -> Option<Permit> {
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.changed.notify_one();
            return None;
        }
        Some(Permit(self.clone()))
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    async fn close_until(&self, deadline: tokio::time::Instant) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        tokio::time::timeout_at(deadline, async {
            loop {
                let changed = self.changed.notified();
                if self.active.load(Ordering::SeqCst) == 0 {
                    break;
                }
                changed.await;
            }
        })
        .await
        .context("Existing operations have not drained")
    }
    #[cfg(test)]
    pub(crate) async fn close_for_test(&self) -> Result<()> {
        self.close_until(tokio::time::Instant::now() + Duration::from_secs(100)).await
    }
    #[cfg(test)]
    pub(crate) fn reopen_for_test(&self) {
        self.reopen();
    }
    fn reopen(&self) {
        self.closed.store(false, Ordering::SeqCst);
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_one();
    }
}

/// Shield an admitted operation from cancellation of its response waiter. The
/// owned task retains its permit through remote effects and final journal writes.
pub async fn admitted<T: Send + 'static>(
    permit: Permit,
    operation: impl std::future::Future<Output = T> + Send + 'static,
) -> std::result::Result<T, tokio::task::JoinError> {
    tokio::spawn(async move {
        let _permit = permit;
        operation.await
    })
    .await
}

#[derive(Clone, Serialize, Deserialize)]
struct Owner {
    activation_id: String,
    epoch: String,
    pid: u32,
    version: String,
    digest: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Receipt {
    pub activation_id: String,
    pub phase: String,
    pub candidate_version: String,
    pub candidate_pid: u32,
    pub owner_committed: bool,
    #[serde(default)]
    pub owner_epoch: String,
    #[serde(default)]
    serving_owner: Option<Owner>,
    pub transport_ready: bool,
    pub error: String,
    #[serde(default)]
    candidate_executable: PathBuf,
    #[serde(default)]
    candidate_digest: String,
}
#[derive(Deserialize)]
pub struct Prepare {
    pub activation_id: String,
    pub candidate_executable: PathBuf,
}
#[derive(Deserialize)]
pub struct Activation {
    pub activation_id: String,
}
#[derive(Deserialize, Serialize)]
pub struct Ingest {
    pub project: String,
    pub app_id: String,
    pub payload: Value,
}

struct Resources {
    listener: TcpListener,
    lock: File,
}
#[cfg(unix)]
struct Candidate {
    child: Child,
    control: BufReader<tokio::net::UnixStream>,
    fingerprint: String,
}
pub struct Service {
    pub gate: Arc<Gate>,
    /// Quiesce manager actors before fencing their checkpoint and tool writes.
    pub(crate) manager_gate: Arc<Gate>,
    dir: PathBuf,
    resources: Mutex<Option<Resources>>,
    #[cfg(unix)]
    candidate: AsyncMutex<Option<Candidate>>,
    receipt: Mutex<Receipt>,
    pub relay: AtomicBool,
    pub accept_stop: tokio_util::sync::CancellationToken,
}
impl Service {
    pub fn new(dir: &Path) -> Self {
        let receipt = fs::read(dir.join("service-activation.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            gate: Arc::new(Gate::default()),
            manager_gate: Arc::new(Gate::default()),
            dir: dir.into(),
            resources: Mutex::new(None),
            #[cfg(unix)]
            candidate: AsyncMutex::new(None),
            receipt: Mutex::new(receipt),
            relay: AtomicBool::new(false),
            accept_stop: tokio_util::sync::CancellationToken::new(),
        }
    }
    pub fn attach(&self, listener: &TcpListener, lock: &File) -> Result<()> {
        *self.resources.lock().unwrap() = Some(Resources {
            listener: listener.try_clone()?,
            lock: lock.try_clone()?,
        });
        Ok(())
    }
    pub fn state(&self) -> Value {
        let mut value = json!(self.receipt.lock().unwrap().clone());
        value["capability"] = json!(CAPABILITY);
        value["owner_pid"] = json!(std::process::id());
        value["admission_closed"] = json!(self.gate.is_closed());
        value["manager_quiescing"] = json!(self.manager_gate.is_closed());
        value
    }
    fn persist(&self, receipt: Receipt) -> Result<Receipt> {
        durable(&self.dir.join("service-activation.json"), &receipt)?;
        *self.receipt.lock().unwrap() = receipt.clone();
        Ok(receipt)
    }
    pub fn started(&self) -> Result<()> {
        let mut receipt = self.receipt.lock().unwrap().clone();
        if receipt
            .serving_owner
            .as_ref()
            .is_some_and(|owner| owner.pid == std::process::id())
            && (receipt.owner_committed || receipt.activation_id.is_empty())
        {
            receipt.phase = "serving".into();
            receipt.error.clear();
            self.persist(receipt)?;
        }
        Ok(())
    }
    pub fn receipt(&self, id: &str) -> Result<Receipt> {
        let r = self.receipt.lock().unwrap().clone();
        ensure!(r.activation_id == id, "Unknown activation");
        Ok(r)
    }
}
fn durable(path: &Path, value: &impl Serialize) -> Result<()> {
    config::write_private(path, &serde_json::to_vec_pretty(value)?)?;
    File::open(path.parent().context("Missing journal directory")?)?.sync_all()?;
    Ok(())
}
fn digest(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}
fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 100
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid activation ID"
    );
    Ok(())
}
/// Exclude inbox/outbox/timer activity. Include every setting and execution
/// identity that changes which transports or native contracts must be prepared.
fn fingerprint(path: &Path, cfg: &Config) -> Result<String> {
    fingerprint_with_order(path, cfg, false)
}
fn fingerprint_with_order(path: &Path, cfg: &Config, insertion_order: bool) -> Result<String> {
    let c =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let schema: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let mut values = vec![json!(cfg), json!(schema)];
    for sql in [
        "SELECT id,agent,generation,terminal,execution FROM projects ORDER BY id",
        "SELECT project_id,app_id,app_secret FROM project_bots ORDER BY project_id",
    ] {
        let mut stmt = c.prepare(sql)?;
        let count = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let mut fields = Vec::new();
            for n in 0..count {
                fields.push(match row.get_ref(n)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Text(bytes) => json!(std::str::from_utf8(bytes)?),
                    _ => anyhow::bail!("Unexpected activation metadata"),
                });
            }
            values.push(json!(fields));
        }
    }
    // This is a cross-version wire fence. Dependencies can enable serde_json's
    // preserve_order feature, but predecessors used lexicographically sorted
    // object keys. Keep that encoding regardless of Cargo feature unification.
    let mut values = json!(values);
    if !insertion_order {
        values.sort_all_objects();
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&values)?)
    ))
}
fn predecessor_insertion_order(owner: Option<&Owner>, parent_pid: u32) -> Result<bool> {
    // 0.6.0 shipped an incidental preserve_order wire encoding. Its private
    // owner journal is written/validated by claim_restart before it serves.
    // Bind this narrow compatibility exception to our actual direct parent,
    // never a stale receipt or a caller-supplied version string.
    if let Some(owner) = owner
        && owner.version == "0.6.0"
    {
        ensure!(
            owner.pid == parent_pid,
            "Preparation owner identity changed"
        );
        return Ok(true);
    }
    Ok(false)
}
fn candidate_identity(path: &Path) -> Result<(PathBuf, String, String)> {
    let canonical = fs::canonicalize(path)?;
    ensure!(
        canonical == path && path.is_file(),
        "Candidate path must be canonical"
    );
    let current = fs::canonicalize(std::env::current_exe()?)?;
    if canonical != current
        || !cfg!(debug_assertions)
        || canonical
            .components()
            .any(|part| part.as_os_str().to_string_lossy().ends_with(".app"))
    {
        let app = canonical
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .context("Candidate must belong to a signed application")?;
        ensure!(
            canonical == app.join("Contents/MacOS/hfb"),
            "Candidate must be the application service"
        );
        crate::update::verify_bundle(app, None)?;
    }
    let output = std::process::Command::new(&canonical)
        .arg("companion-identity")
        .output()?;
    ensure!(output.status.success(), "Candidate identity failed");
    let identity: Value = serde_json::from_slice(&output.stdout)?;
    let version = identity["version"]
        .as_str()
        .context("Missing candidate version")?
        .to_owned();
    ensure!(
        semver::Version::parse(&version)? >= semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
        "Service downgrades are not supported"
    );
    ensure!(
        identity["api_protocol"] == api::PROTOCOL,
        "Candidate service protocol is not supported"
    );
    Ok((canonical, version, digest(path)?))
}
#[cfg(unix)]
async fn receive(control: &mut BufReader<tokio::net::UnixStream>) -> Result<Value> {
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(45), control.read_line(&mut line)).await??;
    ensure!(
        n > 0 && line.len() <= 65536,
        "Candidate control connection ended"
    );
    Ok(serde_json::from_str(&line)?)
}
#[cfg(unix)]
async fn send(control: &mut BufReader<tokio::net::UnixStream>, value: Value) -> Result<()> {
    let bytes = serde_json::to_vec(&value)?;
    control.get_mut().write_all(&bytes).await?;
    control.get_mut().write_all(b"\n").await?;
    Ok(())
}

#[cfg(unix)]
async fn spawn_candidate(app: &Arc<App>, path: &Path, id: &str) -> Result<Candidate> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    // Only a terminated predecessor of this private attempt can own these files.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(app.dir.join(format!(".activation-{id}.sqlite3{suffix}")));
    }
    let resources = app
        .service
        .resources
        .lock()
        .unwrap()
        .as_ref()
        .context("Service listener is not transferable")?
        .listener
        .try_clone()?;
    let lock = app
        .service
        .resources
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .lock
        .try_clone()?;
    let (parent, child) = std::os::unix::net::UnixStream::pair()?;
    parent.set_nonblocking(true)?;
    let fds = [child.as_raw_fd(), resources.as_raw_fd(), lock.as_raw_fd()];
    let mut command = Command::new(path);
    command
        .arg("--data-dir")
        .arg(&app.dir)
        .arg("serve")
        .arg("--handover-control-fd")
        .arg(fds[0].to_string())
        .arg("--handover-listener-fd")
        .arg(fds[1].to_string())
        .arg("--handover-lock-fd")
        .arg(fds[2].to_string())
        .arg("--handover-id")
        .arg(id)
        .stdin(std::process::Stdio::null());
    unsafe {
        command.as_std_mut().pre_exec(move || {
            unsafe extern "C" {
                fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            }
            for fd in fds {
                let flags = fcntl(fd, 1);
                if flags < 0 || fcntl(fd, 2, flags & !1) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child_process = command.spawn()?;
    drop(child);
    drop(resources);
    drop(lock);
    let mut candidate = Candidate {
        child: child_process,
        control: BufReader::new(tokio::net::UnixStream::from_std(parent)?),
        fingerprint: String::new(),
    };
    let ready = match receive(&mut candidate.control).await {
        Ok(ready) if ready["phase"] == "transport_ready" => ready,
        Ok(ready) => {
            let _ = candidate.child.kill().await;
            anyhow::bail!(
                "Candidate preparation failed: {}",
                ready["error"]
                    .as_str()
                    .unwrap_or("Invalid readiness receipt")
            );
        }
        Err(e) => {
            let _ = candidate.child.kill().await;
            return Err(e);
        }
    };
    candidate.fingerprint = ready["fingerprint"]
        .as_str()
        .context("Missing preparation fence")?
        .into();
    Ok(candidate)
}

#[cfg(unix)]
pub async fn prepare(app: &Arc<App>, input: Prepare) -> Result<Receipt> {
    valid_id(&input.activation_id)?;
    let mut slot = app.service.candidate.lock().await;
    ensure!(
        !app.service.relay.load(Ordering::SeqCst),
        "Ownership already moved to the new service"
    );
    let mut previous = app.service.receipt.lock().unwrap().clone();
    if !previous.owner_committed
        && let Some(candidate) = slot.as_mut()
        && candidate.child.try_wait()?.is_some()
    {
        slot.take();
        previous.phase = "failed".into();
        previous.transport_ready = false;
        previous.error = "The previous standby ended before activation".into();
        app.service.persist(previous.clone())?;
    }
    if previous.activation_id == input.activation_id {
        ensure!(
            previous.candidate_executable == input.candidate_executable,
            "Activation ID already belongs to a different candidate"
        );
        if previous.owner_committed {
            return Ok(previous);
        }
        if let Some(candidate) = slot.as_mut()
            && candidate.child.try_wait()?.is_none()
            && previous.phase == "prepared"
            && send(&mut candidate.control, json!({"command":"ping"}))
                .await
                .is_ok()
            && receive(&mut candidate.control)
                .await
                .is_ok_and(|v| v["phase"] == "transport_ready")
        {
            return Ok(previous);
        }
        if let Some(mut candidate) = slot.take() {
            if candidate.child.try_wait()?.is_none() {
                candidate.child.kill().await?;
            }
            candidate.child.wait().await?;
        }
    }
    ensure!(slot.is_none(), "Another activation is already prepared");
    let path = input.candidate_executable;
    let (path, version, hash) =
        tokio::task::spawn_blocking(move || candidate_identity(&path)).await??;
    let candidate = spawn_candidate(app, &path, &input.activation_id).await?;
    let pid = candidate.child.id().context("Missing candidate PID")?;
    let r = app.service.persist(Receipt {
        activation_id: input.activation_id,
        phase: "prepared".into(),
        candidate_version: version,
        candidate_pid: pid,
        owner_committed: false,
        owner_epoch: String::new(),
        serving_owner: previous.serving_owner,
        transport_ready: true,
        error: String::new(),
        candidate_executable: path,
        candidate_digest: hash,
    })?;
    *slot = Some(candidate);
    Ok(r)
}

#[cfg(unix)]
pub async fn abort(app: &Arc<App>, id: &str) -> Result<Receipt> {
    let mut slot = app.service.candidate.lock().await;
    let mut r = app.service.receipt(id)?;
    ensure!(
        !r.owner_committed,
        "Ownership has already committed; use the committed service status"
    );
    if let Some(mut candidate) = slot.take() {
        if candidate.child.try_wait()?.is_none() {
            candidate.child.kill().await?;
        }
        candidate.child.wait().await?;
    }
    app.service.gate.reopen();
    r.phase = "aborted".into();
    app.service.persist(r)
}

#[cfg(unix)]
async fn activate_candidate(
    app: &Arc<App>,
    candidate: &mut Candidate,
    r: &Receipt,
    recovery: bool,
) -> Result<()> {
    send(
        &mut candidate.control,
        json!({"command":if recovery {"recover"}else{"commit"}}),
    )
    .await?;
    let ack = receive(&mut candidate.control).await?;
    ensure!(
        ack["phase"] == "serving",
        "Candidate did not confirm serving"
    );
    let cfg = app.config.read().await.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(response) = app
                .http
                .get(format!(
                    "{}/api/service/activation/status?activation_id={}",
                    cfg.url(),
                    r.activation_id
                ))
                .bearer_auth(&cfg.token)
                .timeout(Duration::from_secs(1))
                .header("connection", "close")
                .send()
                .await
                && let Ok(response) = response.json::<Value>().await
                && response["owner_pid"] == r.candidate_pid
                && response["phase"] == "serving"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("Public listener owner challenge failed")?;
    Ok(())
}

#[cfg(unix)]
pub async fn commit(app: &Arc<App>, id: &str) -> Result<Receipt> {
    let mut slot = app.service.candidate.lock().await;
    let mut r = app.service.receipt(id)?;
    if r.owner_committed {
        return Ok(r);
    }
    let candidate = slot
        .as_mut()
        .context("Prepared candidate is no longer running")?;
    ensure!(
        candidate.child.try_wait()?.is_none(),
        "Prepared candidate exited"
    );
    ensure!(
        digest(&r.candidate_executable)? == r.candidate_digest,
        "Candidate changed after preparation"
    );
    r.phase = "draining".into();
    app.service.persist(r.clone())?;
    // Keep ordinary MCP/control admission open until live manager requests and
    // their tool results reach a durable boundary. One shared bound stays below
    // existing 95-second MCP admission timeouts; expiry leaves the old work alive.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let drained = tokio::time::timeout_at(deadline, async {
        let drain = async {
            app.service.manager_gate.close_until(deadline).await?;
            app.service.gate.close_until(deadline).await
        };
        tokio::pin!(drain);
        loop {
            tokio::select! {
                result = &mut drain => return result,
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    // Standby control reads expire after 45 seconds. Keep the
                    // verified candidate warm while the old actor finishes.
                    send(&mut candidate.control, json!({"command":"ping"})).await?;
                    ensure!(receive(&mut candidate.control).await?["phase"] == "transport_ready",
                        "Prepared candidate is no longer ready");
                }
            }
        }
    }).await.context("Existing operations have not drained").and_then(|result| result);
    if let Err(e) = drained {
        app.service.gate.reopen();
        app.service.manager_gate.reopen();
        r.phase = "failed".into();
        r.error = e.to_string();
        return app.service.persist(r);
    }
    let result = async {
        // All admitted effects have completed. Transient records left behind by
        // previously abandoned operations are uncertain, never replayed or stuck.
        app.db.call(|c|{
            c.execute("UPDATE messages SET status='uncertain',error='The previous operation did not record a result; verify before retrying',updated_at=? WHERE status IN ('sending','submitting')",[crate::db::now()])?;
            crate::work::sync_inbound(c)?;
            Ok(())
        }).await?;
        let cfg = app.config.read().await.clone();
        ensure!(
            fingerprint(&app.dir.join("bridge.sqlite3"), &cfg)? == candidate.fingerprint,
            "Project or bot configuration changed; prepare the upgrade again"
        );
        let valid = tokio::time::timeout_at(deadline, async {
            send(&mut candidate.control, json!({"command":"validate"})).await?;
            receive(&mut candidate.control).await
        }).await.context("Candidate validation exceeded the drain deadline")??;
        ensure!(
            valid["phase"] == "validated",
            "Candidate is no longer ready"
        );
        let mut owned=r.clone();
        owned.phase="committing".into();owned.owner_committed=true;owned.owner_epoch=crate::db::id();
        owned.serving_owner=Some(Owner{activation_id:id.into(),epoch:owned.owner_epoch.clone(),pid:owned.candidate_pid,version:owned.candidate_version.clone(),digest:owned.candidate_digest.clone()});
        // This journal is the sole irreversible boundary. If directory fsync
        // fails after rename, inspect the installed receipt conservatively.
        match app.service.persist(owned.clone()) {
            Ok(committed)=>r=committed,
            Err(error)=>{
                if fs::read(app.dir.join("service-activation.json")).ok()
                    .and_then(|bytes|serde_json::from_slice::<Receipt>(&bytes).ok())
                    .is_some_and(|disk|disk.activation_id==id && disk.owner_committed && disk.candidate_pid==owned.candidate_pid) {r=owned;}
                return Err(error);
            }
        }
        app.service.accept_stop.cancel();
        app.service.relay.store(true, Ordering::SeqCst);
        activate_candidate(app, candidate, &r, false).await?;
        r.phase = "serving".into();
        app.service.persist(r.clone())?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        if !r.owner_committed {
            app.service.gate.reopen();
            app.service.manager_gate.reopen();
            r.phase = "failed".into();
            r.error = error.to_string();
            return app.service.persist(r);
        }
        r.error = error.to_string();
        app.service.accept_stop.cancel();
        app.service.relay.store(true, Ordering::SeqCst);
        // A committed candidate may have effected messages before dying. Prove
        // termination, assign a fresh owner epoch, and start the NEW executable
        // against the latest journal with normal uncertainty recovery.
        let mut recovered = false;
        for _ in 0..2 {
            if let Some(mut dead) = slot.take() {
                if dead.child.try_wait()?.is_none() {
                    dead.child.kill().await?;
                }
                dead.child.wait().await?;
            }
            ensure!(
                digest(&r.candidate_executable)? == r.candidate_digest,
                "Committed executable changed; preserve the committed recovery path"
            );
            match spawn_candidate(app, &r.candidate_executable, &r.activation_id).await {
                Ok(mut successor) => {
                    r.candidate_pid = successor.child.id().context("Missing successor PID")?;
                    r.phase = "committing".into();
                    r.owner_epoch = crate::db::id();
                    r.serving_owner = Some(Owner {
                        activation_id: id.into(),
                        epoch: r.owner_epoch.clone(),
                        pid: r.candidate_pid,
                        version: r.candidate_version.clone(),
                        digest: r.candidate_digest.clone(),
                    });
                    app.service.persist(r.clone())?;
                    match activate_candidate(app, &mut successor, &r, true).await {
                        Ok(()) => {
                            *slot = Some(successor);
                            recovered = true;
                            break;
                        }
                        Err(error) => {
                            r.error = error.to_string();
                            *slot = Some(successor);
                        }
                    }
                }
                Err(error) => r.error = error.to_string(),
            }
        }
        if !recovered {
            if let Some(mut dead) = slot.take() {
                if dead.child.try_wait()?.is_none() {
                    dead.child.kill().await?;
                }
                dead.child.wait().await?;
            }
            r.phase = "failed".into();
            let r = app.service.persist(r)?;
            let shutdown = app.shutdown.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                shutdown.cancel();
            });
            return Ok(r);
        }
        r.phase = "serving".into();
        r.error.clear();
        app.service.persist(r.clone())?;
    }
    // Detach only after the public endpoint acknowledges the expected owner.
    if let Some(mut candidate) = slot.take() {
        tokio::spawn(async move {
            let _ = candidate.child.wait().await;
        });
    }
    let shutdown = app.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        shutdown.cancel();
    });
    Ok(r)
}

/// Synchronous handler worker RPC. Success means the sole owner committed the
/// event before its SDK sends ACK200. Timeout/error yields ACK500 and retry.
pub fn relay(cfg: &Config, input: &Ingest) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let remaining = || {
        deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|d| !d.is_zero())
            .context("Ingress relay deadline exceeded")
    };
    let mut stream = std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], cfg.port).into(),
        Duration::from_millis(400),
    )?;
    let body = serde_json::to_vec(input)?;
    let mut request = Vec::new();
    write!(
        request,
        "POST /api/service/activation/ingest HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        cfg.port,
        cfg.token,
        body.len()
    )?;
    request.extend_from_slice(&body);
    let mut offset = 0;
    while offset < request.len() {
        stream.set_write_timeout(Some(remaining()?))?;
        let n = stream.write(&request[offset..])?;
        ensure!(n > 0, "Ingress owner closed before receiving event");
        offset += n;
    }
    let mut response = Vec::new();
    loop {
        stream.set_read_timeout(Some(remaining()?))?;
        let mut buffer = [0; 64];
        let n = stream.read(&mut buffer)?;
        ensure!(n > 0, "Ingress owner closed before committing event");
        response.extend_from_slice(&buffer[..n]);
        if response.windows(2).any(|pair| pair == b"\r\n") {
            break;
        }
        ensure!(response.len() < 256, "Invalid ingress owner response");
    }
    ensure!(
        response.starts_with(b"HTTP/1.1 200 "),
        "Ingress owner has not committed this event"
    );
    Ok(())
}

/// Bootstrap admission for a published predecessor without handover support.
/// This deliberately does not claim transport readiness or open the live App.
pub fn preflight(dir: &Path) -> Result<Value> {
    let cfg: Config = serde_json::from_slice(&fs::read(dir.join("config.json"))?)?;
    let snapshot = dir.join(format!(".activation-preflight-{}.sqlite3", crate::db::id()));
    let result = (|| -> Result<Value> {
        let conn = rusqlite::Connection::open_with_flags(
            dir.join("bridge.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let schema: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            schema == 7,
            "This schema requires a different upgrade boundary"
        );
        conn.execute("VACUUM INTO ?", [snapshot.to_string_lossy().as_ref()])?;
        let db = Db::open(&snapshot)?;
        let projects=db.sync(|c|{
            Ok(c.prepare("SELECT name,execution,terminal FROM projects")?.query_map([],|r|Ok(json!({"name":r.get::<_,String>(0)?,"execution":serde_json::from_str::<Value>(&r.get::<_,String>(1)?).unwrap_or_default(),"terminal":r.get::<_,Option<String>>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?)
        })?;
        crate::mcp::check_activation(&projects, api::PROTOCOL)?;
        ensure!(
            cfg.port > 0 && cfg.token.len() >= 32,
            "Invalid service configuration"
        );
        Ok(
            json!({"schema_compatible":true,"store_readable":true,"mcp_compatible":true,"transport_ready":false,"api_protocol":api::PROTOCOL,"version":env!("CARGO_PKG_VERSION")}),
        )
    })();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", snapshot.display()));
    }
    result
}

/// Internal child entry. Descriptors are inherited exclusively from the direct
/// parent; no disk socket, argument secret or unauthenticated public control.
#[cfg(unix)]
pub async fn standby(
    dir: PathBuf,
    control_fd: i32,
    listener_fd: i32,
    lock_fd: i32,
    id: String,
) -> Result<()> {
    use std::os::fd::FromRawFd;
    valid_id(&id)?;
    ensure!(
        control_fd > 2
            && listener_fd > 2
            && lock_fd > 2
            && control_fd != listener_fd
            && control_fd != lock_fd
            && listener_fd != lock_fd,
        "Invalid inherited descriptors"
    );
    // Inheritance is only for this direct child, never its HERDR/Agent children.
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    for fd in [control_fd, listener_fd, lock_fd] {
        let flags = unsafe { fcntl(fd, 1) };
        ensure!(
            flags >= 0 && unsafe { fcntl(fd, 2, flags | 1) } >= 0,
            "Cannot seal inherited descriptors"
        );
    }
    let control = unsafe { std::os::unix::net::UnixStream::from_raw_fd(control_fd) };
    control.set_nonblocking(true)?;
    let mut control = BufReader::new(tokio::net::UnixStream::from_std(control)?);
    let listener = unsafe { TcpListener::from_raw_fd(listener_fd) };
    let lock = unsafe { File::from_raw_fd(lock_fd) };
    listener.set_nonblocking(true)?;
    let cfg: Config = serde_json::from_slice(&fs::read(dir.join("config.json"))?)?;
    ensure!(
        listener.local_addr()?.ip().is_loopback() && listener.local_addr()?.port() == cfg.port,
        "Inherited listener identity changed"
    );
    let snapshot = dir.join(format!(".activation-{id}.sqlite3"));
    let result = async {
        let conn = rusqlite::Connection::open_with_flags(
            dir.join("bridge.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.execute("VACUUM INTO ?", [snapshot.to_string_lossy().as_ref()])?;
        drop(conn);
        let original = rusqlite::Connection::open_with_flags(
            &snapshot,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let schema: i64 = original.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            schema == 7,
            "This schema requires a different upgrade boundary"
        );
        drop(original);
        let receipt: Receipt =
            serde_json::from_slice(&fs::read(dir.join("service-activation.json"))?)?;
        let insertion_order = predecessor_insertion_order(
            receipt.serving_owner.as_ref(),
            unsafe { libc::getppid() } as u32,
        )?;
        let fingerprint = fingerprint_with_order(&snapshot, &cfg, insertion_order)?;
        let db = Db::open(&snapshot)?;
        let projects = db
            .projects()
            .await?
            .into_iter()
            .map(|p| json!(p))
            .collect::<Vec<_>>();
        crate::mcp::check_activation(&projects, api::PROTOCOL)?;
        let bindings = db.bots().await?;
        let warm = feishu::warm_ingress(cfg.clone(), bindings).await?;
        send(
            &mut control,
            json!({"phase":"transport_ready","fingerprint":fingerprint}),
        )
        .await?;
        Ok::<_, anyhow::Error>(warm)
    }
    .await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", snapshot.display()));
    }
    let mut warm = match result {
        Ok(warm) => warm,
        Err(error) => {
            let _ = send(
                &mut control,
                json!({"phase":"failed","error":error.to_string()}),
            )
            .await;
            return Err(error);
        }
    };
    let committed = loop {
        let message = receive(&mut control).await;
        match message {
            Ok(v) if v["command"] == "ping" => {
                ensure!(warm.is_ready(), "Prepared transport disconnected");
                send(&mut control, json!({"phase":"transport_ready"})).await?;
            }
            Ok(v) if v["command"] == "validate" => {
                if !warm.is_ready() {
                    send(
                        &mut control,
                        json!({"phase":"failed","error":"Prepared transport disconnected"}),
                    )
                    .await?;
                    return Ok(());
                }
                let c = rusqlite::Connection::open_with_flags(
                    dir.join("bridge.sqlite3"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                let projects=c.prepare("SELECT execution,terminal FROM projects")?.query_map([],|r|Ok(json!({"execution":serde_json::from_str::<Value>(&r.get::<_,String>(0)?).unwrap_or_default(),"terminal":r.get::<_,Option<String>>(1)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
                crate::mcp::check_activation(&projects, api::PROTOCOL)?;
                send(&mut control, json!({"phase":"validated"})).await?;
            }
            Ok(v) if v["command"] == "commit" || v["command"] == "recover" => {
                break if v["command"] == "recover" { 2 } else { 1 };
            }
            Err(_) => {
                let receipt: Receipt =
                    serde_json::from_slice(&fs::read(dir.join("service-activation.json"))?)?;
                break if receipt.activation_id == id
                    && receipt.owner_committed
                    && receipt.candidate_pid == std::process::id()
                {
                    2
                } else {
                    0
                };
            }
            _ => break 0,
        }
    };
    if committed == 0 {
        return Ok(());
    }
    let receipt: Receipt = serde_json::from_slice(&fs::read(dir.join("service-activation.json"))?)?;
    ensure!(
        receipt.activation_id == id
            && receipt.owner_committed
            && receipt.candidate_pid == std::process::id(),
        "Ownership journal does not authorize this process"
    );
    let app = if committed == 2 {
        App::new(dir.clone(), cfg)?
    } else {
        App::new_handover(dir.clone(), cfg)?
    };
    app.service.attach(&listener, &lock)?;
    app.companion()?;
    warm.activate(app.clone()).await;
    let stop = app.shutdown.clone();
    let tcp = tokio::net::TcpListener::from_std(listener)?;
    let serving_app = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(tcp, api::router(serving_app))
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
    });
    let mut receipt: Receipt =
        serde_json::from_slice(&fs::read(dir.join("service-activation.json"))?)?;
    receipt.phase = "serving".into();
    app.service.persist(receipt)?;
    app.workers_with_ingress(Some(warm));
    send(&mut control, json!({"phase":"serving"})).await?;
    drop(control);
    // The inherited open-file description keeps the daemon flock continuously
    // owned even when the previous process exits.
    let shutdown = app.shutdown.clone();
    tokio::spawn(async move {
        crate::service::termination().await;
        shutdown.cancel();
    });
    server.await??;
    drop(lock);
    Ok(())
}

/// The updater calls this only after validating its explicit legacy bootstrap
/// transaction. It is the service's authoritative ownership commit, before DB
/// recovery or workers. Retrying the same verified target is safe.
pub fn claim_bootstrap_restart(dir: &Path, id: &str) -> Result<()> {
    valid_id(id)?;
    let path = dir.join("service-activation.json");
    if let Ok(bytes) = fs::read(&path) {
        let previous: Receipt = serde_json::from_slice(&bytes)?;
        if let Some(owner) = previous.serving_owner {
            ensure!(
                semver::Version::parse(env!("CARGO_PKG_VERSION"))?
                    >= semver::Version::parse(&owner.version)?,
                "Service downgrades are not supported"
            );
        }
    }
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let hash = digest(&executable)?;
    let epoch = crate::db::id();
    let owner = Owner {
        activation_id: id.into(),
        epoch: epoch.clone(),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        digest: hash.clone(),
    };
    durable(
        &path,
        &Receipt {
            activation_id: id.into(),
            phase: "committing".into(),
            candidate_version: env!("CARGO_PKG_VERSION").into(),
            candidate_pid: std::process::id(),
            owner_committed: true,
            owner_epoch: epoch,
            serving_owner: Some(owner),
            candidate_executable: executable,
            candidate_digest: hash,
            ..Receipt::default()
        },
    )
}

/// A normal process restart after a committed handover may only continue the
/// committed executable. It claims a fresh epoch before live-store recovery.
pub fn claim_restart(dir: &Path) -> Result<()> {
    let path = dir.join("service-activation.json");
    let mut receipt: Receipt = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        Receipt::default()
    };
    let hash = digest(&std::env::current_exe()?)?;
    if let Some(owner) = &receipt.serving_owner {
        ensure!(
            owner.version == env!("CARGO_PKG_VERSION") && owner.digest == hash,
            "Continue the committed service executable to recover this upgrade"
        );
    }
    let mut owner = receipt.serving_owner.clone().unwrap_or(Owner {
        activation_id: format!("initial-{}", crate::db::id()),
        epoch: String::new(),
        pid: 0,
        version: env!("CARGO_PKG_VERSION").into(),
        digest: hash,
    });
    owner.pid = std::process::id();
    owner.epoch = crate::db::id();
    if receipt.owner_committed && receipt.activation_id == owner.activation_id {
        receipt.candidate_pid = owner.pid;
        receipt.owner_epoch = owner.epoch.clone();
        receipt.phase = "committing".into();
    } else if !receipt.activation_id.is_empty() {
        receipt.phase = "failed".into();
        receipt.transport_ready = false;
        receipt.error = "The previous preparation ended before activation".into();
    }
    receipt.serving_owner = Some(owner);
    durable(&path, &receipt)
}

pub async fn termination() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activation_fingerprint_matches_pre_preserve_order_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.sqlite3");
        let db = Db::open(&path).unwrap();
        db.sync(|c| {
            c.execute("INSERT INTO projects(id,name,path,agent,generation,created_at,mcp_token) VALUES('p','p','','codex','generation',0,'token')", [])?;
            c.execute("INSERT INTO project_bots(project_id,app_id,app_secret,updated_at) VALUES('p','app','secret',0)", [])?;
            Ok(())
        }).unwrap();
        let cfg = Config {
            token: "fixture-token".into(),
            herdr_session: "fixture".into(),
            ..Config::default()
        };
        // Captured wire encoding from serde_json without preserve_order (0.5.2).
        let legacy = "c3741a670657dc6ae5e185b1e212a5faed2b155bc9b419c4304f0d834cf5c9e3";
        assert_eq!(fingerprint(&path, &cfg).unwrap(), legacy);
        assert_eq!(
            fingerprint_with_order(&path, &cfg, true).unwrap(),
            "0be61639e906b673be4be247fd0e1d98a9998f38b5da5d098de8e253bc181a9e"
        );
        let mut changed_cfg = cfg.clone();
        changed_cfg.herdr_session = "other-session".into();
        assert_ne!(fingerprint(&path, &changed_cfg).unwrap(), legacy);
        db.sync(|c| {
            c.execute("UPDATE projects SET generation='new-generation'", [])?;
            Ok(())
        })
        .unwrap();
        assert_ne!(fingerprint(&path, &cfg).unwrap(), legacy);
    }
    #[test]
    fn insertion_order_compatibility_requires_the_actual_060_parent() {
        let mut owner = Owner {
            activation_id: "previous-activation".into(),
            epoch: "epoch".into(),
            pid: 123,
            version: "0.6.0".into(),
            digest: "validated-by-parent-restart".into(),
        };
        assert!(predecessor_insertion_order(Some(&owner), 123).unwrap());
        assert!(predecessor_insertion_order(Some(&owner), 456).is_err());
        for version in ["0.5.2", "0.6.1", "0.7.0"] {
            owner.version = version.into();
            assert!(!predecessor_insertion_order(Some(&owner), 123).unwrap());
        }
        assert!(!predecessor_insertion_order(None, 123).unwrap());
    }
    #[tokio::test]
    async fn closing_admission_drains_existing_effect_and_reopens_after_abort() {
        let gate = Arc::new(Gate::default());
        let effect = gate.enter().unwrap();
        let closing = gate.clone();
        let task = tokio::spawn(async move { closing.close_until(tokio::time::Instant::now() + Duration::from_secs(100)).await });
        tokio::task::yield_now().await;
        assert!(gate.enter().is_none());
        assert!(!task.is_finished());
        drop(effect);
        task.await.unwrap().unwrap();
        assert!(gate.enter().is_none());
        gate.reopen();
        assert!(gate.enter().is_some());
    }
    #[tokio::test]
    async fn manager_drain_timeout_keeps_live_work_and_control_admission() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let actor = service.manager_gate.enter().unwrap();
        let drain = service.manager_gate.close_until(
            tokio::time::Instant::now() + Duration::from_millis(20),
        );
        assert!(drain.await.is_err());
        assert!(service.manager_gate.enter().is_none());
        // A timeout never drops the actor's permit or closes MCP/Stop admission.
        assert_eq!(service.manager_gate.active.load(Ordering::SeqCst), 1);
        assert!(service.gate.enter().is_some());
        service.manager_gate.reopen();
        drop(actor);
        assert!(service.manager_gate.enter().is_some());
        let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        service.manager_gate.close_until(deadline).await.unwrap();
        service.gate.close_until(deadline).await.unwrap();
    }
    #[test]
    fn clean_handover_and_readonly_preflight_do_not_reclassify_live_effects() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        cfg.save(dir.path()).unwrap();
        let path = dir.path().join("bridge.sqlite3");
        let db = Db::open(&path).unwrap();
        db.sync(|c|{
            c.execute("INSERT INTO projects(id,name,path,agent,created_at,mcp_token) VALUES('p','p','','custom',0,'token')",[])?;
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('m','p','out','source','content','sending',0,0)",[])?;
            Ok(())
        }).unwrap();
        assert!(
            preflight(dir.path()).unwrap()["store_readable"]
                .as_bool()
                .unwrap()
        );
        drop(db);
        let clean = Db::open_handover(&path).unwrap();
        assert_eq!(
            clean
                .sync(|c| Ok(
                    c.query_row("SELECT status FROM messages WHERE id='m'", [], |r| r
                        .get::<_, String>(0))?
                ))
                .unwrap(),
            "sending"
        );
        drop(clean);
        let crash = Db::open(&path).unwrap();
        assert_eq!(
            crash
                .sync(|c| Ok(
                    c.query_row("SELECT status FROM messages WHERE id='m'", [], |r| r
                        .get::<_, String>(0))?
                ))
                .unwrap(),
            "uncertain"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_response_waiter_cannot_release_held_database_effect() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("db.sqlite3")).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let gate = Arc::new(Gate::default());
        let permit = gate.enter().unwrap();
        let worker = db.clone();
        let waiter = tokio::spawn(admitted(permit, async move {
            worker
                .call(move |c| {
                    let tx = c.unchecked_transaction()?;
                    let _ = started_tx.send(());
                    release_rx.recv().unwrap();
                    tx.execute("CREATE TABLE cancelled_handler_effect(value INTEGER)", [])?;
                    tx.execute("INSERT INTO cancelled_handler_effect VALUES(1)", [])?;
                    tx.commit()?;
                    Ok(())
                })
                .await
                .unwrap();
        }));
        started_rx.await.unwrap();
        waiter.abort();
        let closing = gate.clone();
        let drain = tokio::spawn(async move { closing.close_until(tokio::time::Instant::now() + Duration::from_secs(100)).await });
        while !gate.closed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert!(!drain.is_finished());
        assert!(gate.enter().is_none());
        release_tx.send(()).unwrap();
        drain.await.unwrap().unwrap();
        assert_eq!(
            db.call(|c| Ok(c.query_row(
                "SELECT value FROM cancelled_handler_effect",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
            1
        );
    }
    #[test]
    fn abandoned_new_preparation_preserves_previous_serving_owner() {
        let dir = tempfile::tempdir().unwrap();
        claim_restart(dir.path()).unwrap();
        let service = Service::new(dir.path());
        let owner = service
            .receipt
            .lock()
            .unwrap()
            .serving_owner
            .clone()
            .unwrap();
        service
            .persist(Receipt {
                activation_id: "unfinished-attempt".into(),
                phase: "prepared".into(),
                candidate_version: "999.0.0".into(),
                serving_owner: Some(owner.clone()),
                ..Receipt::default()
            })
            .unwrap();
        claim_restart(dir.path()).unwrap();
        let restarted = Service::new(dir.path());
        let receipt = restarted.receipt.lock().unwrap();
        assert!(!receipt.owner_committed);
        assert_eq!(receipt.phase, "failed");
        let current = receipt.serving_owner.as_ref().unwrap();
        assert_eq!(current.activation_id, owner.activation_id);
        assert_eq!(current.version, env!("CARGO_PKG_VERSION"));
        assert_ne!(current.epoch, owner.epoch);
    }
}
