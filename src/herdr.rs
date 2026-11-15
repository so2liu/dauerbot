use crate::{config::Config, db::Project};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

#[derive(Debug)]
pub struct RpcFailure {
    pub kind: String,
    pub message: String,
    pub hint: String,
}
impl std::fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HERDR {}: {} ({})", self.kind, self.message, self.hint)
    }
}
impl std::error::Error for RpcFailure {}
pub fn definitely_not_sent(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RpcFailure>().is_some_and(|e| {
        matches!(e.kind.as_str(), "blocked" | "status_mismatch")
            || e.hint.contains("nothing was typed")
    })
}

pub fn command(config: &Config) -> Command {
    let mut cmd = Command::new(&config.hc);
    cmd.env("HC_SESSION", &config.core_session)
        .env_remove("HC_TERMINAL_ID")
        .env_remove("HC_TERMINAL_SOCKET")
        .env_remove("HC_SOCKET")
        .kill_on_drop(true);
    cmd
}

/// Core is persistent and may have started without the desktop's network environment.
/// Forward existing proxy settings only; retain Core's terminal identity and CLI configuration.
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
        .filter(|(key, _)| KEYS.contains(&key.as_str()))
        .collect()
}

const LOGIN_EXEC: &str = r#"exec "$@""#;
const LOGIN_MARKER: &str = "dauerbot-agent";

/// Start native CLIs with the same profile and environment as a user's terminal.
/// Positional arguments preserve every CLI argument without shell interpolation.
pub fn login_shell_args(argv: Vec<String>) -> Vec<String> {
    #[cfg(unix)]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| {
            if cfg!(target_os = "macos") {
                "/bin/zsh"
            } else {
                "/bin/sh"
            }
            .into()
        });
        let mut wrapped = vec![shell, "-lic".into(), LOGIN_EXEC.into(), LOGIN_MARKER.into()];
        wrapped.extend(argv);
        wrapped
    }
    #[cfg(not(unix))]
    argv
}

pub async fn rpc(config: &Config, method: &str, params: Value) -> Result<Value> {
    let mut child = command(config)
        .arg("connect")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Cannot start hc connect; install herdr-core first")?;
    let result = tokio::time::timeout(Duration::from_secs(100), async {
        let input = child.stdin.as_mut().context("hc stdin is unavailable")?;
        input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        input.flush().await?;
        let mut lines =
            BufReader::new(child.stdout.take().context("hc stdout is unavailable")?).lines();
        while let Some(line) = lines.next_line().await? {
            let v: Value = serde_json::from_str(&line)?;
            if v["id"] != 1 {
                continue;
            }
            if let Some(error) = v.get("error") {
                return Err(RpcFailure {
                    kind: error["data"]["kind"].as_str().unwrap_or("error").into(),
                    message: error["message"].as_str().unwrap_or("RPC failed").into(),
                    hint: error["data"]["hint"].as_str().unwrap_or("").into(),
                }
                .into());
            }
            return Ok(v["result"].clone());
        }
        let mut detail = String::new();
        if let Some(stderr) = child.stderr.take() {
            let _ = stderr.take(8192).read_to_string(&mut detail).await;
        }
        bail!("hc connect exited early: {}", detail.trim())
    })
    .await
    .context("HERDR request timed out; delivery may be uncertain")?;
    let _ = child.kill().await;
    result
}

pub async fn subscribe(
    config: &Config,
) -> Result<(
    Child,
    tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
)> {
    let mut child = command(config)
        .arg("connect")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let req = json!({"jsonrpc":"2.0","id":1,"method":"events.subscribe","params":{"labels":{"hfb.owner":"herdr-feishu-bridge"}}});
    child
        .stdin
        .as_mut()
        .context("hc stdin")?
        .write_all(format!("{req}\n").as_bytes())
        .await?;
    child.stdin.as_mut().unwrap().flush().await?;
    let lines = BufReader::new(child.stdout.take().context("hc stdout")?).lines();
    Ok((child, lines))
}

pub fn check_identity(p: &Project, t: &Value) -> Result<()> {
    if t["state"] != "live" {
        bail!("The main session exited; resume it before continuing");
    }
    if t["labels"]["hfb.project"] != p.id || t["labels"]["hfb.generation"] != p.generation {
        bail!("HERDR terminal labels do not match; delivery stopped");
    }
    if t["pid"].as_i64() != p.terminal_pid || p.terminal_pid.is_none() {
        bail!("Terminal process changed; reconnect the session");
    }
    let argv = t["argv"]
        .as_array()
        .context("Cannot verify the terminal program")?;
    let wrapped = argv.get(1).and_then(Value::as_str) == Some("-lic")
        && argv.get(2).and_then(Value::as_str) == Some(LOGIN_EXEC)
        && argv.get(3).and_then(Value::as_str) == Some(LOGIN_MARKER)
        && argv.get(4).and_then(Value::as_str) == Some(p.agent.as_str())
        && ["codex", "claude", "pi", "agy"].contains(&p.agent.as_str());
    let program = if wrapped {
        t["foreground"]["argv"][0]
            .as_str()
            .context("The main session is still starting")?
    } else {
        argv.first().and_then(Value::as_str).unwrap_or("")
    };
    if matches!(
        Path::new(program).file_name().and_then(|s| s.to_str()),
        Some("sh" | "bash" | "zsh" | "fish" | "nu" | "pwsh")
    ) {
        bail!("The terminal returned to a shell; refusing automatic delivery");
    }
    Ok(())
}

/// Only recognize the known workspace-trust dialog, never an arbitrary permission.
pub fn is_project_trust(p: &Project, t: &Value, screen: &str) -> bool {
    if p.ready
        || p.agent != "agy"
        || check_identity(p, t).is_err()
        || t["cwd"] != p.path
        || t["status"]["state"] != "blocked"
        || t["status"]["app"] != "antigravity"
        || t["status"]["kind"] != "permission"
        || t["status"]["msg"] != "Trust this folder?"
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

pub fn launch_args(
    p: &Project,
    mcp_file: &Path,
    mcp_entry: &Value,
    instructions: &str,
    resume: bool,
) -> Result<Vec<String>> {
    let mut argv: Vec<String> = match p.agent.as_str() {
        "codex" => vec![
            "codex".into(),
            "--no-daemon".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "-c".into(),
            format!("mcp_servers.herdr_feishu.command={}", mcp_entry["command"]),
            "-c".into(),
            format!("mcp_servers.herdr_feishu.args={}", mcp_entry["args"]),
            "-c".into(),
            "mcp_servers.herdr_feishu.env_vars=[\"HC_TERMINAL_ID\",\"HC_TERMINAL_SOCKET\"]".into(),
        ],
        "claude" => vec![
            "claude".into(),
            "--dangerously-skip-permissions".into(),
            "--mcp-config".into(),
            mcp_file.to_string_lossy().into(),
            "--append-system-prompt".into(),
            instructions.into(),
        ],
        "pi" => vec![
            "pi".into(),
            "--approve".into(),
            "--append-system-prompt".into(),
            instructions.into(),
        ],
        "agy" => vec!["agy".into(), "--dangerously-skip-permissions".into()],
        "custom" => p
            .extra_args
            .iter()
            .map(|arg| {
                arg.replace("{mcp_config}", &mcp_file.to_string_lossy())
                    .replace(
                        "{credentials}",
                        &mcp_file
                            .with_file_name("credentials.json")
                            .to_string_lossy(),
                    )
                    .replace(
                        "{bridge_exe}",
                        &std::env::current_exe()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    )
                    .replace("{instructions}", instructions)
            })
            .collect(),
        _ => bail!("Unsupported agent"),
    };
    if p.agent != "custom" {
        argv.extend(p.extra_args.clone());
    }
    if resume {
        if p.native_session.is_empty() {
            bail!("Provide the CLI's native session ID or path before resuming");
        }
        match p.agent.as_str() {
            "codex" => argv.extend(["resume".into(), p.native_session.clone()]),
            "claude" => argv.extend(["--resume".into(), p.native_session.clone()]),
            "pi" => argv.extend(["--session".into(), p.native_session.clone()]),
            "agy" => argv.extend(["--conversation".into(), p.native_session.clone()]),
            _ => bail!(
                "Use custom argv to supply this agent's resume arguments, or reconnect an existing terminal"
            ),
        }
    }
    if p.agent != "custom" {
        if p.agent == "agy" {
            argv.push("-i".into());
        }
        argv.push(instructions.into());
    }
    Ok(argv)
}

/// Only edit our own named server. Invalid/unrelated configuration is preserved.
pub fn merge_mcp(path: &Path, entry: &Value) -> Result<()> {
    let mut cfg: Value = if path.exists() {
        serde_json::from_slice(&std::fs::read(path)?)
            .context("Existing MCP configuration is not valid JSON; left unchanged")?
    } else {
        json!({})
    };
    if !cfg.is_object() {
        bail!("The MCP configuration root must be an object");
    }
    if cfg.get("mcpServers").is_none() {
        cfg["mcpServers"] = json!({});
    }
    let servers = cfg["mcpServers"]
        .as_object_mut()
        .context("mcpServers must be an object")?;
    if let Some(old) = servers.get("herdr_feishu") {
        let ours = old["args"]
            .as_array()
            .is_some_and(|args| args.iter().any(|v| v == "mcp"));
        if !ours {
            bail!("A different herdr_feishu MCP entry already exists; left unchanged");
        }
    }
    servers.insert("herdr_feishu".into(), entry.clone());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::config::write_private(path, &serde_json::to_vec_pretty(&cfg)?)
}
