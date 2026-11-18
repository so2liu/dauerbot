//! Version-scoped, metadata-only Codex app-server observer. No direct native DB access.
use crate::db::Project;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc, oneshot},
};
const MAX_NODES: usize = 127;
const MAX_BYTES: u64 = 2_097_152;
const TIMEOUT: Duration = Duration::from_secs(5);
struct Query {
    root: String,
    reply: oneshot::Sender<Result<Vec<Value>>>,
}
static HELPERS: OnceLock<Mutex<HashMap<String, mpsc::Sender<Query>>>> = OnceLock::new();

pub(crate) async fn query(p: &Project) -> Result<Vec<Value>> {
    query_with_cli(p, Path::new("codex")).await
}
async fn query_with_cli(p: &Project, cli: &Path) -> Result<Vec<Value>> {
    uuid::Uuid::parse_str(&p.native_session)?;
    let key = format!(
        "{}:{}:{}",
        cli.display(),
        p.path,
        std::env::var("CODEX_HOME").unwrap_or_default()
    );
    let mut helpers = HELPERS.get_or_init(Default::default).lock().await;
    helpers.retain(|_, sender| !sender.is_closed());
    let sender = if let Some(sender) = helpers.get(&key) {
        sender.clone()
    } else {
        ensure!(helpers.len() < 8, "Native observer capacity reached");
        let (sender, receiver) = mpsc::channel(4);
        helpers.insert(key, sender.clone());
        let cli = cli.to_owned();
        let cwd = p.path.clone();
        tokio::spawn(async move {
            run(&cli, Path::new(&cwd), receiver).await;
        });
        sender
    };
    drop(helpers);
    let (reply, receive) = oneshot::channel();
    sender
        .try_send(Query {
            root: p.native_session.clone(),
            reply,
        })
        .context("Native observer busy")?;
    tokio::time::timeout(Duration::from_secs(12), receive).await??
}
struct Server {
    child: Child,
    input: ChildStdin,
    output: tokio::io::Take<BufReader<ChildStdout>>,
    sequence: u64,
}
impl Server {
    async fn start(cli: &Path, cwd: &Path) -> Result<Self> {
        // Public protocol proof: official rust-v0.169.1, c376cf7396eff83ce4c6ac61b9553df142e7a0be,
        // ThreadListParams ancestorThreadId/useStateDbOnly/limit. Unknown versions
        // fail closed: older servers may silently ignore a scoped filter.
        let version = tokio::time::timeout(
            Duration::from_secs(2),
            Command::new(cli)
                .arg("--version")
                .current_dir(cwd)
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(
            version.status.success()
                && std::str::from_utf8(&version.stdout)?.trim() == "codex-cli 0.169.1",
            "Native scoped metadata protocol is not verified for this CLI version"
        );
        let mut child = Command::new(cli)
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("No metadata input")?;
        let output =
            BufReader::new(child.stdout.take().context("No metadata output")?).take(MAX_BYTES);
        let mut server = Self {
            child,
            input,
            output,
            sequence: 0,
        };
        server.call("initialize",json!({"clientInfo":{"name":"dauerbot-native-tree","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
        server.write(json!({"method":"initialized"})).await?;
        Ok(server)
    }
    async fn write(&mut self, message: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        self.write(json!({"id":id,"method":method,"params":params}))
            .await?;
        for _ in 0..32 {
            let mut line = Vec::new();
            let n = self.output.read_until(b'\n', &mut line).await?;
            ensure!(
                n > 0 && line.last() == Some(&b'\n'),
                "Native metadata response exceeded its bound or closed"
            );
            let value: Value = serde_json::from_slice(&line)?;
            if value["id"] == id {
                ensure!(
                    value.get("error").is_none(),
                    "Native scoped metadata request rejected"
                );
                return value
                    .get("result")
                    .cloned()
                    .context("No native metadata result");
            }
        }
        bail!("Too many native metadata notifications")
    }
    async fn children(&mut self, root: &str) -> Result<Vec<Value>> {
        let result=self.call("thread/list",json!({"ancestorThreadId":root,"useStateDbOnly":true,"limit":MAX_NODES,"sourceKinds":["subAgentThreadSpawn"],"archived":false})).await?;
        // Preview/title/turns and all other response fields are deliberately
        // discarded. Only parent-linked native identity and names leave here.
        parse(root, &result)
    }
    async fn close(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}
async fn run(cli: &Path, cwd: &Path, mut receiver: mpsc::Receiver<Query>) {
    let mut server: Option<Server> = None;
    while let Ok(Some(query)) = tokio::time::timeout(Duration::from_secs(15), receiver.recv()).await
    {
        if query.reply.is_closed() {
            continue;
        }
        if server.is_none() {
            match tokio::time::timeout(TIMEOUT, Server::start(cli, cwd)).await {
                Ok(Ok(started)) => server = Some(started),
                _ => {
                    let _ = query
                        .reply
                        .send(Err(anyhow::anyhow!("Native metadata observer unavailable")));
                    continue;
                }
            }
        }
        let result =
            tokio::time::timeout(TIMEOUT, server.as_mut().unwrap().children(&query.root)).await;
        match result {
            Ok(Ok(nodes)) => {
                let _ = query.reply.send(Ok(nodes));
            }
            _ => {
                if let Some(mut failed) = server.take() {
                    failed.close().await;
                }
                let _ = query.reply.send(Err(anyhow::anyhow!(
                    "Scoped native metadata observation failed"
                )));
            }
        }
    }
    if let Some(mut server) = server {
        server.close().await;
    }
}
fn parse(root: &str, result: &Value) -> Result<Vec<Value>> {
    let rows = result["data"]
        .as_array()
        .context("No native metadata rows")?;
    ensure!(
        rows.len() <= MAX_NODES,
        "Native metadata exceeds node bound"
    );
    let mut waiting = HashMap::new();
    for row in rows {
        let id = row["id"].as_str().context("No native child ID")?;
        uuid::Uuid::parse_str(id)?;
        let parent = row["parentThreadId"]
            .as_str()
            .context("No native child parent")?;
        uuid::Uuid::parse_str(parent)?;
        ensure!(
            id != root && !waiting.contains_key(id),
            "Duplicate or cyclic native child"
        );
        let name = row["agentNickname"]
            .as_str()
            .or_else(|| row["agentRole"].as_str())
            .unwrap_or(id);
        ensure!(
            !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
            "Invalid native child name"
        );
        waiting.insert(id.to_owned(), (parent.to_owned(), name.to_owned()));
    }
    let mut queue = VecDeque::from([(root.to_owned(), 0usize)]);
    let mut seen = HashSet::from([root.to_owned()]);
    let mut nodes = Vec::new();
    while let Some((parent, depth)) = queue.pop_front() {
        let mut children: Vec<_> = waiting
            .iter()
            .filter(|(_, v)| v.0 == parent)
            .map(|(id, _)| id.clone())
            .collect();
        children.sort();
        for id in children {
            let (_, name) = waiting.remove(&id).unwrap();
            ensure!(
                depth < 8 && seen.insert(id.clone()),
                "Native hierarchy exceeds bound"
            );
            nodes.push(json!({"id":format!("codex:{id}"),"parent_id":if parent==root{"root".into()}else{format!("codex:{parent}")},"name":name,"cli":"codex","state":"unknown","observation":"Native recorded relationship; live activity is unknown"}));
            queue.push_back((id, depth + 1));
        }
    }
    // Pagination can omit a parent outside this bounded sample; never invent it.
    ensure!(
        waiting.is_empty(),
        "Native metadata contains an orphan, cycle, or out-of-scope child"
    );
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, ProjectInput};
    const ROOT: &str = "00000000-0000-4000-8000-000000000001";
    const CHILD: &str = "00000000-0000-4000-8000-000000000002";
    const GRAND: &str = "00000000-0000-4000-8000-000000000003";
    #[test]
    fn scoped_metadata_rejects_cycles_duplicates_and_orphans_without_exposing_previews() {
        let rows = json!({"data":[{"id":GRAND,"parentThreadId":CHILD,"agentNickname":"validator","preview":"private prompt"},{"id":CHILD,"parentThreadId":ROOT,"agentNickname":"reviewer","preview":"private prompt","turns":["private turn"]}]});
        let nodes = parse(ROOT, &rows).unwrap();
        assert_eq!(nodes[0]["name"], "reviewer");
        assert_eq!(nodes[1]["parent_id"], format!("codex:{CHILD}"));
        assert!(!serde_json::to_string(&nodes).unwrap().contains("private"));
        assert_eq!(nodes[0]["state"], "unknown");
        for rows in [
            json!({"data":[{"id":CHILD,"parentThreadId":GRAND},{"id":GRAND,"parentThreadId":CHILD}]}),
            json!({"data":[{"id":CHILD,"parentThreadId":ROOT},{"id":CHILD,"parentThreadId":ROOT}]}),
            json!({"data":[{"id":GRAND,"parentThreadId":CHILD}]}),
        ] {
            assert!(parse(ROOT, &rows).is_err());
        }
    }
    #[tokio::test]
    async fn owned_metadata_helper_is_scoped_reused_and_unsupported_versions_never_list() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("codex-fixture");
        let script = format!(
            r##"#!/usr/bin/env python3
import sys,json,os
from pathlib import Path
base=Path(__file__).parent
if '--version' in sys.argv:
 print('codex-cli '+('0.1.0' if (base/'unsupported').exists() else '0.169.1'));sys.exit(0)
with (base/'starts').open('a') as f:f.write(str(os.getpid())+'\n')
for line in sys.stdin:
 q=json.loads(line)
 with (base/'requests').open('a') as f:f.write(json.dumps(q)+'\n')
 if 'id' not in q:continue
 if q['method']=='initialize':result={{}}
 elif q['method']=='thread/list':
  assert q['params']=={{'ancestorThreadId':'{ROOT}','useStateDbOnly':True,'limit':127,'sourceKinds':['subAgentThreadSpawn'],'archived':False}}
  result={{'data':[{{'id':'{CHILD}','parentThreadId':'{ROOT}','agentNickname':'fixture reviewer','preview':'NEVER EXPORT PRIVATE CONTENT'}}],'nextCursor':None}}
 else:raise AssertionError(q['method'])
 print(json.dumps({{'id':q['id'],'result':result}}),flush=True)
"##
        );
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = Db::open(&dir.path().join("store")).unwrap();
        let p = db
            .create(ProjectInput {
                name: "native metadata".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: ROOT.into(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        for _ in 0..2 {
            let nodes = query_with_cli(&p, &cli).await.unwrap();
            assert_eq!(nodes[0]["name"], "fixture reviewer");
            assert!(!serde_json::to_string(&nodes).unwrap().contains("PRIVATE"));
        }
        let starts = std::fs::read_to_string(dir.path().join("starts")).unwrap();
        assert_eq!(starts.lines().count(), 1, "UI polls reuse one owned helper");
        let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert_eq!(
            requests
                .lines()
                .filter(|line| line.contains("thread/list"))
                .count(),
            2
        );
        let key = format!(
            "{}:{}:{}",
            cli.display(),
            p.path,
            std::env::var("CODEX_HOME").unwrap_or_default()
        );
        HELPERS.get().unwrap().lock().await.remove(&key);
        let pid: i32 = starts.trim().parse().unwrap();
        for _ in 0..50 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "Owned helper is reaped");
        std::fs::write(dir.path().join("unsupported"), "").unwrap();
        assert!(query_with_cli(&p, &cli).await.is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("starts")).unwrap(),
            starts,
            "An unsupported version cannot issue an unscoped list"
        );
        HELPERS.get().unwrap().lock().await.remove(&key);
    }
}
