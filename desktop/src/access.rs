use rusqlite::{Connection, OpenFlags};
use std::{fs, path::Path};

/// Check only the folder the person selected. Never enumerate protected home folders.
pub fn project_access(path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(
        path.is_absolute() && path.is_dir(),
        "Choose an existing project folder"
    );
    let _ = fs::read_dir(path)?.next().transpose()?;
    let probe = path.join(format!(".herdr-access-{}", uuid::Uuid::new_v4()));
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)?;
    drop(file);
    fs::remove_file(probe)?;
    Ok("The selected folder is readable and writable. Verify the HERDR agent's own permissions in its session.".into())
}

/// Read schema metadata, without modifying the database or reading user records.
pub fn database_access(path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(path.is_file(), "Choose an existing SQLite database file");
    // Preserve the OS permission error before SQLite maps it to SQLITE_CANTOPEN.
    drop(fs::File::open(path)?);
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })?;
    Ok("The selected SQLite database opens read-only; no business records were read. This checks this file's current access, not Full Disk Access.".into())
}

pub fn explain(error: &anyhow::Error) -> String {
    let denied = error.chain().any(|e| {
        e.downcast_ref::<std::io::Error>().is_some_and(|e| {
            e.kind() == std::io::ErrorKind::PermissionDenied
                || matches!(e.raw_os_error(), Some(1 | 13))
        })
    });
    if denied {
        format!(
            "macOS or the filesystem denied access. Select the folder again, then check Files and Folders, ownership, and ACLs. Protected databases may require Full Disk Access. Grant access and retry. Details: {error}"
        )
    } else {
        format!(
            "Check failed: {error}. Missing files, encryption, and locks can also cause failures; this does not prove a permission denial."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_probe_leaves_no_file_and_database_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        project_access(dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        let path = dir.path().join("db.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE private(value TEXT); INSERT INTO private VALUES ('untouched');",
        )
        .unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        database_access(&path).unwrap();
        assert_eq!(fs::read(path).unwrap(), before);
    }
    #[test]
    fn missing_file_is_not_reported_as_denied_permission() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound).into();
        assert!(!explain(&error).contains("denied access"));
        let error = std::io::Error::from_raw_os_error(1).into();
        assert!(explain(&error).contains("denied access"));
    }
}
