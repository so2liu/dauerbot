use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub token: String,
    pub hc: String,
    pub core_session: String,
    pub feishu_base: String,
    pub app_id: String,
    pub app_secret: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 49321,
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            hc: find_hc(),
            core_session: "default".into(),
            feishu_base: "https://open.feishu.cn".into(),
            app_id: String::new(),
            app_secret: String::new(),
        }
    }
}

fn find_hc() -> String {
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(if cfg!(windows) { "hc.exe" } else { "hc" }))
            .find(|p| p.is_file())
    }) {
        return path.to_string_lossy().into_owned();
    }
    if let Some(home) = directories::BaseDirs::new() {
        let path = home.home_dir().join(".local/bin/hc");
        if path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }
    "hc".into()
}

pub fn default_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "herdr", "feishu-bridge")
        .expect("home directory is available")
        .data_local_dir()
        .to_owned()
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    Ok(())
}

impl Config {
    pub fn load(dir: &Path) -> Result<Self> {
        private_dir(dir)?;
        let path = dir.join("config.json");
        if path.exists() {
            let cfg: Self = serde_json::from_slice(&fs::read(path)?).context("读取配置失败")?;
            cfg.validate()?;
            Ok(cfg)
        } else {
            let cfg = Self::default();
            cfg.save(dir)?;
            Ok(cfg)
        }
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        self.validate()?;
        write_private(&dir.join("config.json"), &serde_json::to_vec_pretty(self)?)
    }
    fn validate(&self) -> Result<()> {
        if self.port == 0 || self.token.len() < 32 || self.core_session.is_empty() {
            bail!("port、token 或 core_session 配置无效");
        }
        let url = reqwest::Url::parse(&self.feishu_base)?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost")))
        {
            bail!("飞书接口必须为 HTTPS（本机测试可用 HTTP）");
        }
        Ok(())
    }
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}
