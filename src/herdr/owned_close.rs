//! Guarded obsolete-execution inspection and close for the legacy HERDR backend.
//! This module has no product-state or manager authority; callers admit effects
//! using the latest host authority before dispatching a prepared target.
use crate::{config::Config, db::Project};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};

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

#[derive(Clone)]
struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    session: i32,
}
fn exited_census(root: i32, foreground_group: Option<i32>, rows: &[Process]) -> Result<()> {
    ensure!(
        rows.iter().all(|process| process.session >= 0),
        "Process session inspection is incomplete"
    );
    ensure!(
        rows.iter().all(|process| process.pid != root
            && process.parent != root
            && process.session != root
            && process.group != root
            && foreground_group.is_none_or(|group| process.group != group)),
        "An owned root, descendant or process-session member is still alive"
    );
    Ok(())
}
fn confirmed_zombie(state: &str, session_error: Option<i32>) -> bool {
    // macOS getsid returns ESRCH for zombies even while signal zero sees their
    // unreaped PID. Exclude only this positively dead state, never EPERM/unknown.
    state.starts_with('Z') && session_error == Some(libc::ESRCH)
}
async fn process_snapshot(info: &Value, pane: &str) -> Result<Value> {
    ensure!(
        info.is_object() && !pane.is_empty() && info["pane_id"] == pane,
        "Runtime process evidence belongs to an unknown pane"
    );
    let root = info["shell_pid"]
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .filter(|v| *v > 0)
        .context("Original runtime process identity is unavailable")?;
    // A stable live shell can run builtin-only foreground or background work.
    // Legacy 0.9.5 exposes no positive idle-prompt fact, so never infer idleness.
    ensure!(
        super::root_has_exited(Some(i64::from(root))),
        "Legacy runtime cannot prove a live shell is idle; preserve the execution"
    );
    // 0.9.5 omits this Vec when empty. Null/malformed values are not that contract.
    match info.get("foreground_processes") {
        None => {}
        Some(Value::Array(processes)) if processes.is_empty() => {}
        _ => anyhow::bail!("Runtime still reports foreground work or unknown process evidence"),
    }
    let foreground_group = match info.get("foreground_process_group_id") {
        None => None,
        Some(Value::Number(number)) => Some(
            number
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .filter(|v| *v > 0)
                .context("Foreground process group is unknown")?,
        ),
        _ => anyhow::bail!("Foreground process group is unknown"),
    };
    // Only fixed host inspection, never commands or argv selected by the model.
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,pgid=,stat="])
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
        ensure!(fields.len() == 4, "Process inspection is incomplete");
        let pid: i32 = fields[0].parse()?;
        if pid <= 0 {
            continue;
        }
        let parent = fields[1].parse()?;
        let group = fields[2].parse()?;
        let session = unsafe { libc::getsid(pid) };
        let session_error = (session < 0)
            .then(|| std::io::Error::last_os_error().raw_os_error())
            .flatten();
        if session < 0
            && (super::root_has_exited(Some(i64::from(pid)))
                || confirmed_zombie(fields[3], session_error))
        {
            continue;
        }
        rows.push(Process {
            pid,
            parent,
            group,
            session,
        });
    }
    exited_census(root, foreground_group, &rows)?;
    ensure!(
        super::root_has_exited(Some(i64::from(root))),
        "Original runtime PID is alive or reused"
    );
    Ok(
        json!({"state":"verified_exited","pane_id":pane,"original_pid":root,
        "foreground_group":foreground_group,"owned_lineage_absent":true}),
    )
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
    process_snapshot(
        &info["process_info"],
        pane["pane_id"]
            .as_str()
            .context("Pane identity is unknown")?,
    )
    .await?;
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
    let snapshot = process_snapshot(
        &info["process_info"],
        pane["pane_id"]
            .as_str()
            .context("Pane identity is unknown")?,
    )
    .await?;
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
        process_snapshot(
            &final_info["process_info"],
            pane["pane_id"]
                .as_str()
                .context("Pane identity is unknown")?
        )
        .await?
            == snapshot,
        "Process exit evidence changed before close"
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
    use tokio::io::{AsyncBufReadExt, BufReader};
    fn command(script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/bin/bash");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }
    #[tokio::test]
    async fn finite_builtin_root_and_background_shell_are_never_idle_evidence() {
        for script in [
            "printf 'ready\\n'; end=$((SECONDS+2)); while ((SECONDS<end)); do :; done",
            "(end=$((SECONDS+2)); while ((SECONDS<end)); do :; done) & printf 'ready\\n'; wait",
        ] {
            let mut child = command(script).spawn().unwrap();
            let pid = child.id().unwrap();
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ready"));
            let info = json!({"pane_id":"p","shell_pid":pid,"foreground_processes":[{"pid":pid,"name":"bash"}]});
            let error = process_snapshot(&info, "p").await.unwrap_err().to_string();
            assert!(error.contains("cannot prove a live shell is idle"));
            child.wait().await.unwrap();
        }
    }
    #[tokio::test]
    async fn exited_runtime_contract_requires_absence_and_rejects_malformed_or_reused_identity() {
        let mut child = command("exit 0").spawn().unwrap();
        let pid = child.id().unwrap();
        child.wait().await.unwrap();
        // Real 0.9.5 omits foreground_processes when its Vec is empty.
        let info = json!({"pane_id":"p","shell_pid":pid});
        assert_eq!(
            process_snapshot(&info, "p").await.unwrap()["state"],
            "verified_exited"
        );
        let mut explicit = info.clone();
        explicit["foreground_processes"] = json!([]);
        assert!(process_snapshot(&explicit, "p").await.is_ok());
        for invalid in [
            json!({}),
            json!({"pane_id":"p"}),
            json!({"pane_id":"other","shell_pid":pid}),
            json!({"pane_id":"p","shell_pid":pid,"foreground_processes":null}),
            json!({"pane_id":"p","shell_pid":std::process::id()}),
        ] {
            assert!(process_snapshot(&invalid, "p").await.is_err());
        }
    }
    #[test]
    fn only_positive_zombie_state_resolves_a_missing_session_lookup() {
        assert!(confirmed_zombie("Z", Some(libc::ESRCH)));
        assert!(confirmed_zombie("Z+", Some(libc::ESRCH)));
        assert!(!confirmed_zombie("S", Some(libc::ESRCH)));
        assert!(!confirmed_zombie("Z", Some(libc::EPERM)));
        assert!(!confirmed_zombie("", None));
    }
    #[test]
    fn exited_root_cannot_hide_any_remaining_child_group_or_session_member() {
        assert!(exited_census(10, None, &[]).is_ok());
        for process in [
            Process {
                pid: 10,
                parent: 1,
                group: 10,
                session: 10,
            },
            Process {
                pid: 11,
                parent: 10,
                group: 11,
                session: 11,
            },
            Process {
                pid: 12,
                parent: 1,
                group: 12,
                session: 10,
            },
            Process {
                pid: 13,
                parent: 1,
                group: 10,
                session: 13,
            },
            Process {
                pid: 14,
                parent: 1,
                group: 14,
                session: -1,
            },
        ] {
            assert!(exited_census(10, None, &[process]).is_err());
        }
        assert!(
            exited_census(
                10,
                Some(20),
                &[Process {
                    pid: 21,
                    parent: 1,
                    group: 20,
                    session: 21
                }]
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn reaped_root_with_a_finite_remaining_session_child_is_preserved() {
        // The owned job naturally exits. No kill, user pane or background service
        // is involved, and the test waits until its session is genuinely empty.
        let mut root=command(r#"(end=$((SECONDS+3)); while ((SECONDS<end)); do :; done) & printf '%s\n' "$!"; exit 0"#).spawn().unwrap();
        let pid = root.id().unwrap();
        let mut lines = BufReader::new(root.stdout.take().unwrap()).lines();
        let job: u32 = lines.next_line().await.unwrap().unwrap().parse().unwrap();
        root.wait().await.unwrap();
        let info = json!({"pane_id":"p","shell_pid":pid});
        assert!(process_snapshot(&info, "p").await.is_err());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while !super::super::root_has_exited(Some(i64::from(job)))
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(super::super::root_has_exited(Some(i64::from(job))));
        assert!(process_snapshot(&info, "p").await.is_ok());
    }
}
