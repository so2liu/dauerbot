//! Bounded, host-provenanced image references. Pixel bytes never enter checkpoints.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rig_core::message::{DocumentSourceKind, Image, ImageMediaType, Message, UserContent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

pub const MAX_IMAGES: usize = 4;
pub const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    pub project_id: String,
    pub message_id: String,
    pub source_message_id: String,
    pub cache_root: PathBuf,
    pub path: PathBuf,
    pub sha256: String,
    pub media_type: String,
    pub bytes: u64,
}

pub fn image_type(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

impl ImageRef {
    /// Only call with the current source message's committed download metadata.
    pub fn from_resource(
        project: &str,
        message: &str,
        resource: &Value,
        expected_root: &Path,
    ) -> Result<Self> {
        ensure!(
            resource["cache_project_id"] == project,
            "Image cache belongs to another project"
        );
        ensure!(
            resource["cache_message_id"] == message,
            "Image cache belongs to another message"
        );
        let path = PathBuf::from(
            resource["local_path"]
                .as_str()
                .context("Image was not downloaded")?,
        );
        let root = PathBuf::from(
            resource["cache_root"]
                .as_str()
                .context("Image cache provenance is unavailable")?,
        );
        ensure!(
            root == expected_root && root.file_name().and_then(|v| v.to_str()) == Some(project),
            "Image is outside the configured project cache"
        );
        ensure!(
            !std::fs::symlink_metadata(&root)?.file_type().is_symlink()
                && root.canonicalize()? == root,
            "Image cache root changed"
        );
        let mime = resource["media_type"]
            .as_str()
            .context("Image type is unavailable")?;
        ensure!(
            matches!(
                mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ),
            "Image type is unsupported"
        );
        let bytes = resource["bytes"]
            .as_u64()
            .context("Image size is unavailable")?;
        ensure!(
            bytes > 0 && bytes <= MAX_IMAGE_BYTES,
            "Inline image exceeds the 2 MiB limit"
        );
        ensure!(
            path.is_absolute() && root.is_absolute() && path.parent() == Some(root.as_path()),
            "Image is outside its project cache"
        );
        let sha256 = resource["sha256"]
            .as_str()
            .context("Image hash is unavailable")?;
        ensure!(
            sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Image hash is invalid"
        );
        Ok(Self {
            project_id: project.into(),
            message_id: message.into(),
            source_message_id: resource["cache_source_id"]
                .as_str()
                .context("Image source identity is unavailable")?
                .into(),
            cache_root: root,
            path,
            sha256: sha256.into(),
            media_type: mime.into(),
            bytes,
        })
    }

    pub(crate) async fn content(&self) -> Result<UserContent> {
        ensure!(
            !tokio::fs::symlink_metadata(&self.cache_root)
                .await?
                .file_type()
                .is_symlink(),
            "Image cache root became a symlink"
        );
        let root = tokio::fs::canonicalize(&self.cache_root).await?;
        ensure!(root == self.cache_root, "Image cache root changed");
        let parent = tokio::fs::canonicalize(
            self.path
                .parent()
                .context("Image cache directory is missing")?,
        )
        .await?;
        ensure!(parent == root, "Image cache directory changed");
        let file = open_cached_file(&self.cache_root, &self.path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() == self.bytes && metadata.len() <= MAX_IMAGE_BYTES,
            "Image cache size changed"
        );
        let mut bytes = Vec::new();
        tokio::fs::File::from_std(file)
            .take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() as u64 == self.bytes,
            "Image cache changed while reading"
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == self.sha256,
            "Image cache hash changed"
        );
        ensure!(
            image_type(&bytes) == Some(self.media_type.as_str()),
            "Image cache media type changed"
        );
        let media_type = match self.media_type.as_str() {
            "image/png" => ImageMediaType::PNG,
            "image/jpeg" => ImageMediaType::JPEG,
            "image/gif" => ImageMediaType::GIF,
            "image/webp" => ImageMediaType::WEBP,
            _ => anyhow::bail!("Image type is unsupported"),
        };
        Ok(UserContent::Image(Image {
            data: DocumentSourceKind::Base64(STANDARD.encode(bytes)),
            media_type: Some(media_type),
            ..Default::default()
        }))
    }
}

fn open_directory(root: &Path) -> Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    ensure!(root.is_absolute(), "Cache directory must be absolute");
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for component in root.components() {
        let std::path::Component::Normal(name) = component else {
            ensure!(
                matches!(component, std::path::Component::RootDir),
                "Invalid image cache directory"
            );
            continue;
        };
        let name = std::ffi::CString::new(name.as_encoded_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "Image cache directory is unavailable");
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(directory)
}

/// An owned temporary resource anchored to directory descriptors through publish/drop.
/// User-provided filenames and replaced/symlinked cache directories cannot redirect IO.
pub(crate) struct CacheWrite {
    directory: std::fs::File,
    temporary: std::ffi::CString,
    target: std::ffi::CString,
    pub root: PathBuf,
    pub path: PathBuf,
    published: bool,
}
impl CacheWrite {
    pub fn create(
        base: &Path,
        project: &str,
        name: &str,
        temporary: &str,
    ) -> Result<(Self, std::fs::File)> {
        use std::os::fd::{AsRawFd, FromRawFd};
        ensure!(
            !project.is_empty()
                && project
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "Invalid cache project"
        );
        for value in [name, temporary] {
            ensure!(
                Path::new(value).components().count() == 1
                    && matches!(
                        Path::new(value).components().next(),
                        Some(std::path::Component::Normal(_))
                    ),
                "Invalid resource filename"
            );
        }
        fn child(parent: &std::fs::File, value: &str) -> Result<std::fs::File> {
            let value = std::ffi::CString::new(value)?;
            let created = unsafe { libc::mkdirat(parent.as_raw_fd(), value.as_ptr(), 0o700) };
            ensure!(
                created == 0
                    || std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists,
                "Cannot create resource cache directory"
            );
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    value.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            ensure!(
                fd >= 0,
                "Resource cache directory is not a regular directory"
            );
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }
        let base = base.canonicalize()?;
        let owner = open_directory(&base)?;
        let files = child(&owner, "files")?;
        let directory = child(&files, project)?;
        let target = std::ffi::CString::new(name)?;
        let temporary = std::ffi::CString::new(temporary)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(fd >= 0, "Resource temporary file is unavailable");
        let root = base.join("files").join(project);
        let path = root.join(name);
        Ok((
            Self {
                directory,
                temporary,
                target,
                root,
                path,
                published: false,
            },
            unsafe { std::fs::File::from_raw_fd(fd) },
        ))
    }
    pub fn publish(&mut self) -> Result<()> {
        use std::os::fd::AsRawFd;
        ensure!(
            unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    self.temporary.as_ptr(),
                    self.directory.as_raw_fd(),
                    self.target.as_ptr(),
                )
            } == 0,
            "Cannot publish resource cache file"
        );
        self.directory.sync_all()?;
        self.published = true;
        Ok(())
    }
}
impl Drop for CacheWrite {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        if !self.published {
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.temporary.as_ptr(), 0) };
        }
    }
}

fn open_cached_file(root: &Path, path: &Path) -> Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    ensure!(
        root.is_absolute() && path.parent() == Some(root),
        "Image is outside its cache directory"
    );
    let directory = open_directory(root)?;
    let name = std::ffi::CString::new(
        path.file_name()
            .context("Image cache file is unavailable")?
            .as_encoded_bytes(),
    )?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "Image cache file is unavailable");
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
pub fn input_digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn matches_input(message: &Message, digest: &str) -> bool {
    let Message::User { content } = message else {
        return false;
    };
    let text = content
        .iter()
        .filter_map(|part| {
            if let UserContent::Text(text) = part {
                Some(text.text.as_str())
            } else {
                None
            }
        })
        .collect::<String>();
    input_digest(&text) == digest
}
/// Attach only to the run's original user message, including on tool follow-ups.
/// Request copies carry pixels; AgentRun/checkpoint history retains text and refs.
pub async fn hydrate(
    prompt: &mut Message,
    history: &mut [Message],
    images: &[ImageRef],
    digest: Option<&str>,
    supports_images: bool,
) -> Vec<Value> {
    if images.is_empty() {
        return vec![];
    }
    let Some(digest) = digest else {
        return vec![json!({"mode":"path_only","reason":"original_input_unavailable"})];
    };
    let target = if matches_input(prompt, digest) {
        Some(prompt)
    } else {
        history
            .iter_mut()
            .find(|message| matches_input(message, digest))
    };
    let Some(Message::User { content }) = target else {
        return vec![json!({"mode":"path_only","reason":"original_input_unavailable"})];
    };
    let mut decisions = Vec::new();
    for image in images.iter().take(MAX_IMAGES) {
        if !supports_images {
            content.push(UserContent::text(format!("This configured model has no declared image capability. Image pixels were not attached; the original local reference is {}. Do not claim to have viewed it.", image.path.display())));
            decisions.push(json!({"message_id":image.message_id,"path":image.path,"mode":"path_only","reason":"image_capability_not_declared"}));
            continue;
        }
        match image.content().await {
            Ok(part) => {
                content.push(UserContent::text(format!("User image attachment from source_message_id={}, cache_message_id={}, local_path={}. The following image belongs to this attachment only.", image.source_message_id, image.message_id, image.path.display())));
                content.push(part);
                decisions.push(json!({"message_id":image.message_id,"path":image.path,"mode":"image_part","media_type":image.media_type,"bytes":image.bytes,"sha256":image.sha256}));
            }
            Err(_) => {
                content.push(UserContent::text(format!("Image pixels are unavailable because the cached image failed verification. The original reference is {}. Do not claim to have viewed it.", image.path.display())));
                decisions.push(json!({"message_id":image.message_id,"path":image.path,"mode":"path_only","reason":"cache_verification_failed"}));
            }
        }
    }
    decisions
}

pub fn fallback_note(path: &Path) -> String {
    format!(
        "Current native-session transport carries text and file references, not attached image pixels. Use the native CLI's image/file inspection tool on this exact local path if supported: {}. A path alone does not prove the model has viewed the image; state any inability to inspect it.",
        path.display()
    )
}

#[cfg(test)]
pub(crate) fn fixture_resource(dir: &Path, project: &str, message: &str) -> Value {
    let root = dir.join("files").join(project);
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let path = root.join(format!("{message}.png"));
    let bytes = include_bytes!("../tests/fixtures/input.png");
    std::fs::write(&path, bytes).unwrap();
    json!({"kind":"image","file_key":"img_fixture","local_path":path,"cache_root":root,"cache_project_id":project,"cache_message_id":message,"cache_source_id":format!("om_{message}"),"sha256":format!("{:x}",Sha256::digest(bytes)),"media_type":"image/png","bytes":bytes.len()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_cache_writes_reject_root_and_temporary_symlinks_and_clean_owned_partials() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let files = directory.path().join("files");
        std::fs::create_dir(&files).unwrap();
        std::os::unix::fs::symlink(outside.path(), files.join("project")).unwrap();
        assert!(
            CacheWrite::create(directory.path(), "project", "image.png", ".owned.part").is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        std::fs::remove_file(files.join("project")).unwrap();
        std::fs::create_dir(files.join("project")).unwrap();
        let outside_file = outside.path().join("untouched");
        std::fs::write(&outside_file, b"preserve").unwrap();
        std::os::unix::fs::symlink(&outside_file, files.join("project/.collision.part")).unwrap();
        assert!(
            CacheWrite::create(directory.path(), "project", "image.png", ".collision.part")
                .is_err()
        );
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"preserve");
        let (partial, mut file) =
            CacheWrite::create(directory.path(), "project", "image.png", ".owned.part").unwrap();
        file.write_all(b"partial").unwrap();
        drop(file);
        drop(partial);
        assert!(!files.join("project/.owned.part").exists());
        assert!(!files.join("project/image.png").exists());
        let (mut complete, mut file) =
            CacheWrite::create(directory.path(), "project", "image.png", ".owned.part").unwrap();
        file.write_all(include_bytes!("../tests/fixtures/input.png"))
            .unwrap();
        file.sync_all().unwrap();
        complete.publish().unwrap();
        assert_eq!(
            std::fs::read(&complete.path).unwrap(),
            include_bytes!("../tests/fixtures/input.png")
        );
    }
    #[tokio::test]
    async fn verified_pixels_are_ephemeral_and_changed_or_symlinked_cache_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let resource = fixture_resource(dir.path(), "project", "message");
        let image = ImageRef::from_resource(
            "project",
            "message",
            &resource,
            Path::new(resource["cache_root"].as_str().unwrap()),
        )
        .unwrap();
        let mut original = Message::from("Inspect this image");
        let audit = hydrate(
            &mut original,
            &mut [],
            std::slice::from_ref(&image),
            Some(&input_digest("Inspect this image")),
            true,
        )
        .await;
        assert_eq!(audit[0]["mode"], "image_part");
        let Message::User { content } = original else {
            panic!("user")
        };
        let UserContent::Image(part) = &content[2] else {
            panic!("image")
        };
        assert_eq!(
            part.data,
            DocumentSourceKind::Base64(
                STANDARD.encode(include_bytes!("../tests/fixtures/input.png"))
            )
        );
        assert!(!serde_json::to_string(&image).unwrap().contains("base64"));
        std::fs::write(&image.path, b"not pixels").unwrap();
        let mut prompt = Message::from("Now changed");
        let audit = hydrate(
            &mut prompt,
            &mut [],
            std::slice::from_ref(&image),
            Some(&input_digest("Now changed")),
            true,
        )
        .await;
        assert_eq!(audit[0]["reason"], "cache_verification_failed");
        assert!(
            !serde_json::to_string(&prompt)
                .unwrap()
                .contains("\"type\":\"image\"")
        );
        std::fs::remove_file(&image.path).unwrap();
        let elsewhere = dir.path().join("outside.png");
        std::fs::write(&elsewhere, include_bytes!("../tests/fixtures/input.png")).unwrap();
        std::os::unix::fs::symlink(elsewhere, &image.path).unwrap();
        let audit = hydrate(
            &mut Message::from("Symlink"),
            &mut [],
            &[image],
            Some(&input_digest("Symlink")),
            true,
        )
        .await;
        assert_eq!(audit[0]["mode"], "path_only");
    }
    #[tokio::test]
    async fn root_redirection_and_unrelated_user_history_never_receive_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let resource = fixture_resource(dir.path(), "project", "message");
        let image = ImageRef::from_resource(
            "project",
            "message",
            &resource,
            Path::new(resource["cache_root"].as_str().unwrap()),
        )
        .unwrap();
        let mut history = [Message::from("unrelated"), Message::from("original source")];
        let mut prompt = Message::from("tool follow-up");
        let audit = hydrate(
            &mut prompt,
            &mut history,
            std::slice::from_ref(&image),
            Some(&input_digest("original source")),
            true,
        )
        .await;
        assert_eq!(audit[0]["mode"], "image_part");
        assert!(
            !serde_json::to_string(&history[0])
                .unwrap()
                .contains(r#""type":"image""#)
        );
        assert!(
            serde_json::to_string(&history[1])
                .unwrap()
                .contains(r#""type":"image""#)
        );
        assert!(
            !serde_json::to_string(&prompt)
                .unwrap()
                .contains(r#""type":"image""#)
        );
        let relocated = image.cache_root.with_extension("old");
        std::fs::rename(&image.cache_root, relocated).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(
            outside.join("message.png"),
            include_bytes!("../tests/fixtures/input.png"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside, &image.cache_root).unwrap();
        let audit = hydrate(
            &mut Message::from("redirected"),
            &mut [],
            &[image],
            Some(&input_digest("redirected")),
            true,
        )
        .await;
        assert_eq!(audit[0]["mode"], "path_only");
    }
    #[test]
    fn image_refs_reject_cross_project_source_type_and_oversize_declarations() {
        let dir = tempfile::tempdir().unwrap();
        let resource = fixture_resource(dir.path(), "project", "message");
        assert!(
            ImageRef::from_resource(
                "other",
                "message",
                &resource,
                Path::new(resource["cache_root"].as_str().unwrap())
            )
            .is_err()
        );
        assert!(
            ImageRef::from_resource(
                "project",
                "other",
                &resource,
                Path::new(resource["cache_root"].as_str().unwrap())
            )
            .is_err()
        );
        let mut changed = resource.clone();
        changed["bytes"] = json!(MAX_IMAGE_BYTES + 1);
        assert!(
            ImageRef::from_resource(
                "project",
                "message",
                &changed,
                Path::new(changed["cache_root"].as_str().unwrap())
            )
            .is_err()
        );
        changed = resource;
        changed["media_type"] = json!("image/svg+xml");
        assert!(
            ImageRef::from_resource(
                "project",
                "message",
                &changed,
                Path::new(changed["cache_root"].as_str().unwrap())
            )
            .is_err()
        );
        assert_eq!(image_type(b"text called image.png"), None);
    }
}
