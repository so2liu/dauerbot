use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use herdr_feishu_bridge::{
    App, api,
    config::{self, Config},
    db, mcp,
};
use serde_json::{Value, json};
use std::{fs::OpenOptions, path::PathBuf, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Parser)]
#[command(
    name = "hfb",
    version,
    about = "HERDR 飞书项目桥接：daemon、CLI、MCP 与本地管理界面"
)]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// 初始化本机私有数据目录
    Init,
    /// 在前台运行 daemon，可交给 launchd/systemd 托管
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        core_session: Option<String>,
        #[arg(long)]
        hc: Option<String>,
        #[arg(long, hide = true)]
        feishu_base: Option<String>,
    },
    /// 查看 daemon、飞书与项目状态
    Status,
    /// 打开本地管理界面
    Ui,
    /// 项目管理
    Project {
        #[command(subcommand)]
        command: Projects,
    },
    /// 主会话管理
    Session {
        #[command(subcommand)]
        command: Sessions,
    },
    /// 消息管理
    Message {
        #[command(subcommand)]
        command: Messages,
    },
    /// 飞书凭证管理（secret 从 stdin 读取）
    Feishu {
        #[command(subcommand)]
        command: Feishu,
    },
    /// MCP stdio 服务，通常由主 CLI 自动启动
    Mcp {
        #[arg(long)]
        credentials: PathBuf,
    },
    /// 打印本机用户服务配置
    Service,
}
#[derive(Subcommand)]
enum Projects {
    List,
    Add {
        name: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value = "codex")]
        agent: String,
        #[arg(long, default_value = "")]
        chat_id: String,
        #[arg(long, default_value = "")]
        native_session: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    Show {
        project: String,
    },
    Pause {
        project: String,
    },
    Unpause {
        project: String,
    },
    Remove {
        project: String,
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Subcommand)]
enum Sessions {
    Start {
        project: String,
    },
    Resume {
        project: String,
    },
    Stop {
        project: String,
    },
    Screen {
        project: String,
    },
    Show {
        project: String,
    },
    Attach {
        project: String,
    },
    /// 切换项目的当前智能体，保留机器人、项目和消息队列
    Switch {
        project: String,
        #[arg(long)]
        agent: String,
    },
}
#[derive(Subcommand)]
enum Messages {
    Inject {
        project: String,
        text: String,
    },
    Send {
        project: String,
        text: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    File {
        project: String,
        path: PathBuf,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    Retry {
        id: String,
    },
    Dismiss {
        id: String,
    },
}
#[derive(Subcommand)]
enum Feishu {
    Set {
        #[arg(long)]
        app_id: String,
    },
    Check,
}

struct Client {
    http: reqwest::Client,
    cfg: Config,
}
impl Client {
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let mut request = self
            .http
            .request(method.parse()?, format!("{}/api{}", self.cfg.url(), path))
            .bearer_auth(&self.cfg.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .context("daemon 不可用；请先运行 hfb serve")?;
        let status = response.status();
        let v: Value = response.json().await?;
        if !status.is_success() {
            bail!("{}", v["error"].as_str().unwrap_or("操作失败"));
        }
        Ok(v)
    }
    async fn id(&self, key: &str) -> Result<String> {
        let state = self.request("GET", "/state", None).await?;
        state["projects"]
            .as_array()
            .context("无项目列表")?
            .iter()
            .find(|p| p["id"] == key || p["name"] == key)
            .and_then(|p| p["id"].as_str())
            .map(str::to_owned)
            .context("项目不存在")
    }
}
fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "herdr_feishu_bridge=info",
        ))
        .init();
    if let Commands::Mcp { credentials } = &cli.command {
        return mcp::serve(credentials).await;
    }
    let dir = cli.data_dir.unwrap_or_else(config::default_dir);
    config::private_dir(&dir)?;
    let dir = std::fs::canonicalize(dir).context("无法解析数据目录")?;
    let mut cfg = Config::load(&dir)?;
    if let Commands::Serve {
        port,
        core_session,
        hc,
        feishu_base,
    } = cli.command
    {
        if let Some(port) = port {
            cfg.port = port;
        }
        if let Some(session) = core_session {
            cfg.core_session = session;
        }
        if let Some(hc) = hc {
            cfg.hc = hc;
        }
        if let Some(base) = feishu_base {
            cfg.feishu_base = base;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("daemon.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("此数据目录已有 daemon 在运行")?;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", cfg.port))
            .await
            .context("管理端口被占用")?;
        cfg.save(&dir)?;
        let app = App::new(dir.clone(), cfg.clone())?;
        app.workers();
        eprintln!(
            "Herdr Feishu Bridge {} · {} · 数据目录 {}",
            env!("CARGO_PKG_VERSION"),
            cfg.url(),
            dir.display()
        );
        eprintln!("运行 hfb --data-dir '{}' ui 打开管理界面", dir.display());
        let shutdown = app.shutdown.clone();
        let signal = async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            shutdown.cancel();
        };
        axum::serve(listener, api::router(app))
            .with_graceful_shutdown(signal)
            .await?;
        drop(lock);
        return Ok(());
    }
    let client = Client {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(110))
            .build()?,
        cfg: cfg.clone(),
    };
    let value=match cli.command{
        Commands::Init=>json!({"data_dir":dir,"url":cfg.url()}),
        Commands::Status=>client.request("GET","/state",None).await?,
        Commands::Ui=>{
            client.request("GET","/state",None).await?;
            webbrowser::open(&format!("{}/#token={}",cfg.url(),cfg.token))?;
            json!({"opened":cfg.url()})
        }
        Commands::Project{command}=>match command{
            Projects::List=>client.request("GET","/state",None).await?["projects"].clone(),
            Projects::Add{name,path,agent,chat_id,native_session,args}=>client.request("POST","/projects",Some(json!({"name":name,"path":path,"agent":agent,"chat_id":chat_id,"native_session":native_session,"extra_args":args}))).await?,
            Projects::Show{project}=>{let id=client.id(&project).await?;client.request("GET",&format!("/projects/{id}"),None).await?}
            Projects::Pause{project}=>{
                let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/pause"),Some(json!({"paused":true}))).await?
            }
            Projects::Unpause{project}=>{
                let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/pause"),Some(json!({"paused":false}))).await?
            }
            Projects::Remove{project,yes}=>{
                if !yes{bail!("删除项目会清除消息记录，但不删除代码；确认后添加 --yes");}
                let id=client.id(&project).await?;client.request("DELETE",&format!("/projects/{id}"),None).await?
            }
        },
        Commands::Session{command}=>{
            if let Sessions::Switch{project,agent}=command{
                let id=client.id(&project).await?;
                print(&client.request("POST",&format!("/projects/{id}/agent"),Some(json!({"agent":agent}))).await?);
                return Ok(())
            }
            if let Sessions::Attach{project}=command{
                let id=client.id(&project).await?;
                let p=client.request("GET",&format!("/projects/{id}"),None).await?;
                let terminal=p["project"]["terminal"].as_str().context("项目没有主会话")?;
                let status=herdr_feishu_bridge::herdr::command(&cfg)
                    .args(["attach",terminal])
                    .stdin(std::process::Stdio::inherit()).stdout(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit())
                    .status().await?;
                if !status.success(){bail!("终端接管结束，hc 返回 {}",status);}
                return Ok(())
            }
            let (project,operation)=match command{
                Sessions::Start{project}=>(project,"start"),Sessions::Resume{project}=>(project,"resume"),
                Sessions::Stop{project}=>(project,"stop"),Sessions::Screen{project}=>(project,"screen"),Sessions::Show{project}=>(project,"show"),
                Sessions::Attach{..}=>unreachable!(),
                Sessions::Switch{..}=>unreachable!(),
            };
            let id=client.id(&project).await?;client.request(if operation=="screen"{"GET"}else{"POST"},&format!("/projects/{id}/{operation}"),None).await?
        }
        Commands::Message{command}=>match command{
            Messages::Inject{project,text}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/messages"),Some(json!({"content":text}))).await?}
            Messages::Send{project,text,request_id,to}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/outbox"),Some(json!({"content":text,"to":to,"request_id":request_id.unwrap_or_else(db::id)}))).await?}
            Messages::File{project,path,request_id,to}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/outbox"),Some(json!({"path":std::fs::canonicalize(path)?,"to":to,"request_id":request_id.unwrap_or_else(db::id)}))).await?}
            Messages::Retry{id}=>client.request("POST",&format!("/messages/{id}/retry"),None).await?,
            Messages::Dismiss{id}=>client.request("POST",&format!("/messages/{id}/dismiss"),None).await?,
        },
        Commands::Feishu{command}=>match command{
            Feishu::Set{app_id}=>{
                eprintln!("从 stdin 读取 app secret（不会输出或写入命令参数）");
                let mut secret=String::new();tokio::io::stdin().read_to_string(&mut secret).await?;
                let v=client.request("PUT","/settings",Some(json!({"app_id":app_id,"app_secret":secret.trim()}))).await?;
                secret.clear();v
            }
            Feishu::Check=>client.request("POST","/settings/check",None).await?,
        },
        Commands::Service=>{
            let exe=std::env::current_exe()?;
            #[cfg(target_os="macos")]
            println!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>dev.herdr.feishu-bridge</string><key>ProgramArguments</key><array><string>{}</string><string>--data-dir</string><string>{}</string><string>serve</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardErrorPath</key><string>{}/service.log</string></dict></plist>",xml(&exe.to_string_lossy()),xml(&dir.to_string_lossy()),xml(&dir.to_string_lossy()));
            #[cfg(not(target_os="macos"))]
            println!("[Unit]\nDescription=HERDR Feishu Bridge\nAfter=network-online.target\n\n[Service]\nExecStart=\"{}\" --data-dir \"{}\" serve\nRestart=on-failure\n\n[Install]\nWantedBy=default.target",exe.display(),dir.display());
            return Ok(())
        }
        Commands::Serve{..}|Commands::Mcp{..}=>unreachable!(),
    };
    print(&value);
    Ok(())
}
#[cfg(target_os = "macos")]
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
