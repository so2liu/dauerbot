//! Read-only access to a project's two fixed continuity documents.
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};

pub const MAX_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Document {
    Agents,
    Handoff,
}
impl Document {
    pub fn filename(self) -> &'static str {
        match self {
            Self::Agents => "AGENTS.md",
            Self::Handoff => "HANDOFF.md",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Handoff => "handoff",
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Content {
    pub document: Document,
    pub status: String,
    pub text: String,
    pub message: String,
}
/// Call on a blocking worker, never the GUI or async runtime thread. Open the
/// selected directory first so renames cannot redirect the final relative open.
pub fn read(root: &Path, document: Document) -> Content {
    let result = (|| -> anyhow::Result<Option<String>> {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(root)?;
        anyhow::ensure!(
            directory.metadata()?.is_dir(),
            "Project folder is unavailable"
        );
        let name = std::ffi::CString::new(document.filename())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error.into());
        }
        use std::os::fd::FromRawFd;
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        anyhow::ensure!(metadata.is_file(), "The document must be a regular file");
        anyhow::ensure!(
            metadata.len() <= MAX_BYTES,
            "The document exceeds the 256 KiB viewing limit"
        );
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "The document exceeds the 256 KiB viewing limit"
        );
        Ok(Some(String::from_utf8(bytes)?))
    })();
    let (status, text, message) = match result {
        Ok(Some(text)) => ("available", text, String::new()),
        Ok(None) => (
            "missing",
            String::new(),
            format!(
                "{} has not been created in this project yet.",
                document.filename()
            ),
        ),
        Err(error) => (
            "error",
            String::new(),
            format!("Cannot read {}: {error}", document.filename()),
        ),
    };
    Content {
        document,
        status: status.into(),
        text,
        message,
    }
}
