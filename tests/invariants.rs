use herdr_feishu_bridge::{
    App, api,
    config::Config,
    db::{Db, ProjectInput},
    feishu, herdr,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;

fn input(dir: &TempDir, name: &str, chat: &str) -> ProjectInput {
    ProjectInput {
        name: name.into(),
        path: dir.path().to_string_lossy().into(),
        agent: "codex".into(),
        chat_id: chat.into(),
        native_session: String::new(),
        extra_args: vec![],
    }
}

#[tokio::test]
async fn durable_dedup_and_uncertain_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db.sqlite");
    let db = Db::open(&path).unwrap();
    let p = db.create(input(&dir, "一", "oc_test")).await.unwrap();
    db.sync(|c| herdr_feishu_bridge::bot::save(c, &p.id, "cli_fixture", "secret", false))
        .unwrap();
    let key = db
        .ingest_bot(
            &p.id,
            "cli_fixture",
            "oc_test",
            "om_one",
            "hello",
            &json!({}),
        )
        .unwrap()
        .unwrap();
    assert!(
        db.ingest_bot(
            &p.id,
            "cli_fixture",
            "oc_test",
            "om_one",
            "hello",
            &json!({})
        )
        .unwrap()
        .is_none()
    );
    assert!(
        db.ingest_bot(
            &p.id,
            "cli_fixture",
            "oc_unbound",
            "om_two",
            "hello",
            &json!({})
        )
        .unwrap()
        .is_some()
    );
    db.state(&key, "submitting", "").await.unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.message(&key).await.unwrap().status, "uncertain");
    assert!(
        db.message(&key)
            .await
            .unwrap()
            .error
            .contains("verify the result")
    );
}

#[tokio::test]
async fn outbox_request_ids_do_not_create_duplicate_messages() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let p = db.create(input(&dir, "一", "oc_test")).await.unwrap();
    let first = db
        .enqueue_out(&p.id, "stable-id", "hello", json!({}))
        .await
        .unwrap();
    let again = db
        .enqueue_out(&p.id, "stable-id", "hello", json!({}))
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(db.messages(&p.id).await.unwrap().len(), 1);
    assert!(
        db.create(input(&dir, "二", "oc_test")).await.is_ok(),
        "The receiving bot selects the project, so projects can share a group"
    );
}

#[tokio::test]
async fn one_projects_backlog_cannot_starve_another_project() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let a = db.create(input(&dir, "积压项目", "oc_a")).await.unwrap();
    let b = db.create(input(&dir, "其他项目", "oc_b")).await.unwrap();
    db.sync(|c| herdr_feishu_bridge::bot::save(c, &a.id, "cli_fixture", "secret", false))
        .unwrap();
    db.pause(&a.id, true).await.unwrap();
    for n in 0..40 {
        db.ingest_bot(
            &a.id,
            "cli_fixture",
            "oc_a",
            &format!("om_a_{n}"),
            "backlog",
            &json!({}),
        )
        .unwrap();
    }
    // Local projects can receive independent tasks through the authenticated API.
    let project_id = b.id.clone();
    db.call(move |c| {
        c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES('local-task',?,'in','om_b','new message','queued',0,0)", [&project_id])?;
        Ok(())
    }).await.unwrap();
    let heads = db.pending("in").await.unwrap();
    assert_eq!(heads.len(), 2);
    assert!(heads.iter().any(|m| m.project_id == b.id));
    assert_eq!(
        heads
            .iter()
            .find(|m| m.project_id == a.id)
            .unwrap()
            .source_id,
        "om_a_0"
    );
}

fn legacy_record(p: &mut herdr_feishu_bridge::db::Project, pid: i64) -> Value {
    p.generation = "11111111-2222-4333-8444-555555555555".into();
    p.terminal = Some("term_owned".into());
    p.terminal_pid = Some(pid);
    let name = herdr::managed_name(&p.generation).unwrap();
    p.execution = json!({"backend":"legacy","workspace_id":"w1","pane_id":"w1:p1","terminal_id":"term_owned",
        "managed_name":name,"generation":p.generation,"shell_pid":pid,"launch_permit":"/private/fixture/launch.permit"});
    json!({"state":"live","pid":pid,"execution":p.execution,"launch_pending":false,
        "foreground":{"pid":pid,"name":p.agent,"argv":[p.agent]},
        "agent":{"agent":p.agent,"workspace_id":"w1","pane_id":"w1:p1","terminal_id":"term_owned"},
        "tokens":{"hfb_owner":"dauerbot","hfb_project":p.id,"hfb_generation":p.generation,"hfb_execution":name},
        "cwd":p.path,"status":{"state":"idle","app":p.agent}})
}

#[tokio::test]
async fn process_and_generation_are_required_before_typing() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "Identity", "")).await.unwrap();
    let record = legacy_record(&mut p, 100);
    assert!(herdr::check_identity(&p, &record).is_ok());
    for (pointer, value) in [
        ("/pid", json!(101)),
        ("/foreground/name", json!("zsh")),
        ("/tokens/hfb_generation", json!("replaced")),
        ("/state", json!("finished")),
        ("/agent/pane_id", json!("w2:p1")),
        ("/agent/workspace_id", json!("w2")),
        ("/agent/terminal_id", json!("term_other")),
        ("/tokens/hfb_owner", json!("other")),
    ] {
        let mut changed = record.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(herdr::check_identity(&p, &changed).is_err(), "{pointer}");
    }
}

#[tokio::test]
async fn idle_label_does_not_retire_foreground_tool_or_unknown_wrapper() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "Quiescence", "")).await.unwrap();
    let mut record = legacy_record(&mut p, 100);
    record["processes"] = json!([{"name":"codex","argv":["codex"]}]);
    assert!(herdr::foreground_quiescent(&p, &record));
    record["processes"] = json!([
        {"name":"codex","argv":["codex"]},
        {"name":"hfb","argv":[std::env::current_exe().unwrap(),"mcp","--credentials","/private/fixture/credentials.json"]}
    ]);
    assert!(
        herdr::foreground_quiescent(&p, &record),
        "The generation's parked MCP child is not active background work"
    );
    record["processes"][1]["argv"][3] = json!("/another/generation/credentials.json");
    assert!(!herdr::foreground_quiescent(&p, &record));
    p.execution["launch_permit"] = json!("/private/app/sessions/project/generation/launch.permit");
    record["processes"][1]["argv"] = json!([
        std::env::current_exe().unwrap(),
        "--data-dir",
        "/private/app",
        "mcp"
    ]);
    assert!(herdr::foreground_quiescent(&p, &record));
    record["processes"][1]["argv"][2] = json!("/another/installation");
    assert!(!herdr::foreground_quiescent(&p, &record));
    for processes in [
        json!([{"name":"bash","argv":["bash","-c","long task"]}]),
        json!([{"name":"codex","argv":["codex"]},{"name":"python3","argv":["python3","task.py"]}]),
        json!([{"name":"node","argv":["node","unknown-wrapper.js"]}]),
        json!([]),
    ] {
        record["processes"] = processes;
        assert!(!herdr::foreground_quiescent(&p, &record));
    }
    #[cfg(target_os = "macos")]
    {
        p.agent = "claude".into();
        record["agent"]["agent"] = json!("claude");
        let mut inhibitor = std::process::Command::new("/usr/bin/caffeinate")
            .args(["-i", "-t", "2"])
            .spawn()
            .unwrap();
        record["processes"] = json!([
            {"pid":100,"name":"Claude.exe","argv":["claude"]},
            {"pid":inhibitor.id(),"name":"caffeinate","argv":["caffeinate","-i","-t","2"]}
        ]);
        assert!(
            herdr::foreground_quiescent(&p, &record),
            "Claude's verified system sleep inhibitor performs no Agent work"
        );
        record["processes"][1]["argv"] =
            json!(["caffeinate", "-i", "-t", "2", "python3", "task.py"]);
        assert!(
            !herdr::foreground_quiescent(&p, &record),
            "caffeinate wrapping a command is active work"
        );
        let _ = inhibitor.kill();
        let _ = inhibitor.wait();
    }
    p.agent = "custom".into();
    p.extra_args = vec![
        "/usr/local/bin/python3".into(),
        "/private/fixture/agent.py".into(),
        "{credentials}".into(),
    ];
    record["processes"] = json!([{"pid":100,"name":"Python","argv":["/Frameworks/Python.app/Contents/MacOS/Python","/private/fixture/agent.py","credentials"]}]);
    assert!(
        herdr::foreground_quiescent(&p, &record),
        "macOS Python launchers exec the framework binary"
    );
    record["processes"][0]
        .as_object_mut()
        .unwrap()
        .remove("argv");
    assert!(
        herdr::foreground_quiescent(&p, &record),
        "macOS may omit the framework interpreter argv entirely"
    );
    record["processes"][0]["pid"] = json!(101);
    assert!(
        !herdr::foreground_quiescent(&p, &record),
        "A child interpreter cannot inherit the root launcher exemption"
    );
}

#[tokio::test]
async fn attachments_cannot_escape_project_via_path_or_symlink() {
    let root = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let db = Db::open(&root.path().join("db")).unwrap();
    let p = db.create(input(&project, "一", "")).await.unwrap();
    std::fs::create_dir(root.path().join("files")).unwrap();
    std::fs::write(project.path().join("good.txt"), "ok").unwrap();
    std::fs::write(root.path().join("secret"), "private").unwrap();
    assert!(feishu::allowed_file(&p, &project.path().join("good.txt"), root.path()).is_ok());
    assert!(feishu::allowed_file(&p, &root.path().join("secret"), root.path()).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path().join("secret"), project.path().join("link"))
            .unwrap();
        assert!(feishu::allowed_file(&p, &project.path().join("link"), root.path()).is_err());
    }
}

#[test]
fn inbound_filters_bots_and_normalizes_messages() {
    let mut v = json!({"header":{"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_me"}},"message":{"chat_id":"oc_test","message_id":"om_1","message_type":"text","content":"{\"text\":\"hello\\n世界\"}"}}});
    let (_, _, text, _) = feishu::normalize(&v).unwrap().unwrap();
    assert!(text.contains("hello\n世界"));
    v["event"]["sender"]["sender_type"] = json!("app");
    assert!(feishu::normalize(&v).unwrap().is_none());
    v["event"]["sender"]["sender_type"] = json!("user");
    v["event"]["message"]["content"] = json!("not json");
    assert!(feishu::normalize(&v).is_err());
}

#[test]
fn native_submission_retries_only_when_legacy_confirms_no_input() {
    let safe: anyhow::Error = herdr::RpcFailure {
        kind: "agent_not_ready".into(),
        message: "still starting".into(),
        hint: "nothing was typed; check the screen".into(),
    }
    .into();
    assert!(herdr::definitely_not_sent(&safe));
    let ambiguous: anyhow::Error = herdr::RpcFailure {
        kind: "unavailable".into(),
        message: "connection lost".into(),
        hint: "retry".into(),
    }
    .into();
    assert!(!herdr::definitely_not_sent(&ambiguous));
    assert!(!herdr::definitely_not_sent(&anyhow::anyhow!(
        "transport timed out after writing"
    )));
}

async fn test_server() -> (TempDir, Arc<App>, String, tokio::task::JoinHandle<()>) {
    let dir = TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg = Config {
        port: listener.local_addr().unwrap().port(),
        ..Config::default()
    };
    let url = cfg.url();
    let app = App::new(dir.path().to_owned(), cfg).unwrap();
    let router = api::router(app.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, app, url, task)
}

#[tokio::test]
async fn local_api_auth_origin_and_project_scope() {
    let (dir, app, url, server) = test_server().await;
    let cfg = app.config.read().await.clone();
    let http = reqwest::Client::new();
    let p = app.db.create(input(&dir, "一", "oc_1")).await.unwrap();
    let q = app.db.create(input(&dir, "二", "oc_2")).await.unwrap();
    let result = http.get(format!("{url}/api/state")).send().await.unwrap();
    assert_eq!(result.status(), 401);
    let result = http
        .get(format!("{url}/api/state"))
        .bearer_auth(&cfg.token)
        .header("Origin", "https://untrusted.example")
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 403);
    let result: Value = http
        .get(format!("{url}/api/state"))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(result["projects"][0].get("mcp_token").is_none());
    assert!(result["settings"].get("app_secret").is_none());
    app.db
        .set_session(&p.id, Some("t1".into()), Some(1), "current".into())
        .await
        .unwrap();
    for (path, generation) in [
        ("/service/shutdown".into(), "current"),
        (format!("/projects/{}/stop", p.id), "current"),
        (format!("/projects/{}/outbox", q.id), "current"),
        (format!("/projects/{}/register", p.id), "stale"),
    ] {
        let result = http
            .post(format!("{url}/api{path}"))
            .bearer_auth(&p.mcp_token)
            .header("x-hfb-generation", generation)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 401);
    }
    // A valid scoped credential cannot access removed lifecycle endpoints.
    let result = http
        .post(format!("{url}/api/projects/{}/ack", p.id))
        .bearer_auth(&p.mcp_token)
        .header("x-hfb-generation", "current")
        .json(&json!({"message_id":"om_unrelated"}))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 404);
    // An old Core terminal reference cannot become a legacy terminal descriptor.
    let result = http
        .post(format!("{url}/api/projects/{}/show", p.id))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 400);
    let result = http
        .post(format!("{url}/api/service/shutdown"))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    assert!(app.shutdown.is_cancelled());
    server.abort();
}

#[tokio::test]
async fn antigravity_uses_interactive_prompt_flag_instead_of_a_positional_prompt() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut input = input(&dir, "交互式 AGY", "");
    input.agent = "agy".into();
    let mut p = db.create(input).await.unwrap();
    let args = herdr::launch_args(
        &p,
        &dir.path().join("mcp.json"),
        &json!({}),
        "等待飞书任务",
        false,
    )
    .unwrap();
    assert_eq!(&args[args.len() - 2..], &["-i", "等待飞书任务"]);
    assert!(!args.iter().any(|v| v == "-p" || v == "--print"));
    p.native_session = "conversation-id".into();
    let args = herdr::launch_args(
        &p,
        &dir.path().join("mcp.json"),
        &json!({}),
        "继续待命",
        true,
    )
    .unwrap();
    assert!(
        args.windows(2)
            .any(|v| v == ["--conversation", "conversation-id"])
    );
    assert_eq!(&args[args.len() - 2..], &["-i", "继续待命"]);
}

#[tokio::test]
async fn workspace_trust_requires_exact_project_identity_and_selected_yes() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "Trust boundary", "")).await.unwrap();
    p.agent = "agy".into();
    let mut record = legacy_record(&mut p, 123);
    record["status"]["state"] = json!("blocked");
    let screen = format!(
        "Accessing workspace:\n\n{}\n\nDo you trust the contents of this project?\n\n> Yes, I trust this folder\n  No, exit\n",
        p.path
    );
    assert!(herdr::is_project_trust(&p, &record, &screen));
    for text in [
        screen.replace(&p.path, "/another/project"),
        screen.replace("> Yes, I trust this folder", "  Yes, I trust this folder"),
        "Allow shell command execution?".into(),
    ] {
        assert!(!herdr::is_project_trust(&p, &record, &text));
    }
    for (pointer, value) in [
        ("/status/state", json!("working")),
        ("/tokens/hfb_generation", json!("old")),
        ("/pid", json!(999)),
        ("/state", json!("finished")),
        ("/cwd", json!("/another/project")),
    ] {
        let mut invalid = record.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(!herdr::is_project_trust(&p, &invalid, &screen), "{pointer}");
    }
    p.agent = "claude".into();
    let claude = format!(
        "Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source\n{}\n❯ 1. Yes, I trust this folder\n  2. No, exit\n",
        p.path
    );
    assert!(herdr::is_project_trust(&p, &record, &claude));
    assert!(!herdr::is_project_trust(
        &p,
        &record,
        &claude.replace("❯ 1.", "  1.")
    ));
    assert!(!herdr::is_project_trust(
        &p,
        &record,
        &claude.replace(&p.path, "/another/project")
    ));
    p.ready = true;
    assert!(!herdr::is_project_trust(&p, &record, &claude));
}

#[tokio::test]
async fn native_launch_keeps_cli_model_defaults_and_user_overrides() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "原生模型", "")).await.unwrap();
    for (agent, approval) in [
        ("codex", "--dangerously-bypass-approvals-and-sandbox"),
        ("claude", "--dangerously-skip-permissions"),
        ("pi", "--approve"),
        ("agy", "--dangerously-skip-permissions"),
    ] {
        p.agent = agent.into();
        p.extra_args.clear();
        let args = herdr::launch_args(
            &p,
            &dir.path().join("mcp.json"),
            &json!({"command":"hfb","args":["mcp"]}),
            "登记 MCP 并待命",
            false,
        )
        .unwrap();
        assert!(args.iter().any(|v| v == approval), "{agent}");
        assert!(
            !args.iter().any(|v| [
                "--model",
                "--provider",
                "--settings",
                "--mode",
                "--effort",
                "--thinking",
                "-m"
            ]
            .contains(&v.as_str())),
            "{agent}"
        );
        assert!(
            !args
                .iter()
                .any(|v| v.starts_with("model=") || v.starts_with("model_provider=")),
            "{agent}"
        );
        p.extra_args = vec!["--model".into(), "user-selected-model".into()];
        let args = herdr::launch_args(
            &p,
            &dir.path().join("mcp.json"),
            &json!({}),
            "登记 MCP 并待命",
            false,
        )
        .unwrap();
        assert_eq!(args.iter().filter(|v| v.as_str() == "--model").count(), 1);
        assert!(
            args.windows(2)
                .any(|v| v == ["--model", "user-selected-model"])
        );
    }
}

#[test]
fn launch_forwards_existing_network_settings_without_overriding_identity_or_models() {
    let vars = [
        ("HTTPS_PROXY", "http://127.0.0.1:7897"),
        ("no_proxy", "localhost,127.0.0.1"),
        ("HC_TERMINAL_ID", "unrelated"),
        ("HC_SESSION", "unrelated"),
        ("HERDR_PANE_ID", "unrelated"),
        ("HERDR_SOCKET_PATH", "unrelated"),
        ("ANTHROPIC_MODEL", "unrelated-model"),
        ("GEMINI_API_KEY", "unrelated-key"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()));
    let forwarded = herdr::launch_network_env(vars);
    assert_eq!(forwarded.len(), 2);
    assert_eq!(forwarded["HTTPS_PROXY"], "http://127.0.0.1:7897");
    assert_eq!(forwarded["no_proxy"], "localhost,127.0.0.1");
    assert!(herdr::launch_network_env([]).is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn login_shell_loads_profile_and_preserves_literal_cli_arguments() {
    let home = TempDir::new().unwrap();
    std::fs::write(
        home.path().join(".zshrc"),
        "export HTTPS_PROXY=http://127.0.0.1:7897\n",
    )
    .unwrap();
    let literal = format!(
        "$(touch {}) 'quoted' ; echo unexpected",
        home.path().join("injected").display()
    );
    let mut args = herdr::login_shell_args(vec![
        "/usr/bin/printf".into(),
        "%s\\0".into(),
        literal.clone(),
        "Gemini 4 Hydrogen (Medium)".into(),
    ]);
    args[0] = "/bin/zsh".into();
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .env_clear()
        .env("HOME", home.path())
        .env("ZDOTDIR", home.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("{literal}\0Gemini 4 Hydrogen (Medium)\0").as_bytes()
    );
    assert!(!home.path().join("injected").exists());
    let mut args = herdr::login_shell_args(vec!["/usr/bin/printenv".into(), "HTTPS_PROXY".into()]);
    args[0] = "/bin/zsh".into();
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .env_clear()
        .env("HOME", home.path())
        .env("ZDOTDIR", home.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"http://127.0.0.1:7897\n");
}

#[cfg(unix)]
#[tokio::test]
async fn native_execution_accepts_running_agent_but_rejects_startup_and_restored_shell() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "Native identity", "")).await.unwrap();
    p.agent = "agy".into();
    let mut record = legacy_record(&mut p, 123);
    assert!(herdr::check_identity(&p, &record).is_ok());
    record["launch_pending"] = json!(true);
    record["foreground"]["name"] = json!("sh");
    assert!(
        herdr::check_identity(&p, &record)
            .unwrap_err()
            .is::<herdr::SessionStarting>()
    );
    assert!(
        herdr::check_execution_identity(&p, &record).is_ok(),
        "The saved startup gate remains viewable"
    );
    record["tokens"]["hfb_generation"] = json!("replaced");
    assert!(
        !herdr::check_identity(&p, &record)
            .unwrap_err()
            .is::<herdr::SessionStarting>()
    );
    record["tokens"]["hfb_generation"] = json!(p.generation);
    record["launch_pending"] = json!(false);
    assert!(
        !herdr::check_identity(&p, &record)
            .unwrap_err()
            .is::<herdr::SessionStarting>(),
        "A returned shell is never a pending native launch"
    );
    record["foreground"]["name"] = json!("agy");
    record["pid"] = json!(999);
    assert!(
        herdr::check_identity(&p, &record).is_err(),
        "A restored root PID cannot inherit the previous generation"
    );
}

#[tokio::test]
async fn old_core_terminal_ids_cannot_control_legacy_terminals() {
    let dir = TempDir::new().unwrap();
    let db = Db::open(&dir.path().join("db")).unwrap();
    let mut p = db.create(input(&dir, "Old Core", "")).await.unwrap();
    p.terminal = Some("t1".into());
    p.terminal_pid = Some(123);
    p.generation = "old-core".into();
    assert!(herdr::ExecutionRef::from_project(&p).is_err());
    assert!(herdr::check_identity(&p, &json!({"state":"live","pid":123})).is_err());
}

#[test]
fn independent_data_directories_get_distinct_stable_legacy_sessions() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let first = Config::load(a.path()).unwrap();
    let again = Config::load(a.path()).unwrap();
    let other = Config::load(b.path()).unwrap();
    assert_eq!(first.herdr_session, again.herdr_session);
    assert_ne!(first.herdr_session, other.herdr_session);
    assert_eq!(first.token, again.token);
    assert_eq!(first.herdr_config, again.herdr_config);
    assert!(
        std::fs::read_to_string(&first.herdr_config)
            .unwrap()
            .contains("resume_agents_on_restore = false")
    );
}

#[test]
fn managed_names_preserve_the_entire_execution_generation() {
    let zero = herdr::managed_name("00000000-0000-0000-0000-000000000000").unwrap();
    let maximum = herdr::managed_name("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
    let neighbor = herdr::managed_name("ffffffff-ffff-ffff-ffff-fffffffffffe").unwrap();
    assert_ne!(maximum, neighbor);
    for name in [zero, maximum, neighbor] {
        assert!(name.len() <= 32);
        assert!(name.starts_with("db-"));
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        );
    }
}
