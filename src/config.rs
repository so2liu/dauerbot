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
    /// Previous Core settings are retained for reading existing installations.
    pub hc: String,
    pub herdr: String,
    pub herdr_session: String,
    pub herdr_config: String,
    pub core_session: String,
    pub feishu_base: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub app_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub app_secret: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 49321,
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            hc: "hc".into(),
            herdr: find_program("herdr"),
            herdr_session: "dauerbot".into(),
            herdr_config: String::new(),
            core_session: "default".into(),
            feishu_base: "https://open.feishu.cn".into(),
            app_id: String::new(),
            app_secret: String::new(),
        }
    }
}

fn find_program(name: &str) -> String {
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| {
                p.join(if cfg!(windows) {
                    format!("{name}.exe")
                } else {
                    name.into()
                })
            })
            .find(|p| p.is_file())
    }) {
        return path.to_string_lossy().into_owned();
    }
    if let Some(home) = directories::BaseDirs::new() {
        let path = home.home_dir().join(".local/bin").join(name);
        if path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }
    name.into()
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
        use sha2::{Digest, Sha256};
        private_dir(dir)?;
        let path = dir.join("config.json");
        let raw = if path.exists() {
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path)?)
                .context("Cannot read configuration")?
        } else {
            serde_json::json!({})
        };
        let mut cfg: Self =
            serde_json::from_value(raw.clone()).context("Cannot read configuration")?;
        let canonical = fs::canonicalize(dir)?;
        if raw.get("herdr_session").is_none() {
            let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
            cfg.herdr_session = format!(
                "dauerbot-{}",
                digest[..6]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
        }
        // A product-owned file prevents personal HERDR settings from restoring
        // native Agents independently of the durable supervisor.
        if cfg.herdr_config.is_empty() {
            cfg.herdr_config = canonical
                .join("legacy-herdr.toml")
                .to_string_lossy()
                .into_owned();
        }
        let owned_config = Path::new(&cfg.herdr_config);
        if !owned_config.exists() {
            write_private(
                owned_config,
                b"onboarding = false\n[session]\nresume_agents_on_restore = false\n",
            )?;
        }
        cfg.validate()?;
        if !path.exists() || raw.get("herdr_session").is_none() || raw.get("herdr_config").is_none()
        {
            cfg.save(dir)?;
        }
        Ok(cfg)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        self.validate()?;
        write_private(&dir.join("config.json"), &serde_json::to_vec_pretty(self)?)
    }
    fn validate(&self) -> Result<()> {
        if self.port == 0 || self.token.len() < 32 {
            bail!("Invalid port or token configuration");
        }
        if self.herdr.trim().is_empty()
            || self.herdr_session.is_empty()
            || self.herdr_session.len() > 64
            || !self
                .herdr_session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            bail!(
                "Provide a HERDR executable and a session name using letters, digits, hyphens, or underscores"
            );
        }
        let url = reqwest::Url::parse(&self.feishu_base)?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost")))
        {
            bail!("Feishu endpoints require HTTPS (HTTP is allowed for local tests)");
        }
        Ok(())
    }
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}
