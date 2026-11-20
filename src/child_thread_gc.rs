//! Reversible native archival; never deletes stores or terminates a live worker.
use crate::{child_threads, codex_tree, db};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

#[derive(Serialize)]
pub struct Entry {
    project: String,
    thread_prefix: String,
    decision: child_threads::Decision,
    result: &'static str,
    verified_archived: usize,
}
#[derive(Default, Serialize)]
pub struct Report {
    pub examined: usize,
    pub eligible: usize,
    pub archived: usize,
    pub entries: Vec<Entry>,
    pub unavailable_projects: usize,
}
#[derive(Clone)]
struct Target {
    project: String,
    name: String,
    cwd: PathBuf,
    root: String,
}

fn readonly(path: &Path) -> Result<Connection> {
    ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_file(),
        "Not a regular metadata store"
    );
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    c.busy_timeout(Duration::from_millis(100))?;
    c.pragma_update(None, "query_only", true)?;
    Ok(c)
}
fn targets(bridge: &Path) -> Result<Vec<Target>> {
    let c = readonly(bridge)?;
    ensure!(
        child_threads::protected_roots(&c)?.len() <= 512,
        "Native root inventory exceeds bound"
    );
    let mut s = c.prepare("SELECT DISTINCT p.id,p.name,p.path,e.native_session FROM projects p JOIN work_executions e ON e.project_id=p.id WHERE e.agent='codex' AND e.native_session!='' UNION SELECT id,name,path,native_session FROM projects WHERE agent='codex' AND native_session!=''")?;
    Ok(s.query_map([], |r| {
        Ok(Target {
            project: r.get(0)?,
            name: r.get(1)?,
            cwd: PathBuf::from(r.get::<_, String>(2)?),
            root: r.get(3)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?)
}

// lsof only reads open-descriptor metadata in this exact native lock directory.
// An empty result is useful only together with native archive's own atomic
// cross-process writer conflict checks (verified in both pinned CLI versions).
async fn writers(home: &Path) -> Result<HashSet<String>> {
    let dir = home.join("thread-writer-locks");
    ensure!(
        std::fs::symlink_metadata(&dir)?.is_dir(),
        "Native writer coverage unavailable"
    );
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new("/usr/sbin/lsof")
            .args(["-nP", "-F", "pn", "+D"])
            .arg(&dir)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        out.stderr.is_empty()
            && matches!(out.status.code(), Some(0 | 1))
            && out.stdout.len() < 524288,
        "Native writer coverage incomplete"
    );
    parse_writers(&dir, &out.stdout)
}
fn parse_writers(dir: &Path, data: &[u8]) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    // lsof emits a process record followed by one or more descriptor/name
    // pairs. Reject partial output instead of treating missing names as idle.
    let mut state = 0;
    ensure!(
        data.is_empty() || data.ends_with(b"\n"),
        "Truncated writer output"
    );
    for line in std::str::from_utf8(data)?.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            ensure!(
                matches!(state, 0 | 3) && pid.parse::<u32>()? != 0,
                "Invalid writer process record"
            );
            state = 1;
        } else if let Some(descriptor) = line.strip_prefix('f') {
            ensure!(matches!(state, 1 | 3), "Unexpected writer descriptor");
            let _: u32 = descriptor.parse()?;
            state = 2;
        } else if let Some(name) = line.strip_prefix('n') {
            ensure!(state == 2, "Missing writer descriptor");
            let path = Path::new(name);
            ensure!(
                path.parent() == Some(dir),
                "Writer descriptor escaped native scope"
            );
            let id = path
                .file_stem()
                .and_then(|v| v.to_str())
                .context("Invalid writer descriptor")?;
            uuid::Uuid::parse_str(id)?;
            ensure!(
                path.extension().is_some_and(|s| s == "lock"),
                "Unknown writer descriptor"
            );
            ids.insert(id.to_owned());
            state = 3;
        } else {
            anyhow::bail!("Incomplete native writer descriptor output");
        }
    }
    ensure!(matches!(state, 0 | 3), "Incomplete writer record");
    Ok(ids)
}
fn members<'a>(
    records: &'a [codex_tree::metadata::Record],
    id: &str,
) -> Vec<&'a codex_tree::metadata::Record> {
    let mut found = HashSet::from([id.to_owned()]);
    loop {
        let before = found.len();
        for r in records {
            if found.contains(&r.parent) {
                found.insert(r.id.clone());
            }
        }
        if found.len() == before {
            break;
        }
    }
    records.iter().filter(|r| found.contains(&r.id)).collect()
}
fn inspect(
    bridge: &Path,
    _home: &Path,
    target: &Target,
    records: &[codex_tree::metadata::Record],
    id: &str,
    owners: Option<&HashSet<String>>,
) -> Result<child_threads::Decision> {
    let mut b = readonly(bridge)?;
    let b = b.transaction()?;
    let mut evidence = Vec::new();
    for r in members(records, id) {
        let pending = r
            .queued_count
            .context("Native queue snapshot unavailable")?;
        let aliases = if r.name != r.id {
            vec![r.name.as_str()]
        } else {
            vec![]
        };
        evidence.push(child_threads::Evidence {
            metadata_verified: true,
            writer: match owners {
                Some(o) if o.contains(&r.id) => child_threads::WriterObservation::Live,
                Some(_) => child_threads::WriterObservation::Absent,
                None => child_threads::WriterObservation::Unknown,
            },
            native_pending: pending,
            updated_at_ms: r.updated_at_ms,
            bridge: child_threads::bridge_evidence(&b, &target.project, &r.id, &aliases)?,
        });
    }
    Ok(child_threads::subtree_decision(&evidence, true, db::now()))
}

/// Fresh, local stdio server. It never loads/resumes/starts a business thread;
/// its ThreadManager cannot shutdown the original CLI's in-memory work. Native
/// archive independently refuses writers held by any other native process.
async fn native_archive(cli: &Path, cwd: &Path, thread: &str) -> Result<()> {
    uuid::Uuid::parse_str(thread)?;
    let mut child = Command::new(cli)
        .args(["app-server", "--listen", "stdio://"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().context("No native metadata input")?;
    let mut output =
        BufReader::new(child.stdout.take().context("No native metadata output")?).take(2_097_152);
    let operation = async {
        for (id, method, params) in [
            (
                1,
                "initialize",
                json!({"clientInfo":{"name":"dauerbot-child-collection","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
            ),
            (2, "thread/archive", json!({"threadId":thread})),
        ] {
            input
                .write_all(
                    format!("{}\n", json!({"id":id,"method":method,"params":params})).as_bytes(),
                )
                .await?;
            input.flush().await?;
            let mut matched = false;
            for _ in 0..64 {
                let mut line = String::new();
                let n = output.read_line(&mut line).await?;
                ensure!(
                    n > 0 && n <= 2_097_152,
                    "Native archive response unavailable"
                );
                let value: Value = serde_json::from_str(&line)?;
                if value["id"] == id {
                    ensure!(
                        value.get("error").is_none() && value.get("result").is_some(),
                        "Native archive refused or conflicted"
                    );
                    matched = true;
                    break;
                }
            }
            ensure!(
                matched,
                "Native archive response exceeded notification budget"
            );
            if id == 1 {
                input.write_all(b"{\"method\":\"initialized\"}\n").await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(8), operation).await;
    drop(input);
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    result??;
    Ok(())
}
fn audit(dir: &Path, entry: &Entry) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir)?;
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(dir.join("child-thread-audit.jsonl"))?;
    serde_json::to_writer(
        &mut file,
        &json!({"at":db::now(),"entry":entry,"recovery":"Native thread/unarchive; history retained, no hard delete"}),
    )?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

/// Shared by the existing hourly GC and the explicit host maintenance example.
/// The bridge connection is always read-only; native archive is the sole write.
pub async fn collect(bridge: &Path, audit_dir: &Path, apply: bool, limit: usize) -> Result<Report> {
    collect_for_project(bridge, audit_dir, apply, limit, None).await
}

pub async fn collect_for_project(
    bridge: &Path,
    audit_dir: &Path,
    apply: bool,
    limit: usize,
    project: Option<&Path>,
) -> Result<Report> {
    let home = codex_tree::native_home()?;
    let cli = Path::new("codex");
    let mut report = Report::default();
    let mut seen = HashSet::new();
    for target in targets(bridge)?
        .into_iter()
        .filter(|target| project.is_none_or(|path| target.cwd == path))
    {
        let version = match codex_tree::version(cli, &target.cwd).await {
            Ok(v) if codex_tree::metadata::verified(&v) => v,
            _ => {
                report.unavailable_projects += 1;
                continue;
            }
        };
        let snapshot = match codex_tree::metadata::read_with_queue(&home, &target.root, &version) {
            Ok(v) => v,
            Err(_) => {
                report.unavailable_projects += 1;
                continue;
            }
        };
        // Descendants before ancestors; still inspect the full recursive scope.
        for r in snapshot.records.iter().rev() {
            if r.archived || !seen.insert(r.id.clone()) {
                continue;
            }
            report.examined += 1;
            let owners = writers(&home).await.ok();
            let decision = inspect(
                bridge,
                &home,
                &target,
                &snapshot.records,
                &r.id,
                owners.as_ref(),
            )
            .unwrap_or(child_threads::Decision::Retain(vec![
                child_threads::RetainReason::UnverifiedMetadata,
            ]));
            let mut entry = Entry {
                project: target.name.clone(),
                thread_prefix: r.id.chars().take(13).collect(),
                decision,
                result: "retained",
                verified_archived: 0,
            };
            if entry.decision == child_threads::Decision::Archive {
                report.eligible += 1;
                entry.result = "eligible_dry_run";
                if apply
                    && members(&snapshot.records, &r.id)
                        .iter()
                        .filter(|entry| !entry.archived)
                        .count()
                        <= limit.min(128).saturating_sub(report.archived)
                {
                    // Re-read the native scope and thread-specific bridge/queue
                    // references immediately before admission. A resumed writer
                    // racing this check is rejected by native archive itself.
                    let fresh =
                        codex_tree::metadata::read_with_queue(&home, &target.root, &version)?;
                    let owners = writers(&home).await.ok();
                    let checked = inspect(
                        bridge,
                        &home,
                        &target,
                        &fresh.records,
                        &r.id,
                        owners.as_ref(),
                    )?;
                    let count = members(&fresh.records, &r.id)
                        .iter()
                        .filter(|entry| !entry.archived)
                        .count();
                    if count > limit.min(128).saturating_sub(report.archived) {
                        entry.result = "fresh_scope_exceeds_budget";
                    } else if checked != child_threads::Decision::Archive {
                        entry.decision = checked;
                        entry.result = "changed_before_admission";
                    } else {
                        entry.result = "archive_submitting";
                        audit(audit_dir, &entry)?;
                        let accepted = native_archive(cli, &target.cwd, &r.id).await.is_ok();
                        let after =
                            codex_tree::metadata::read_with_queue(&home, &target.root, &version)?;
                        // An empty successful RPC is NOT proof every descendant
                        // moved. Count only independently re-read archive bits.
                        let scope: HashSet<_> = members(&fresh.records, &r.id)
                            .into_iter()
                            .filter(|n| !n.archived)
                            .map(|n| n.id.as_str())
                            .collect();
                        let newly_archived: Vec<_> = after
                            .records
                            .iter()
                            .filter(|n| n.archived && scope.contains(n.id.as_str()))
                            .collect();
                        for n in &newly_archived {
                            seen.insert(n.id.clone());
                        }
                        entry.verified_archived = newly_archived.len();
                        report.archived += entry.verified_archived;
                        entry.result =
                            if entry.verified_archived == scope.len() && !scope.is_empty() {
                                "archived_verified"
                            } else if entry.verified_archived > 0 {
                                "partially_archived_verified"
                            } else if accepted {
                                "archive_not_verified"
                            } else {
                                "native_conflict_or_failure"
                            };
                    }
                }
            }
            audit(audit_dir, &entry)?;
            report.entries.push(entry);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn archive_uses_only_the_native_initialized_archive_sequence() {
        use std::os::unix::fs::PermissionsExt;

        let folder = tempfile::tempdir().unwrap();
        let cli = folder.path().join("codex-fixture");
        std::fs::write(&cli, r#"#!/bin/sh
set -eu
[ "$*" = "app-server --listen stdio://" ]
read -r first
case "$first" in *'"id":1,"method":"initialize"'*) ;; *) exit 1 ;; esac
echo '{"id":1,"result":{}}'
read -r second
[ "$second" = '{"method":"initialized"}' ]
read -r third
case "$third" in *'"id":2,"method":"thread/archive","params":{"threadId":"00000000-0000-4000-8000-000000000002"}'*) ;; *) exit 1 ;; esac
echo '{"id":2,"result":{}}'
if read -r extra; then exit 1; fi
"#).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        native_archive(&cli, folder.path(), "00000000-0000-4000-8000-000000000002")
            .await
            .unwrap();
    }

    #[test]
    fn writer_metadata_never_treats_incomplete_or_other_paths_as_absent() {
        let dir = Path::new("/native/thread-writer-locks");
        let id = "00000000-0000-4000-8000-000000000002";
        assert!(
            parse_writers(
                dir,
                format!("p12\nf5\nn/native/thread-writer-locks/{id}.lock\nf9\nn/native/thread-writer-locks/{id}.lock\np73\nf7\nn/native/thread-writer-locks/{id}.lock\n").as_bytes()
            )
            .unwrap()
            .contains(id)
        );
        for value in [
            "p12\nf4\nn/other/escape.lock\n",
            "pnot-a-pid\n",
            "unexpected\n",
            "p12\nf4\n",
            "p12\nn/other/escape.lock\n",
        ] {
            assert!(parse_writers(dir, value.as_bytes()).is_err());
        }
    }
}
