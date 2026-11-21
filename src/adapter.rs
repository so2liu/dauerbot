//! Native interactive CLI launch plans. Model selection remains with each CLI.
use crate::db::Project;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};

pub fn launch_args(
    p: &Project,
    mcp_file: &Path,
    mcp_entry: &Value,
    instructions: &str,
    resume: bool,
) -> Result<Vec<String>> {
    launch_args_with_selection(p, mcp_file, mcp_entry, instructions, resume, None)
}

pub fn launch_args_with_selection(
    p: &Project,
    mcp_file: &Path,
    mcp_entry: &Value,
    instructions: &str,
    resume: bool,
    selection: Option<&crate::models::Selection>,
) -> Result<Vec<String>> {
    let mut argv: Vec<String> = match p.agent.as_str() {
        "codex" => vec![
            "codex".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "-c".into(),
            format!("mcp_servers.herdr_feishu.command={}", mcp_entry["command"]),
            "-c".into(),
            format!("mcp_servers.herdr_feishu.args={}", mcp_entry["args"]),
            "-c".into(),
            "mcp_servers.herdr_feishu.env_vars=[\"HERDR_PANE_ID\",\"HERDR_SOCKET_PATH\",\"CODEX_THREAD_ID\"]".into(),
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
                        mcp_entry["command"].as_str().unwrap_or_default(),
                    )
                    .replace("{instructions}", instructions)
                    .replace("{native_session}", &p.native_session)
            })
            .collect(),
        _ => bail!("Unsupported agent"),
    };
    if p.agent != "custom" {
        argv.extend(crate::models::extra_args(
            &p.agent,
            &p.extra_args,
            selection,
        )?);
        if let Some(selection) = selection {
            argv.extend(crate::models::selection_args(&p.agent, selection)?);
        }
    } else if selection.is_some() {
        bail!("Model selection is unavailable for custom CLIs");
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
            "custom"
                if p.extra_args
                    .iter()
                    .any(|arg| arg.contains("{native_session}")) => {}
            _ => bail!("Custom resume requires an explicit {{native_session}} argv placeholder"),
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

/// Launch an interactive task worker with native defaults and no bridge tools.
pub(crate) fn plain_worker_args(
    p: &Project,
    selection: Option<&crate::models::Selection>,
) -> Result<Vec<String>> {
    let mut argv = match p.agent.as_str() {
        "codex" | "claude" | "pi" | "agy" => vec![p.agent.clone()],
        "custom" => {
            let (program, args) = p
                .extra_args
                .split_first()
                .context("Custom task workers require a configured executable")?;
            anyhow::ensure!(
                !program.trim().is_empty() && !legacy_placeholder(program),
                "The custom worker executable cannot depend on bridge placeholders"
            );
            let mut argv = vec![program.clone()];
            argv.extend(plain_extra_args(args));
            return Ok(argv);
        }
        _ => bail!("Unsupported agent"),
    };
    argv.extend(plain_extra_args(&crate::models::extra_args(
        &p.agent,
        &p.extra_args,
        selection,
    )?));
    if let Some(selection) = selection {
        argv.extend(crate::models::selection_args(&p.agent, selection)?);
    }
    Ok(argv)
}

/// Verify native CLI availability without inspecting or changing login state.
/// The version command runs through the login shell used for native launches.
pub(crate) async fn plain_worker_available(p: &Project) -> bool {
    let Ok(argv) = plain_worker_args(p, None) else {
        return false;
    };
    if p.agent == "custom" {
        let program = Path::new(&argv[0]);
        let executable = |path: &Path| {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        };
        if program.components().count() > 1 || program.is_absolute() {
            return executable(&Path::new(&p.path).join(program));
        }
        return std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path)
                .any(|directory| executable(&Path::new(&p.path).join(directory).join(program)))
        });
    }
    let shell = crate::herdr::login_shell_args(vec![argv[0].clone(), "--version".into()]);
    let mut command = tokio::process::Command::new(&shell[0]);
    command
        .args(&shell[1..])
        .current_dir(&p.path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(3), command.status())
        .await
        .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

fn legacy_placeholder(arg: &str) -> bool {
    [
        "{mcp_config}",
        "{credentials}",
        "{bridge_exe}",
        "{instructions}",
        "{native_session}",
    ]
    .iter()
    .any(|placeholder| arg.contains(placeholder))
}

fn plain_extra_args(args: &[String]) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut input = args.iter().peekable();
    while let Some(arg) = input.next() {
        if arg == "--mcp-config" {
            input.next();
            continue;
        }
        // Legacy templates may use an application-specific flag followed by a
        // host value. Drop the pair, leaving unrelated custom args intact.
        if arg.starts_with('-')
            && !arg.contains('=')
            && input.peek().is_some_and(|next| {
                !next.starts_with('-')
                    && (legacy_placeholder(next) || next.contains("mcp_servers."))
            })
        {
            input.next();
            continue;
        }
        if !legacy_placeholder(arg)
            && !arg.contains("mcp_servers.")
            && !arg.starts_with("--mcp-config=")
        {
            filtered.push(arg.clone());
        }
    }
    filtered
}

/// Only edit our own named server. Invalid/unrelated configuration is preserved.
pub fn merge_mcp(path: &Path, entry: &Value) -> Result<()> {
    let mut cfg: Value = if path.exists() {
        let bytes = std::fs::read(path)?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            json!({})
        } else {
            serde_json::from_slice(&bytes)
                .context("Existing MCP configuration is not valid JSON; left unchanged")?
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_worker_plans_keep_valid_native_args_without_legacy_injection() {
        let mut project: Project = serde_json::from_value(json!({
            "id":"fixture","name":"fixture","path":"/","agent":"codex","chat_id":"",
            "paused":false,"terminal":null,"terminal_pid":null,"generation":"",
            "ready":false,"execution":null,"native_session":"","extra_args":[],
            "created_at":0,"mcp_token":"fixture"
        }))
        .unwrap();
        for agent in ["codex", "claude", "pi", "agy"] {
            project.agent = agent.into();
            assert_eq!(plain_worker_args(&project, None).unwrap(), [agent]);
        }
        project.agent = "custom".into();
        project.extra_args = [
            "/usr/bin/true",
            "--work",
            "ordinary",
            "--mcp-config",
            "{mcp_config}",
            "--bridge-mode={bridge_exe}",
            "-c",
            "mcp_servers.herdr_feishu.args=[]",
            "--native-id",
            "{native_session}",
            "--another",
            "argument",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            plain_worker_args(&project, None).unwrap(),
            [
                "/usr/bin/true",
                "--work",
                "ordinary",
                "--another",
                "argument"
            ]
        );
    }
}
