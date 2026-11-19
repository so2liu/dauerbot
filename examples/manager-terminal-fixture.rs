//! Opt-in legacy-PTY smoke fixture. With no args this binary is the Rust CLI.
//! Run with an absolute HERDR 0.9.5 executable in an isolated HOME/XDG/session.
use anyhow::{Context, Result, ensure};
use herdr_feishu_bridge::{config::Config, db::Project, herdr};
use serde_json::json;
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};

fn echo_cli() -> Result<()> {
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::tcgetattr(0, &mut term) } == 0,
        "Fixture stdin must be a PTY"
    );
    unsafe {
        libc::cfmakeraw(&mut term);
    }
    ensure!(
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) } == 0,
        "Cannot set raw fixture PTY"
    );
    print!("\x1b[2J\x1b[HREADY pure Rust fixture\r\n");
    std::io::stdout().flush()?;
    let mut text = Vec::new();
    let mut alt = false;
    let mut escape = false;
    for byte in std::io::BufReader::new(std::io::stdin()).bytes() {
        let byte = byte?;
        if escape {
            alt |= byte == b'm';
            escape = false;
            continue;
        }
        match byte {
            0x1b => escape = true,
            b'\r' => {
                print!(
                    "\x1b[2J\x1b[HRESULT {}\r\nalt_m={alt}\r\nenter=true\r\n",
                    String::from_utf8_lossy(&text)
                );
                std::io::stdout().flush()?;
            }
            value => text.push(value),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let Some(program) = std::env::args().nth(1) else {
        return echo_cli();
    };
    ensure!(
        PathBuf::from(&program).is_absolute(),
        "Provide an absolute HERDR executable"
    );
    let temp = tempfile::Builder::new().prefix("db-mt-").tempdir()?;
    let executable = std::env::current_exe()?;
    let config_path = temp.path().join("herdr.toml");
    std::fs::write(
        &config_path,
        format!(
            "onboarding=false\n[terminal]\ndefault_shell={}\nshell_mode=\"non_login\"\n[update]\nversion_check=false\nmanifest_check=false\n",
            serde_json::to_string(&executable.to_string_lossy())?
        ),
    )?;
    let cfg = Config {
        herdr: program,
        herdr_session: format!("mt{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        herdr_config: config_path.to_string_lossy().into(),
        ..Default::default()
    };
    // All lifecycle effects below address only this uniquely named private session.
    let result=async {
        let deadline=tokio::time::Instant::now()+Duration::from_secs(15);
        loop {
            if herdr::ensure_server(&cfg).await.is_ok() {break;}
            ensure!(tokio::time::Instant::now()<deadline,"Fixture HERDR did not become ready");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let workspace=herdr::rpc(&cfg,"workspace.create",json!({"cwd":temp.path(),"label":"isolated Rust CLI fixture","focus":false})).await?;
        let pane_id=workspace["root_pane"]["pane_id"].as_str().context("No fixture pane")?;
        let pane=herdr::rpc(&cfg,"pane.get",json!({"pane_id":pane_id})).await?["pane"].clone();
        let info=herdr::rpc(&cfg,"pane.process_info",json!({"pane_id":pane_id})).await?["process_info"].clone();
        let generation="fixture-generation";
        let execution=herdr::ExecutionRef {backend:"legacy".into(),workspace_id:pane["workspace_id"].as_str().unwrap().into(),pane_id:pane_id.into(),terminal_id:pane["terminal_id"].as_str().unwrap().into(),managed_name:"fixture-owned".into(),generation:generation.into(),shell_pid:info["shell_pid"].as_i64(),launch_permit:temp.path().join("unused-permit")};
        let project=Project {id:"terminal-fixture".into(),name:"Rust CLI".into(),path:temp.path().to_string_lossy().into(),agent:"custom".into(),chat_id:String::new(),paused:false,terminal:Some(execution.terminal_id.clone()),terminal_pid:execution.shell_pid,generation:generation.into(),ready:true,execution:serde_json::to_value(&execution)?,native_session:String::new(),extra_args:vec![],created_at:0,mcp_token:String::new()};
        herdr::rpc(&cfg,"pane.report_metadata",json!({"pane_id":pane_id,"source":"dauerbot","tokens":{"hfb_owner":"dauerbot","hfb_project":project.id,"hfb_generation":generation,"hfb_execution":"fixture-owned"}})).await?;
        let segments=serde_json::from_value::<Vec<herdr::terminal_control::Segment>>(json!([{"text":"停止当前工作"},{"key":"alt+m"},{"key":"enter"}]))?;
        herdr::terminal_control::validate(&segments)?;
        for segment in &segments {herdr::terminal_control::submit(&cfg,&project,segment).await?;}
        let deadline=tokio::time::Instant::now()+Duration::from_secs(3);
        loop {
            let visible=herdr::terminal_control::screen(&cfg,&project).await?;
            let text=visible["text"].as_str().unwrap();
            if text.contains("停止当前工作") && text.contains("alt_m=true") && text.contains("enter=true") {
                println!("{{\"result\":\"passed\",\"backend\":\"legacy_pty\",\"segments\":3,\"readback\":true,\"rows\":null,\"cols\":null,\"cursor\":null}}");break;
            }
            ensure!(tokio::time::Instant::now()<deadline,"PTY readback did not confirm the sequence");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok::<(),anyhow::Error>(())
    }.await;
    let cleanup = herdr::rpc(&cfg, "server.stop", json!({})).await;
    ensure!(
        cleanup.is_ok(),
        "Could not stop the fixture-owned HERDR session"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::UnixStream::connect(herdr::socket_path(&cfg)?)
        .await
        .is_ok()
    {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Fixture HERDR remained reachable after stop"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    result
}
