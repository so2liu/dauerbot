//! Explicit opt-in: create .local/live.json with app_id/app_secret/chat_id.
//! This test sends exactly one marked Markdown and one tiny file. No WebSocket listener is started.
use herdr_feishu_bridge::{App, config::Config, db::ProjectInput};
use serde_json::{Value, json};
#[tokio::test]
#[ignore = "requires explicitly selected existing bot and test chat"]
async fn real_markdown_and_file_delivery() -> anyhow::Result<()> {
    let credentials: Value = serde_json::from_slice(&std::fs::read(".local/live.json")?)?;
    let dir = tempfile::TempDir::new()?;
    let cfg = Config::default();
    let app = App::new(dir.path().into(), cfg)?;
    let project = app
        .db
        .create(ProjectInput {
            name: "Bridge 开发验证".into(),
            path: dir.path().to_string_lossy().into(),
            agent: "codex".into(),
            chat_id: credentials["chat_id"].as_str().unwrap().into(),
            native_session: String::new(),
            extra_args: vec![],
        })
        .await?;
    app.db.sync(|c| {
        herdr_feishu_bridge::bot::save(
            c,
            &project.id,
            credentials["app_id"].as_str().unwrap(),
            credentials["app_secret"].as_str().unwrap(),
            false,
        )
    })?;
    let markdown=app.outbox(&project.id,"live-markdown","**HERDR Bridge 开发测试**\n\nRust daemon 的 Markdown 发送链路已连通。此为授权的开发验证消息，无需回复。",json!({})).await?;
    assert_eq!(markdown.status, "sent", "{}", markdown.error);
    let file = dir.path().join("herdr-bridge-test.txt");
    std::fs::write(
        &file,
        "HERDR Feishu Bridge\nRust streaming attachment delivery test.\n",
    )?;
    let attachment = app
        .outbox(&project.id, "live-file", "", json!({"path":file}))
        .await?;
    assert_eq!(attachment.status, "sent", "{}", attachment.error);
    std::fs::create_dir_all("test-results")?;
    std::fs::write(
        "test-results/live-receipts.json",
        serde_json::to_vec_pretty(
            &json!({"markdown":markdown.attachment,"file":attachment.attachment,"status":"sent"}),
        )?,
    )?;
    Ok(())
}
