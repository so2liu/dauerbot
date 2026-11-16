//! Legacy HERDR execution and terminal presentation transport.
//!
//! `hc` is intentionally absent from this module. Persisted legacy execution
//! identities must be checked before sending input or closing a pane.
use crate::{config::Config, db::Project};
use anyhow::{Context, Result, bail};
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

pub use crate::adapter::{launch_args, merge_mcp};
const PROTOCOL: u64 = 22;
static START_LOCK: Mutex<()> = Mutex::const_new(());

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
pub struct NotSubmitted(anyhow::Error);
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
    let argv = login_shell_args(vec![
        config.herdr.clone(),
        "--session".into(),
        config.herdr_session.clone(),
        "server".into(),
    ]);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    command_environment(config, &mut cmd);
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

pub async fn ensure_server(config: &Config) -> Result<()> {
    let _guard = START_LOCK.lock().await;
    if UnixStream::connect(socket_path(config)?).await.is_ok() {
        probe(config).await?;
        return Ok(());
    }
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
            if value["protocol"].as_u64() != Some(PROTOCOL) {
                bail!("HERDR server protocol differs from the supported installation");
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
            // A slow live server remains owned by HERDR, not this view or request.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            bail!("HERDR server did not become ready; open its session and retry");
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
/// Older Codex builds do not recognize this flag; their normal PTY is already
/// dedicated. Newer builds need it to avoid reusing a daemon's terminal context.
async fn compatible_launch_args(
    config: &Config,
    p: &Project,
    argv: &[String],
) -> Result<Vec<String>> {
    let mut effective = argv.to_vec();
    if p.agent != "codex" || argv.iter().any(|arg| arg == "--no-daemon") {
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
    let advertised = String::from_utf8_lossy(&output.stdout).contains("--no-daemon")
        || String::from_utf8_lossy(&output.stderr).contains("--no-daemon");
    if advertised {
        effective.insert(1, "--no-daemon".into());
    }
    Ok(effective)
}

/// Create the identity before launch so an early MCP registration can validate it.
pub async fn prepare_execution(
    config: &Config,
    p: &Project,
    argv: &[String],
    generation: &str,
) -> Result<Value> {
    ensure_server(config).await?;
    if argv.is_empty() {
        bail!("The Agent launch plan is empty");
    }
    let argv = compatible_launch_args(config, p, argv).await?;
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
    if p.agent == "custom" {
        command.extend(argv.to_vec());
    } else {
        command.extend(login_shell_args(argv.to_vec()));
    }
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

/// Save the execution before releasing this gate. No Agent can call MCP earlier.
pub async fn launch_execution(_config: &Config, p: &Project, _argv: &[String]) -> Result<()> {
    let execution = ExecutionRef::from_project(p)?;
    crate::config::write_private(&execution.launch_permit, p.generation.as_bytes())?;
    Ok(())
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
            return Ok(json!({"state":"finished","execution":execution}));
        }
        Err(error) => return Err(error),
    };
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
            return Ok(json!({"state":"finished","execution":execution}));
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
        "processes":processes,"execution":execution,"agent":native_agent,"pane_id":pane["pane_id"],"terminal_id":pane["terminal_id"],"tokens":pane["tokens"],
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

pub fn check_identity(p: &Project, record: &Value) -> Result<()> {
    check_execution_identity(p, record)?;
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
    if check_identity(p, record).is_err()
        || !matches!(record["status"]["state"].as_str(), Some("idle" | "done"))
    {
        return false;
    }
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
            argv.len() == 4
                && argv[1] == "mcp"
                && argv[2] == "--credentials"
                && argv[3].as_str().map(Path::new) == credential.as_deref()
                && argv[0].as_str().is_some_and(|program| {
                    same_program(
                        program,
                        &std::env::current_exe()
                            .unwrap_or_default()
                            .to_string_lossy(),
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
    let preflight: Result<ExecutionRef> = async {
        let record = observe(config, p).await?;
        check_identity(p, &record)?;
        ExecutionRef::from_project(p)
    }
    .await;
    let execution = preflight.map_err(NotSubmitted)?;
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
    Ok(json!({"submitted":true,"started":true}))
}

pub async fn stop(config: &Config, p: &Project) -> Result<()> {
    let record = observe(config, p).await?;
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
    match check_execution_identity(p, &record) {
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
    match check_execution_identity(p, &record) {
        Ok(()) => {}
        Err(error) if error.is::<SessionStarting>() => {}
        Err(error) => return Err(error),
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

/// Recognize only the native workspace trust screen already authorized by the user.
pub fn is_project_trust(p: &Project, t: &Value, screen: &str) -> bool {
    if p.ready
        || !matches!(p.agent.as_str(), "agy" | "claude")
        || t["cwd"] != p.path
        || t["status"]["state"] != "blocked"
        || check_execution_identity(p, t).is_err()
    {
        return false;
    }
    let lines: Vec<_> = screen.lines().map(str::trim).collect();
    if !lines.contains(&p.path.as_str()) {
        return false;
    }
    match p.agent.as_str() {
        "agy" => {
            lines.contains(&"Accessing workspace:")
                && lines.contains(&"Do you trust the contents of this project?")
                && lines.contains(&"> Yes, I trust this folder")
                && lines.contains(&"No, exit")
        }
        "claude" => {
            lines.iter().any(|line| {
                line.starts_with(
                    "Quick safety check: Is this a project you created or one you trust?",
                )
            }) && lines.contains(&"❯ 1. Yes, I trust this folder")
                && lines.contains(&"2. No, exit")
        }
        _ => false,
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
    let screen = screen(config, p, 0).await?;
    if !is_project_trust(p, &latest, &screen) {
        bail!("The workspace trust prompt changed before confirmation");
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
        "native_session_source":"herdr.agent_session","model_confirmation":"unverified","background_quiescence":"requires_tracked_jobs"})
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
