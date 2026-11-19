//! Guarded obsolete-execution inspection and close for the legacy HERDR backend.
//! This module has no product-state or manager authority; callers admit effects
//! using the latest host authority before dispatching a prepared target.
use crate::{config::Config, db::Project};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};

fn owns(p: &Project, generation: &str, terminal: &str, pane: &Value) -> bool {
    pane["terminal_id"] == terminal
        && pane["tokens"]["hfb_owner"] == "dauerbot"
        && pane["tokens"]["hfb_project"] == p.id
        && pane["tokens"]["hfb_generation"] == generation
        && super::managed_name(generation).is_ok_and(|name| pane["tokens"]["hfb_execution"] == name)
        && pane["pane_id"].as_str().is_some_and(|id| !id.is_empty())
        && pane["workspace_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
}

fn shell_snapshot(info: &Value) -> Result<Value> {
    let processes = info["foreground_processes"]
        .as_array()
        .context("Foreground state is unknown")?;
    ensure!(!processes.is_empty(), "Foreground state is unknown");
    let mut ids = Vec::new();
    for process in processes {
        let name = process["name"]
            .as_str()
            .context("Process identity is unknown")?;
        let basename = Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim_start_matches('-');
        ensure!(
            matches!(
                basename,
                "sh" | "bash" | "zsh" | "fish" | "dash" | "nu" | "pwsh" | "login"
            ),
            "A native or other foreground process is still alive"
        );
        let pid = process["pid"]
            .as_i64()
            .filter(|pid| *pid > 0)
            .context("Process identity is unknown")?;
        ids.push(json!({"pid":pid,"name":basename}));
    }
    ids.sort_by_key(|v| v["pid"].as_i64());
    let shell_pid = info["shell_pid"]
        .as_i64()
        .filter(|pid| *pid > 0)
        .context("Shell identity is unknown")?;
    Ok(json!({"shell_pid":shell_pid,"foreground":ids}))
}

#[derive(Clone)]
struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    session: i32,
    started: String,
    name: String,
}
fn process_census(root: i32, rows: &[Process]) -> Result<Value> {
    let leader = rows
        .iter()
        .find(|p| p.pid == root)
        .context("Owned shell process is unavailable")?;
    ensure!(
        leader.session == root && leader.group > 0,
        "Owned process session is unknown or shared"
    );
    let mut included = std::collections::BTreeSet::from([root]);
    loop {
        let count = included.len();
        for process in rows {
            if included.contains(&process.parent)
                || process.session == leader.session
                || process.group == leader.group
            {
                included.insert(process.pid);
            }
        }
        if included.len() == count {
            break;
        }
    }
    let mut evidence = Vec::new();
    for process in rows.iter().filter(|p| included.contains(&p.pid)) {
        ensure!(
            process.session > 0 && process.pid > 0 && !process.started.is_empty(),
            "Process lineage is unknown"
        );
        ensure!(
            matches!(
                process.name.as_str(),
                "sh" | "bash" | "zsh" | "fish" | "dash" | "nu" | "pwsh" | "login"
            ),
            "A native or background process is still alive"
        );
        evidence.push(json!({"pid":process.pid,"parent":process.parent,"group":process.group,"session":process.session,"started":process.started,"name":process.name}));
    }
    evidence.sort_by_key(|v| v["pid"].as_i64());
    Ok(json!(evidence))
}
async fn process_snapshot(info: &Value) -> Result<Value> {
    let foreground = shell_snapshot(info)?;
    let root = info["shell_pid"]
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .filter(|v| *v > 0)
        .context("Owned shell identity is unavailable")?;
    // Fixed host inspection only: no shell, user-supplied command or arguments.
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,pgid=,lstart=,comm="])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Process inspection timed out")??;
    ensure!(
        output.status.success() && output.stdout.len() <= 8 * 1024 * 1024,
        "Process inspection is unavailable or exceeds its bound"
    );
    let text = std::str::from_utf8(&output.stdout).context("Process inspection is not UTF-8")?;
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(fields.len() >= 9, "Process inspection is incomplete");
        let pid: i32 = fields[0].parse()?;
        let parent: i32 = fields[1].parse()?;
        let group: i32 = fields[2].parse()?;
        let executable = fields[8..].join(" ");
        let name = Path::new(&executable)
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .trim_start_matches('-')
            .to_owned();
        let session = unsafe { libc::getsid(pid) };
        rows.push(Process {
            pid,
            parent,
            group,
            session,
            started: fields[3..8].join(" "),
            name,
        });
    }
    let census = process_census(root, &rows)?;
    for process in foreground["foreground"]
        .as_array()
        .context("Foreground inspection is unavailable")?
    {
        ensure!(
            census
                .as_array()
                .is_some_and(|rows| rows.iter().any(|p| p["pid"] == process["pid"])),
            "Foreground process lineage changed"
        );
    }
    Ok(json!({"foreground":foreground,"process_census":census}))
}

fn workspace_scope(pane: &Value, list: &Value) -> Result<Value> {
    let workspaces = list["workspaces"]
        .as_array()
        .context("Workspace registry is unavailable")?;
    let matches: Vec<_> = workspaces
        .iter()
        .filter(|w| w["workspace_id"] == pane["workspace_id"])
        .collect();
    ensure!(
        matches.len() == 1,
        "Workspace identity is missing or ambiguous"
    );
    let target = matches[0];
    let count = target["pane_count"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("Workspace pane count is unknown")?;
    if let Some(worktree) = target.get("worktree").filter(|v| !v.is_null()) {
        let key = worktree["repo_key"]
            .as_str()
            .filter(|v| !v.is_empty())
            .context("Workspace group identity is unknown")?;
        ensure!(
            worktree["is_linked_worktree"].is_boolean(),
            "Workspace group lineage is unknown"
        );
        let associated = workspaces
            .iter()
            .filter(|w| w["worktree"]["repo_key"] == key)
            .count();
        ensure!(
            associated == 1,
            "Closing this pane could affect an associated workspace group"
        );
    }
    Ok(json!({"workspace_id":pane["workspace_id"],"pane_count":count,"associated_group":false}))
}
async fn workspace_snapshot(config: &Config, pane: &Value) -> Result<Value> {
    let list = super::rpc(config, "workspace.list", json!({})).await?;
    workspace_scope(pane, &list)
}

async fn pane_for(config: &Config, terminal: &str) -> Result<Value> {
    let list = super::rpc(config, "pane.list", json!({})).await?;
    let panes = list["panes"]
        .as_array()
        .context("Pane inventory is unavailable")?;
    let found: Vec<_> = panes
        .iter()
        .filter(|p| p["terminal_id"] == terminal)
        .collect();
    ensure!(found.len() == 1, "Execution pane is missing or ambiguous");
    Ok(found[0].clone())
}

/// Read-only runtime evidence; never closes a pane.
pub(crate) async fn inspect(
    config: &Config,
    project: &Project,
    generation: &str,
    terminal: &str,
) -> Result<Value> {
    let pane = pane_for(config, terminal).await?;
    ensure!(
        owns(project, generation, terminal, &pane),
        "Runtime ownership is unknown; historical session labels are not authority"
    );
    let info = super::rpc(
        config,
        "pane.process_info",
        json!({"pane_id":pane["pane_id"]}),
    )
    .await?;
    process_snapshot(&info["process_info"]).await?;
    workspace_snapshot(config, &pane).await?;
    Ok(pane["pane_id"].clone())
}

/// Constructed only after validating immutable ownership and stable OS/scope evidence.
pub(crate) struct PreparedClose {
    pane: Value,
    snapshot: Value,
    terminal: String,
}
impl PreparedClose {
    pub(crate) fn pane_id(&self) -> &Value {
        &self.pane["pane_id"]
    }
    pub(crate) fn snapshot(&self) -> &Value {
        &self.snapshot
    }
    /// Call only after the caller commits fresh host authority and audit intent.
    pub(crate) async fn dispatch(self, config: &Config) -> (bool, bool) {
        let sent = super::rpc(
            config,
            "pane.close",
            json!({"pane_id":self.pane["pane_id"]}),
        )
        .await;
        // Legacy HERDR has no atomic conditional-close RPC. A transport error
        // remains uncertain unless the exact immutable terminal is verified absent.
        let absent = match super::rpc(config, "pane.list", json!({})).await {
            Ok(list) => list["panes"]
                .as_array()
                .is_some_and(|panes| panes.iter().all(|p| p["terminal_id"] != self.terminal)),
            Err(_) => false,
        };
        (sent.is_ok(), absent)
    }
}
pub(crate) async fn prepare(
    config: &Config,
    p: &Project,
    generation: &str,
    terminal: &str,
) -> Result<PreparedClose> {
    let pane = pane_for(config, terminal).await?;
    ensure!(
        owns(p, generation, terminal, &pane),
        "Runtime ownership is unknown; preserve the pane"
    );
    let info = super::rpc(
        config,
        "pane.process_info",
        json!({"pane_id":pane["pane_id"]}),
    )
    .await?;
    let snapshot = process_snapshot(&info["process_info"]).await?;
    let scope = workspace_snapshot(config, &pane).await?;
    let fresh = super::rpc(config, "pane.get", json!({"pane_id":pane["pane_id"]})).await?;
    ensure!(
        owns(p, generation, terminal, &fresh["pane"])
            && fresh["pane"]["workspace_id"] == pane["workspace_id"],
        "Execution identity changed before close"
    );
    let final_info = super::rpc(
        config,
        "pane.process_info",
        json!({"pane_id":pane["pane_id"]}),
    )
    .await?;
    ensure!(
        process_snapshot(&final_info["process_info"]).await? == snapshot,
        "Foreground identity changed before close"
    );
    ensure!(
        workspace_snapshot(config, &pane).await? == scope,
        "Workspace scope changed before close"
    );
    Ok(PreparedClose {
        pane,
        snapshot: json!({"process":snapshot,"workspace":scope}),
        terminal: terminal.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_and_orphan_session_processes_veto_closure() {
        let shell = Process {
            pid: 10,
            parent: 1,
            group: 10,
            session: 10,
            started: "start".into(),
            name: "zsh".into(),
        };
        assert!(process_census(10, std::slice::from_ref(&shell)).is_ok());
        for process in [
            Process {
                pid: 11,
                parent: 10,
                group: 11,
                session: 10,
                started: "start".into(),
                name: "codex".into(),
            },
            Process {
                pid: 12,
                parent: 1,
                group: 12,
                session: 10,
                started: "start".into(),
                name: "agy".into(),
            },
            Process {
                pid: 13,
                parent: 10,
                group: 13,
                session: -1,
                started: "start".into(),
                name: "sh".into(),
            },
        ] {
            assert!(process_census(10, &[shell.clone(), process]).is_err());
        }
        assert!(process_census(99, &[shell]).is_err());
    }
    #[test]
    fn shell_identity_cannot_be_missing_or_change_unnoticed() {
        assert!(shell_snapshot(&json!({"foreground_processes":[]})).is_err());
        assert!(shell_snapshot(&json!({"foreground_processes":[{"pid":2,"name":"zsh"}]})).is_err());
        assert!(
            shell_snapshot(
                &json!({"shell_pid":1,"foreground_processes":[{"pid":2,"name":"codex"}]})
            )
            .is_err()
        );
        assert_ne!(
            shell_snapshot(&json!({"shell_pid":1,"foreground_processes":[{"pid":2,"name":"zsh"}]}))
                .unwrap(),
            shell_snapshot(&json!({"shell_pid":1,"foreground_processes":[{"pid":3,"name":"zsh"}]}))
                .unwrap()
        );
    }
}
