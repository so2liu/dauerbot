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
pub fn definitely_not_sent(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RpcFailure>().is_some_and(|e| {
        matches!(
            e.kind.as_str(),
            "agent_blocked"
                | "agent_not_ready"
                | "agent_not_found"
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
        let mut stream = UnixStream::connect(&path).await.with_context(|| {
            format!("Cannot connect to HERDR session '{}'", config.herdr_session)
        })?;
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
    let layout=rpc(config,"layout.apply",json!({"tab_id":workspace["tab"]["tab_id"],"focus":false,"tab_label":p.name,
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
        launch_permit: permit,
    };
    Ok(serde_json::to_value(execution)?)
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
    let native_agent = match rpc(config, "agent.get", json!({"target":execution.terminal_id})).await
    {
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
    let pending = !p.ready && (agent_label.is_empty() || is_shell(&foreground));
    let state = if processes.is_empty() {
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
        "execution":execution,"agent":native_agent,"pane_id":pane["pane_id"],"terminal_id":pane["terminal_id"],"tokens":pane["tokens"],
        "cwd":pane["foreground_cwd"].as_str().or(pane["cwd"].as_str()).unwrap_or("")}),
    )
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

pub async fn send(config: &Config, p: &Project, text: &str) -> Result<Value> {
    let record = observe(config, p).await?;
    check_identity(p, &record)?;
    let execution = ExecutionRef::from_project(p)?;
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
            json!({"target":execution.terminal_id,"text":text}),
        )
        .await?;
    }
    Ok(json!({"submitted":true,"started":true}))
}

pub async fn stop(config: &Config, p: &Project) -> Result<()> {
    let record = observe(config, p).await?;
    if record["state"] == "finished" {
        return Ok(());
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
    let value = rpc(
        config,
        "pane.read",
        json!({"pane_id":execution.pane_id,
        "source":if lines==0 {"visible"} else {"recent"},"lines":lines.max(1),"strip_ansi":true}),
    )
    .await?;
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
        || p.agent != "agy"
        || t["cwd"] != p.path
        || t["status"]["state"] != "blocked"
        || check_execution_identity(p, t).is_err()
    {
        return false;
    }
    let lines: Vec<_> = screen.lines().map(str::trim).collect();
    lines.contains(&p.path.as_str())
        && lines.contains(&"Accessing workspace:")
        && lines.contains(&"Do you trust the contents of this project?")
        && lines.contains(&"> Yes, I trust this folder")
        && lines.contains(&"No, exit")
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
