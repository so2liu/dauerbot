//! Managed native-child evidence. Parent terminal state cannot prove child idle.
use crate::{config, db::Project};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub const PI_VERSIONS: &[&str] = &["1.5.1"];
const FRESH_MS: i64 = 10_000;

/// Explicit coverage; unknown is a retirement veto, never a successful census.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Active,
    Quiescent,
    Unknown,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    schema: u8,
    generation: String,
    session: String,
    #[serde(rename = "sessionFile")]
    session_file: String,
    epoch: String,
    revision: u64,
    state: State,
    updated: i64,
}

pub fn install(
    dir: &Path,
    generation: &str,
    version: &str,
    args: &[String],
    mcp: &Value,
) -> Result<PathBuf> {
    config::private_dir(dir)?;
    for (name, contents) in [
        ("index.ts", include_str!("../assets/pi-subagent/index.ts")),
        ("agents.ts", include_str!("../assets/pi-subagent/agents.ts")),
        (
            "coverage.ts",
            include_str!("../assets/pi-subagent/coverage.ts"),
        ),
        (
            "managed.ts",
            include_str!("../assets/pi-subagent/managed.ts"),
        ),
        ("LICENSE", include_str!("../assets/pi-subagent/LICENSE")),
        (
            "PROVENANCE.md",
            include_str!("../assets/pi-subagent/PROVENANCE.md"),
        ),
    ] {
        config::write_private(&dir.join(name), contents.as_bytes())?;
    }
    config::private_dir(&dir.join("agents"))?;
    config::write_private(
        &dir.join("agents/worker.md"),
        include_bytes!("../assets/pi-subagent/agents/worker.md"),
    )?;
    // Extra native extension switches are preserved, but cannot prove coverage.
    let incomplete = !PI_VERSIONS.contains(&version)
        || args
            .iter()
            .any(|s| s.starts_with("-e") || s.starts_with("--extension"));
    config::write_private(
        &dir.join("managed.json"),
        &serde_json::to_vec(&json!({"generation":generation,"incomplete":incomplete,"mcp":mcp}))?,
    )?;
    Ok(dir.join("managed.ts"))
}

fn directory(p: &Project) -> Option<PathBuf> {
    (p.agent == "pi")
        .then(|| p.execution["child_adapter"].as_str().map(PathBuf::from))
        .flatten()
}

/// Bounded, fresh, identity-matched, atomically published evidence only.
pub fn observe(p: &Project, now: i64) -> Value {
    let unknown = || json!({"state":"unknown","revision":null});
    let Some(dir) = directory(p) else {
        return unknown();
    };
    let read = || -> Result<Evidence> {
        let file = dir.join("children.json");
        let meta = fs::symlink_metadata(&file)?;
        ensure!(
            meta.is_file() && meta.len() <= 4096,
            "Invalid child evidence file"
        );
        let mut bytes = Vec::new();
        fs::File::open(file)?.take(4097).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "Child evidence exceeds its size bound");
        let value: Evidence = serde_json::from_slice(&bytes)?;
        ensure!(
            value.schema == 1
                && value.generation == p.generation
                && !p.native_session.is_empty()
                && (value.session == p.native_session || value.session_file == p.native_session)
                && !value.epoch.is_empty()
                && value.updated <= now
                && now - value.updated <= FRESH_MS,
            "Unknown child coverage"
        );
        Ok(value)
    };
    let Ok(value) = read() else { return unknown() };
    json!({"state":value.state,"revision":format!("{}:{}:{}",value.session,value.epoch,value.revision)})
}

/// Atomic mkdir is shared with Node's synchronous dispatch critical section.
/// A crashed holder leaves a veto; never guess that an old lock is safe to break.
pub struct RetirementGate {
    dir: PathBuf,
    committed: bool,
    marked: bool,
}
impl RetirementGate {
    pub fn acquire(p: &Project, expected: &Value, now: i64) -> Result<Self> {
        let dir = directory(p).ok_or_else(|| anyhow::anyhow!("Child coverage is unknown"))?;
        fs::create_dir(dir.join("dispatch.lock"))?;
        let mut gate = Self {
            dir,
            committed: false,
            marked: false,
        };
        ensure!(
            !gate.dir.join("retiring").exists(),
            "Native child dispatch already fenced"
        );
        let current = observe(p, now);
        ensure!(
            current["state"] == "quiescent" && current == *expected,
            "Child activity changed before retirement"
        );
        config::write_private(
            &gate.dir.join("retiring"),
            b"Retirement claimed; native child dispatch disabled\n",
        )?;
        gate.marked = true;
        Ok(gate)
    }
    pub fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for RetirementGate {
    fn drop(&mut self) {
        if self.marked && !self.committed {
            let _ = fs::remove_file(self.dir.join("retiring"));
        }
        let _ = fs::remove_dir(self.dir.join("dispatch.lock"));
    }
}
