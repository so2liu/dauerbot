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
    about = "Dauerbot: native desktop, daemon, CLI, and MCP for Feishu and HERDR"
)]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    #[command(hide = true)]
    CompanionIdentity,
    /// Initialize the private local data directory
    Init,
    /// Run the daemon in the foreground (or use launchd/systemd)
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        herdr: Option<String>,
        #[arg(long)]
        herdr_session: Option<String>,
        #[arg(long, hide = true)]
        feishu_base: Option<String>,
    },
    /// Show daemon, Feishu, and project status
    Status,
    /// Open the native Dauerbot desktop application
    Ui,
    /// Manage projects
    Project {
        #[command(subcommand)]
        command: Projects,
    },
    /// Manage main sessions
    Session {
        #[command(subcommand)]
        command: Sessions,
    },
    /// Resolve an input delivery after reviewing its saved result
    Event {
        #[command(subcommand)]
        command: Events,
    },
    /// Manage durable schedules
    Timer {
        #[command(subcommand)]
        command: Timers,
    },
    /// Manage messages
    Message {
        #[command(subcommand)]
        command: Messages,
    },
    /// Manage Feishu credentials (read the secret from stdin)
    Feishu {
        #[command(subcommand)]
        command: Feishu,
    },
    /// Run the MCP stdio server, normally started by the agent CLI
    Mcp {
        #[arg(long)]
        credentials: Option<PathBuf>,
    },
    /// Print a local user-service configuration
    Service,
    /// Check, install, or configure signed desktop updates
    Update {
        #[command(subcommand)]
        command: Updates,
    },
}
#[derive(Subcommand)]
enum Updates {
    Status,
    Check,
    Install {
        #[arg(long)]
        app: Option<PathBuf>,
    },
    Auto {
        #[arg(value_parser = ["on", "off"])]
        mode: String,
    },
    Channel {
        #[arg(value_parser = ["stable", "preview"])]
        channel: String,
    },
    Rollback,
}
#[derive(Subcommand)]
enum Projects {
    List,
    Add {
        name: String,
        #[arg(long)]
        path: Option<PathBuf>,
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
    /// Pause automatic activity; incoming messages remain saved.
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
    /// End the current process. New work automatically resumes the conversation.
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
    /// Switch the active agent while preserving the bot, project, and queues
    Switch {
        project: String,
        #[arg(long)]
        agent: String,
    },
}
#[derive(Subcommand)]
enum Events {
    /// Retry work explicitly; review its result first to avoid repeated effects
    Retry { id: String },
    /// Dismiss an event after reviewing its result
    Dismiss { id: String },
}
#[derive(Subcommand)]
enum Timers {
    List {
        project: String,
    },
    Add {
        project: String,
        name: String,
        prompt: String,
        #[arg(long,value_parser=["once","interval","cron"])]
        kind: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        interval_seconds: Option<i64>,
        #[arg(long)]
        cron: Option<String>,
        #[arg(long, default_value = "UTC")]
        timezone: String,
        #[arg(long)]
        to: Option<String>,
    },
    Remove {
        id: String,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
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
        /// Trusted message ID for the discussion; omit to start a new topic.
        #[arg(long)]
        reply_to_message_id: Option<String>,
    },
    File {
        project: String,
        path: PathBuf,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
        /// Trusted message ID for the discussion; omit to start a new topic.
        #[arg(long)]
        reply_to_message_id: Option<String>,
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
        project: String,
        #[arg(long)]
        app_id: String,
    },
    Check {
        project: String,
    },
    Disconnect {
        project: String,
    },
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
            .context("Daemon unavailable; run hfb serve first")?;
        let status = response.status();
        let v: Value = response.json().await?;
        if !status.is_success() {
            bail!("{}", v["error"].as_str().unwrap_or("Operation failed"));
        }
        Ok(v)
    }
    async fn id(&self, key: &str) -> Result<String> {
        let state = self.request("GET", "/state", None).await?;
        state["projects"]
            .as_array()
            .context("Missing project list")?
            .iter()
            .find(|p| p["id"] == key || p["name"] == key)
            .and_then(|p| p["id"].as_str())
            .map(str::to_owned)
            .context("Project not found")
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
        return match credentials {
            Some(path) => mcp::serve(path).await,
            None => {
                mcp::serve_managed(&cli.data_dir.clone().unwrap_or_else(config::default_dir)).await
            }
        };
    }
    if matches!(cli.command, Commands::CompanionIdentity) {
        print(&json!({"version":env!("CARGO_PKG_VERSION"),"api_protocol":api::PROTOCOL}));
        return Ok(());
    }
    let dir = cli.data_dir.unwrap_or_else(config::default_dir);
    config::private_dir(&dir)?;
    let dir = std::fs::canonicalize(dir).context("Cannot resolve the data directory")?;
    if let Commands::Ui = cli.command {
        let desktop = std::env::current_exe()?
            .parent()
            .context("Cannot locate the application directory")?
            .join("HerdrBridge");
        if !desktop.is_file() {
            bail!(
                "Native desktop executable not found; launch Dauerbot.app or run scripts/build-desktop.sh"
            );
        }
        // Let the desktop choose a free port when this directory is new.
        std::process::Command::new(&desktop)
            .arg("--data-dir")
            .arg(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        print(&json!({"opened":desktop}));
        return Ok(());
    }
    if let Commands::Update { command } = cli.command {
        use herdr_feishu_bridge::update::{self, Preferences, Status};
        let mut prefs = Preferences::load(&dir)?;
        let value = match command {
            Updates::Auto { mode } => {
                prefs.automatic = mode == "on";
                prefs.save(&dir)?;
                json!(prefs)
            }
            Updates::Channel { channel } => {
                prefs.channel = channel;
                prefs.save(&dir)?;
                json!(prefs)
            }
            Updates::Status => {
                let file = dir.join("updates/status.json");
                if file.exists() {
                    {
                        let mut status: Value = serde_json::from_slice(&std::fs::read(file)?)?;
                        status["preferences"] = json!(prefs);
                        status["current"] = json!(update::CURRENT);
                        status["rollback_available"] =
                            json!(dir.join("updates/installed.json").exists());
                        status
                    }
                } else {
                    json!(Status::new(prefs))
                }
            }
            Updates::Rollback => {
                update::rollback_and_restart(&dir).await?;
                json!({"rolled_back":true,"restart_required":true})
            }
            command => {
                let client = update::client()?;
                let target = if let Updates::Install { app } = &command {
                    app.clone().or_else(update::bundle)
                } else {
                    None
                };
                let current = target
                    .as_ref()
                    .map(|app| update::bundle_version(app))
                    .transpose()?
                    .unwrap_or_else(|| update::CURRENT.into());
                let latest = update::check_for_version(&client, &prefs, &current).await?;
                if let Some(release) = latest {
                    if let Updates::Install { app } = command {
                        let app = app
                            .or_else(update::bundle)
                            .context("Specify the installed Dauerbot.app with --app")?;
                        update::install(
                            &client,
                            &dir,
                            &app,
                            &release,
                            |p| eprintln!("Downloading update: {p}%"),
                            false,
                        )
                        .await?;
                        json!({"installed":release.version(),"restart_required":true})
                    } else {
                        json!({"current":update::CURRENT,"latest":release.version(),"notes":release.notes,"url":release.page})
                    }
                } else {
                    json!({"current":update::CURRENT,"up_to_date":true})
                }
            }
        };
        print(&value);
        return Ok(());
    }
    let mut cfg = Config::load(&dir)?;
    if let Commands::Serve {
        port,
        herdr,
        herdr_session,
        feishu_base,
    } = cli.command
    {
        if let Some(port) = port {
            cfg.port = port;
        }
        if let Some(path) = herdr {
            cfg.herdr = path;
        }
        if let Some(session) = herdr_session {
            cfg.herdr_session = session;
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
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("A daemon is already running for this data directory")?;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", cfg.port))
            .await
            .context("The management port is already in use")?;
        cfg.save(&dir)?;
        let app = App::new(dir.clone(), cfg.clone())?;
        app.companion()?;
        app.workers();
        eprintln!(
            "Dauerbot {} · {} · data directory {}",
            env!("CARGO_PKG_VERSION"),
            cfg.url(),
            dir.display()
        );
        eprintln!(
            "Run hfb --data-dir '{}' ui to open the native desktop",
            dir.display()
        );
        let shutdown = app.shutdown.clone();
        let stop = app.shutdown.clone();
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
            .with_graceful_shutdown(async move {
                tokio::select! { _ = signal => {}, _ = stop.cancelled() => {} }
            })
            .await?;
        drop(lock);
        return Ok(());
    }
    let client = Client {
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(110))
            .build()?,
        cfg: cfg.clone(),
    };
    if !matches!(&cli.command, Commands::Init | Commands::Service) {
        let state = client.request("GET", "/state", None).await?;
        if state["api_protocol"].as_u64() != Some(api::PROTOCOL) {
            bail!(
                "An incompatible Dauerbot daemon is using this data directory. Stop the old daemon and run this build's hfb serve, or use --data-dir for an independent instance."
            );
        }
    }
    let value=match cli.command{
        Commands::Init=>json!({"data_dir":dir,"url":cfg.url()}),
        Commands::Status=>client.request("GET","/state",None).await?,
        Commands::Project{command}=>match command{
            Projects::List=>client.request("GET","/state",None).await?["projects"].clone(),
            Projects::Add{name,path,agent,chat_id,native_session,args}=>client.request("POST","/projects",Some(json!({"name":name,"path":path.unwrap_or_default(),"agent":agent,"chat_id":chat_id,"native_session":native_session,"extra_args":args}))).await?,
            Projects::Show{project}=>{let id=client.id(&project).await?;client.request("GET",&format!("/projects/{id}"),None).await?}
            Projects::Pause{project}=>{
                let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/pause"),Some(json!({"paused":true}))).await?
            }
            Projects::Unpause{project}=>{
                let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/pause"),Some(json!({"paused":false}))).await?
            }
            Projects::Remove{project,yes}=>{
                if !yes{bail!("Removing a project deletes its message history, but not code. Add --yes to confirm.");}
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
                let descriptor=client.request("GET",&format!("/projects/{id}/terminal"),None).await?;
                let terminal=descriptor["terminal_id"].as_str().context("The project has no main terminal")?;
                let generation=descriptor["generation"].as_str().context("The execution identity is missing")?;
                let lease=|active|Some(json!({"generation":generation,"active":active}));
                client.request("POST",&format!("/projects/{id}/takeover"),lease(true)).await?;
                let status=herdr_feishu_bridge::herdr::command(&cfg)
                    .args(["terminal","attach",terminal])
                    .stdin(std::process::Stdio::inherit()).stdout(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit())
                    .status().await;
                match status {
                    Ok(status) if status.success()=>{client.request("POST",&format!("/projects/{id}/takeover"),lease(false)).await?;},
                    Ok(status)=>bail!("Terminal attach ended with {status}. The control pin remains; release it in Dauerbot after checking your input."),
                    Err(error)=>{let _=client.request("POST",&format!("/projects/{id}/takeover"),lease(false)).await;return Err(error.into())},
                }
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
        Commands::Event{command}=> {
            let (id,action)=match command {
                Events::Retry{id}=>(id,"retry"),
                Events::Dismiss{id}=>(id,"dismiss"),
            };
            client.request("POST",&format!("/events/{id}/resolve"),Some(json!({"action":action}))).await?
        },
        Commands::Timer{command}=>match command {
            Timers::List{project}=>{let id=client.id(&project).await?;client.request("GET",&format!("/projects/{id}/timers"),None).await?},
            Timers::Add{project,name,prompt,kind,at,interval_seconds,cron,timezone,to}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/timers"),Some(json!({"name":name,"prompt":prompt,"kind":kind,"at":at,"interval_seconds":interval_seconds,"cron":cron,"timezone":timezone,"to":to}))).await?},
            Timers::Remove{id}=>client.request("DELETE",&format!("/timers/{id}"),None).await?,
            Timers::Pause{id}=>client.request("POST",&format!("/timers/{id}/pause"),Some(json!({"enabled":false}))).await?,
            Timers::Resume{id}=>client.request("POST",&format!("/timers/{id}/pause"),Some(json!({"enabled":true}))).await?,
        },
        Commands::Message{command}=>match command{
            Messages::Inject{project,text}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/messages"),Some(json!({"content":text}))).await?}
            Messages::Send{project,text,request_id,to,reply_to_message_id}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/outbox"),Some(json!({"content":text,"to":to,"request_id":request_id.unwrap_or_else(db::id),"reply_to_message_id":reply_to_message_id}))).await?}
            Messages::File{project,path,request_id,to,reply_to_message_id}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/outbox"),Some(json!({"path":std::fs::canonicalize(path)?,"to":to,"request_id":request_id.unwrap_or_else(db::id),"reply_to_message_id":reply_to_message_id}))).await?}
            Messages::Retry{id}=>client.request("POST",&format!("/messages/{id}/retry"),None).await?,
            Messages::Dismiss{id}=>client.request("POST",&format!("/messages/{id}/dismiss"),None).await?,
        },
        Commands::Feishu{command}=>match command{
            Feishu::Set{project,app_id}=>{
                let id=client.id(&project).await?;
                eprintln!("Reading the app secret from stdin (never logged or added to command arguments)");
                let mut secret=String::new();tokio::io::stdin().read_to_string(&mut secret).await?;
                let v=client.request("PUT",&format!("/projects/{id}/bot"),Some(json!({"app_id":app_id,"app_secret":secret.trim()}))).await?;
                secret.clear();v
            }
            Feishu::Check{project}=>{let id=client.id(&project).await?;client.request("POST",&format!("/projects/{id}/bot/check"),None).await?},
            Feishu::Disconnect{project}=>{let id=client.id(&project).await?;client.request("DELETE",&format!("/projects/{id}/bot"),None).await?},
        },
        Commands::Service=>{
            let exe=std::env::current_exe()?;
            #[cfg(target_os="macos")]
            println!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>dev.herdr.feishu-bridge</string><key>ProgramArguments</key><array><string>{}</string><string>--data-dir</string><string>{}</string><string>serve</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardErrorPath</key><string>{}/service.log</string></dict></plist>",xml(&exe.to_string_lossy()),xml(&dir.to_string_lossy()),xml(&dir.to_string_lossy()));
            #[cfg(not(target_os="macos"))]
            println!("[Unit]\nDescription=HERDR Feishu Bridge\nAfter=network-online.target\n\n[Service]\nExecStart=\"{}\" --data-dir \"{}\" serve\nRestart=on-failure\n\n[Install]\nWantedBy=default.target",exe.display(),dir.display());
            return Ok(())
        }
        Commands::Serve{..}|Commands::Mcp{..}|Commands::Update{..}|Commands::Ui|Commands::CompanionIdentity=>unreachable!(),
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
