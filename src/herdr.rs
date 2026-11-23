//! Legacy HERDR execution and terminal presentation transport.
//!
//! `hc` is intentionally absent from this module. Persisted legacy execution
//! identities must be checked before sending input or closing a pane.
use crate::{config::Config, db::Project};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
    sync::Mutex,
};

pub mod runtime;
pub mod terminal_control;
pub(crate) mod owned_close;

pub use crate::adapter::{launch_args, merge_mcp};
const PROTOCOL: u64 = 22;
struct StartAttempt {
    retry_at: tokio::time::Instant,
    deadline: tokio::time::Instant,
    child: Option<tokio::process::Child>,
}
static START_LOCK: Mutex<BTreeMap<PathBuf, StartAttempt>> = Mutex::const_new(BTreeMap::new());

#[derive(Clone, Debug)]
pub(crate) struct HostBoot {
    pub id: String,
    pub started_at: i64,
}
pub(crate) fn host_boot() -> Option<HostBoot> {
    #[cfg(target_os = "macos")]
    {
        let mut time: libc::timeval = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::timeval>();
        let mut id = [0u8; 64];
        let mut id_length = id.len();
        unsafe {
            if libc::sysctlbyname(
                c"kern.boottime".as_ptr(),
                (&mut time as *mut libc::timeval).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            ) != 0
                || libc::sysctlbyname(
                    c"kern.bootsessionuuid".as_ptr(),
                    id.as_mut_ptr().cast(),
                    &mut id_length,
                    std::ptr::null_mut(),
                    0,
                ) != 0
            {
                return None;
            }
        }
        let id = std::str::from_utf8(&id[..id_length])
            .ok()?
            .trim_end_matches('\0');
        Some(HostBoot {
            id: uuid::Uuid::parse_str(id).ok()?.to_string(),
            started_at: time.tv_sec.checked_mul(1000)?,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let seconds = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .parse::<i64>()
            .ok()?;
        Some(HostBoot {
            id: uuid::Uuid::parse_str(id.trim()).ok()?.to_string(),
            started_at: seconds.checked_mul(1000)?,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}
fn execution_predates_boot(execution: &Value, boot: Option<&HostBoot>) -> bool {
    let Some(boot) = boot.filter(|boot| boot.started_at > 0) else {
        return false;
    };
    if let Some(id) = execution["host_boot_id"].as_str() {
        return uuid::Uuid::parse_str(id)
            .ok()
            .is_some_and(|id| id.to_string() != boot.id);
    }
    // Legacy launch time is hydrated only from the matching durable execution
    // journal. Missing or ambiguous history never weakens child coverage.
    execution["launch_started_at"]
        .as_i64()
        .is_some_and(|at| at > 0 && at < boot.started_at)
}
pub(crate) fn runtime_missing(error: &anyhow::Error) -> bool {
    error.is::<NotSubmitted>()
        && error
            .chain()
            .filter_map(|e| e.downcast_ref::<std::io::Error>())
            .any(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            })
}

#[derive(Debug)]
pub struct RpcFailure {
    pub kind: String,
    pub message: String,
    pub hint: String,
}
impl std::fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HERDR {}: {}", self.kind, self.message)
    }
}
impl std::error::Error for RpcFailure {}
#[derive(Debug)]
pub struct NotSubmitted(pub(crate) anyhow::Error);
impl std::fmt::Display for NotSubmitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for NotSubmitted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub fn definitely_not_sent(error: &anyhow::Error) -> bool {
    if error.is::<NotSubmitted>() || error.is::<SessionStarting>() {
        return true;
    }
    error.downcast_ref::<RpcFailure>().is_some_and(|e| {
        matches!(
            e.kind.as_str(),
            "agent_blocked"
                | "agent_not_ready"
                | "agent_not_found"
                | "agent_target_ambiguous"
                | "empty_agent_prompt"
                | "invalid_target"
        )
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionRef {
    pub backend: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub managed_name: String,
    pub generation: String,
    pub shell_pid: Option<i64>,
    pub launch_permit: PathBuf,
}
impl ExecutionRef {
    pub fn from_project(p: &Project) -> Result<Self> {
        let value: Self = serde_json::from_value(p.execution.clone())
            .context("This execution predates the legacy HERDR backend; start a new session")?;
        if value.backend != "legacy"
            || value.generation != p.generation
            || p.terminal.as_deref() != Some(value.terminal_id.as_str())
            || value.shell_pid != p.terminal_pid
        {
            bail!("The saved execution identity changed; reconnect the project");
        }
        Ok(value)
    }
}

fn command_environment(config: &Config, cmd: &mut Command) {
    // Never route product traffic through the development agent's terminal.
    for key in [
        "SSH_CLIENT",
        "SSH_CONNECTION",
        "SSH_TTY",
        "SSH_ORIGINAL_COMMAND",
        "HC_SESSION",
        "HC_TERMINAL_ID",
        "HC_TERMINAL_SOCKET",
        "HC_SOCKET",
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TERMINAL_ID",
        "HERDR_SESSION",
    ] {
        cmd.env_remove(key);
    }
    if !config.herdr_config.is_empty() {
        cmd.env("HERDR_CONFIG_PATH", &config.herdr_config);
    }
    cmd.kill_on_drop(true);
}

pub fn command(config: &Config) -> Command {
    let mut cmd = Command::new(&config.herdr);
    cmd.args(["--session", &config.herdr_session]);
    command_environment(config, &mut cmd);
    cmd
}

fn server_command(config: &Config) -> Command {
    // The background server needs no interactive shell initialization. Native
    // Agent launches still use the login shell to load their PATH and settings.
    let mut cmd = command(config);
    cmd.arg("server");
    cmd
}

pub fn socket_path(config: &Config) -> Result<PathBuf> {
    let root = if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path)
    } else {
        directories::BaseDirs::new()
            .context("Home directory is unavailable")?
            .home_dir()
            .join(".config")
    };
    let root = root.join("herdr");
    if config.herdr_session == "default" {
        Ok(root.join("herdr.sock"))
    } else {
        Ok(root
            .join("sessions")
            .join(&config.herdr_session)
            .join("herdr.sock"))
    }
}

/// Only a request/reply exchange is retried by callers; input ambiguity is retained.
pub async fn rpc(config: &Config, method: &str, params: Value) -> Result<Value> {
    let path = socket_path(config)?;
    let id = crate::db::id();
    tokio::time::timeout(Duration::from_secs(12), async {
        let mut stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("Cannot connect to HERDR session '{}'", config.herdr_session))
            .map_err(NotSubmitted)?;
        let bytes = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        stream.write_all(&bytes).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        let mut lines = BufReader::new(stream).lines();
        while let Some(line) = lines.next_line().await? {
            let response: Value =
                serde_json::from_str(&line).context("HERDR returned invalid JSON")?;
            if response["id"] != id {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(RpcFailure {
                    kind: error["code"].as_str().unwrap_or("error").into(),
                    message: error["message"].as_str().unwrap_or("Request failed").into(),
                    hint: String::new(),
                }
                .into());
            }
            return response
                .get("result")
                .cloned()
                .context("HERDR returned no result");
        }
        bail!("HERDR closed the connection before replying")
    })
    .await
    .context("HERDR request timed out; input delivery may be uncertain")?
}

pub async fn probe(config: &Config) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        command(config).arg("--version").output(),
    )
    .await
    .context("HERDR version check timed out")?
    .context("Install the legacy herdr CLI 0.9.5 to run agents")?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || version != "herdr 0.9.5" {
        bail!("Dauerbot currently supports legacy herdr 0.9.5; found '{version}'");
    }
    let value = rpc(config, "ping", json!({})).await?;
    if value["protocol"].as_u64() != Some(PROTOCOL) || value["version"] != "0.9.5" {
        bail!(
            "The running HERDR server differs from the supported 0.9.5 installation; open the matching session before continuing"
        );
    }
    Ok(value)
}

pub(crate) async fn startup_pending(config: &Config) -> Result<bool> {
    Ok(START_LOCK
        .lock()
        .await
        .get(&socket_path(config)?)
        .is_some_and(|attempt| attempt.child.is_some()))
}

async fn expire_owned_start(attempt: &mut StartAttempt) -> Result<bool> {
    if tokio::time::Instant::now() < attempt.deadline {
        return Ok(false);
    }
    let Some(child) = attempt.child.as_mut() else {
        return Ok(false);
    };
    // Child retains the exact unreaped process identity. Never signal a PID
    // reconstructed from a registry, socket, or another daemon's state.
    if child.try_wait()?.is_none() {
        child.start_kill()?;
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .context("Waiting for the owned unready HERDR starter to exit")??;
    }
    attempt.child = None;
    attempt.retry_at = tokio::time::Instant::now() + Duration::from_secs(5);
    Ok(true)
}

pub async fn ensure_server(config: &Config) -> Result<()> {
    let path = socket_path(config)?;
    let mut attempts = START_LOCK.lock().await;
    match UnixStream::connect(&path).await {
        Ok(_) => {
            // Late readiness wins even after the candidate's deadline. A live
            // foreign socket is never closed or restarted by this path.
            if let Err(error) = probe(config).await {
                if let Some(attempt) = attempts.get_mut(&path) {
                    expire_owned_start(attempt).await?;
                }
                return Err(error);
            }
            if let Some(mut attempt) = attempts.remove(&path)
                && let Some(mut child) = attempt.child.take()
            {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            }
            return Ok(());
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error).context("Cannot inspect the configured HERDR socket"),
    }
    if let Some(attempt) = attempts.get_mut(&path) {
        if expire_owned_start(attempt).await? {
            bail!(
                "The owned unready HERDR starter exceeded its deadline; retrying after bounded backoff"
            );
        }
        if let Some(child) = attempt.child.as_mut()
            && child.try_wait()?.is_none()
        {
            bail!(
                "The owned HERDR startup is still running; readiness will be rechecked without launching a second server"
            );
        }
        if tokio::time::Instant::now() < attempt.retry_at {
            bail!("Retrying the missing HERDR runtime after a bounded startup backoff");
        }
    }
    attempts.insert(
        path.clone(),
        StartAttempt {
            retry_at: tokio::time::Instant::now() + Duration::from_secs(15),
            deadline: tokio::time::Instant::now() + Duration::from_secs(30),
            child: None,
        },
    );
    // Only create a missing server; never restart an existing session.
    let version = tokio::time::timeout(
        Duration::from_secs(5),
        command(config).arg("--version").output(),
    )
    .await??;
    if !version.status.success() || String::from_utf8_lossy(&version.stdout).trim() != "herdr 0.9.5"
    {
        bail!("Install legacy herdr 0.9.5 before starting this project");
    }
    let mut child = server_command(config)
        .kill_on_drop(false)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Cannot start the HERDR background server")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(value) = rpc(config, "ping", json!({})).await {
            if value["protocol"].as_u64() != Some(PROTOCOL) || value["version"] != "0.9.5" {
                attempts.get_mut(&path).expect("recorded startup").child = Some(child);
                bail!(
                    "HERDR server version or protocol differs from the supported installation; the owned startup remains supervised"
                );
            }
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            // Another project may have started the shared server concurrently.
            if probe(config).await.is_ok() {
                return Ok(());
            }
            bail!(
                "HERDR server exited during startup ({status}); open its session to inspect the error"
            );
        }
        if tokio::time::Instant::now() >= deadline {
            // Keep the exact owned startup handle. A slow candidate must not
            // cause concurrent projects or future sweeps to spawn duplicates.
            attempts.get_mut(&path).expect("recorded startup").child = Some(child);
            bail!(
                "HERDR startup has not reached readiness; the same owned process will be reobserved"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Full UUID encoded in base36 fits HERDR's 32-character managed-name limit.
pub fn managed_name(generation: &str) -> Result<String> {
    let mut number = uuid::Uuid::parse_str(generation)?.as_u128();
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(alphabet[(number % 36) as usize]);
        number /= 36;
        if number == 0 {
            break;
        }
    }
    digits.reverse();
    Ok(format!("db-{}", String::from_utf8(digits)?))
}

/// Probe through the same login shell and cwd used by the actual native launch.
/// Older Codex builds lack no-daemon; their normal PTY is already dedicated.
/// Advertised hook-trust bypass applies the user's automatic permission choice
/// to the managed native hook without writing global hook-trust settings.
async fn compatible_launch_args(
    config: &Config,
    p: &Project,
    argv: &[String],
) -> Result<Vec<String>> {
    let mut effective = argv.to_vec();
    if p.agent != "codex" {
        return Ok(effective);
    }
    let probe = login_shell_args(vec!["codex".into(), "--help".into()]);
    let mut cmd = Command::new(&probe[0]);
    cmd.args(&probe[1..]).current_dir(&p.path);
    command_environment(config, &mut cmd);
    let output = tokio::time::timeout(Duration::from_secs(5), cmd.output())
        .await
        .context("Codex capability check timed out; check codex --help in your terminal")?
        .context("Cannot inspect the Codex CLI selected by your login shell")?;
    if !output.status.success() {
        bail!(
            "The selected Codex CLI could not show its capabilities; run codex --help in your terminal"
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for flag in ["--no-daemon", "--dangerously-bypass-hook-trust"] {
        if (stdout.contains(flag) || stderr.contains(flag))
            && !effective.iter().any(|arg| arg == flag)
        {
            effective.insert(1, flag.into());
        }
    }
    Ok(effective)
}

/// Identify the user's selected Pi without modifying its configuration.
pub async fn pi_version(config: &Config, p: &Project) -> Result<String> {
    let probe = login_shell_args(vec!["pi".into(), "--version".into()]);
    let mut cmd = Command::new(&probe[0]);
    cmd.args(&probe[1..]).current_dir(&p.path);
    command_environment(config, &mut cmd);
    let output = tokio::time::timeout(Duration::from_secs(5), cmd.output())
        .await
        .context("Pi version check timed out")??;
    ensure!(
        output.status.success(),
        "Cannot identify the selected Pi runtime"
    );
    let version = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok(version)
}

/// Create the identity before launch so an early MCP registration can validate it.
pub async fn prepare_execution(
    config: &Config,
    p: &Project,
    argv: &[String],
    generation: &str,
) -> Result<Value> {
    ensure_server(config).await?;
    ensure!(!argv.is_empty(), "The Agent launch plan is empty");
    let argv = compatible_launch_args(config, p, argv).await?;
    let argv = execution_args(config, p.agent != "custom", argv)?;
    prepare_gated_execution(config, p, argv, generation).await
}

/// Establish the PTY identity without adding legacy CLI compatibility flags
/// or writing any native CLI or MCP settings.
pub(crate) async fn prepare_plain_worker(
    config: &Config,
    p: &Project,
    argv: &[String],
    generation: &str,
) -> Result<Value> {
    ensure!(!argv.is_empty(), "The Agent launch plan is empty");
    ensure_server(config).await?;
    let argv = if p.agent == "custom" {
        argv.to_vec()
    } else {
        login_shell_args(argv.to_vec())
    };
    prepare_gated_execution(config, p, argv, generation).await
}

async fn prepare_gated_execution(
    config: &Config,
    p: &Project,
    argv: Vec<String>,
    generation: &str,
) -> Result<Value> {
    let permit_root = if config.herdr_config.is_empty() {
        socket_path(config)?
            .parent()
            .context("HERDR session directory is unavailable")?
            .to_path_buf()
    } else {
        PathBuf::from(&config.herdr_config)
            .parent()
            .context("HERDR configuration directory is unavailable")?
            .to_path_buf()
    };
    let permit_dir = permit_root.join("sessions").join(&p.id).join(generation);
    crate::config::private_dir(&permit_dir)?;
    let permit = permit_dir.join("launch.permit");
    if permit.exists() {
        bail!("This execution generation already has a launch permit");
    }
    let workspace = rpc(
        config,
        "workspace.create",
        json!({"cwd":p.path,"label":format!("Dauerbot · {}",p.name),"focus":false}),
    )
    .await?;
    // The gate is the actual PTY root. Once permitted, exec keeps its PID while
    // changing into the login wrapper and then the Agent. A crash before the
    // durable identity is saved times out instead of starting an orphan Agent.
    let script = r#"count=0; while [ ! -f "$1" ]; do count=$((count+1)); [ "$count" -lt 300 ] || exit 75; sleep 0.1; done; rm -f "$1"; shift; exec "$@""#;
    let mut command = vec![
        "/bin/sh".to_owned(),
        "-c".into(),
        script.into(),
        "dauerbot-launch".into(),
        permit.to_string_lossy().into_owned(),
    ];
    command.extend(argv);
    let result: Result<Value> = async {
    // Add a command tab rather than replacing the initial tab: a user may have
    // split or added a pane since workspace.create returned.
    let layout=rpc(config,"layout.apply",json!({"workspace_id":workspace["workspace"]["workspace_id"],"focus":false,"tab_label":p.name,
        "root":{"type":"pane","cwd":p.path,"command":command,"env":launch_network_env(std::env::vars()),"label":p.name}})).await?;
    let pane_id = layout["layout"]["root"]["pane_id"]
        .as_str()
        .context("HERDR returned no command pane identity")?;
    let pane = rpc(config, "pane.get", json!({"pane_id":pane_id})).await?;
    let pane = &pane["pane"];
    let name = managed_name(generation)?;
    rpc(config,"pane.report_metadata",json!({"pane_id":pane_id,"source":"dauerbot","tokens":{
        "hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":generation,"hfb_execution":name
    }})).await?;
    let info = rpc(config, "pane.process_info", json!({"pane_id":pane_id})).await?;
    let execution = ExecutionRef {
        backend: "legacy".into(),
        workspace_id: pane["workspace_id"]
            .as_str()
            .context("HERDR returned no workspace")?
            .into(),
        pane_id: pane_id.into(),
        terminal_id: pane["terminal_id"]
            .as_str()
            .context("HERDR returned no terminal")?
            .into(),
        managed_name: name,
        generation: generation.into(),
        shell_pid: Some(
            info["process_info"]["shell_pid"]
                .as_i64()
                .context("HERDR command has no root process identity")?,
        ),
        launch_permit: permit.clone(),
    };
    // Closing only the original terminal preserves any panes added by the user.
    close_matching_pane(config, &workspace["root_pane"]).await?;
    Ok(serde_json::to_value(execution)?)
    }.await;
    match result {
        Ok(execution) => Ok(execution),
        Err(error) => {
            // layout.apply can fail after creating its PTY. Recover only the pane
            // whose root argv contains this private generation's gate capability.
            match cleanup_preparation(config, &workspace, &permit).await {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("Execution cleanup failed: {cleanup:#}")))
                }
            }
        }
    }
}

async fn close_matching_pane(config: &Config, expected: &Value) -> Result<()> {
    let pane_id = expected["pane_id"]
        .as_str()
        .context("Missing cleanup pane identity")?;
    let current = match rpc(config, "pane.get", json!({"pane_id":pane_id})).await {
        Ok(value) => value["pane"].clone(),
        Err(error) if missing_pane(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    if current["terminal_id"] != expected["terminal_id"]
        || current["workspace_id"] != expected["workspace_id"]
    {
        bail!("HERDR cleanup identity changed; the pane was preserved");
    }
    rpc(config, "pane.close", json!({"pane_id":pane_id})).await?;
    Ok(())
}

fn missing_pane(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RpcFailure>().is_some_and(|e| {
        matches!(
            e.kind.as_str(),
            "pane_not_found" | "target_not_found" | "workspace_not_found"
        )
    })
}

async fn cleanup_preparation(config: &Config, workspace: &Value, permit: &Path) -> Result<()> {
    let panes = rpc(
        config,
        "pane.list",
        json!({"workspace_id":workspace["workspace"]["workspace_id"]}),
    )
    .await;
    match panes {
        Ok(value) => {
            for pane in value["panes"]
                .as_array()
                .context("Missing cleanup pane list")?
            {
                let info = rpc(
                    config,
                    "pane.process_info",
                    json!({"pane_id":pane["pane_id"]}),
                )
                .await?;
                let owns_gate = info["process_info"]["foreground_processes"]
                    .as_array()
                    .is_some_and(|processes| {
                        processes.iter().any(|process| {
                            process["argv"].as_array().is_some_and(|args| {
                                args.iter().any(|arg| arg.as_str() == permit.to_str())
                            })
                        })
                    });
                let owned_generation = permit
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str());
                let owned_project = permit
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str());
                let owns_metadata = pane["tokens"]["hfb_owner"] == "dauerbot"
                    && pane["tokens"]["hfb_generation"].as_str() == owned_generation
                    && pane["tokens"]["hfb_project"].as_str() == owned_project;
                if owns_gate || owns_metadata {
                    close_matching_pane(config, pane).await?;
                }
            }
        }
        Err(error) if missing_pane(&error) => return Ok(()),
        Err(error) => return Err(error),
    }
    close_matching_pane(config, &workspace["root_pane"]).await?;
    Ok(())
}

/// Save the execution before releasing this gate so the host can verify its identity.
pub async fn launch_execution(_config: &Config, p: &Project, _argv: &[String]) -> Result<()> {
    let execution = ExecutionRef::from_project(p)?;
    crate::config::write_private(&execution.launch_permit, p.generation.as_bytes())?;
    Ok(())
}

// Pane addresses are reusable after a HERDR restart. Only a complete census
// of immutable terminals, plus definite process death, can prove disappearance.
async fn execution_registry(
    config: &Config,
    p: &Project,
    terminal_id: &str,
    managed_name: Option<&str>,
    unknown_pane_address: Option<&str>,
) -> Result<Option<BTreeMap<String, Value>>> {
    let list = rpc(config, "workspace.list", json!({})).await?;
    let workspaces = list["workspaces"]
        .as_array()
        .context("Missing complete HERDR workspace registry")?;
    let mut registry = BTreeMap::new();
    for workspace in workspaces {
        let id = workspace["workspace_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Invalid workspace registry identity")?;
        let count = workspace["pane_count"]
            .as_u64()
            .context("Missing workspace pane count")?;
        let listed = rpc(config, "pane.list", json!({"workspace_id":id})).await?;
        let panes = listed["panes"]
            .as_array()
            .context("Missing complete workspace pane registry")?;
        ensure!(
            panes.len() as u64 == count,
            "HERDR registry changed during disappearance verification"
        );
        ensure!(
            registry
                .insert(format!("workspace:{id}"), json!(count))
                .is_none(),
            "Duplicate workspace registry identity"
        );
        for pane in panes {
            let pane_id = pane["pane_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Invalid pane registry identity")?;
            let terminal = pane["terminal_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Invalid immutable terminal identity")?;
            ensure!(
                pane["workspace_id"] == id,
                "Incomplete HERDR workspace registry"
            );
            if terminal == terminal_id
                || unknown_pane_address.is_some_and(|address| pane_id == address || pane_id == terminal_id)
                || (!p.generation.is_empty() && pane["tokens"]["hfb_generation"] == p.generation)
                || managed_name.is_some_and(|name| pane["tokens"]["hfb_execution"] == name)
            {
                return Ok(None);
            }
            ensure!(
                registry
                    .insert(
                        format!("terminal:{terminal}"),
                        json!({"pane":pane_id,"workspace":id,"tokens":pane["tokens"]})
                    )
                    .is_none(),
                "Duplicate immutable terminal identity"
            );
        }
    }
    Ok(Some(registry))
}
async fn vanished_execution(
    config: &Config,
    p: &Project,
    execution: &ExecutionRef,
) -> Result<bool> {
    if !root_has_exited(execution.shell_pid) {
        return Ok(false);
    }
    let preboot = execution_predates_boot(&p.execution, host_boot().as_ref());
    let children = crate::native_children::observe(p, crate::db::now());
    if !preboot
        && (children["state"] == "active"
            || (p.execution["child_adapter"].is_string() && children["state"] != "quiescent"))
    {
        return Ok(false);
    }
    let name = Some(execution.managed_name.as_str());
    let Some(first) = execution_registry(config, p, &execution.terminal_id, name, None).await? else {
        return Ok(false);
    };
    let Some(second) = execution_registry(config, p, &execution.terminal_id, name, None).await? else {
        return Ok(false);
    };
    ensure!(
        first == second,
        "HERDR registry changed during disappearance verification"
    );
    // PID reuse, permission errors, and missing managed child evidence retain
    // the old fence; no signal is sent to either the old or replacement pane.
    let children = crate::native_children::observe(p, crate::db::now());
    let preboot = preboot && execution_predates_boot(&p.execution, host_boot().as_ref());
    Ok(root_has_exited(execution.shell_pid)
        && (preboot
            || (children["state"] != "active"
                && (!p.execution["child_adapter"].is_string()
                    || children["state"] == "quiescent"))))
}

pub(crate) async fn retire_for_update(config: &Config, p: &Project, switching: bool) -> Result<()> {
    if let Ok(execution) = ExecutionRef::from_project(p) {
        let record = observe(config, p).await?;
        if record["vanished_execution"] == true {
            return Ok(());
        }
        if record["state"] == "finished" {
            stop(config, p).await?;
        } else {
            check_pane_ownership(p, &record)?;
            ensure!(
                !switching || recovery_foreground_quiescent(p, &record),
                "Waiting for the CLI transition's foreground work before recovering the Agent"
            );
            close_matching_pane(config, &record).await?;
        }
        ensure!(
            vanished_execution(config, p, &execution).await?,
            "Waiting to verify that the old Agent has stopped"
        );
        return Ok(());
    }
    let terminal = p.terminal.as_deref().filter(|id| !id.is_empty())
        .context("No immutable terminal identity is available")?;
    ensure!(
        p.execution.get("terminal_id").is_none_or(|id| id == terminal)
            && p.execution.get("generation").is_none_or(|id| id == &p.generation)
            && p.execution.get("backend").is_none_or(|backend| backend == "legacy"),
        "Saved HERDR identities disagree"
    );
    let root_absent = || p.terminal_pid.is_none_or(|pid| root_has_exited(Some(pid)))
        && p.execution.get("shell_pid").is_none_or(|pid| pid.is_null() || root_has_exited(pid.as_i64()));
    let children_absent = || {
        let children = crate::native_children::observe(p, crate::db::now());
        children["state"] != "active"
            && (!p.execution["child_adapter"].is_string() || children["state"] == "quiescent")
    };
    let name = p.execution["managed_name"].as_str().filter(|name| !name.is_empty());
    ensure!(root_absent() && children_absent(), "Saved Agent processes may still be running");
    let address = p.execution["pane_id"].as_str().or(Some(terminal));
    let first = execution_registry(config, p, terminal, name, address).await?;
    let second = execution_registry(config, p, terminal, name, address).await?;
    ensure!(
        first.is_some() && first == second && root_absent() && children_absent(),
        "HERDR could not verify that the saved execution is absent"
    );
    Ok(())
}
fn vanished_record(execution: &ExecutionRef) -> Value {
    json!({"state":"finished","vanished_execution":true,"execution":execution})
}

pub async fn observe(config: &Config, p: &Project) -> Result<Value> {
    let execution = ExecutionRef::from_project(p)?;
    let pane = match rpc(config, "pane.get", json!({"pane_id":execution.pane_id})).await {
        Ok(v) => v["pane"].clone(),
        Err(error)
            if error.downcast_ref::<RpcFailure>().is_some_and(|e| {
                matches!(e.kind.as_str(), "pane_not_found" | "target_not_found")
            }) =>
        {
            ensure!(
                vanished_execution(config, p, &execution).await?,
                "The saved HERDR pane is missing, but execution disappearance is not verified"
            );
            return Ok(vanished_record(&execution));
        }
        Err(error) => return Err(error),
    };
    if (pane["terminal_id"] != execution.terminal_id
        || pane["workspace_id"] != execution.workspace_id)
        && vanished_execution(config, p, &execution).await?
    {
        return Ok(vanished_record(&execution));
    }
    let native_agent = match rpc(config, "agent.get", json!({"target":execution.pane_id})).await {
        Ok(v) => v["agent"].clone(),
        Err(error)
            if error
                .downcast_ref::<RpcFailure>()
                .is_some_and(|e| e.kind == "agent_not_found") =>
        {
            pane.clone()
        }
        Err(error) => return Err(error),
    };
    let info = match rpc(
        config,
        "pane.process_info",
        json!({"pane_id":execution.pane_id}),
    )
    .await
    {
        Ok(value) => value,
        Err(error)
            if error.downcast_ref::<RpcFailure>().is_some_and(|e| {
                matches!(e.kind.as_str(), "pane_not_found" | "target_not_found")
            }) =>
        {
            ensure!(
                vanished_execution(config, p, &execution).await?,
                "The saved HERDR pane is missing, but execution disappearance is not verified"
            );
            return Ok(vanished_record(&execution));
        }
        Err(error) => return Err(error),
    };
    let info = &info["process_info"];
    let processes = info["foreground_processes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let foreground = processes
        .iter()
        .find(|v| !is_shell(v))
        .or_else(|| processes.first())
        .cloned()
        .unwrap_or(Value::Null);
    let agent_label = native_agent["agent"].as_str().unwrap_or("");
    // A transient empty foreground lookup is not proof of PTY exit. In
    // particular, native executable wrappers can disappear briefly at startup.
    let no_foreground = processes.is_empty();
    let pending = no_foreground
        || (!p.ready && ((p.agent != "custom" && agent_label.is_empty()) || is_shell(&foreground)));
    let state = if no_foreground && root_has_exited(execution.shell_pid) {
        "finished"
    } else {
        "live"
    };
    Ok(
        json!({"state":state,"pid":info["shell_pid"],"argv":foreground["argv"],"foreground":foreground,
        "status":{"state":native_agent["agent_status"].as_str().unwrap_or("unknown"),"app":agent_label},
        "native_session":native_agent.get("agent_session").cloned().unwrap_or(Value::Null),
        "interactive_ready":native_agent["interactive_ready"].as_bool().unwrap_or(!pending),
        "launch_pending":pending,"completion_seq":native_agent["completion_seq"],"state_change_seq":native_agent["state_change_seq"],
        "children":crate::native_children::observe(&crate::repair_lane::child_view(p), crate::db::now()),"processes":processes,"execution":execution,"agent":native_agent,"pane_id":pane["pane_id"],"terminal_id":pane["terminal_id"],"workspace_id":pane["workspace_id"],"tokens":pane["tokens"],
        "cwd":pane["foreground_cwd"].as_str().or(pane["cwd"].as_str()).unwrap_or("")}),
    )
}

fn root_has_exited(pid: Option<i64>) -> bool {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        let Some(pid) = pid
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
        else {
            return false;
        };
        // Signal zero does not modify the process. ESRCH is 3 on both supported
        // Unix targets (macOS and Linux); EPERM and all unknown errors mean alive
        // or unknown, so they preserve the interactive pane.
        unsafe { kill(pid, 0) == -1 && std::io::Error::last_os_error().raw_os_error() == Some(3) }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn is_shell(process: &Value) -> bool {
    let name = process["name"]
        .as_str()
        .or(process["argv0"].as_str())
        .unwrap_or("");
    let name = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .trim_start_matches('-');
    matches!(
        name,
        "sh" | "bash" | "zsh" | "fish" | "nu" | "pwsh" | "dash" | "login"
    )
}

/// Raw pane ownership authorizes inspection/closure even when native Agent
/// detection is stale. It never authorizes sending input to that detection.
pub fn check_pane_ownership(p: &Project, record: &Value) -> Result<()> {
    let execution = ExecutionRef::from_project(p)?;
    anyhow::ensure!(record["state"] == "live", "The main Agent has exited");
    anyhow::ensure!(
        record["pane_id"] == execution.pane_id
            && record["terminal_id"] == execution.terminal_id
            && record["workspace_id"] == execution.workspace_id
            && (execution.shell_pid.is_none() || record["pid"].as_i64() == execution.shell_pid),
        "The owned HERDR pane identity changed; the pane was preserved"
    );
    anyhow::ensure!(
        record["tokens"]["hfb_owner"] == "dauerbot"
            && record["tokens"]["hfb_project"] == p.id
            && record["tokens"]["hfb_generation"] == p.generation
            && record["tokens"]["hfb_execution"] == execution.managed_name,
        "The owned HERDR pane generation changed; the pane was preserved"
    );
    Ok(())
}

pub fn check_execution_identity(p: &Project, record: &Value) -> Result<()> {
    let execution = ExecutionRef::from_project(p)?;
    if record["state"] != "live" {
        bail!("The main Agent has exited");
    }
    let observed: ExecutionRef = serde_json::from_value(record["execution"].clone())?;
    if observed.generation != execution.generation
        || observed.terminal_id != execution.terminal_id
        || observed.pane_id != execution.pane_id
        || observed.workspace_id != execution.workspace_id
        || record["agent"]["terminal_id"] != execution.terminal_id
        || record["agent"]["pane_id"] != execution.pane_id
        || record["agent"]["workspace_id"] != execution.workspace_id
        || (execution.shell_pid.is_some() && record["pid"].as_i64() != execution.shell_pid)
    {
        bail!("HERDR execution identity changed; automatic input was stopped");
    }
    if record["tokens"]["hfb_owner"] != "dauerbot"
        || record["tokens"]["hfb_project"] != p.id
        || record["tokens"]["hfb_generation"] != p.generation
        || record["tokens"]["hfb_execution"] != execution.managed_name
    {
        bail!("HERDR execution generation metadata changed; automatic input was stopped");
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct NativeSessionChanged;
impl std::fmt::Display for NativeSessionChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The resumed CLI reported a different native conversation; saved continuity and pending input remain held")
    }
}
impl std::error::Error for NativeSessionChanged {}
pub(crate) fn same_native_session(agent: &str, expected: &str, actual: &str) -> bool {
    expected == actual
        || (agent == "pi"
            && Path::new(expected).is_absolute()
            && Path::new(actual).is_absolute()
            && std::fs::metadata(expected).is_ok_and(|m| m.is_file())
            && std::fs::metadata(actual).is_ok_and(|m| m.is_file())
            && std::fs::canonicalize(expected)
                .ok()
                .zip(std::fs::canonicalize(actual).ok())
                .is_some_and(|(a, b)| a == b))
}
pub(crate) fn intended_native_resume(p: &Project, resume: bool) -> Option<String> {
    (resume && !p.native_session.is_empty() && capabilities(&p.agent)["native_resume"] == true)
        .then(|| p.native_session.clone())
}
pub(crate) fn check_native_resume(p: &Project, record: &Value) -> Result<()> {
    if let (Some(expected), Some(actual)) = (
        p.execution["intended_native_session"]
            .as_str()
            .filter(|s| !s.is_empty()),
        record["native_session"]["value"]
            .as_str()
            .filter(|s| !s.is_empty()),
    ) && !same_native_session(&p.agent, expected, actual)
    {
        return Err(NativeSessionChanged.into());
    }
    Ok(())
}

pub fn check_identity(p: &Project, record: &Value) -> Result<()> {
    check_execution_identity(p, record)?;
    check_native_resume(p, record)?;
    if p.execution["intended_native_session"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
        && record["native_session"]["value"]
            .as_str()
            .is_none_or(|s| s.is_empty())
    {
        return Err(SessionStarting.into());
    }
    if record["launch_pending"] == true {
        return Err(SessionStarting.into());
    }
    if is_shell(&record["foreground"]) {
        bail!("The Agent returned to a shell; automatic input was stopped");
    }
    if p.agent != "custom" {
        let actual = record["agent"]["agent"].as_str().unwrap_or("");
        let expected = if p.agent == "agy" {
            "antigravity"
        } else if p.agent == "claude" {
            "claude"
        } else {
            &p.agent
        };
        if actual != expected && !(p.agent == "agy" && actual == "agy") {
            bail!("The foreground Agent differs from the selected CLI");
        }
    }
    Ok(())
}

fn same_program(actual: &str, expected: &str) -> bool {
    actual == expected
        || (Path::new(actual).is_absolute()
            && Path::new(expected).is_absolute()
            && std::fs::canonicalize(actual)
                .ok()
                .zip(std::fs::canonicalize(expected).ok())
                .is_some_and(|(a, b)| a == b))
}

fn is_claude_sleep_inhibitor(process: &Value) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        #[link(name = "proc")]
        unsafe extern "C" {
            fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
        }
        let Some(args) = process["argv"].as_array() else {
            return false;
        };
        if args.len() != 4
            || args[1] != "-i"
            || args[2] != "-t"
            || args[3]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .is_none_or(|seconds| seconds == 0)
        {
            return false;
        }
        let Some(pid) = process["pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
        else {
            return false;
        };
        let mut buffer = [0_u8; 4096];
        let length = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if length <= 0 {
            return false;
        }
        let bytes = buffer.split(|byte| *byte == 0).next().unwrap_or_default();
        Path::new(std::ffi::OsStr::from_bytes(bytes)) == Path::new("/usr/bin/caffeinate")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = process;
        false
    }
}

/// Retirement requires the CLI itself in the foreground, not a tool subprocess
/// beneath a stale HERDR agent label. Unrecognized launch wrappers stay warm.
pub fn quiescent(p: &Project, record: &Value) -> bool {
    record["children"]["state"] == "quiescent" && foreground_quiescent(p, record)
}

/// Necessary foreground checks; these do not establish native child coverage.
pub fn foreground_quiescent(p: &Project, record: &Value) -> bool {
    if check_identity(p, record).is_err()
        || !matches!(record["status"]["state"].as_str(), Some("idle" | "done"))
    {
        return false;
    }
    foreground_processes_match(p, record)
}

/// Recovery can close an owned CLI whose detector is blocked or stale, but a
/// foreground tool still prevents closure. This never authorizes native input.
pub fn recovery_foreground_quiescent(p: &Project, record: &Value) -> bool {
    check_pane_ownership(p, record).is_ok()
        && record["status"]["state"] != "working"
        && record["children"]["state"] != "active"
        && foreground_processes_match(p, record)
}

fn foreground_processes_match(p: &Project, record: &Value) -> bool {
    let Some(processes) = record["processes"].as_array().filter(|v| !v.is_empty()) else {
        return false;
    };
    processes.iter().all(|process| {
        let args = process["argv"].as_array();
        let name = process["name"].as_str().unwrap_or("");
        // Our idle stdio MCP child shares the PTY process group but is not a
        // background Agent job. Match its full generation-scoped credential path.
        if args.is_some_and(|argv| {
            let credential = ExecutionRef::from_project(p)
                .ok()
                .map(|execution| execution.launch_permit.with_file_name("credentials.json"));
            let managed_dir = credential.as_ref().and_then(|path| path.ancestors().nth(4));
            let generation_entry = argv.len() == 4
                && argv[1] == "mcp"
                && argv[2] == "--credentials"
                && argv[3].as_str().map(Path::new) == credential.as_deref();
            let managed_entry = argv.len() == 4
                && argv[1] == "--data-dir"
                && argv[2].as_str().map(Path::new) == managed_dir
                && argv[3] == "mcp";
            (generation_entry || managed_entry)
                && argv[0].as_str().is_some_and(|program| {
                    same_program(
                        program,
                        p.execution["companion"].as_str().unwrap_or(
                            &std::env::current_exe()
                                .unwrap_or_default()
                                .to_string_lossy(),
                        ),
                    )
                })
        }) {
            return true;
        }
        if p.agent == "custom" {
            let literal_prefix: Vec<_> = p
                .extra_args
                .iter()
                .take_while(|arg| !arg.contains('{'))
                .collect();
            let python = |program: &str| {
                Path::new(program)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.to_ascii_lowercase().starts_with("python"))
            };
            // macOS can expose only the framework Python process name, omitting
            // KERN_PROCARGS2 entirely. The direct-command root PID is immutable;
            // only this configured interpreter root can use the fallback.
            if args.is_none()
                && literal_prefix.len() > 1
                && process["pid"] == record["pid"]
                && python(name)
                && python(literal_prefix[0])
            {
                return true;
            }
            return !literal_prefix.is_empty()
                && args.is_some_and(|argv| {
                    literal_prefix.iter().enumerate().all(|(index, arg)| {
                        argv.get(index)
                            .and_then(Value::as_str)
                            .is_some_and(|actual| {
                                if index == 0 {
                                    same_program(actual, arg)
                                        || (literal_prefix.len() > 1
                                            && process["pid"] == record["pid"]
                                            && python(actual)
                                            && python(arg))
                                } else {
                                    actual == arg.as_str()
                                }
                            })
                    })
                });
        }
        let executable = Path::new(name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name);
        if p.agent == "claude" && executable == "caffeinate" && is_claude_sleep_inhibitor(process) {
            return true;
        }
        let expected = |name: &str| {
            let name = name.to_ascii_lowercase();
            match p.agent.as_str() {
                "codex" => name == "codex" || name.starts_with("codex-"),
                "claude" => matches!(name.as_str(), "claude" | "claude.exe" | "claude-code"),
                "pi" => name == "pi",
                "agy" => matches!(name.as_str(), "agy" | "antigravity" | "antigravity-cli"),
                _ => false,
            }
        };
        if expected(executable)
            || process["argv0"]
                .as_str()
                .and_then(|arg| Path::new(arg).file_name())
                .and_then(|name| name.to_str())
                .is_some_and(expected)
        {
            return true;
        }
        if !matches!(executable, "node" | "bun" | "deno") {
            return false;
        }
        args.and_then(|argv| argv.get(1))
            .and_then(Value::as_str)
            .is_some_and(|script| {
                let script_name = Path::new(script)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                expected(script_name)
                    || match p.agent.as_str() {
                        "claude" => script.contains("/@anthropic-ai/claude-code/"),
                        "pi" => script.contains("/pi-coding-agent/"),
                        "agy" => script.contains("/antigravity-cli/"),
                        "codex" => script.contains("/@openai/codex/"),
                        _ => false,
                    }
            })
    })
}

pub async fn send(config: &Config, p: &Project, text: &str) -> Result<Value> {
    let preflight: Result<(ExecutionRef, Value)> = async {
        let record = observe(config, p).await?;
        check_identity(p, &record)?;
        anyhow::ensure!(
            record["status"]["state"] != "blocked",
            "The Agent is waiting for interactive input"
        );
        Ok((
            ExecutionRef::from_project(p)?,
            record["state_change_seq"].clone(),
        ))
    }
    .await;
    let (execution, sequence) = preflight.map_err(NotSubmitted)?;
    if p.agent == "custom" {
        rpc(
            config,
            "pane.send_input",
            json!({"pane_id":execution.pane_id,"text":text,"keys":["Enter"]}),
        )
        .await?;
    } else {
        rpc(
            config,
            "agent.prompt",
            json!({"target":execution.pane_id,"text":text}),
        )
        .await?;
    }
    Ok(json!({"submitted":true,"started":true,"native_seq":sequence}))
}

pub async fn stop(config: &Config, p: &Project) -> Result<()> {
    let record = observe(config, p).await?;
    if record["vanished_execution"] == true {
        // observe verified the immutable old terminal is gone. Its reusable
        // address may belong to a user shell now; there is nothing to close.
        return Ok(());
    }
    if record["state"] == "finished" {
        let execution = ExecutionRef::from_project(p)?;
        // Exited direct-command panes remain in HERDR. Their immutable terminal
        // identity and generation tokens still establish ownership without a PID.
        let pane = match rpc(config, "pane.get", json!({"pane_id":execution.pane_id})).await {
            Ok(value) => value["pane"].clone(),
            Err(error) if missing_pane(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        if pane["tokens"]["hfb_owner"] != "dauerbot"
            || pane["tokens"]["hfb_project"] != p.id
            || pane["tokens"]["hfb_generation"] != p.generation
            || pane["tokens"]["hfb_execution"] != execution.managed_name
        {
            bail!("HERDR exited execution metadata changed; the pane was preserved");
        }
        return close_matching_pane(
            config,
            &json!({"pane_id":execution.pane_id,
            "terminal_id":execution.terminal_id,"workspace_id":execution.workspace_id}),
        )
        .await;
    }
    match check_pane_ownership(p, &record) {
        Ok(()) => {}
        Err(error) if error.is::<SessionStarting>() => {}
        Err(error) => return Err(error),
    }
    let execution = ExecutionRef::from_project(p)?;
    rpc(config, "pane.close", json!({"pane_id":execution.pane_id})).await?;
    Ok(())
}

pub async fn screen(config: &Config, p: &Project, lines: u32) -> Result<String> {
    let execution = ExecutionRef::from_project(p)?;
    let mut params = json!({"pane_id":execution.pane_id,
        "source":if lines==0 {"visible"} else {"recent"},"strip_ansi":true});
    // HERDR applies `lines` to visible snapshots too. Omit it for the whole
    // viewport so workspace trust recognition can see the path and selection.
    if lines > 0 {
        params["lines"] = json!(lines);
    }
    let value = rpc(config, "pane.read", params).await?;
    value["read"]["text"]
        .as_str()
        .or(value["text"].as_str())
        .map(str::to_owned)
        .context("HERDR returned no terminal text")
}

pub async fn terminal_descriptor(config: &Config, p: &Project) -> Result<Value> {
    let record = observe(config, p).await?;
    if crate::manager_workers::is_plain(p) {
        check_pane_ownership(p, &record)?;
        check_native_resume(p, &record)?;
    } else {
        check_execution_identity(p, &record)?;
    }
    let execution = ExecutionRef::from_project(p)?;
    let mut public_execution = serde_json::to_value(&execution)?;
    public_execution
        .as_object_mut()
        .context("Invalid execution descriptor")?
        .remove("launch_permit");
    Ok(
        json!({"backend":"legacy","program":config.herdr,"herdr":config.herdr,"session":config.herdr_session,"herdr_session":config.herdr_session,
        "terminal_id":execution.terminal_id,"terminal":execution.terminal_id,"pane_id":execution.pane_id,"generation":execution.generation,
        "project_id":p.id,"execution":public_execution,"can_control":true,"config_path":config.herdr_config}),
    )
}

pub async fn subscribe(config: &Config) -> Result<tokio::io::Lines<BufReader<UnixStream>>> {
    let mut stream = UnixStream::connect(socket_path(config)?).await?;
    let req = json!({"id":crate::db::id(),"method":"events.subscribe","params":{"subscriptions":[
        {"type":"pane.updated"},{"type":"pane.exited"},{"type":"pane.agent_detected"}
    ]}});
    stream.write_all(format!("{req}\n").as_bytes()).await?;
    stream.flush().await?;
    Ok(BufReader::new(stream).lines())
}

#[derive(Debug)]
pub struct SessionStarting;
impl std::fmt::Display for SessionStarting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The main Agent is still starting")
    }
}
impl std::error::Error for SessionStarting {}

/// Preserve the login environment used by ordinary user terminals.
pub fn login_shell_args(argv: Vec<String>) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut out = vec![
        shell,
        "-lic".into(),
        r#"exec "$@""#.into(),
        "dauerbot-agent".into(),
    ];
    out.extend(argv);
    out
}

/// Apply the resolved runtime after login-shell startup, which may reset PATH.
fn execution_args(config: &Config, login: bool, argv: Vec<String>) -> Result<Vec<String>> {
    let parent = Path::new(&config.herdr)
        .parent()
        .filter(|p| !p.as_os_str().is_empty());
    let mut effective = if let Some(parent) = parent {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"PATH="$1:$PATH"; export PATH; shift; exec "$@""#.into(),
            "dauerbot-runtime".into(),
            parent.to_string_lossy().into_owned(),
        ]
    } else {
        vec![]
    };
    effective.extend(argv);
    Ok(if login {
        login_shell_args(effective)
    } else {
        effective
    })
}

pub fn launch_network_env(
    vars: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    const KEYS: [&str; 8] = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ];
    vars.into_iter()
        .filter(|(k, _)| KEYS.contains(&k.as_str()))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrustSelection {
    Yes,
    No,
}

/// Recognize only the native workspace trust screen already authorized by the user.
pub fn is_project_trust(p: &Project, t: &Value, screen: &str) -> bool {
    project_trust_selection(p, t, screen).is_some()
}

fn project_trust_selection(p: &Project, t: &Value, screen: &str) -> Option<TrustSelection> {
    if p.ready
        || !matches!(p.agent.as_str(), "agy" | "claude")
        || t["cwd"] != p.path
        || t["status"]["state"] != "blocked"
        || check_execution_identity(p, t).is_err()
    {
        return None;
    }
    let lines: Vec<_> = screen.lines().map(str::trim).collect();
    if !lines.contains(&p.path.as_str()) {
        return None;
    }
    match p.agent.as_str() {
        "agy"
            if lines.contains(&"Accessing workspace:")
                && lines.contains(&"Do you trust the contents of this project?")
                && lines.contains(&"> Yes, I trust this folder")
                && lines.contains(&"No, exit") =>
        {
            Some(TrustSelection::Yes)
        }
        "claude"
            if lines.iter().any(|line| {
                line.starts_with(
                    "Quick safety check: Is this a project you created or one you trust?",
                )
            }) =>
        {
            if (lines.contains(&"❯ 1. Yes, I trust this folder") && lines.contains(&"2. No, exit"))
                || (lines.contains(&"❯ Yes, I trust this folder") && lines.contains(&"No, exit"))
            {
                Some(TrustSelection::Yes)
            } else if lines.contains(&"❯ No, exit") && lines.contains(&"Yes, I trust this folder")
            {
                Some(TrustSelection::No)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub async fn confirm_project_trust(config: &Config, p: &Project) -> Result<()> {
    let record = observe(config, p).await?;
    let text = screen(config, p, 0).await?;
    if !is_project_trust(p, &record, &text) {
        bail!("The terminal is not the recognized project trust prompt");
    }
    let execution = ExecutionRef::from_project(p)?;
    let marker = execution
        .launch_permit
        .parent()
        .context("Execution directory is unavailable")?
        .join("trust.attempted");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file=options.open(&marker).context("Workspace trust confirmation was already attempted for this execution; continue in its terminal")?;
    file.sync_all()?;
    // A recorded attempt is never repeated after a transport failure or restart.
    // Recheck the exact screen and generation after committing that decision.
    let latest = observe(config, p).await?;
    let text = screen(config, p, 0).await?;
    let selected = project_trust_selection(p, &latest, &text)
        .context("The workspace trust prompt changed before confirmation")?;
    if selected == TrustSelection::No {
        rpc(
            config,
            "pane.send_keys",
            json!({"pane_id":execution.pane_id,"keys":["Down"]}),
        )
        .await?;
        // Send one navigation key, then prove the affirmative selection appeared.
        // A delayed redraw is allowed, but uncertainty never triggers Enter.
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let latest = observe(config, p).await?;
                let text = screen(config, p, 0).await?;
                match project_trust_selection(p, &latest, &text) {
                    Some(TrustSelection::Yes) => return Ok::<(), anyhow::Error>(()),
                    Some(TrustSelection::No) => tokio::time::sleep(Duration::from_millis(40)).await,
                    None => bail!("The workspace trust prompt changed during navigation"),
                }
            }
        })
        .await
        .context("The workspace trust selection did not update; continue in the terminal")??;
    }
    rpc(
        config,
        "pane.send_keys",
        json!({"pane_id":execution.pane_id,"keys":["Enter"]}),
    )
    .await?;
    Ok(())
}

pub fn capabilities(agent: &str) -> Value {
    json!({"backend":"legacy","interactive_terminal":true,"native_resume":(["codex","claude","pi","agy"].contains(&agent)),
        "native_session_source":"herdr.agent_session","model_confirmation":"unverified","background_quiescence":"managed_child_evidence_required;unsupported_or_incomplete_coverage_retains_session"})
}

fn integration_target(agent: &str) -> Result<&'static str> {
    match agent {
        "codex" => Ok("codex"),
        "claude" => Ok("claude"),
        "pi" => Ok("pi"),
        "agy" => Ok("antigravity_cli"),
        _ => bail!("This custom CLI has no managed HERDR integration"),
    }
}

pub async fn integration_status(config: &Config, agent: &str) -> Result<Value> {
    let target = integration_target(agent)?;
    let list = rpc(config, "integration.list", json!({})).await?;
    list["integrations"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["target"] == target))
        .cloned()
        .context("The installed HERDR release has no integration for this CLI")
}

/// HERDR's installer merges its managed hook entry and preserves unrelated keys.
/// This is called only for an explicit native project launch, never by probes.
pub async fn install_integration(config: &Config, agent: &str) -> Result<Value> {
    let current = integration_status(config, agent).await?;
    if current["state"] == "current" {
        return Ok(current);
    }
    let target = integration_target(agent)?;
    rpc(config, "integration.install", json!({"target":target}))
        .await
        .with_context(|| {
            format!(
                "Cannot install the {agent} session hook; check its native configuration and retry"
            )
        })?;
    let installed = integration_status(config, agent).await?;
    if installed["state"] != "current" {
        bail!("The {agent} session hook is not ready after installation");
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn custom_resume_preserves_explicit_native_argument_without_inventing_native_confirmation()
     {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            herdr_session: dir.path().to_string_lossy().into(),
            ..Config::default()
        };
        let db = crate::db::Db::open(&dir.path().join("db")).unwrap();
        let mut p = db
            .create(crate::db::ProjectInput {
                name: "Custom resume".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "custom".into(),
                chat_id: String::new(),
                native_session: "opaque-custom-conversation".into(),
                extra_args: vec![
                    "custom-agent".into(),
                    "--session".into(),
                    "{native_session}".into(),
                ],
            })
            .await
            .unwrap();
        p.generation = "custom-generation".into();
        p.terminal = Some("custom-terminal".into());
        p.terminal_pid = Some(100);
        p.ready = true;
        p.execution = json!({"backend":"legacy","workspace_id":"w1","pane_id":"w1:p1","terminal_id":"custom-terminal","managed_name":"custom-managed","generation":p.generation,"shell_pid":100,"launch_permit":dir.path().join("permit")});
        if let Some(native) = intended_native_resume(&p, true) {
            p.execution["intended_native_session"] = json!(native);
        }
        let args = crate::adapter::launch_args(
            &p,
            &dir.path().join("mcp.json"),
            &json!({"command":"hfb","args":[]}),
            "context",
            true,
        )
        .unwrap();
        assert_eq!(
            args,
            vec!["custom-agent", "--session", "opaque-custom-conversation"]
        );
        let pane = json!({"pane_id":"w1:p1","workspace_id":"w1","terminal_id":"custom-terminal","agent":"custom-agent","agent_status":"idle","tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":p.generation,"hfb_execution":"custom-managed"}});
        let listener = tokio::net::UnixListener::bind(socket_path(&cfg).unwrap()).unwrap();
        let worker = tokio::spawn(async move {
            for expected in [
                "pane.get",
                "agent.get",
                "pane.process_info",
                "pane.send_input",
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let (input, mut output) = stream.into_split();
                let request: Value = serde_json::from_str(
                    &BufReader::new(input)
                        .lines()
                        .next_line()
                        .await
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(request["method"], expected);
                let result = match expected {
                    "pane.get" => json!({"pane":pane}),
                    "agent.get" => json!({"agent":pane}),
                    "pane.process_info" => {
                        json!({"process_info":{"shell_pid":100,"foreground_processes":[{"pid":100,"name":"custom-agent","argv":["custom-agent"]}]}})
                    }
                    _ => {
                        assert_eq!(request["params"]["text"], "new custom input");
                        assert_eq!(request["params"]["keys"], json!(["Enter"]));
                        json!({})
                    }
                };
                output
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        assert!(send(&cfg, &p, "new custom input").await.unwrap()["submitted"] == true);
        worker.await.unwrap();
        assert!(p.execution.get("native_confirmed").is_none());
        assert!(p.execution.get("intended_native_session").is_none());
        p.agent = "codex".into();
        assert_eq!(
            intended_native_resume(&p, true).as_deref(),
            Some("opaque-custom-conversation")
        );
        assert!(intended_native_resume(&p, false).is_none());
    }
    #[test]
    fn reboot_proof_never_uses_age_to_override_a_known_same_boot() {
        let boot = HostBoot {
            id: uuid::Uuid::new_v4().to_string(),
            started_at: 10_000,
        };
        assert!(!execution_predates_boot(
            &json!({"host_boot_id":boot.id,"launch_started_at":1}),
            Some(&boot)
        ));
        assert!(execution_predates_boot(
            &json!({"host_boot_id":uuid::Uuid::new_v4().to_string()}),
            Some(&boot)
        ));
        assert!(!execution_predates_boot(
            &json!({"host_boot_id":"unknown","launch_started_at":1}),
            Some(&boot)
        ));
        assert!(execution_predates_boot(
            &json!({"launch_started_at":9999}),
            Some(&boot)
        ));
        assert!(!execution_predates_boot(
            &json!({"launch_started_at":10000}),
            Some(&boot)
        ));
        assert!(!execution_predates_boot(&json!({}), Some(&boot)));
        assert!(!execution_predates_boot(
            &json!({"launch_started_at":1}),
            None
        ));
        assert!(runtime_missing(&anyhow::Error::new(NotSubmitted(
            std::io::Error::from(std::io::ErrorKind::NotFound).into()
        ))));
        assert!(!runtime_missing(&anyhow::Error::new(NotSubmitted(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()
        ))));
    }
    #[tokio::test]
    async fn reconcile_expires_owned_starter_even_when_a_bound_socket_returns_rpc_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("herdr");
        std::fs::write(&executable, "#!/bin/sh\necho 'herdr 0.9.5'\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = Config {
            herdr: executable.to_string_lossy().into(),
            herdr_session: dir.path().to_string_lossy().into(),
            ..Config::default()
        };
        let app = crate::App::new(dir.path().join("store"), cfg.clone()).unwrap();
        let p = app
            .db
            .create(crate::db::ProjectInput {
                name: "Bound unready runtime".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: "saved-native".into(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let execution = json!({"api_protocol":7,"mcp_contract":1,"backend":"legacy","workspace_id":"w1","pane_id":"w1:p1","terminal_id":"saved-terminal","managed_name":"saved-managed","generation":"saved-generation","shell_pid":12345,"launch_permit":dir.path().join("permit")});
        app.db.sync(|c|{
            c.execute("UPDATE projects SET paused=1,terminal='saved-terminal',terminal_pid=12345,generation='saved-generation',ready=1,execution=? WHERE id=?",rusqlite::params![execution.to_string(),p.id])?;
            crate::work::begin_execution(c,&p.id,"saved-generation",crate::db::now())?;
            c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('pending',?,'in','pending','hold','queued',1,1)",[&p.id])?;crate::work::sync_inbound(c)?;Ok(())
        }).unwrap();
        let path = socket_path(&cfg).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let (input, mut output) = stream.into_split();
                    let Some(line) = BufReader::new(input).lines().next_line().await.unwrap()
                    else {
                        return;
                    };
                    let request: Value = serde_json::from_str(&line).unwrap();
                    seen.lock()
                        .unwrap()
                        .push(request["method"].as_str().unwrap().to_owned());
                    output.write_all(format!("{}\n",json!({"id":request["id"],"error":{"code":"runtime_not_ready","message":"bound but not ready"}})).as_bytes()).await.unwrap();
                });
            }
        });
        let child = tokio::process::Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let owned_pid = child.id().unwrap();
        let mut foreign = std::process::Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        START_LOCK.lock().await.insert(
            path.clone(),
            StartAttempt {
                retry_at: tokio::time::Instant::now(),
                deadline: tokio::time::Instant::now(),
                child: Some(child),
            },
        );
        let paused = app.reconcile(&p.id).await;
        let paused_preserved = !root_has_exited(Some(i64::from(owned_pid)));
        app.db.pause(&p.id, false).await.unwrap();
        app.db
            .sync(|c| crate::work::set_takeover(c, &p.id, "saved-generation", true, 2))
            .unwrap();
        let takeover = app.reconcile(&p.id).await;
        let takeover_preserved = !root_has_exited(Some(i64::from(owned_pid)));
        app.db
            .sync(|c| crate::work::set_takeover(c, &p.id, "saved-generation", false, 3))
            .unwrap();
        let recovery = app.reconcile(&p.id).await;
        let owned_expired = root_has_exited(Some(i64::from(owned_pid)));
        let foreign_preserved = foreign.try_wait().unwrap().is_none();
        if let Some(mut attempt) = START_LOCK.lock().await.remove(&path)
            && let Some(mut child) = attempt.child.take()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        foreign.kill().unwrap();
        foreign.wait().unwrap();
        server.abort();
        let _ = server.await;
        assert!(paused.is_ok() && paused_preserved);
        assert!(takeover.is_ok() && takeover_preserved);
        assert!(recovery.is_err() && owned_expired && foreign_preserved);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|method| matches!(method.as_str(), "pane.get" | "ping")),
            "No mutation may reach the foreign socket"
        );
        let saved = app.db.project(&p.id).await.unwrap();
        assert_eq!(saved.native_session, "saved-native");
        assert_eq!(saved.terminal.as_deref(), Some("saved-terminal"));
        app.db
            .sync(|c| {
                assert_eq!(crate::work::event(c, &p.id, "pending")?.status, "managed");
                Ok(())
            })
            .unwrap();
    }
    #[tokio::test]
    async fn expired_owned_starter_is_reaped_with_backoff_without_touching_foreign_processes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            herdr: "/nonexistent/fixture-herdr".into(),
            herdr_session: dir.path().to_string_lossy().into(),
            ..Config::default()
        };
        let child = tokio::process::Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let owned_pid = child.id().unwrap();
        let mut foreign = std::process::Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        let path = socket_path(&cfg).unwrap();
        START_LOCK.lock().await.insert(
            path.clone(),
            StartAttempt {
                retry_at: tokio::time::Instant::now(),
                deadline: tokio::time::Instant::now(),
                child: Some(child),
            },
        );
        assert!(startup_pending(&cfg).await.unwrap());
        let expired = ensure_server(&cfg).await.unwrap_err();
        assert!(!startup_pending(&cfg).await.unwrap());
        let backed_off = ensure_server(&cfg).await.unwrap_err();
        let foreign_preserved = foreign.try_wait().unwrap().is_none();
        foreign.kill().unwrap();
        foreign.wait().unwrap();
        let attempt = START_LOCK.lock().await.remove(&path).unwrap();
        assert!(expired.to_string().contains("exceeded its deadline"));
        assert!(backed_off.to_string().contains("backoff"));
        assert!(attempt.child.is_none());
        assert!(root_has_exited(Some(i64::from(owned_pid))));
        assert!(foreign_preserved);
    }
    #[tokio::test]
    async fn concurrent_missing_runtime_reconnects_share_one_start_and_preserve_a_live_server() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            herdr_session: dir.path().to_string_lossy().into_owned(),
            herdr: dir.path().join("herdr").to_string_lossy().into_owned(),
            ..Config::default()
        };
        let script = format!(
            r#"#!/usr/bin/python3
import json,os,socket,sys,threading
from pathlib import Path
if '--version' in sys.argv: print('herdr 0.9.5');sys.exit(0)
p=Path({directory:?});(p/'started').open('a').write('start\n');(p/'pid').write_text(str(os.getpid()))
s=socket.socket(socket.AF_UNIX);s.bind(str(p/'herdr.sock'));s.listen()
def respond(c):
 with c:
  line=c.makefile().readline()
  if not line:return
  r=json.loads(line);assert r['method']=='ping'
  c.sendall((json.dumps({{'id':r['id'],'result':{{'protocol':22,'version':'0.9.5'}}}})+'\n').encode())
while True:
 c,_=s.accept();threading.Thread(target=respond,args=(c,),daemon=True).start()
"#,
            directory = dir.path().to_string_lossy()
        );
        std::fs::write(&cfg.herdr, script).unwrap();
        std::fs::set_permissions(&cfg.herdr, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A candidate that becomes ready after its deadline wins over timeout
        // cleanup; retain its exact Child handle until the live probe succeeds.
        let late = command(&cfg)
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let late_pid = late.id().unwrap();
        let path = socket_path(&cfg).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while UnixStream::connect(&path).await.is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        START_LOCK.lock().await.insert(
            path.clone(),
            StartAttempt {
                retry_at: tokio::time::Instant::now(),
                deadline: tokio::time::Instant::now(),
                child: Some(late),
            },
        );
        let ready = ensure_server(&cfg).await;
        let late_preserved = !root_has_exited(Some(i64::from(late_pid)));
        unsafe {
            libc::kill(late_pid as i32, libc::SIGTERM);
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while !root_has_exited(Some(i64::from(late_pid))) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(ready.is_ok(), "{ready:?}");
        assert!(late_preserved);
        let (a, b, c) = tokio::join!(
            ensure_server(&cfg),
            ensure_server(&cfg),
            ensure_server(&cfg)
        );
        let pid = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        // Always clean only the server created by this isolated fixture.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        assert!(a.is_ok(), "{a:?}");
        assert!(b.is_ok(), "{b:?}");
        assert!(c.is_ok(), "{c:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("started"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
    #[tokio::test]
    async fn reused_pane_recovery_requires_global_absence_and_never_mutates_foreign_pane() {
        for scenario in [
            "gone",
            "moved",
            "incomplete",
            "alive",
            "unknown_pid",
            "unknown_children",
            "active_children",
            "registry_race",
            "preboot_pi",
            "sameboot_pi",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = Config {
                herdr_session: dir.path().to_string_lossy().into_owned(),
                ..Config::default()
            };
            let db = crate::db::Db::open(&dir.path().join("db")).unwrap();
            let mut p = db
                .create(crate::db::ProjectInput {
                    name: "Vanished native fixture".into(),
                    path: dir.path().to_string_lossy().into(),
                    agent: "codex".into(),
                    chat_id: String::new(),
                    native_session: "saved-native".into(),
                    extra_args: vec![],
                })
                .await
                .unwrap();
            let mut child = std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .spawn()
                .unwrap();
            let dead_pid = i64::from(child.id());
            child.wait().unwrap();
            p.generation = "old-generation".into();
            p.terminal = Some("old-terminal".into());
            p.ready = true;
            p.terminal_pid = match scenario {
                "alive" => Some(i64::from(std::process::id())),
                "unknown_pid" => None,
                _ => Some(dead_pid),
            };
            p.execution = json!({"backend":"legacy","workspace_id":"w1","pane_id":"w1:p1","terminal_id":"old-terminal","managed_name":"old-managed","generation":p.generation,"shell_pid":p.terminal_pid,"launch_permit":dir.path().join("permit")});
            if matches!(
                scenario,
                "unknown_children" | "active_children" | "preboot_pi" | "sameboot_pi"
            ) {
                p.agent = "pi".into();
                if scenario == "preboot_pi" {
                    p.execution["host_boot_id"] = json!(uuid::Uuid::new_v4().to_string());
                }
                if scenario == "sameboot_pi" {
                    p.execution["host_boot_id"] = json!(host_boot().unwrap().id);
                    p.execution["launch_started_at"] = json!(1);
                }
                let adapter = dir.path().join("adapter");
                std::fs::create_dir(&adapter).unwrap();
                p.execution["child_adapter"] = json!(adapter);
                if scenario == "active_children" {
                    std::fs::write(adapter.join("children.json"),json!({"schema":1,"generation":p.generation,"session":p.native_session,"sessionFile":"","epoch":"epoch","revision":1,"state":"active","updated":crate::db::now()}).to_string()).unwrap();
                }
            }
            let foreign = json!({"pane_id":"w1:p1","workspace_id":"w1","terminal_id":"replacement-terminal","agent_status":"unknown"});
            let old = json!({"pane_id":"w2:p1","workspace_id":"w2","terminal_id":"old-terminal"});
            let other =
                json!({"pane_id":"w2:p1","workspace_id":"w2","terminal_id":"unrelated-terminal"});
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let listener = tokio::net::UnixListener::bind(socket_path(&cfg).unwrap()).unwrap();
            let worker = tokio::spawn(async move {
                let mut registry_reads = 0;
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (input, mut output) = stream.into_split();
                    let request: Value = serde_json::from_str(
                        &BufReader::new(input)
                            .lines()
                            .next_line()
                            .await
                            .unwrap()
                            .unwrap(),
                    )
                    .unwrap();
                    let method = request["method"].as_str().unwrap();
                    seen.lock().unwrap().push(method.into());
                    let result = match method {
                        "pane.get" => json!({"pane":foreign}),
                        "workspace.list" => {
                            registry_reads += 1;
                            json!({"workspaces":[{"workspace_id":"w1","pane_count":1},{"workspace_id":"w2","pane_count":if scenario=="incomplete"{2}else{1}}]})
                        }
                        "pane.list" => {
                            if request["params"]["workspace_id"] == "w1" {
                                json!({"panes":[foreign]})
                            } else if scenario == "moved" {
                                json!({"panes":[old]})
                            } else if scenario == "registry_race" && registry_reads % 2 == 0 {
                                json!({"panes":[{"pane_id":"w2:p2","workspace_id":"w2","terminal_id":"another-terminal"}]})
                            } else {
                                json!({"panes":[other]})
                            }
                        }
                        "agent.get" => json!({"agent":foreign}),
                        "pane.process_info" => {
                            json!({"process_info":{"shell_pid":std::process::id(),"foreground_processes":[{"pid":std::process::id(),"name":"zsh","argv":["zsh"]}]}})
                        }
                        _ => panic!("Unexpected effect on unrelated pane: {method}"),
                    };
                    output
                        .write_all(
                            format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                        )
                        .await
                        .unwrap();
                }
            });
            let observed = observe(&cfg, &p).await;
            if matches!(scenario, "gone" | "preboot_pi") {
                assert_eq!(observed.unwrap()["vanished_execution"], true);
                assert!(stop(&cfg, &p).await.is_ok());
            } else {
                if let Ok(record) = observed {
                    assert_ne!(record["state"], "finished", "{scenario}");
                }
                assert!(stop(&cfg, &p).await.is_err(), "{scenario}");
            }
            assert!(seen_read_only(&calls.lock().unwrap()));
            assert!(
                !calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|m| m == "pane.close" || m == "agent.prompt" || m == "pane.send_input")
            );
            worker.abort();
            let _ = worker.await;
        }
    }
    fn seen_read_only(methods: &[String]) -> bool {
        methods.iter().all(|m| {
            matches!(
                m.as_str(),
                "pane.get" | "workspace.list" | "pane.list" | "agent.get" | "pane.process_info"
            )
        })
    }
    #[tokio::test]
    async fn native_observation_and_prompt_use_public_pane_target() {
        for starting in [false, true] {
            let dir = tempfile::TempDir::new().unwrap();
            let cfg = Config {
                herdr_session: dir.path().to_string_lossy().into_owned(),
                ..Config::default()
            };
            let db = crate::db::Db::open(&dir.path().join("db")).unwrap();
            let mut p = db
                .create(crate::db::ProjectInput {
                    name: "Native target".into(),
                    path: dir.path().to_string_lossy().into_owned(),
                    agent: "codex".into(),
                    chat_id: String::new(),
                    native_session: String::new(),
                    extra_args: vec![],
                })
                .await
                .unwrap();
            p.generation = uuid::Uuid::new_v4().to_string();
            p.terminal = Some("term_uuid".into());
            let root_pid = if starting {
                i64::from(std::process::id())
            } else {
                100
            };
            p.terminal_pid = Some(root_pid);
            p.ready = !starting;
            let name = managed_name(&p.generation).unwrap();
            p.execution = json!({"backend":"legacy","workspace_id":"w1","pane_id":"w1:p1","terminal_id":"term_uuid","managed_name":name,"generation":p.generation,"shell_pid":root_pid,"launch_permit":dir.path().join("launch.permit")});
            let pane = json!({"pane_id":"w1:p1","workspace_id":"w1","terminal_id":"term_uuid","agent":if starting {""} else {"codex"},"agent_status":"idle","tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":p.generation,"hfb_execution":name}});
            let listener = tokio::net::UnixListener::bind(socket_path(&cfg).unwrap()).unwrap();
            let worker = tokio::spawn(async move {
                let expected_methods = if starting {
                    vec!["pane.read", "pane.get", "agent.get", "pane.process_info"]
                } else {
                    vec![
                        "pane.read",
                        "pane.get",
                        "agent.get",
                        "pane.process_info",
                        "agent.prompt",
                    ]
                };
                for expected in expected_methods {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (input, mut output) = stream.into_split();
                    let request: Value = serde_json::from_str(
                        &BufReader::new(input)
                            .lines()
                            .next_line()
                            .await
                            .unwrap()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(request["method"], expected);
                    if expected.starts_with("agent.") {
                        assert_eq!(
                            request["params"]["target"], "w1:p1",
                            "Agent targets do not accept terminal UUIDs"
                        );
                    }
                    let result = match expected {
                        "pane.read" => {
                            assert_eq!(request["params"]["source"], "visible");
                            assert!(
                                request["params"].get("lines").is_none(),
                                "A trust check needs the complete visible viewport"
                            );
                            json!({"read":{"text":"project path\ntrust question\nselected yes"}})
                        }
                        "pane.get" => json!({"pane":pane}),
                        "agent.get" => json!({"agent":pane}),
                        "pane.process_info" => {
                            json!({"process_info":{"shell_pid":root_pid,"foreground_processes":if starting { json!([]) } else { json!([{"pid":root_pid,"name":"codex","argv":["codex"]}]) }}})
                        }
                        _ => json!({"agent":pane}),
                    };
                    output
                        .write_all(
                            format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                        )
                        .await
                        .unwrap();
                }
                if starting {
                    assert!(
                        tokio::time::timeout(Duration::from_millis(30), listener.accept())
                            .await
                            .is_err(),
                        "Preflight refusal must never write a prompt request"
                    );
                }
            });
            assert_eq!(
                screen(&cfg, &p, 0).await.unwrap(),
                "project path\ntrust question\nselected yes"
            );
            let result = send(&cfg, &p, "test prompt").await;
            if starting {
                let error = result.unwrap_err();
                assert!(definitely_not_sent(&error));
                assert!(error.is::<NotSubmitted>());
                assert!(error.chain().any(|e| e.is::<SessionStarting>()));
            } else {
                assert_eq!(result.unwrap()["submitted"], true);
            }
            worker.await.unwrap();
            let error = rpc(
                &cfg,
                "agent.prompt",
                json!({"target":"w1:p1","text":"no connection"}),
            )
            .await
            .unwrap_err();
            assert!(
                definitely_not_sent(&error),
                "A failed socket connection wrote no request bytes"
            );
        }
    }

    #[tokio::test]
    async fn claude_trust_moves_once_and_verifies_yes_before_enter() {
        for changed_after_navigation in [false, true] {
            let dir = tempfile::TempDir::new().unwrap();
            let cfg = Config {
                herdr_session: dir.path().to_string_lossy().into_owned(),
                ..Config::default()
            };
            let db = crate::db::Db::open(&dir.path().join("db")).unwrap();
            let mut p = db
                .create(crate::db::ProjectInput {
                    name: "Trust navigation".into(),
                    path: dir.path().to_string_lossy().into_owned(),
                    agent: "claude".into(),
                    chat_id: String::new(),
                    native_session: String::new(),
                    extra_args: vec![],
                })
                .await
                .unwrap();
            p.generation = uuid::Uuid::new_v4().to_string();
            p.terminal = Some("term_uuid".into());
            p.terminal_pid = Some(100);
            let name = managed_name(&p.generation).unwrap();
            p.execution = json!({"backend":"legacy","workspace_id":"w1","pane_id":"w1:p1","terminal_id":"term_uuid","managed_name":name,"generation":p.generation,"shell_pid":100,"launch_permit":dir.path().join("launch.permit")});
            let pane = json!({"pane_id":"w1:p1","workspace_id":"w1","terminal_id":"term_uuid","agent":"claude","agent_status":"blocked","cwd":p.path,"tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":p.generation,"hfb_execution":name}});
            let no = format!(
                "Quick safety check: Is this a project you created or one you trust? (Like your own code\n{}\n❯ No, exit\nYes, I trust this folder",
                p.path
            );
            let yes = no.replace("❯ No, exit\nYes,", "No, exit\n❯ Yes,");
            let listener = tokio::net::UnixListener::bind(socket_path(&cfg).unwrap()).unwrap();
            let keys = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = keys.clone();
            let worker = tokio::spawn(async move {
                let mut selected_yes = false;
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (input, mut output) = stream.into_split();
                    let request: Value = serde_json::from_str(
                        &BufReader::new(input)
                            .lines()
                            .next_line()
                            .await
                            .unwrap()
                            .unwrap(),
                    )
                    .unwrap();
                    let mut finish = false;
                    let result = match request["method"].as_str().unwrap() {
                        "pane.get" => json!({"pane":pane}),
                        "agent.get" => json!({"agent":pane}),
                        "pane.process_info" => {
                            json!({"process_info":{"shell_pid":100,"foreground_processes":[{"pid":100,"name":"Claude.exe","argv":["claude"]}]}})
                        }
                        "pane.read" => {
                            assert!(request["params"].get("lines").is_none());
                            let text = if selected_yes && changed_after_navigation {
                                "Unrelated permission prompt"
                            } else if selected_yes {
                                &yes
                            } else {
                                &no
                            };
                            json!({"read":{"text":text}})
                        }
                        "pane.send_keys" => {
                            let key = request["params"]["keys"][0].as_str().unwrap();
                            observed.lock().unwrap().push(key.to_owned());
                            if key == "Down" {
                                assert!(!selected_yes);
                                selected_yes = true;
                            } else {
                                assert_eq!(key, "Enter");
                                assert!(selected_yes && !changed_after_navigation);
                                finish = true;
                            }
                            json!({})
                        }
                        method => panic!("Unexpected request {method}"),
                    };
                    output
                        .write_all(
                            format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                        )
                        .await
                        .unwrap();
                    if finish {
                        break;
                    }
                }
            });
            let result = confirm_project_trust(&cfg, &p).await;
            if changed_after_navigation {
                assert!(result.is_err());
                assert_eq!(*keys.lock().unwrap(), ["Down"]);
                worker.abort();
            } else {
                result.unwrap();
                worker.await.unwrap();
                assert_eq!(*keys.lock().unwrap(), ["Down", "Enter"]);
            }
            assert!(dir.path().join("trust.attempted").exists());
        }
    }

    #[tokio::test]
    async fn failed_preparation_cleanup_preserves_unrelated_panes() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = Config {
            // A fake socket transport requires no real HERDR server or user config.
            herdr_session: dir.path().to_string_lossy().into_owned(),
            ..Config::default()
        };
        let listener = tokio::net::UnixListener::bind(socket_path(&cfg).unwrap()).unwrap();
        let permit = dir.path().join("project/generation/launch.permit");
        let initial =
            json!({"pane_id":"initial","terminal_id":"initial-terminal","workspace_id":"owned"});
        let gated = json!({"pane_id":"gated","terminal_id":"gate-terminal","workspace_id":"owned"});
        let user = json!({"pane_id":"user","terminal_id":"user-terminal","workspace_id":"owned"});
        let workspace = json!({"workspace":{"workspace_id":"owned"},"root_pane":initial});
        let expected_permit = permit.clone();
        let closed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = closed.clone();
        let worker = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (input, mut output) = stream.into_split();
                let request: Value = serde_json::from_str(
                    &BufReader::new(input)
                        .lines()
                        .next_line()
                        .await
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                let pane = match request["params"]["pane_id"].as_str() {
                    Some("initial") => initial.clone(),
                    Some("gated") => gated.clone(),
                    _ => user.clone(),
                };
                let result = match request["method"].as_str().unwrap() {
                    "pane.list" => json!({"panes":[initial,gated,user]}),
                    "pane.get" => json!({"pane":pane}),
                    "pane.process_info" => {
                        let argv = if pane["pane_id"] == "gated" {
                            json!(["/bin/sh", "dauerbot-launch", expected_permit])
                        } else {
                            json!(["/bin/sh"])
                        };
                        json!({"process_info":{"foreground_processes":[{"argv":argv}]}})
                    }
                    "pane.close" => {
                        recorded
                            .lock()
                            .unwrap()
                            .push(request["params"]["pane_id"].as_str().unwrap().to_owned());
                        json!({})
                    }
                    method => panic!("Unexpected request: {method}"),
                };
                output
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        cleanup_preparation(&cfg, &workspace, &permit)
            .await
            .unwrap();
        worker.abort();
        assert_eq!(*closed.lock().unwrap(), ["gated", "initial"]);
    }
}
