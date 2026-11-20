//! Presentation-only native relationships. Never a child-liveness or retirement proof.
use crate::{db, db::Project};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_NODES: usize = 128;
const MAX_DEPTH: usize = 8;
const MAX_TREE_BYTES: u64 = 65_536;
const FRESH_MS: i64 = 10_000;

struct Sample {
    value: Value,
    started: Instant,
    running: bool,
}
type Samples = Arc<Mutex<std::collections::HashMap<String, Sample>>>;
static SAMPLES: OnceLock<Samples> = OnceLock::new();

/// State requests return the scoped read model immediately. Native file access
/// runs on a blocking worker, single-flight per scope, never on the UI thread.
pub(crate) async fn observe(p: Project) -> Value {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".codex")));
    let key = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        p.id,
        p.generation,
        p.native_session,
        p.agent,
        p.execution["child_adapter"],
        p.execution["repair_lane"],
        home.as_ref()
            .map(|v| v.to_string_lossy())
            .unwrap_or_default()
    );
    let samples = SAMPLES
        .get_or_init(|| Arc::new(Mutex::new(Default::default())))
        .clone();
    let fallback = snapshot(
        &p,
        "unknown",
        vec![],
        "Native relationships have not been observed yet",
    );
    let Ok(mut cache) = samples.lock() else {
        return fallback;
    };
    if cache.len() >= 64 && !cache.contains_key(&key) {
        cache.retain(|_, v| v.running);
        if cache.len() >= 64 {
            return fallback;
        }
    }
    let entry = cache.entry(key.clone()).or_insert_with(|| Sample {
        value: fallback,
        started: Instant::now() - Duration::from_secs(3),
        running: false,
    });
    let result = entry.value.clone();
    if !entry.running && entry.started.elapsed() >= Duration::from_secs(2) {
        entry.running = true;
        entry.started = Instant::now();
        let worker_samples = samples.clone();
        tokio::spawn(async move {
            let failed = snapshot(
                &p,
                "unknown",
                vec![],
                "Native relationship observation failed",
            );
            let value =
                if p.agent == "codex" && !p.generation.is_empty() && !p.native_session.is_empty() {
                    codex_snapshot(&p).await
                } else {
                    tokio::task::spawn_blocking(move || observe_at(&p, db::now()))
                        .await
                        .unwrap_or(failed)
                };
            if let Ok(mut cache) = worker_samples.lock()
                && let Some(sample) = cache.get_mut(&key)
            {
                sample.value = value;
                sample.running = false;
            }
        });
    }
    result
}

fn root(p: &Project) -> Value {
    json!({"id":"root","parent_id":null,"name":p.name,"cli":p.agent,"state":"unknown","observation":if p.execution.get("repair_lane").is_some(){"Repair-only native execution; original conversation is preserved"}else{"Current project session; activity is observed separately"}})
}
fn snapshot(p: &Project, status: &str, mut nodes: Vec<Value>, observation: &str) -> Value {
    nodes.insert(0, root(p));
    let view = crate::repair_lane::child_view(p);
    json!({"project_id":p.id,"generation":p.generation,"native_session":view.native_session,"project_native_session":p.native_session,"observed_at":db::now(),"status":status,"observation":observation,"nodes":nodes})
}
fn observe_at(p: &Project, now: i64) -> Value {
    if p.generation.is_empty() || p.native_session.is_empty() {
        return snapshot(
            p,
            "unknown",
            vec![],
            "No matching ordinary native execution is observed",
        );
    }
    let observed = match p.agent.as_str() {
        "pi" => pi(&crate::repair_lane::child_view(p), now),
        "codex" => {
            return snapshot(
                p,
                "unknown",
                vec![],
                "Codex metadata is observed asynchronously",
            );
        }
        _ => {
            return snapshot(
                p,
                "unknown",
                vec![],
                "This CLI has no verified native child relationship source",
            );
        }
    };
    match observed {
        Ok((status, nodes, observation)) => snapshot(p, &status, nodes, &observation),
        Err(_) => snapshot(
            p,
            "unknown",
            vec![],
            "Native relationship metadata is unavailable or does not match this session",
        ),
    }
}

fn bounded(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_TREE_BYTES,
        "Invalid native tree file"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_TREE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_TREE_BYTES,
        "Native tree exceeds bound"
    );
    Ok(bytes)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PiTree {
    schema: u8,
    generation: String,
    session: String,
    #[serde(rename = "sessionFile")]
    session_file: String,
    updated: i64,
    nodes: Vec<PiNode>,
    truncated: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PiNode {
    id: String,
    parent_id: Option<String>,
    name: String,
    state: String,
}
fn label(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}
fn pi(p: &Project, now: i64) -> Result<(String, Vec<Value>, String)> {
    let dir = p.execution["child_adapter"]
        .as_str()
        .context("No managed adapter")?;
    let value: PiTree = serde_json::from_slice(&bounded(&Path::new(dir).join("tree.json"))?)?;
    ensure!(
        value.schema == 1
            && value.generation == p.generation
            && (value.session == p.native_session || value.session_file == p.native_session)
            && value.nodes.len() < MAX_NODES
            && value.updated <= now,
        "Native tree identity mismatch"
    );
    let mut seen = HashSet::from(["root".to_owned()]);
    let mut depths = std::collections::HashMap::from([("root".to_owned(), 0usize)]);
    let mut nodes = Vec::new();
    // The publisher emits parents first. Reject cycles, duplicates and orphaned
    // edges instead of moving them under a convenient project by guesswork.
    for n in value.nodes {
        ensure!(
            label(&n.id) && n.id != "root" && label(&n.name) && !seen.contains(&n.id),
            "Invalid native tree node"
        );
        let parent = n.parent_id.context("Child has no parent")?;
        ensure!(
            seen.contains(&parent) && depths.get(&parent).is_some_and(|depth| *depth < MAX_DEPTH),
            "Unknown or over-depth native parent"
        );
        depths.insert(n.id.clone(), depths[&parent] + 1);
        ensure!(
            matches!(
                n.state.as_str(),
                "pending" | "running" | "completed" | "failed" | "aborted"
            ),
            "Unknown native state"
        );
        seen.insert(n.id.clone());
        nodes.push(json!({"id":n.id,"parent_id":parent,"name":n.name,"cli":"pi","state":if now-value.updated>FRESH_MS {"unknown"}else{&n.state},"observation":"Managed native subagent invocation; presentation only"}));
    }
    let stale = now - value.updated > FRESH_MS;
    Ok((
        if stale {
            "stale"
        } else if value.truncated {
            "partial"
        } else {
            "observed"
        }
        .into(),
        nodes,
        "Managed invocation metadata does not establish retirement coverage".into(),
    ))
}

async fn codex_snapshot(p: &Project) -> Value {
    let view = crate::repair_lane::child_view(p);
    match crate::codex_tree::query(&view).await {
        Ok(nodes) => {
            let mut result = snapshot(
                p,
                "partial",
                nodes,
                "Native recorded relationships and verified lifecycle when available; original process activity and complete coverage are unknown",
            );
            result["observed_native_session"] = json!(view.native_session);
            result
        }
        Err(_) => snapshot(
            p,
            "unknown",
            vec![],
            "Scoped native metadata API is unavailable or unsupported",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, ProjectInput};
    async fn fixture() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("store")).unwrap();
        let mut p = db
            .create(ProjectInput {
                name: "Named project".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "pi".into(),
                chat_id: String::new(),
                native_session: "native-A".into(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        p.generation = "generation".into();
        p.execution = json!({"child_adapter":dir.path()});
        (dir, p)
    }
    fn write(dir: &Path, p: &Project, nodes: Value, updated: i64) {
        fs::write(dir.join("tree.json"),serde_json::to_vec(&json!({"schema":1,"generation":p.generation,"session":p.native_session,"sessionFile":"/native/A.jsonl","updated":updated,"nodes":nodes,"truncated":false})).unwrap()).unwrap();
    }
    #[tokio::test]
    async fn presentation_preserves_named_parentage_but_cannot_prove_child_retirement() {
        let (dir, p) = fixture().await;
        write(
            dir.path(),
            &p,
            json!([{"id":"pi:one","parent_id":"root","name":"reviewer","state":"running"},{"id":"pi:two","parent_id":"pi:one","name":"validator","state":"completed"}]),
            100,
        );
        let observed = observe_at(&p, 100);
        assert_eq!(observed["status"], "observed");
        assert_eq!(observed["nodes"][2]["parent_id"], "pi:one");
        assert_eq!(observed["nodes"][1]["name"], "reviewer");
        assert_eq!(
            crate::native_children::observe(&p, 100)["state"],
            "unknown",
            "Presentation is not a retirement census"
        );
        let stale = observe_at(&p, 10_101);
        assert_eq!(stale["status"], "stale");
        assert_eq!(stale["nodes"][1]["state"], "unknown");
        let mut changed = p.clone();
        changed.generation = "next-generation".into();
        assert_eq!(observe_at(&changed, 100)["status"], "unknown");
    }
    #[tokio::test]
    async fn invalid_edges_oversize_and_other_sessions_do_not_become_project_children() {
        let (dir, p) = fixture().await;
        for nodes in [
            json!([{"id":"pi:a","parent_id":"pi:a","name":"cycle","state":"running"}]),
            json!([{"id":"pi:a","parent_id":"root","name":"one","state":"running"},{"id":"pi:a","parent_id":"root","name":"duplicate","state":"running"}]),
            json!([{"id":"pi:a","parent_id":"unrelated","name":"foreign","state":"running"}]),
        ] {
            write(dir.path(), &p, nodes, 100);
            assert_eq!(observe_at(&p, 100)["status"], "unknown");
        }
        write(dir.path(), &p, json!([]), 100);
        let mut other = p.clone();
        other.native_session = "native-B".into();
        assert_eq!(observe_at(&other, 100)["status"], "unknown");
        fs::write(
            dir.path().join("tree.json"),
            vec![b' '; MAX_TREE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(observe_at(&p, 100)["status"], "unknown");
    }
    #[tokio::test]
    async fn repair_tree_observes_b_without_relabeling_old_a_children_or_persisting_b() {
        let (dir, mut p) = fixture().await;
        write(
            dir.path(),
            &p,
            json!([{"id":"pi:old","parent_id":"root","name":"old-A","state":"running"}]),
            100,
        );
        p.execution["repair_lane"] = json!({"request_id":"repair","generation":p.generation,"expected_native":p.native_session,"observed_native":"native-B","mcp_registered":true});
        assert_eq!(observe_at(&p, 100)["status"], "unknown");
        let view = crate::repair_lane::child_view(&p);
        write(
            dir.path(),
            &view,
            json!([{"id":"pi:new","parent_id":"root","name":"repair-B-worker","state":"running"}]),
            100,
        );
        let tree = observe_at(&p, 100);
        assert_eq!(tree["native_session"], "native-B");
        assert_eq!(tree["project_native_session"], "native-A");
        assert_eq!(tree["nodes"][1]["name"], "repair-B-worker");
        assert_eq!(p.native_session, "native-A");
    }
}
