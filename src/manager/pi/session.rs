//! Pi only edits a staged copy. The daemon publishes a snapshot with input settlement.
use super::{Context, Path, PathBuf, Result, ensure};
use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read},
    os::unix::fs::OpenOptionsExt,
};

pub(super) const RECOVERY: &str =
    "Pi session could not be restored; started a new discussion session and reloaded host context.";

pub(in crate::manager) fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS manager_pi_sessions(
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            scope TEXT NOT NULL, snapshot TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL,
            digest TEXT NOT NULL, PRIMARY KEY(project_id,scope)
        )",
    )?;
    Ok(())
}

#[derive(Clone)]

pub(super) struct Snapshot {
    id: String,
    pub(super) session_id: String,
    digest: String,
}

pub(super) fn lookup(c: &Connection, project: &str, scope: &str) -> Result<Option<Snapshot>> {
    Ok(c.query_row(
        "SELECT snapshot,session_id,digest FROM manager_pi_sessions WHERE project_id=? AND scope=?",
        params![project, scope],
        |r| {
            Ok(Snapshot {
                id: r.get(0)?,
                session_id: r.get(1)?,
                digest: r.get(2)?,
            })
        },
    )
    .optional()?)
}

pub(super) fn directory(data: &Path) -> Result<PathBuf> {
    let mut root = data.canonicalize()?;
    for part in [".dauerbot", "manager-pi-sessions"] {
        root.push(part);
        match fs::symlink_metadata(&root) {
            Ok(meta) => ensure!(meta.file_type().is_dir(), "Invalid Pi state directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        crate::config::private_dir(&root)?;
    }
    Ok(root)
}

fn path(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok() && id.len() == 36,
        "Invalid Pi snapshot identifier"
    );
    Ok(root.join(format!("{id}.jsonl")))
}

fn inspect(path: &Path, expected_id: &str, cwd: &Path) -> Result<BufReader<File>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "Invalid Pi transcript file");
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    (&mut reader).take(8192).read_line(&mut first)?;
    let header: Value = serde_json::from_str(&first)?;
    ensure!(
        header["type"] == "session"
            && header["id"] == expected_id
            && header["cwd"].as_str().map(Path::new) == Some(cwd),
        "Pi transcript identity changed"
    );
    use std::io::Seek;
    reader.rewind()?;
    Ok(reader)
}

pub(super) fn stage(root: &Path, snapshot: &Snapshot, staged: &Path) -> Result<()> {
    let mut src = inspect(&path(root, &snapshot.id)?, &snapshot.session_id, root)?;
    let mut dst = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(staged)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut src, &mut hash_writer(&mut dst, &mut hash))?;
    ensure!(
        format!("{:x}", hash.finalize()) == snapshot.digest,
        "Pi transcript checksum changed"
    );
    Ok(())
}

fn hash_writer<'a>(file: &'a mut File, hash: &'a mut Sha256) -> impl std::io::Write + 'a {
    struct Writer<'a>(&'a mut File, &'a mut Sha256);
    impl std::io::Write for Writer<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let written = self.0.write(bytes)?;
            self.1.update(&bytes[..written]);
            Ok(written)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }
    Writer(file, hash)
}

pub(super) fn publish(root: &Path, staged: &Path, session_id: String) -> Result<Snapshot> {
    let mut src = inspect(staged, &session_id, root)?;
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    let mut hash = Sha256::new();
    std::io::copy(
        &mut src,
        &mut hash_writer(temporary.as_file_mut(), &mut hash),
    )?;
    temporary.as_file().sync_all()?;
    let id = crate::db::id();
    temporary
        .persist_noclobber(path(root, &id)?)
        .context("Could not publish Pi transcript")?;
    File::open(root)?.sync_all()?;
    Ok(Snapshot {
        id,
        session_id,
        digest: format!("{:x}", hash.finalize()),
    })
}

pub(super) fn finish(
    c: &Connection,
    ctx: &Context,
    scope: &str,
    next: &Snapshot,
    note: &str,
) -> Result<()> {
    let tx = c.unchecked_transaction()?;
    super::super::valid(&tx, ctx)?;
    tx.execute(
        "INSERT INTO manager_pi_sessions(project_id,scope,snapshot,session_id,digest) VALUES(?,?,?,?,?)
         ON CONFLICT(project_id,scope) DO UPDATE SET snapshot=excluded.snapshot,session_id=excluded.session_id,digest=excluded.digest",
        params![ctx.project, scope, next.id, next.session_id, next.digest],
    )?;
    super::super::close_turn(&tx, ctx, note)?;
    super::super::recovered(&tx, ctx)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn remove(root: &Path, snapshot: &Snapshot) {
    if let Ok(path) = path(root, &snapshot.id) {
        let _ = fs::remove_file(path);
    }
}
