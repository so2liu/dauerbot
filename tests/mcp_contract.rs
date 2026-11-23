use herdr_feishu_bridge::{mcp, App, config::Config, db::Db, service};
use serde_json::json;

#[tokio::test]
async fn preflight_admits_stale_contracts_without_rewriting_executions() {
    let dir = tempfile::tempdir().unwrap();
    Config::default().save(dir.path()).unwrap();
    let db = Db::open(&dir.path().join("bridge.sqlite3")).unwrap();
    let cases = [
        ("empty", json!({}), None),
        ("legacy", json!({"api_protocol":6}), None),
        ("unsupported", json!({"mcp_contract":2,"api_protocol":7}), None),
        ("published", json!({"api_protocol":7}), Some(1)),
        ("modern", json!({"mcp_contract":1,"api_protocol":7}), Some(1)),
    ];
    for (name, execution, _) in &cases {
        db.sync(|c| {
            c.execute(
                "INSERT INTO projects(id,name,path,agent,created_at,mcp_token,terminal,generation,execution,native_session)
                 VALUES(?1,?1,'','custom',0,'token','fixture-terminal','fixture-generation',?2,'saved-native')",
                rusqlite::params![name, execution.to_string()],
            )?;
            Ok(())
        }).unwrap();
    }
    assert_eq!(service::preflight(dir.path()).unwrap()["mcp_compatible"], true);
    for (name, execution, contract) in cases {
        let project = db.project(name).await.unwrap();
        assert_eq!(project.execution, execution);
        assert_eq!(project.native_session, "saved-native");
        assert_eq!(mcp::generation_contract(&project.execution), contract);
        assert_eq!(App::update_required(&project), contract.is_none());
    }
}
