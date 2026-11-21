//! Collection is deliberately confined to the companion store and exact managed
//! configuration locations. Session archives and native credentials are not GC.
use super::*;
use crate::db::Db;
use serde_json::Value;
use std::{
    collections::HashSet,
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const LIMIT: usize = 512;
const BATCH: usize = 8;

fn read_json(path: &Path) -> Result<Option<Value>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 1024 * 1024,
        "Uncertain companion reference file"
    );
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn integrations(data: &Path) -> Result<HashSet<PathBuf>> {
    let Some(value) = read_json(&data.join("runtime/hfb/integrations.json"))? else {
        return Ok(HashSet::new());
    };
    let paths: Vec<PathBuf> = serde_json::from_value(value)?;
    ensure!(
        paths.len() <= LIMIT
            && paths
                .iter()
                .all(|p| p.is_absolute() && p.ends_with(".pi/mcp.json")),
        "Invalid integration locations"
    );
    Ok(paths.into_iter().collect())
}

/// Remember a location before publishing its command. Keep it after project
/// deletion: Pi's persistent configuration can still launch that executable.
pub fn remember_integration(data: &Path, path: &Path) -> Result<()> {
    let lock = store_lock(data)?;
    lock.lock_exclusive()?;
    let mut paths = integrations(data)?;
    ensure!(
        path.is_absolute() && path.ends_with(".pi/mcp.json"),
        "Invalid integration location"
    );
    paths.insert(path.into());
    ensure!(paths.len() <= LIMIT, "Too many integration locations");
    crate::config::write_private(
        &data.join("runtime/hfb/integrations.json"),
        &serde_json::to_vec(&paths)?,
    )
}

#[derive(Default)]
struct Roots {
    paths: HashSet<PathBuf>,
    literal_arguments: Vec<String>,
    protocols: HashSet<u64>,
    rollback_digest: Option<String>,
}

impl Roots {
    fn config(&mut self, path: &Path) -> Result<()> {
        if let Some(value) = read_json(path)?
            && let Some(entry) = value.get("mcpServers").and_then(|v| v.get("herdr_feishu"))
        {
            let command = entry["command"]
                .as_str()
                .context("Invalid managed MCP command")?;
            self.paths.insert(command.into());
        }
        Ok(())
    }

    fn load(data: &Path, db: &Db, global: &Path) -> Result<Self> {
        let mut roots = Self::default();
        let mut locations = integrations(data)?;
        db.sync(|c| {
            let mut stmt =
                c.prepare("SELECT id,path,generation,terminal,execution,extra_args FROM projects
                    UNION ALL
                    SELECT p.id,p.path,w.generation,json_extract(w.execution,'$.terminal_id'),w.execution,'[]'
                    FROM manager_workers w JOIN projects p ON p.id=w.project_id
                    WHERE w.state!='closed' AND json_type(w.execution)='object'")?;
            let mut rows = stmt.query([])?;
            let mut count = 0;
            while let Some(row) = rows.next()? {
                count += 1;
                ensure!(count <= LIMIT, "Too many projects for companion collection");
                let project: String = row.get(0)?;
                let path: String = row.get(1)?;
                let generation: String = row.get(2)?;
                let terminal: Option<String> = row.get(3)?;
                // Include every known project, even if its agent selection changed.
                ensure!(
                    Path::new(&path).is_absolute(),
                    "Invalid project integration directory"
                );
                locations.insert(PathBuf::from(path).join(".pi/mcp.json"));
                // Explicit launch arguments can still run after retirement.
                // Generated placeholders resolve to the current companion.
                let arguments: Vec<String> = serde_json::from_str(&row.get::<_, String>(5)?)?;
                roots.literal_arguments.extend(arguments);
                if terminal.is_none() {
                    continue;
                }
                for component in [&project, &generation] {
                    ensure!(
                        !component.is_empty()
                            && Path::new(component).components().count() == 1
                            && matches!(
                                Path::new(component).components().next(),
                                Some(std::path::Component::Normal(_))
                            ),
                        "Invalid active generation"
                    );
                }
                let execution: Value = serde_json::from_str(&row.get::<_, String>(4)?)?;
                if let Some(path) = execution["companion"].as_str() {
                    roots.paths.insert(path.into());
                } else {
                    // Old Agents can launch additional MCP children without a
                    // lease. Keep their whole protocol until the terminal retires.
                    roots.protocols.insert(
                        execution["api_protocol"]
                            .as_u64()
                            .context("Unknown active execution protocol")?,
                    );
                }
                roots.config(
                    &data
                        .join("sessions")
                        .join(project)
                        .join(generation)
                        .join("mcp.json"),
                )?;
            }
            Ok(())
        })?;
        ensure!(locations.len() <= LIMIT, "Too many integration locations");
        for path in &locations {
            roots.config(path)?;
        }
        // Seed pre-GC installations before any later project deletion.
        crate::config::write_private(
            &data.join("runtime/hfb/integrations.json"),
            &serde_json::to_vec(&locations)?,
        )?;
        roots.config(global)?;
        roots.rollback_digest = crate::update::rollback_companion_digest(data)?;
        Ok(roots)
    }
}

fn entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let entries = fs::read_dir(path)?
        .take(LIMIT + 1)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= LIMIT,
        "Companion collection enumeration limit"
    );
    Ok(entries)
}

fn owned_directory(path: &Path, owner: &fs::Metadata) -> Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != owner.uid() || meta.permissions().mode() & 0o777 != 0o700 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn namespace(name: &str) -> Option<u64> {
    let (version, protocol) = name.rsplit_once("-api")?;
    if semver::Version::parse(version).ok()?.to_string() != version {
        return None;
    }
    let number = protocol.parse::<u64>().ok()?;
    (number.to_string() == protocol).then_some(number)
}

/// A successful no-holder result is specifically exit 1 with empty stdout and
/// stderr. Warnings (including visibility/permission errors), unavailable tools,
/// unexpected output and timeouts all fail closed.
fn unused(path: &Path) -> Result<bool> {
    #[cfg(target_os = "macos")]
    let tool = "/usr/sbin/lsof";
    #[cfg(not(target_os = "macos"))]
    let tool = "lsof";
    let mut child = Command::new(tool)
        .args(["-nP", "-t", "--"])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Companion in-use check timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .context("Missing lsof output")?
        .take(65536)
        .read_to_string(&mut stdout)?;
    child
        .stderr
        .take()
        .context("Missing lsof errors")?
        .take(65536)
        .read_to_string(&mut stderr)?;
    no_holders(status.code(), &stdout, &stderr)
}

fn no_holders(code: Option<i32>, stdout: &str, stderr: &str) -> Result<bool> {
    ensure!(
        stderr.is_empty(),
        "Companion in-use check reported an error"
    );
    if code == Some(1) && stdout.is_empty() {
        return Ok(true);
    }
    ensure!(
        code == Some(0)
            && !stdout.is_empty()
            && stdout
                .lines()
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
        "Uncertain companion in-use result"
    );
    Ok(false)
}

/// Runs only on a blocking worker. Lock order is updater -> store -> entry.
/// Take the state snapshot under the store lock, before checking entry leases.
pub fn collect(data: &Path, db: &Db) -> Result<usize> {
    let home = directories::BaseDirs::new().context("Missing home directory")?;
    collect_with(
        data,
        db,
        &home.home_dir().join(".gemini/config/mcp_config.json"),
        unused,
    )
}

fn collect_with(
    data: &Path,
    db: &Db,
    global: &Path,
    is_unused: impl Fn(&Path) -> Result<bool>,
) -> Result<usize> {
    private_directory(&data.join("updates"))?;
    let update = lock_file(&data.join("updates/update.lock"))?;
    update.try_lock_exclusive()?;
    let lock = store_lock(data)?;
    lock.try_lock_exclusive()?;
    let roots = Roots::load(data, db, global)?;
    let root = data.join("runtime/hfb");
    let owner = fs::metadata(data)?;
    let mut candidates = Vec::new();
    let versions = entries(&root)?;
    let mut enumerated = versions.len();
    for version in versions {
        let Some(protocol) = namespace(&version.file_name().to_string_lossy()) else {
            continue;
        };
        if !owned_directory(&version.path(), &owner)? || roots.protocols.contains(&protocol) {
            continue;
        }
        for digest in entries(&version.path())? {
            enumerated += 1;
            ensure!(enumerated <= LIMIT, "Too many companion entries");
            let name = digest.file_name().to_string_lossy().into_owned();
            if name.len() != 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || !owned_directory(&digest.path(), &owner)?
                || roots.rollback_digest.as_ref() == Some(&name)
            {
                continue;
            }
            let executable = digest.path().join("hfb");
            let literal_reference = executable.to_str().is_some_and(|path| {
                roots
                    .literal_arguments
                    .iter()
                    .any(|argument| argument.contains(path))
            });
            if !roots.paths.contains(&executable) && !literal_reference {
                candidates.push(executable);
            }
        }
    }
    let mut removed = 0;
    // Rotate through candidates so a busy/unknown first entry cannot starve GC.
    candidates.sort();
    let offset = read_json(&root.join("cursor.json"))?
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let count = candidates.len().min(BATCH);
    for i in 0..count {
        let path = &candidates[(offset.wrapping_add(i)) % candidates.len()];
        let directory = path.parent().context("Missing companion directory")?;
        if entries(directory)?
            .iter()
            .any(|e| e.file_name() != "hfb" && e.file_name() != ".lease")
        {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(path) else {
            continue;
        };
        if !meta.is_file()
            || meta.file_type().is_symlink()
            || meta.len() > 256 * 1024 * 1024
            || validate(path).is_err()
        {
            continue;
        }
        let lease = match lock_file(&directory.join(".lease")) {
            Ok(lease) => lease,
            Err(_) => continue,
        };
        if lease.try_lock_exclusive().is_err() || !is_unused(path)? {
            continue;
        }
        // Never recursively delete: concurrent unknown additions remain intact.
        fs::remove_file(path)?;
        fs::remove_file(directory.join(".lease"))?;
        let _ = fs::remove_dir(directory);
        removed += 1;
    }
    crate::config::write_private(
        &root.join("cursor.json"),
        &serde_json::to_vec(&(offset.wrapping_add(count)))?,
    )?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Db, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
        let global = dir.path().join("global.json");
        (dir, db, global)
    }

    fn publish(data: &Path, bytes: &[u8], protocol: u64) -> Retained {
        let source = data.join("source");
        fs::write(&source, bytes).unwrap();
        pin(data, &source, "0.4.6", protocol).unwrap()
    }

    fn command(path: &Path, executable: &Path) {
        crate::config::private_dir(path.parent().unwrap()).unwrap();
        crate::config::write_private(
            path,
            &serde_json::to_vec(&json!({
                "mcpServers":{"herdr_feishu":{"command":executable,"args":["mcp"]}}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn active_and_persistent_roots_expire_but_archives_remain() {
        let (dir, db, global) = fixture();
        let data = dir.path();
        fs::create_dir(data.join("project")).unwrap();
        let p = db
            .create(crate::db::ProjectInput {
                name: "fixture".into(),
                path: data.join("project").to_string_lossy().into(),
                agent: "pi".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let retained = publish(data, b"active-generation", 6);
        let executable = retained.path().to_owned();
        let archive = data.join("sessions").join(&p.id).join("old/mcp.json");
        command(&archive, &executable);
        let pi = PathBuf::from(&p.path).join(".pi/mcp.json");
        remember_integration(data, &pi).unwrap();
        db.save_execution(
            &p.id,
            json!({"terminal_id":"terminal","companion":executable,"api_protocol":6}),
            "old",
        )
        .await
        .unwrap();
        drop(retained);
        assert_eq!(collect_with(data, &db, &global, |_| Ok(true)).unwrap(), 0);
        // Neither pause nor terminal takeover retires the execution.
        db.sync(|c| {
            c.execute("UPDATE projects SET paused=1", [])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(collect_with(data, &db, &global, |_| Ok(true)).unwrap(), 0);
        command(&pi, &executable);
        db.set_session(&p.id, None, None, String::new())
            .await
            .unwrap();
        assert_eq!(collect_with(data, &db, &global, |_| Ok(true)).unwrap(), 0);
        // The remembered Pi location remains operational after project deletion.
        db.sync(|c| {
            c.execute("DELETE FROM projects", [])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(collect_with(data, &db, &global, |_| Ok(true)).unwrap(), 0);
        fs::remove_file(pi).unwrap();
        command(&global, &executable);
        assert_eq!(collect_with(data, &db, &global, |_| Ok(true)).unwrap(), 0);
        fs::remove_file(global.clone()).unwrap();
        assert_eq!(collect_with(data, &db, &global, |_| Ok(true)).unwrap(), 1);
        assert!(!executable.exists());
        assert!(
            archive.exists(),
            "Archived credentials/configuration are preserved, not roots"
        );
    }

    #[tokio::test]
    async fn legacy_protocol_roots_and_uncertainty_fail_closed() {
        let (dir, db, global) = fixture();
        let retained = publish(dir.path(), b"legacy", 4);
        let executable = retained.path().to_owned();
        drop(retained);
        let p = db
            .create(crate::db::ProjectInput {
                name: "legacy".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        db.save_execution(
            &p.id,
            json!({"terminal_id":"terminal","api_protocol":4}),
            "old",
        )
        .await
        .unwrap();
        assert_eq!(
            collect_with(dir.path(), &db, &global, |_| Ok(true)).unwrap(),
            0
        );
        db.save_execution(&p.id, json!({"terminal_id":"terminal"}), "old")
            .await
            .unwrap();
        assert!(collect_with(dir.path(), &db, &global, |_| Ok(true)).is_err());
        assert!(executable.exists());
        db.set_session(&p.id, None, None, String::new())
            .await
            .unwrap();
        for arguments in [
            vec![executable.to_string_lossy().into_owned()],
            vec![
                "-c".into(),
                format!(
                    "mcp_servers.herdr_feishu.command=\"{}\"",
                    executable.display()
                ),
            ],
            vec![
                "sh".into(),
                "-c".into(),
                format!("exec '{}' mcp", executable.display()),
            ],
        ] {
            db.sync(|c| {
                c.execute(
                    "UPDATE projects SET extra_args=?",
                    [serde_json::to_string(&arguments)?],
                )?;
                Ok(())
            })
            .unwrap();
            assert_eq!(
                collect_with(dir.path(), &db, &global, |_| Ok(true)).unwrap(),
                0
            );
        }
        db.sync(|c| {
            c.execute("UPDATE projects SET extra_args='[]'", [])?;
            Ok(())
        })
        .unwrap();
        assert!(
            collect_with(dir.path(), &db, &global, |_| anyhow::bail!(
                "OS check failed"
            ))
            .is_err()
        );
        assert!(executable.exists());
        assert_eq!(
            collect_with(dir.path(), &db, &global, |_| Ok(true)).unwrap(),
            1
        );
    }

    #[test]
    fn publication_lease_and_collection_are_serialized() {
        let (dir, db, global) = fixture();
        let data = dir.path();
        let retained = publish(data, b"publication", 6);
        let executable = retained.path().to_owned();
        // Published but not yet referenced: the lease alone must protect it.
        assert_eq!(
            collect_with(data, &db, &global, |_| panic!("leased entry checked")).unwrap(),
            0
        );
        drop(retained);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let db_ref = &db;
            let global_ref = &global;
            let collector = scope.spawn(move || {
                collect_with(data, db_ref, global_ref, |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(true)
                })
                .unwrap()
            });
            entered_rx.recv().unwrap();
            let publisher = scope.spawn(|| {
                started_tx.send(()).unwrap();
                let retained = publish(data, b"publication", 6);
                published_tx.send(()).unwrap();
                retained
            });
            started_rx.recv().unwrap();
            let blocked = matches!(
                published_rx.recv_timeout(Duration::from_millis(50)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            );
            release_tx.send(()).unwrap();
            assert_eq!(collector.join().unwrap(), 1);
            let retained = publisher.join().unwrap();
            assert!(
                blocked,
                "Publication must wait while collection holds the store lock"
            );
            assert_eq!(retained.path(), executable);
            validate(retained.path()).unwrap();
            assert_eq!(
                collect_with(data, &db, &global, |_| panic!("leased entry checked")).unwrap(),
                0
            );
        });
        assert_eq!(collect_with(data, &db, &global, |_| Ok(true)).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn inherited_lease_survives_exec_of_an_unmodified_child() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let (dir, db, global) = fixture();
        let retained = publish(dir.path(), b"old-companion", 4);
        let executable = retained.path().to_owned();
        let mut command = Command::new("/bin/sh");
        // Clear CLOEXEC only in this child. Other tests can spawn concurrently
        // without accidentally inheriting this fixture's companion lease.
        unsafe {
            command.pre_exec(move || {
                retained
                    .inherit_across_exec()
                    .map_err(std::io::Error::other)
            });
        }
        let mut child = command
            .args(["-c", "echo ready; exec /bin/sleep 60"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        drop(command);
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");
        let kept = collect_with(dir.path(), &db, &global, |_| Ok(true));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            kept.unwrap(),
            0,
            "The child inherited the lease across exec"
        );
        assert_eq!(
            collect_with(dir.path(), &db, &global, |_| Ok(true)).unwrap(),
            1
        );
        assert!(!executable.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn os_check_protects_a_process_without_a_lease() {
        let (dir, db, global) = fixture();
        let retained = pin(dir.path(), Path::new("/bin/sleep"), "0.4.4", 4).unwrap();
        let executable = retained.path().to_owned();
        let mut child = Command::new(&executable).arg("60").spawn().unwrap();
        // Pin's descriptor is CLOEXEC; this represents an existing legacy child.
        drop(retained);
        let kept = collect_with(dir.path(), &db, &global, unused);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(kept.unwrap(), 0);
        assert_eq!(collect_with(dir.path(), &db, &global, unused).unwrap(), 1);
    }

    #[test]
    fn unknown_entries_and_symlinks_are_preserved() {
        let (dir, db, global) = fixture();
        let retained = publish(dir.path(), b"unknown-neighbor", 6);
        let executable = retained.path().to_owned();
        let neighbor = executable.with_file_name("user-file");
        fs::write(&neighbor, b"preserve").unwrap();
        drop(retained);
        assert_eq!(
            collect_with(dir.path(), &db, &global, |_| Ok(true)).unwrap(),
            0
        );
        fs::remove_file(neighbor).unwrap();
        #[cfg(unix)]
        {
            fs::remove_file(&executable).unwrap();
            std::os::unix::fs::symlink(dir.path().join("source"), &executable).unwrap();
            assert_eq!(
                collect_with(dir.path(), &db, &global, |_| Ok(true)).unwrap(),
                0
            );
            assert!(
                fs::symlink_metadata(executable)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn only_unambiguous_no_holder_result_allows_deletion() {
        assert!(no_holders(Some(1), "", "").unwrap());
        assert!(!no_holders(Some(0), "123\n", "").unwrap());
        for (code, out, err) in [
            (Some(1), "", "permission denied"),
            (Some(2), "", ""),
            (None, "", ""),
            (Some(0), "", ""),
            (Some(1), "123\n", ""),
            (Some(0), "unexpected", ""),
            (Some(0), "123\n", "warning"),
        ] {
            assert!(no_holders(code, out, err).is_err());
        }
    }
}
