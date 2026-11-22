//! Bounded video preview decoding from an already-owned cache descriptor.
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const FORMATS: &str = "mov,matroska,webm,avi,mpeg,mpegts,ogg,flv,asf";

pub(crate) struct Frame {
    pub duration: f64,
    pub timestamp: f64,
    pub bytes: Vec<u8>,
}

async fn run(program: &Path, input: File, args: &[&str], limit: u64) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Video preview decoder is unavailable")?;
    let mut data = Vec::new();
    child
        .stdout
        .take()
        .context("Video preview output unavailable")?
        .take(limit + 1)
        .read_to_end(&mut data)
        .await?;
    ensure!(
        data.len() as u64 <= limit,
        "Video preview exceeds its size limit"
    );
    ensure!(
        child.wait().await?.success(),
        "Video preview decoder rejected the media"
    );
    Ok(data)
}

pub(crate) fn decoder(name: &str) -> Option<PathBuf> {
    crate::config::program_path(name).or_else(|| {
        #[cfg(target_os = "macos")]
        {
            ["/opt/homebrew/bin", "/usr/local/bin"]
                .into_iter()
                .map(|dir| Path::new(dir).join(name))
                .find(|path| path.is_file())
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    })
}

pub(crate) async fn extract(input: File, resolve: fn(&str) -> Option<PathBuf>) -> Result<Frame> {
    let ffprobe = resolve("ffprobe").context("ffprobe was not found")?;
    let ffmpeg = resolve("ffmpeg").context("ffmpeg was not found")?;
    tokio::time::timeout(Duration::from_secs(8), async {
        let probe = run(
            &ffprobe,
            input.try_clone()?,
            &[
                "-v",
                "error",
                "-protocol_whitelist",
                "fd",
                "-format_whitelist",
                FORMATS,
                "-i",
                "fd:",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ],
            256,
        )
        .await?;
        let duration = std::str::from_utf8(&probe)?.trim().parse::<f64>()?;
        ensure!(
            duration.is_finite() && duration > 0.0,
            "Video duration is unavailable"
        );
        let timestamp = duration * 0.2;
        // ffprobe and ffmpeg inherit duplicates of the same open file description.
        let mut input = input;
        std::io::Seek::rewind(&mut input)?;
        let bytes = run(
            &ffmpeg,
            input,
            &[
                "-v",
                "error",
                "-nostdin",
                "-protocol_whitelist",
                "fd",
                "-format_whitelist",
                FORMATS,
                "-ss",
                &format!("{timestamp:.6}"),
                "-i",
                "fd:",
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-vf",
                "scale='min(960,iw)':-2",
                "-threads",
                "1",
                "-c:v",
                "png",
                "-f",
                "image2pipe",
                "pipe:1",
            ],
            crate::multimodal::MAX_IMAGE_BYTES,
        )
        .await?;
        ensure!(
            crate::multimodal::image_type(&bytes) == Some("image/png"),
            "Decoder did not produce PNG"
        );
        Ok(Frame {
            duration,
            timestamp,
            bytes,
        })
    })
    .await
    .context("Video preview timed out")?
}
