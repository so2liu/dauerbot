//! Offline provisioning of the app's supported legacy runtime.
use crate::config::{Config, private_dir};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const VERSION: &str = "0.9.5";
const UPSTREAM: &str = "fb77c2232a05a8035421382c894392f4174e4e38a18e48af3dd0290157c3a7e2";

#[derive(serde::Deserialize)]
struct Payload {
    version: String,
    upstream_sha256: String,
    sha256: String,
    size: u64,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
}

fn executable(path: &Path) -> bool {
    if !regular(path) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o100 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn version(path: &Path) -> Result<()> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "Cannot run HERDR at {}. Check the configured executable or reinstall Dauerbot",
                path.display()
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("HERDR version check timed out at {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success() && output.stdout.trim_ascii() == b"herdr 0.9.5",
        "Dauerbot requires HERDR 0.9.5; check the configured executable at {}",
        path.display()
    );
    Ok(())
}

/// Called on the backend worker, never on GPUI's main thread. Only the default
/// executable and paths owned by this data directory opt into provisioning.
pub fn resolve(config: &mut Config, data: &Path) -> Result<()> {
    let executable = std::env::current_exe()?;
    let bundle = executable
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "MacOS"))
        .and_then(Path::parent);
    resolve_from(config, data, bundle)
}

fn resolve_from(config: &mut Config, data: &Path, contents: Option<&Path>) -> Result<()> {
    let root = data.join("runtime/herdr");
    let configured = Path::new(&config.herdr);
    let owned = configured.strip_prefix(&root).ok().is_some_and(|p| {
        let parts: Vec<_> = p.components().collect();
        parts.len() == 3
            && parts[0].as_os_str() == VERSION
            && parts[2].as_os_str() == "herdr"
            && parts[1]
                .as_os_str()
                .to_str()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    if config.herdr != "herdr" && !owned {
        // Explicit paths, including persisted user installs, must never be replaced.
        return Ok(());
    }
    if owned && executable(configured) {
        let expected = configured
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if hash(&fs::read(configured)?) == expected {
            version(configured)?;
            return Ok(());
        }
    }
    if !owned && let Some(candidate) = crate::config::program_path("herdr") {
        version(&candidate)?;
        config.herdr = candidate.to_string_lossy().into_owned();
        return Ok(());
    }
    let Some(contents) = contents else {
        // Source/standalone CLI builds keep the existing install requirement.
        ensure!(
            !owned,
            "App-managed HERDR is missing or corrupt; reopen the installed Dauerbot app to repair it"
        );
        return Ok(());
    };
    let metadata: Payload = serde_json::from_slice(
        &fs::read(contents.join("Resources/herdr-runtime.json"))
            .context("Bundled HERDR metadata is missing; reinstall Dauerbot")?,
    )?;
    ensure!(
        metadata.version == VERSION && metadata.upstream_sha256 == UPSTREAM,
        "Unsupported bundled HERDR provenance; reinstall Dauerbot"
    );
    let source = contents.join("MacOS/herdr");
    ensure!(
        regular(&source),
        "Bundled HERDR is missing or is a symlink; reinstall Dauerbot"
    );
    let bytes = fs::read(&source)?;
    ensure!(
        bytes.len() as u64 == metadata.size && hash(&bytes) == metadata.sha256,
        "Bundled HERDR checksum differs; reinstall Dauerbot"
    );
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(contents.parent().context("Missing app bundle")?)
            .status()?;
        ensure!(
            status.success(),
            "Dauerbot bundle signature differs; reinstall the app"
        );
    }
    version(&source)?;
    let installed = install(&root, &bytes, &metadata.sha256)?;
    version(&installed)?;
    config.herdr = installed.to_string_lossy().into_owned();
    Ok(())
}

fn owned_dir(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "Runtime directory is not a private directory: {}",
            path.display()
        );
    }
    private_dir(path)
}

fn install(root: &Path, bytes: &[u8], digest: &str) -> Result<PathBuf> {
    // Lock across processes; never truncate or rewrite a running executable.
    owned_dir(root.parent().context("Missing runtime root")?)?;
    owned_dir(root)?;
    let lock_path = root.join("install.lock");
    ensure!(
        !fs::symlink_metadata(&lock_path).is_ok_and(|m| m.file_type().is_symlink()),
        "Runtime lock must not be a symlink"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let version_root = root.join(VERSION);
    owned_dir(&version_root)?;
    let directory = version_root.join(digest);
    owned_dir(&directory)?;
    let target = directory.join("herdr");
    if executable(&target) && hash(&fs::read(&target)?) == digest {
        return Ok(target);
    }
    // A symlink is never followed or replaced, even within the owned tree.
    ensure!(
        !fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()),
        "App-managed HERDR must not be a symlink"
    );
    let temp = directory.join(format!(".install-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &target)?;
        fs::File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_install_reuses_and_repairs_only_owned_bytes() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path().join("runtime/herdr");
        let bytes = b"runtime fixture";
        let digest = hash(bytes);
        let target = install(&root, bytes, &digest).unwrap();
        let before = fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(install(&root, bytes, &digest).unwrap(), target);
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            install(&root, bytes, &digest).unwrap();
            assert!(executable(&target));
        }
        fs::write(&target, b"partial").unwrap();
        install(&root, bytes, &digest).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        let newer = install(&root, b"updated", &hash(b"updated")).unwrap();
        assert_ne!(newer, target);
        assert_eq!(fs::read(target).unwrap(), bytes);
    }
    #[test]
    #[cfg(unix)]
    fn symlinks_cannot_redirect_installation() {
        use std::os::unix::fs::symlink;
        let data = tempfile::tempdir().unwrap();
        let outside = data.path().join("user-herdr");
        fs::write(&outside, b"user owned").unwrap();
        let root = data.path().join("runtime/herdr");
        let target = install(&root, b"runtime", &hash(b"runtime")).unwrap();
        fs::remove_file(&target).unwrap();
        symlink(&outside, &target).unwrap();
        assert!(install(&root, b"runtime", &hash(b"runtime")).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"user owned");
        let other = data.path().join("other");
        fs::create_dir(&other).unwrap();
        symlink(&other, root.join("0.9.5").join(hash(b"new"))).unwrap();
        assert!(install(&root, b"new", &hash(b"new")).is_err());
    }
    #[test]
    fn explicit_missing_executable_is_preserved() {
        let data = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            herdr: "/custom/missing/herdr".into(),
            ..Config::default()
        };
        resolve_from(&mut cfg, data.path(), None).unwrap();
        assert_eq!(cfg.herdr, "/custom/missing/herdr");
    }
}
