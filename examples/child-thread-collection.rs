//! Explicit host maintenance using the same guarded collector as hourly GC.
//! Defaults to dry-run. Never opens the bridge for writing or starts a daemon.
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let bridge = PathBuf::from(args.next().context("Expected bridge.sqlite3 path")?);
    let audit = PathBuf::from(args.next().context("Expected private audit directory")?);
    let apply = match args.next().as_deref() {
        None | Some("dry-run") => false,
        Some("apply") => true,
        _ => anyhow::bail!("Expected dry-run or apply"),
    };
    let limit = args.next().map(|v| v.parse()).transpose()?.unwrap_or(1);
    let project = args.next().map(PathBuf::from);
    ensure!(args.next().is_none(), "Unexpected arguments");
    let report = herdr_feishu_bridge::child_thread_gc::collect_for_project(
        &bridge,
        &audit,
        apply,
        limit,
        project.as_deref(),
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
