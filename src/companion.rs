//! Retained executables whose paths cannot be overwritten by app replacement.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

mod collection;
pub use collection::{collect, remember_integration};

/// The lease bridges publication to durable references, and historical exec.
/// The descriptor is close-on-exec; the OS executable check then protects it.
pub struct Retained {
    path: PathBuf,
    _lease: fs::File,
}

impl AsRef<Path> for Retained {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Retained {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Old MCP executables do not know about leases. Inherit the locked open
    /// file description across exec so their entire lifetime is protected.
    #[cfg(unix)]
    pub fn inherit_across_exec(&self) -> Result<()> {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn fcntl(fd: i32, command: i32, ...) -> i32;
        }
        let fd = self._lease.as_raw_fd();
        let flags = unsafe { fcntl(fd, 1) }; // F_GETFD
        ensure!(flags >= 0, "Cannot read companion lease flags");
        ensure!(
            unsafe { fcntl(fd, 2, flags & !1) } >= 0,
            "Cannot inherit companion lease"
        ); // F_SETFD, FD_CLOEXEC
        Ok(())
    }
}

fn lock_file(path: &Path) -> Result<fs::File> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "Invalid companion lock"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                meta.uid() == fs::metadata(path.parent().context("Missing parent")?)?.uid(),
                "Invalid companion lock owner"
            );
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn store_lock(data: &Path) -> Result<fs::File> {
    private_directory(&data.join("runtime"))?;
    private_directory(&data.join("runtime/hfb"))?;
    lock_file(&data.join("runtime/hfb/.collection.lock"))
}

fn lease(path: &Path) -> Result<Retained> {
    validate(path)?;
    let file = lock_file(&path.with_file_name(".lease"))?;
    FileExt::lock_shared(&file)?;
    Ok(Retained {
        path: path.into(),
        _lease: file,
    })
}

pub fn retain(data: &Path, path: &Path) -> Result<Retained> {
    let lock = store_lock(data)?;
    lock.lock_exclusive()?;
    lease(path)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn private_directory(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "Companion storage is not a private directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                meta.uid() == fs::metadata(path.parent().context("Missing parent")?)?.uid(),
                "Companion storage has a different owner"
            );
        }
    }
    crate::config::private_dir(path)
}

/// Publish once without overwriting an inode, even if its bytes are corrupt.
pub fn pin(data: &Path, source: &Path, version: &str, protocol: u64) -> Result<Retained> {
    let lock = store_lock(data)?;
    lock.lock_exclusive()?;
    ensure!(
        semver::Version::parse(version)?.to_string() == version,
        "Invalid companion version"
    );
    let meta = fs::symlink_metadata(source)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Companion must be a regular file"
    );
    let bytes = fs::read(source)?;
    let digest = hash(&bytes);
    let mut directory = data.to_path_buf();
    for component in [
        "runtime",
        "hfb",
        &format!("{version}-api{protocol}"),
        &digest,
    ] {
        directory.push(component);
        private_directory(&directory)?;
    }
    let target = directory.join("hfb");
    if fs::symlink_metadata(&target).is_ok() {
        validate(&target)?;
        return lease(&target);
    }
    let temporary = directory.join(format!(".install-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o500);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        // hard_link atomically creates the name; rename could clobber a live binary.
        match fs::hard_link(&temporary, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        validate(&target)?;
        fs::File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result?;
    lease(&target)
}

pub fn validate(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Pinned companion is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            meta.uid() == fs::metadata(path.parent().context("Missing parent")?)?.uid(),
            "Pinned companion has a different owner"
        );
        ensure!(
            meta.permissions().mode() & 0o777 == 0o500,
            "Pinned companion permissions changed"
        );
    }
    let expected = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .context("Invalid companion path")?;
    ensure!(
        hash(&fs::read(path)?) == expected,
        "Pinned companion checksum changed; recover the installation"
    );
    Ok(())
}

pub fn current(data: &Path) -> Result<Retained> {
    let path = pin(
        data,
        &std::env::current_exe()?,
        crate::update::CURRENT,
        crate::api::PROTOCOL,
    )?;
    let output = std::process::Command::new(path.path())
        .arg("companion-identity")
        .output()?;
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        output.status.success()
            && identity["version"] == crate::update::CURRENT
            && identity["api_protocol"] == crate::api::PROTOCOL,
        "The app changed while the daemon started; reopen the installed app"
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_replacement_preserves_old_commands_and_corruption_fails_closed() {
        let data = tempfile::tempdir().unwrap();
        let source = data.path().join("source");
        fs::write(&source, b"N").unwrap();
        let old = pin(data.path(), &source, "0.4.6", 6).unwrap();
        fs::write(&source, b"N+1").unwrap();
        let new = pin(data.path(), &source, "0.4.7", 7).unwrap();
        assert_ne!(old.path(), new.path());
        assert_eq!(fs::read(old).unwrap(), b"N");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&new, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(&new, b"corrupt").unwrap();
            assert!(pin(data.path(), &source, "0.4.7", 7).is_err());
            assert_eq!(fs::read(new).unwrap(), b"corrupt");
        }
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_storage() {
        let data = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), data.path().join("runtime")).unwrap();
        let source = data.path().join("source");
        fs::write(&source, b"fixture").unwrap();
        assert!(pin(data.path(), &source, "0.4.6", 6).is_err());
        assert!(!other.path().join("hfb").exists());
    }
}
