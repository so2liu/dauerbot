//! Retained executables whose paths cannot be overwritten by app replacement.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

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
/// Never automatically collect these paths: saved configurations can outlive daemons.
pub fn pin(data: &Path, source: &Path, version: &str, protocol: u64) -> Result<PathBuf> {
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
        return Ok(target);
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
    Ok(target)
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

pub fn current(data: &Path) -> Result<PathBuf> {
    let path = pin(
        data,
        &std::env::current_exe()?,
        crate::update::CURRENT,
        crate::api::PROTOCOL,
    )?;
    let output = std::process::Command::new(&path)
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
        assert_ne!(old, new);
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
