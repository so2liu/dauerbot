//! Project-local delegation resources. Never claim ownership of user content.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Component, Path},
};

const BEGIN: &str = "<!-- dauerbot:delegation:begin -->";
const END: &str = "<!-- dauerbot:delegation:end -->";
const LIMIT: u64 = 1_000_000;
const SKILL: &[u8] = include_bytes!("../assets/herdr-cli-subagents/SKILL.md");
const POLICY: &str = "1. For Herdr delegation, read the bundled project-local skill at `.dauerbot/skills/herdr-cli-subagents/SKILL.md` and follow its verified-parent, reporting and cleanup instructions.\n2. Delegate substantial or time-consuming work so the main Agent stays responsive to user steering. Prefer responsive native subagents for same-CLI work; use Herdr for cross-CLI/model collaboration or when native delegation is insufficient. Handle small immediate tasks directly, and keep the main Agent responsible for acceptance and delivery.";

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn name(path: &Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(CString::new(
        path.file_name().context("Missing file name")?.as_bytes(),
    )?)
}

fn open_at(dir: &fs::File, name: &CString, flags: i32) -> std::io::Result<fs::File> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

// Resolve every component without following links; final relative operations
// remain anchored to the opened directory even when a parent is renamed.
fn open_directory(path: &Path) -> Result<fs::File> {
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    ensure!(path.is_absolute(), "Expected an absolute project path");
    let mut dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => {
                dir = open_at(
                    &dir,
                    &CString::new(part.as_bytes())?,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                )
                .with_context(|| {
                    format!("Cannot open project resource directory {}", path.display())
                })?;
            }
            _ => bail!("Unexpected project path component"),
        }
    }
    Ok(dir)
}

fn directory(path: &Path) -> Result<()> {
    let dir = open_directory(path.parent().context("Missing parent directory")?)?;
    let name = name(path)?;
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    open_at(&dir, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    dir.sync_all()?;
    Ok(())
}

fn read(path: &Path) -> Result<Option<(Vec<u8>, fs::Permissions)>> {
    let dir = open_directory(path.parent().context("Missing parent directory")?)?;
    read_at(&dir, &name(path)?)
}

fn read_at(dir: &fs::File, name: &CString) -> Result<Option<(Vec<u8>, fs::Permissions)>> {
    let mut file = match open_at(dir, name, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= LIMIT,
        "The project instruction resource must be a regular file no larger than one megabyte"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "The project instruction resource exceeds one megabyte"
    );
    Ok(Some((bytes, meta.permissions())))
}

fn write(path: &Path, bytes: &[u8], permissions: Option<fs::Permissions>) -> Result<()> {
    write_checked(path, bytes, permissions, |_| Ok(()))
}

fn write_checked(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
    check: impl FnOnce(&fs::File) -> Result<()>,
) -> Result<()> {
    let dir = open_directory(path.parent().context("Missing project directory")?)?;
    let temp = CString::new(format!(".delegation-{}.tmp", uuid::Uuid::new_v4()))?;
    let result: Result<()> = (|| {
        let mut file = open_at(&dir, &temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        check(&dir)?;
        if unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                temp.as_ptr(),
                dir.as_raw_fd(),
                name(path)?.as_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
    }
    result.with_context(|| format!("Cannot publish project resource {}", path.display()))
}

fn block(file: &str) -> Result<String> {
    let replacement = format!("{BEGIN}\n{POLICY}\n{END}");
    let begins: Vec<_> = file.match_indices(BEGIN).map(|(at, _)| at).collect();
    let ends: Vec<_> = file.match_indices(END).map(|(at, _)| at).collect();
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(format!(
            "{file}{}{replacement}\n",
            if file.is_empty() || file.ends_with("\n\n") {
                ""
            } else if file.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            }
        )),
        ([begin], [end]) if begin < end => Ok(format!(
            "{}{replacement}{}",
            &file[..*begin],
            &file[*end + END.len()..]
        )),
        _ => bail!(
            "AGENTS.md has ambiguous Dauerbot delegation markers; repair or remove only that block before starting this project"
        ),
    }
}

/// Serialized per project, including two project records pointing at one folder.
/// Existing live sessions are not restarted or rewritten by this operation.
pub fn ensure(project: &Path) -> Result<()> {
    provision(project, SKILL)
}

fn provision(project: &Path, skill: &[u8]) -> Result<()> {
    let project = fs::canonicalize(project).context("Cannot open the project folder")?;
    let local = project.join(".dauerbot");
    directory(&local)?;
    let lock_path = local.join("delegation.lock");
    let dir = open_directory(&local)?;
    let lock_name = name(&lock_path)?;
    let lock = (|| -> Result<fs::File> {
        // Separate exclusive creation from opening an existing lock. Darwin
        // can report ENOENT while concurrent O_NOFOLLOW creators race.
        for _ in 0..16 {
            match open_at(
                &dir,
                &lock_name,
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            ) {
                Ok(file) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    match open_at(&dir, &lock_name, libc::O_RDWR) {
                        Ok(file) => return Ok(file),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        bail!("The project delegation lock kept changing; retry starting the project")
    })()
    .context("Cannot open the project delegation lock")?;
    ensure!(
        lock.metadata()?.is_file(),
        "The project delegation lock must be a regular file"
    );
    fs2::FileExt::lock_exclusive(&lock)?;
    let agents = project.join("AGENTS.md");
    let previous = read(&agents)?;
    let text = match &previous {
        Some((bytes, _)) => std::str::from_utf8(bytes).context("AGENTS.md must be UTF-8")?,
        None => "",
    };
    let next = block(text)?;
    ensure!(next.len() as u64 <= LIMIT, "AGENTS.md exceeds one megabyte");
    install_skill(&local, skill)?;
    if next.as_bytes() != text.as_bytes() {
        update_agents(&agents, next.as_bytes(), previous)?;
    }
    Ok(())
}

fn update_agents(
    agents: &Path,
    next: &[u8],
    previous: Option<(Vec<u8>, fs::Permissions)>,
) -> Result<()> {
    let expected = previous.as_ref().map(|(bytes, _)| bytes.as_slice());
    write_checked(
        agents,
        next,
        previous.as_ref().map(|(_, mode)| mode.clone()),
        |dir| {
            let current = read_at(dir, &name(agents)?)?;
            ensure!(
                current.as_ref().map(|(bytes, _)| bytes.as_slice()) == expected,
                "AGENTS.md changed during project preparation; its new content was preserved. Retry starting the project"
            );
            Ok(())
        },
    )
}

fn install_skill(local: &Path, contents: &[u8]) -> Result<()> {
    let skills = local.join("skills");
    directory(&skills)?;
    let target = skills.join("herdr-cli-subagents");
    let owner = target.join(".dauerbot-managed.json");
    let skill = target.join("SKILL.md");
    let present = match open_directory(&target) {
        Ok(_) => true,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            false
        }
        Err(error) => return Err(error),
    };
    let wanted = hash(contents);
    if !present {
        return publish_skill(&skills, contents, &wanted);
    }
    let current = read(&skill)?;
    let raw = read(&owner)?.with_context(|| format!(
        "Dauerbot will not replace an unowned skill at {}; move it aside or choose another project folder",
        target.display()
    ))?;
    let accepted: Vec<String> =
        serde_json::from_slice(&raw.0).context("Cannot read the project skill ownership record")?;
    if let Some((bytes, _)) = &current {
        ensure!(
            accepted.contains(&hash(bytes)),
            "The bundled skill at {} was edited; preserve or move it aside before starting this project",
            skill.display()
        );
        if *bytes == contents {
            return Ok(());
        }
    } else {
        ensure!(
            !accepted.is_empty(),
            "Invalid project skill ownership record"
        );
    }
    // Record both hashes before replacing the payload. Either side of a crash
    // remains a known owned version and can complete the update on next launch.
    let expected = current.as_ref().map(|(bytes, _)| bytes.clone());
    let mut hashes = vec![wanted.clone()];
    if let Some((bytes, _)) = current {
        let old = hash(&bytes);
        if old != wanted {
            hashes.push(old);
        }
    }
    write(&owner, &serde_json::to_vec(&hashes)?, None)?;
    replace_skill(&skill, contents, expected.as_deref())?;
    write(&owner, &serde_json::to_vec(&vec![wanted])?, None)
}

fn replace_skill(skill: &Path, contents: &[u8], expected: Option<&[u8]>) -> Result<()> {
    write_checked(skill, contents, None, |dir| {
        let current = read_at(dir, &name(skill)?)?;
        ensure!(
            current.as_ref().map(|(bytes, _)| bytes.as_slice()) == expected,
            "The bundled project skill changed during preparation; its new content was preserved. Retry starting the project"
        );
        Ok(())
    })
}

/// Publish a complete resource directory without replacing even an empty
/// directory belonging to the user. A crash leaves only private staging.
fn publish_skill(skills: &Path, contents: &[u8], wanted: &str) -> Result<()> {
    let stage = skills.join(format!(".herdr-cli-subagents-{}", uuid::Uuid::new_v4()));
    directory(&stage)?;
    let result = (|| {
        write(&stage.join("SKILL.md"), contents, None)?;
        write(
            &stage.join(".dauerbot-managed.json"),
            &serde_json::to_vec(&vec![wanted])?,
            None,
        )?;
        let dir = open_directory(skills)?;
        let source = name(&stage)?;
        let target = CString::new("herdr-cli-subagents")?;
        #[cfg(target_os = "macos")]
        let renamed = unsafe {
            libc::renameatx_np(
                dir.as_raw_fd(),
                source.as_ptr(),
                dir.as_raw_fd(),
                target.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(target_os = "linux")]
        let renamed = unsafe {
            libc::renameat2(
                dir.as_raw_fd(),
                source.as_ptr(),
                dir.as_raw_fd(),
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let renamed: i32 = {
            bail!("Atomic project skill publication is not supported on this platform");
        };
        if renamed < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        dir.sync_all()?;
        Ok(())
    })();
    // Leave private staging on error: resolving its path again for cleanup
    // could follow a concurrently replaced directory into user content.
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_user_bytes_updates_owned_skill_and_does_not_rewrite_identical_files() {
        let root = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(root.path()).unwrap();
        let agents = project.join("AGENTS.md");
        let user = "# 用户规则\r\nKeep my rules.\r\n";
        fs::write(&agents, user).unwrap();
        provision(root.path(), b"first bundled version").unwrap();
        let first = fs::read_to_string(&agents).unwrap();
        assert!(first.starts_with(user));
        assert_eq!(first.matches(BEGIN).count(), 1);
        let with_tail = format!("{first}\nMy final rule.\n");
        fs::write(&agents, &with_tail).unwrap();
        provision(root.path(), b"updated bundled version").unwrap();
        assert_eq!(fs::read_to_string(&agents).unwrap(), with_tail);
        let modified = fs::metadata(&agents).unwrap().modified().unwrap();
        let skill = root
            .path()
            .join(".dauerbot/skills/herdr-cli-subagents/SKILL.md");
        let skill_modified = fs::metadata(&skill).unwrap().modified().unwrap();
        provision(root.path(), b"updated bundled version").unwrap();
        assert_eq!(fs::metadata(agents).unwrap().modified().unwrap(), modified);
        assert_eq!(
            fs::metadata(skill).unwrap().modified().unwrap(),
            skill_modified
        );
    }

    #[test]
    fn collisions_and_malformed_markers_preserve_user_content() {
        let root = tempfile::tempdir().unwrap();
        let skill = root
            .path()
            .join(".dauerbot/skills/herdr-cli-subagents/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, b"user skill").unwrap();
        assert!(
            ensure(root.path())
                .unwrap_err()
                .to_string()
                .contains("unowned")
        );
        assert_eq!(fs::read(&skill).unwrap(), b"user skill");
        assert!(!root.path().join("AGENTS.md").exists());
        fs::remove_dir_all(root.path().join(".dauerbot")).unwrap();
        ensure(root.path()).unwrap();
        fs::write(&skill, b"user edits").unwrap();
        assert!(
            ensure(root.path())
                .unwrap_err()
                .to_string()
                .contains("edited")
        );
        assert_eq!(fs::read(&skill).unwrap(), b"user edits");
        fs::write(root.path().join("AGENTS.md"), BEGIN).unwrap();
        assert!(
            ensure(root.path())
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert_eq!(
            fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
            BEGIN
        );
    }

    #[test]
    fn concurrent_starts_share_one_managed_block() {
        let root = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| ensure(root.path()).unwrap());
            }
        });
        let text = fs::read_to_string(root.path().join("AGENTS.md")).unwrap();
        assert_eq!(text.matches(BEGIN).count(), 1);
        assert_eq!(text.matches(END).count(), 1);
    }

    #[test]
    fn changed_user_file_and_unknown_empty_directory_are_never_replaced() {
        let root = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(root.path()).unwrap();
        let agents = project.join("AGENTS.md");
        fs::write(&agents, b"original rules").unwrap();
        let snapshot = read(&agents).unwrap();
        fs::write(&agents, b"user changed these rules").unwrap();
        assert!(update_agents(&agents, b"generated rules", snapshot).is_err());
        assert_eq!(fs::read(&agents).unwrap(), b"user changed these rules");
        let skill = project.join("SKILL.md");
        fs::write(&skill, b"user changed the skill").unwrap();
        assert!(replace_skill(&skill, b"updated bundle", Some(b"old bundle")).is_err());
        assert_eq!(fs::read(&skill).unwrap(), b"user changed the skill");
        let target = project.join("herdr-cli-subagents");
        fs::create_dir(&target).unwrap();
        assert!(publish_skill(&project, b"bundled resource", "hash").is_err());
        assert!(target.is_dir());
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_inputs_never_change_external_files() {
        use std::os::unix::fs::symlink;
        for location in ["AGENTS.md", ".dauerbot"] {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let file = outside.path().join("rules");
            fs::write(&file, b"user content").unwrap();
            let target = if location == "AGENTS.md" {
                &file
            } else {
                outside.path()
            };
            symlink(target, root.path().join(location)).unwrap();
            assert!(ensure(root.path()).is_err());
            assert_eq!(fs::read(file).unwrap(), b"user content");
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_documents_and_locks_fail_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        for location in ["AGENTS.md", ".dauerbot/delegation.lock"] {
            let root = tempfile::tempdir().unwrap();
            if location.starts_with(".dauerbot") {
                fs::create_dir(root.path().join(".dauerbot")).unwrap();
            }
            let path = CString::new(root.path().join(location).as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            assert!(ensure(root.path()).is_err());
        }
    }
}
