//! Native interactive CLI launch plans. Model selection remains with each CLI.
use crate::db::Project;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;

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
