use sha2::{Digest, Sha256};
use std::{io, path::Path};
use tokio::io::AsyncReadExt;

const MAX_BYTES: u64 = 64 * 1024;

pub(super) async fn read(workspace: &Path) -> io::Result<(String, String)> {
    let path = workspace.join("AGENTS.md");
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tokio::fs::metadata(workspace).await?;
            return Ok(("missing".into(), "Workspace AGENTS.md is absent.".into()));
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AGENTS.md is not a regular file",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = tokio::fs::OpenOptions::from(options)
        .read(true)
        .open(path)
        .await?;
    if !file.metadata().await?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AGENTS.md is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AGENTS.md exceeds 64 KiB",
        ));
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((
        digest,
        format!(
            "Current workspace AGENTS.md:\n{}",
            String::from_utf8_lossy(&bytes)
        ),
    ))
}
