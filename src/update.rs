//! Signed GitHub releases, streamed downloads and transactional macOS bundle updates.
use crate::config;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

pub const REPOSITORY: &str = "so2liu/dauerbot";
pub const BUNDLE_ID: &str = "dev.herdr.feishu-bridge";
pub const TEAM_ID: &str = "366ADQ28F6";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const SIGNING_CONTEXT: &[u8] = b"Dauerbot update v1\n";
const PUBLIC_KEY: &str = include_str!("../packaging/update-public-key.txt");
const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub automatic: bool,
    pub channel: String,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            automatic: true,
            channel: "stable".into(),
        }
    }
}
impl Preferences {
    pub fn load(data: &Path) -> Result<Self> {
        let path = data.join("updates/preferences.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let prefs: Self =
            serde_json::from_slice(&fs::read(path)?).context("升级偏好文件损坏，已保留原文件")?;
        ensure!(
            matches!(prefs.channel.as_str(), "stable" | "preview"),
            "未知更新通道"
        );
        Ok(prefs)
    }
    pub fn save(&self, data: &Path) -> Result<()> {
        ensure!(
            matches!(self.channel.as_str(), "stable" | "preview"),
            "未知更新通道"
        );
        config::private_dir(&data.join("updates"))?;
        config::write_private(
            &data.join("updates/preferences.json"),
            &serde_json::to_vec(self)?,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub phase: String,
    pub message: String,
    pub current: String,
    pub latest: Option<String>,
    pub progress: Option<u8>,
    pub notes: String,
    pub release_url: String,
    pub preferences: Preferences,
    #[serde(default)]
    pub rollback_available: bool,
}
impl Status {
    pub fn new(prefs: Preferences) -> Self {
        Self {
            phase: "idle".into(),
            message: "尚未检查更新".into(),
            current: CURRENT.into(),
            latest: None,
            progress: None,
            notes: String::new(),
            release_url: String::new(),
            preferences: prefs,
            rollback_available: false,
        }
    }
    pub fn save(&self, data: &Path) -> Result<()> {
        config::private_dir(&data.join("updates"))?;
        config::write_private(
            &data.join("updates/status.json"),
            &serde_json::to_vec(self)?,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub bundle_id: String,
    pub team_id: String,
    pub architecture: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}
#[derive(Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    #[serde(default)]
    body: String,
    assets: Vec<Asset>,
}
pub struct Release {
    manifest: Manifest,
    url: String,
    pub notes: String,
    pub page: String,
}
impl Release {
    pub fn version(&self) -> &str {
        &self.manifest.version
    }
}
fn version(text: &str) -> Result<Version> {
    Version::parse(text.strip_prefix('v').unwrap_or(text)).context("无效发布版本")
}
pub fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    Ok(version(candidate)?
        .cmp_precedence(&version(current)?)
        .is_gt())
}
pub fn bundle_version(app: &Path) -> Result<String> {
    plist(app, "CFBundleShortVersionString")
}
fn architecture() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    }
}
fn verify_manifest(bytes: &[u8], signature: &[u8], key: &str) -> Result<Manifest> {
    let key: [u8; 32] = STANDARD
        .decode(key.trim())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("无效更新公钥"))?;
    let signature =
        Signature::from_slice(&STANDARD.decode(std::str::from_utf8(signature)?.trim())?)?;
    VerifyingKey::from_bytes(&key)?
        .verify_strict(&[SIGNING_CONTEXT, bytes].concat(), &signature)
        .context("更新清单签名不匹配，拒绝安装")?;
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    let parsed = version(&manifest.version)?;
    ensure!(parsed.to_string() == manifest.version, "发布版本格式不规范");
    ensure!(
        manifest.bundle_id == BUNDLE_ID && manifest.team_id == TEAM_ID,
        "更新包身份不匹配"
    );
    ensure!(
        manifest.architecture == architecture(),
        "当前设备没有对应架构的安装包"
    );
    ensure!(
        manifest.file
            == format!(
                "Dauerbot-v{}-macos-{}.zip",
                manifest.version,
                architecture()
            ),
        "无效安装包名称"
    );
    ensure!(
        manifest.size > 0 && manifest.size <= MAX_ARCHIVE,
        "安装包大小超过限制"
    );
    ensure!(
        manifest.sha256.len() == 64 && manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "无效 SHA-256"
    );
    Ok(manifest)
}
fn secure_url(text: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(text)?;
    ensure!(
        url.scheme() == "https" && matches!(url.host_str(), Some("github.com" | "api.github.com")),
        "更新地址必须来自 GitHub HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "更新地址不得包含凭证"
    );
    Ok(url)
}
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("Dauerbot/{CURRENT}"))
        .connect_timeout(Duration::from_secs(10))
        .https_only(true)
        .build()
        .map_err(Into::into)
}
async fn small(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut response = client
            .get(secure_url(url)?)
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() + chunk.len() <= limit, "更新元数据超过限制");
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .context("检查更新超时")?
}
fn newest(releases: Vec<GithubRelease>, channel: &str) -> Option<GithubRelease> {
    releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| version(&r.tag_name).ok().map(|v| (v, r)))
        .filter(|(v, r)| channel == "preview" || (!r.prerelease && v.pre.is_empty()))
        .max_by(|a, b| a.0.cmp_precedence(&b.0))
        .map(|(_, r)| r)
}
pub async fn check(client: &reqwest::Client, prefs: &Preferences) -> Result<Option<Release>> {
    check_for_version(client, prefs, CURRENT).await
}
pub async fn check_for_version(
    client: &reqwest::Client,
    prefs: &Preferences,
    current: &str,
) -> Result<Option<Release>> {
    let bytes = small(
        client,
        &format!("https://api.github.com/repos/{REPOSITORY}/releases?per_page=100"),
        2 * 1024 * 1024,
    )
    .await?;
    let Some(release) = newest(serde_json::from_slice(&bytes)?, &prefs.channel) else {
        return Ok(None);
    };
    if !is_newer(&release.tag_name, current)? {
        return Ok(None);
    }
    let find = |name: &str| -> Result<&Asset> {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("发布产物不完整：缺少 {name}"))
    };
    let bytes = small(
        client,
        &find("update.json")?.browser_download_url,
        16 * 1024,
    )
    .await?;
    let sig = small(client, &find("update.json.sig")?.browser_download_url, 256).await?;
    let manifest = verify_manifest(&bytes, &sig, PUBLIC_KEY)?;
    ensure!(
        version(&manifest.version)? == version(&release.tag_name)?,
        "清单版本与发布标签不符"
    );
    let url = find(&manifest.file)?.browser_download_url.clone();
    secure_url(&url)?;
    Ok(Some(Release {
        manifest,
        url,
        notes: release.body,
        page: release.html_url,
    }))
}
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app" && exe.parent()?.file_name()? == "MacOS").then(|| app.to_path_buf())
}
fn output(command: &mut Command) -> Result<String> {
    let result = command.output()?;
    ensure!(
        result.status.success(),
        "校验失败：{}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}
fn plist(app: &Path, key: &str) -> Result<String> {
    output(
        Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(app.join("Contents/Info.plist")),
    )
}
pub fn verify_bundle(app: &Path, expected: Option<&str>) -> Result<()> {
    ensure!(cfg!(target_os = "macos"), "自动安装目前仅支持 macOS");
    ensure!(
        plist(app, "CFBundleIdentifier")? == BUNDLE_ID,
        "应用 bundle ID 不匹配"
    );
    let installed = plist(app, "CFBundleShortVersionString")?;
    version(&installed)?;
    if let Some(expected) = expected {
        ensure!(installed == expected, "应用版本与更新清单不匹配");
    }
    for name in ["HerdrBridge", "hfb"] {
        let mut header = [0u8; 8];
        File::open(app.join("Contents/MacOS").join(name))?.read_exact(&mut header)?;
        let cpu = u32::from_le_bytes(header[4..8].try_into()?);
        let expected = if architecture() == "arm64" {
            0x0100_000c
        } else {
            0x0100_0007
        };
        ensure!(
            header[..4] == [0xcf, 0xfa, 0xed, 0xfe] && cpu == expected,
            "应用可执行文件的架构不匹配"
        );
    }
    let requirement =
        format!("anchor apple generic and certificate leaf[subject.OU] = \"{TEAM_ID}\"");
    output(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", "-R"])
            .arg(format!("={requirement} and identifier \"{BUNDLE_ID}\""))
            .arg(app),
    )?;
    output(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "-R"])
            .arg(format!("={requirement}"))
            .arg(app.join("Contents/MacOS/hfb")),
    )?;
    Ok(())
}
fn file_hash(path: &Path) -> Result<String> {
    let mut source = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
async fn download(
    client: &reqwest::Client,
    release: &Release,
    path: &Path,
    progress: &impl Fn(u8),
) -> Result<()> {
    let mut response = tokio::time::timeout(
        Duration::from_secs(45),
        client.get(secure_url(&release.url)?).send(),
    )
    .await
    .context("下载连接停滞，请重试")??
    .error_for_status()?;
    let mut file = tokio::fs::File::create(path).await?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut reported = None;
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(45), response.chunk())
            .await
            .context("下载停滞，可重新检查更新后重试")??;
        let Some(chunk) = chunk else {
            break;
        };
        count += chunk.len() as u64;
        ensure!(count <= release.manifest.size, "下载包大小与签名清单不符");
        hash.update(&chunk);
        file.write_all(&chunk).await?;
        let percent = ((count * 100) / release.manifest.size) as u8;
        if reported != Some(percent) {
            reported = Some(percent);
            progress(percent);
        }
    }
    file.sync_all().await?;
    ensure!(count == release.manifest.size, "安装包下载不完整");
    ensure!(
        format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&release.manifest.sha256),
        "安装包 SHA-256 不符，拒绝安装"
    );
    Ok(())
}
fn safe_entry(name: &str, mode: u32) -> Result<Option<PathBuf>> {
    ensure!(!name.contains(['\\', '\0']), "压缩包包含不安全路径");
    let path = Path::new(name);
    ensure!(
        !path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
        "压缩包路径越界"
    );
    if path
        .components()
        .next()
        .is_some_and(|c| c.as_os_str() == "__MACOSX")
    {
        return Ok(None);
    }
    let relative = path
        .strip_prefix("Dauerbot.app")
        .context("压缩包包含意外的顶层目录")?;
    ensure!(mode & 0o170000 != 0o120000, "更新包不接受符号链接");
    Ok(Some(relative.to_path_buf()))
}
fn extract(archive: &Path, payload: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
    ensure!(archive.len() <= 2048, "更新包文件数量过多");
    fs::create_dir_all(payload)?;
    let mut total = 0;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let mode = file.unix_mode().unwrap_or(0o644);
        let Some(relative) = safe_entry(file.name(), mode)? else {
            continue;
        };
        let destination = payload.join(relative);
        if file.is_dir() {
            fs::create_dir_all(&destination)?;
            continue;
        }
        ensure!(
            file.size() <= MAX_EXPANDED - total,
            "更新包解压大小超过限制"
        );
        fs::create_dir_all(destination.parent().context("缺少父目录")?)?;
        let mut target = File::create(&destination)?;
        let copied = std::io::copy(&mut (&mut file).take(MAX_EXPANDED - total + 1), &mut target)?;
        total += copied;
        ensure!(
            total <= MAX_EXPANDED && copied == file.size(),
            "解压文件大小不符"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &destination,
                fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 }),
            )?;
        }
    }
    Ok(())
}
/// Both renames happen on one filesystem. A failed second rename restores the old app.
fn swap(payload: &Path, app: &Path, backup: &Path) -> Result<()> {
    fs::rename(app, backup)
        .context("无法移动旧应用，请将应用移至你有写入权限的 Applications 目录")?;
    if let Err(error) = fs::rename(payload, app) {
        fs::rename(backup, app).context("新应用替换失败且旧应用恢复失败，请从备份目录恢复")?;
        return Err(error).context("新应用替换失败，已恢复旧版本");
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct InstallRecord {
    app: PathBuf,
    backup: PathBuf,
    version: String,
}
pub async fn install(
    client: &reqwest::Client,
    data: &Path,
    app: &Path,
    release: &Release,
    progress: impl Fn(u8),
    automatic: bool,
) -> Result<()> {
    verify_bundle(app, None).context("当前应用不是受信任的签名安装版，开发构建不自动替换")?;
    ensure!(
        is_newer(
            release.version(),
            &plist(app, "CFBundleShortVersionString")?
        )?,
        "此应用已是相同或更高版本"
    );
    let updates = data.join("updates");
    config::private_dir(&updates)?;
    let lock = File::create(updates.join("update.lock"))?;
    lock.try_lock_exclusive()
        .context("另一个更新任务正在运行")?;
    let requested_preferences = Preferences::load(data)?;
    ensure!(
        requested_preferences.channel == "preview" || version(release.version())?.pre.is_empty(),
        "稳定通道不安装预发布版本，请重新检查更新"
    );
    let archive = updates.join("download.zip");
    let cached = archive.is_file()
        && archive.metadata()?.len() == release.manifest.size
        && file_hash(&archive)?.eq_ignore_ascii_case(&release.manifest.sha256);
    if cached {
        progress(100);
    } else {
        let partial = updates.join("download.zip.part");
        download(client, release, &partial, &progress).await?;
        fs::rename(partial, &archive)?;
    }
    let parent = app.parent().context("无效应用目录")?;
    let stage = parent.join(format!(".dauerbot-update-{}", uuid::Uuid::new_v4()));
    config::private_dir(&stage).context("无法写入应用所在目录，请先把应用移到 Applications")?;
    let payload = stage.join("payload");
    let prepared = (|| {
        extract(&archive, &payload)?;
        verify_bundle(&payload, Some(release.version()))?;
        ensure!(
            Preferences::load(data)?.channel == requested_preferences.channel,
            "更新通道已更改，保留当前版本，请重新检查更新"
        );
        if automatic {
            ensure!(
                Preferences::load(data)? == requested_preferences
                    && requested_preferences.automatic,
                "自动升级已关闭，保留当前版本"
            );
        }
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = prepared {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    let backup = stage.join("old");
    let record_path = updates.join("installed.json");
    let previous = if record_path.exists() {
        Some(fs::read(&record_path)?)
    } else {
        None
    };
    let previous_record = previous
        .as_ref()
        .map(|bytes| serde_json::from_slice::<InstallRecord>(bytes))
        .transpose()
        .context("旧升级记录损坏，已保留原应用和记录")?;
    let record = InstallRecord {
        app: app.to_owned(),
        backup: backup.clone(),
        version: release.version().into(),
    };
    // Save recovery information before the first rename; disk errors cannot strand an unrecorded swap.
    config::write_private(&record_path, &serde_json::to_vec(&record)?)?;
    if let Err(error) = swap(&payload, app, &backup) {
        if app.exists() {
            restore_record(&record_path, previous.as_deref())?;
        }
        return Err(error);
    }
    let executable = app.join("Contents/MacOS/hfb");
    if let Err(error) = output(Command::new(&executable).arg("--version")).and_then(|v| {
        ensure!(v == format!("hfb {}", release.version()), "新 CLI 版本不符");
        Ok(())
    }) {
        let failed = stage.join("failed");
        fs::rename(app, failed)?;
        fs::rename(&backup, app)?;
        restore_record(&record_path, previous.as_deref())?;
        let _ = fs::remove_dir_all(&stage);
        return Err(error).context("新程序启动检查失败，已恢复旧应用");
    }
    if let Some(old) = previous_record
        && old.app == app
        && let Some(parent) = old.backup.parent()
        && parent.parent() == app.parent()
        && parent
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(".dauerbot-update-"))
    {
        let _ = fs::remove_dir_all(parent);
    }
    Ok(())
}
fn restore_record(path: &Path, previous: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = previous {
        config::write_private(path, bytes)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}
pub fn rollback(data: &Path) -> Result<PathBuf> {
    let lock = File::create(data.join("updates/update.lock"))?;
    lock.try_lock_exclusive()
        .context("另一个更新任务正在运行")?;
    let path = data.join("updates/installed.json");
    let record: InstallRecord =
        serde_json::from_slice(&fs::read(&path).context("没有可回退的安装记录")?)?;
    let stage = record.backup.parent().context("无效备份目录")?;
    ensure!(
        stage.parent() == record.app.parent()
            && stage
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".dauerbot-update-")),
        "备份不属于此应用"
    );
    verify_bundle(&record.backup, None)?;
    let mut prefs = Preferences::load(data)?;
    prefs.automatic = false;
    prefs.save(data)?;
    if record.app.exists() {
        swap(&record.backup, &record.app, &stage.join("rolled-back"))?;
    } else {
        fs::rename(&record.backup, &record.app)?;
    }
    fs::remove_file(path)?;
    Ok(record.app)
}
pub async fn rollback_and_restart(data: &Path) -> Result<()> {
    let app = rollback(data)?;
    // A CLI-only install may not have started a daemon at all.
    if !data.join("config.json").exists() {
        return Ok(());
    }
    let cfg = config::Config::load(data)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()?;
    let probe = client
        .get(format!("{}/api/state", cfg.url()))
        .bearer_auth(&cfg.token)
        .send()
        .await;
    let Ok(response) = probe else {
        return Ok(());
    };
    response.error_for_status()?;
    stop_service(data, &cfg).await?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("desktop-service.log"))?;
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        paths.extend([
            PathBuf::from(&home).join(".local/bin"),
            PathBuf::from(home).join(".cargo/bin"),
        ]);
    }
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    Command::new(app.join("Contents/MacOS/hfb"))
        .arg("--data-dir")
        .arg(data)
        .arg("serve")
        .env("PATH", std::env::join_paths(paths)?)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    Ok(())
}
pub fn relaunch_after_exit(app: &Path, data: &Path, pid: u32) -> Result<()> {
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("update-relaunch.log"))?;
    // Positional arguments avoid interpolating paths into shell source.
    Command::new("/bin/sh").arg("-c")
        .arg("n=0; while kill -0 \"$1\" 2>/dev/null; do n=$((n+1)); [ \"$n\" -lt 100 ] || exit 1; sleep 0.2; done; exec \"$2/Contents/MacOS/HerdrBridge\" --data-dir \"$3\"")
        .arg("dauerbot-relaunch").arg(pid.to_string()).arg(app).arg(data)
        .stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?;
    Ok(())
}

/// Stop only the authenticated local daemon; HERDR terminals and MCP processes remain alive.
pub async fn stop_service(data: &Path, cfg: &config::Config) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client
        .post(format!("{}/api/service/shutdown", cfg.url()))
        .bearer_auth(&cfg.token)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // Compatibility for the first release, which had no shutdown endpoint.
        // Identify by listening port AND the exact daemon arguments, never by executable name.
        let pids = output(
            Command::new("/usr/sbin/lsof")
                .args(["-nP", "-t"])
                .arg(format!("-iTCP:{}", cfg.port))
                .arg("-sTCP:LISTEN"),
        )?;
        let pids: Vec<u32> = pids
            .lines()
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
        ensure!(
            pids.len() == 1 && pids[0] != std::process::id(),
            "无法唯一识别旧后台，未停止任何进程"
        );
        let pid = pids[0].to_string();
        let command = output(Command::new("/bin/ps").args(["-p", &pid, "-o", "command="]))?;
        ensure!(
            command.contains("/hfb ")
                && command.contains(&format!("--data-dir {} serve", data.display())),
            "端口所属进程不是此应用的后台"
        );
        output(Command::new("/bin/kill").args(["-TERM", &pid]))?;
    } else {
        response.error_for_status()?;
    }
    tokio::time::timeout(Duration::from_secs(110), async {
        loop {
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(data.join("daemon.lock"))?;
            if lock.try_lock_exclusive().is_ok() {
                FileExt::unlock(&lock)?;
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .context("旧后台仍在完成请求，已保留原服务和会话，请稍后重新打开桌面")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_order_and_channels_do_not_follow_publish_dates() {
        let release = |tag: &str, pre| GithubRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: pre,
            html_url: String::new(),
            body: String::new(),
            assets: vec![],
        };
        assert!(!is_newer("0.3.0+build.2", "0.3.0+build.1").unwrap());
        let versions = vec![
            release("v0.3.9", false),
            release("v0.3.10", false),
            release("v0.4.0-beta.1", true),
            release("v0.2.99", false),
        ];
        assert_eq!(
            newest(versions.clone(), "stable").unwrap().tag_name,
            "v0.3.10"
        );
        assert_eq!(
            newest(versions, "preview").unwrap().tag_name,
            "v0.4.0-beta.1"
        );
    }
    #[test]
    fn rejects_archive_traversal_and_links_before_writing() {
        for name in [
            "/etc/file",
            "../file",
            "Dauerbot.app/../../file",
            "other/file",
            "Dauerbot.app/..\\\\file",
        ] {
            assert!(safe_entry(name, 0o100644).is_err(), "{name}");
        }
        assert!(safe_entry("Dauerbot.app/Contents/link", 0o120777).is_err());
        assert_eq!(
            safe_entry("Dauerbot.app/Contents/Info.plist", 0o100644)
                .unwrap()
                .unwrap(),
            PathBuf::from("Contents/Info.plist")
        );
    }
    #[test]
    fn failed_swap_restores_the_previous_app() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = dir.path().join("Dauerbot.app");
        fs::create_dir(&app)?;
        fs::write(app.join("version"), "old")?;
        let backup = dir.path().join("backup");
        assert!(swap(&dir.path().join("missing-payload"), &app, &backup).is_err());
        assert_eq!(fs::read_to_string(app.join("version"))?, "old");
        assert!(!backup.exists());
        Ok(())
    }
    #[test]
    fn manifest_signature_covers_version_hash_and_identity() -> Result<()> {
        let key = ed25519_dalek::SigningKey::from_bytes(&[19; 32]);
        use ed25519_dalek::Signer;
        let public = STANDARD.encode(key.verifying_key().as_bytes());
        let manifest = Manifest {
            version: "0.3.0".into(),
            bundle_id: BUNDLE_ID.into(),
            team_id: TEAM_ID.into(),
            architecture: architecture().into(),
            file: format!("Dauerbot-v0.3.0-macos-{}.zip", architecture()),
            size: 10,
            sha256: "a".repeat(64),
        };
        let bytes = serde_json::to_vec(&manifest)?;
        let sig = STANDARD.encode(key.sign(&[SIGNING_CONTEXT, &bytes].concat()).to_bytes());
        assert!(verify_manifest(&bytes, sig.as_bytes(), &public).is_ok());
        let mut changed = bytes.clone();
        changed[12] ^= 1;
        assert!(verify_manifest(&changed, sig.as_bytes(), &public).is_err());
        let other = STANDARD.encode(
            ed25519_dalek::SigningKey::from_bytes(&[20; 32])
                .verifying_key()
                .as_bytes(),
        );
        assert!(verify_manifest(&bytes, sig.as_bytes(), &other).is_err());
        Ok(())
    }
}
