use rusqlite::{Connection, OpenFlags};
use std::{fs, path::Path};

/// Check only the folder the person selected. Never enumerate protected home folders.
pub fn project_access(path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(
        path.is_absolute() && path.is_dir(),
        "请选择一个存在的项目文件夹"
    );
    let _ = fs::read_dir(path)?.next().transpose()?;
    let probe = path.join(format!(".herdr-access-{}", uuid::Uuid::new_v4()));
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)?;
    drop(file);
    fs::remove_file(probe)?;
    Ok("所选目录可以读取和写入。Agent 由 HERDR 启动，其实际访问权限仍需通过会话验证。".into())
}

/// Read schema metadata, without modifying the database or reading user records.
pub fn database_access(path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(path.is_file(), "请选择一个现有 SQLite 数据库文件");
    // Preserve the OS permission error before SQLite maps it to SQLITE_CANTOPEN.
    drop(fs::File::open(path)?);
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })?;
    Ok("所选 SQLite 数据库可以只读打开；未读取业务记录。这仅证明该文件当前可访问，不代表已取得完全磁盘访问权限。".into())
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
            "macOS 或文件系统拒绝了访问。请重新选择该文件夹；仍失败时检查「文件与文件夹」权限、目录所有权及 ACL。受保护应用数据库可能需要「完全磁盘访问权限」。授权后重新检测。详情：{error}"
        )
    } else {
        format!(
            "检测未通过：{error}。文件不存在、数据库加密或被锁定也会失败，不能据此判断为权限拒绝。"
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
        assert!(!explain(&error).contains("拒绝了访问"));
        let error = std::io::Error::from_raw_os_error(1).into();
        assert!(explain(&error).contains("拒绝了访问"));
    }
}
