use herdr_feishu_bridge::{App, api, config::Config, db::ProjectInput, update};
use serde_json::json;

#[tokio::test]
async fn authenticated_prior_protocol_applies_only_to_the_captured_generation() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(dir.path().into(), Config::default()).unwrap();
    let mut projects = Vec::new();
    for name in ["Compatible", "Changed", "Unknown"] {
        let p = app
            .db
            .create(ProjectInput {
                name: name.into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: "native".into(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        app.db
            .save_execution(
                &p.id,
                json!({"terminal_id":"terminal","shell_pid":1,"backend":"legacy"}),
                "generation",
            )
            .await
            .unwrap();
        projects.push(p);
    }
    update::record_service(
        dir.path(),
        &json!({
            "api_protocol":api::PROTOCOL,
            "projects":[
                {"id":projects[0].id,"generation":"generation","terminal":"terminal"},
                {"id":projects[1].id,"generation":"stale-generation","terminal":"terminal"}
            ]
        }),
    )
    .unwrap();
    update::inherit_service_protocol(dir.path(), &app.db).unwrap();
    let compatible = app.db.project(&projects[0].id).await.unwrap();
    assert!(!App::update_required(&compatible));
    assert_eq!(compatible.native_session, "native");
    for p in &projects[1..] {
        assert!(App::update_required(&app.db.project(&p.id).await.unwrap()));
    }
    // A later daemon snapshot must not overwrite a known incompatible contract.
    app.db
        .sync(|c| {
            c.execute(
                "UPDATE projects SET execution=json_set(execution,'$.api_protocol',4) WHERE id=?",
                [&projects[0].id],
            )?;
            Ok(())
        })
        .unwrap();
    update::inherit_service_protocol(dir.path(), &app.db).unwrap();
    assert!(App::update_required(
        &app.db.project(&projects[0].id).await.unwrap()
    ));
}
