#![allow(non_snake_case)]
mod access;
mod backend;
mod updater;

use backend::{Backend, CommandRequest, Reply};
use gpui::{prelude::*, *};
use gpui_component::{
    button::*,
    input::{Input, InputState},
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

const INK: u32 = 0x202a35;
const MUTED: u32 = 0x74808c;
const LINE: u32 = 0xe6e9ec;
const ACCENT: u32 = 0x1c796c;
const STEPS: [&str; 8] = [
    "Welcome",
    "Create a Feishu bot",
    "Connect your bot",
    "Message events",
    "Local permissions",
    "Add your first project",
    "Workspace",
    "App Updates",
];
actions!(bridge, [Quit, ShowUpdates]);

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Setup {
    page: usize,
    events_confirmed: bool,
    published: bool,
}
struct Desktop {
    backend: Backend,
    independent_data: bool,
    setup: Setup,
    state: Value,
    detail: Value,
    selected: String,
    picked_path: String,
    name: Entity<InputState>,
    path: Entity<InputState>,
    app_id: Entity<InputState>,
    secret: Entity<InputState>,
    message: Entity<InputState>,
    agent: usize,
    notice: String,
    error: bool,
    busy: bool,
    access_result: String,
    environment: Vec<(String, String)>,
    credentials_checked: bool,
    pending_message: String,
    pending_secret: String,
    update_status: Value,
}
impl Desktop {
    fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let independent_data = independent_data_dir(&backend.dir);
        let mut setup: Setup = std::fs::read(backend.dir.join("desktop.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        setup.page = setup.page.min(7);
        let mut input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("For example: personal website", cx);
        let path = input("Choose the project folder", cx);
        let app_id = input("cli_…", cx);
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Stored only on this Mac")
                .masked(true)
        });
        let message = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Send a task to the main session...")
                .multi_line(true)
                .rows(3)
        });
        let _ = backend.tx.send(CommandRequest::Environment);
        cx.spawn_in(window, async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                if entity
                    .update_in(cx, |this, window, cx| {
                        if this.receive(window, cx) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            backend,
            independent_data,
            setup,
            state: Value::Null,
            detail: Value::Null,
            selected: String::new(),
            picked_path: String::new(),
            name,
            path,
            app_id,
            secret,
            message,
            agent: 0,
            notice: "Starting the local service...".into(),
            error: false,
            busy: false,
            access_result: String::new(),
            environment: vec![],
            credentials_checked: false,
            pending_message: String::new(),
            pending_secret: String::new(),
            update_status: Value::Null,
        }
    }
    fn receive(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.backend.rx.try_recv() {
            changed = true;
            match reply {
                Reply::Update(status) => self.update_status = status,
                Reply::Restart => cx.quit(),
                Reply::State(value) => {
                    if self.state.is_null() {
                        self.notice =
                            "Local service ready. Connect a bot or create a local project.".into();
                        if let Some(id) = value["settings"]["app_id"].as_str()
                            && self.app_id.read(cx).value().is_empty()
                        {
                            self.app_id
                                .update(cx, |input, cx| input.set_value(id.to_owned(), window, cx));
                        }
                    }
                    if self.selected.is_empty()
                        && let Some(id) = value["bot_project"].as_str()
                    {
                        self.selected = id.to_owned();
                        let _ = self.backend.tx.send(CommandRequest::Watch(id.to_owned()));
                    }
                    self.state = value;
                }
                Reply::Connection(Some(reason)) => {
                    self.notice = format!("Local service connection failed: {reason}");
                    self.error = true;
                }
                Reply::Connection(None) => {
                    if self.notice.starts_with("Local service connection failed: ") {
                        self.notice = "Local service connection restored".into();
                        self.error = false;
                    }
                }
                Reply::Environment(value) => self.environment = value,
                Reply::Detail(value) => {
                    if value["project"]["id"] == self.selected {
                        self.detail = value;
                    }
                }
                Reply::Access(text) => {
                    self.access_result = text;
                    self.busy = false;
                }
                Reply::Error(text) => {
                    self.notice = text;
                    self.error = true;
                    self.busy = false;
                }
                Reply::Done {
                    path,
                    value,
                    message,
                } => {
                    self.busy = false;
                    self.error = false;
                    if path == "/settings"
                        && self.secret.read(cx).value().as_ref() == self.pending_secret
                    {
                        self.secret
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        self.pending_secret.clear();
                    }
                    if path.ends_with("/messages")
                        && self.message.read(cx).value().as_ref() == self.pending_message
                    {
                        self.message
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        self.pending_message.clear();
                    }
                    if !message.is_empty() {
                        self.notice = message;
                    }
                    if path == "/settings/check" {
                        self.credentials_checked = true;
                    }
                    if path == "/projects" {
                        self.selected = value["id"].as_str().unwrap_or_default().into();
                        let _ = self
                            .backend
                            .tx
                            .send(CommandRequest::Watch(self.selected.clone()));
                        self.go(6);
                    } else if path.starts_with("/projects/")
                        && !path.ends_with("/start")
                        && !path.ends_with("/stop")
                        && !path.ends_with("/resume")
                        && value.get("project").is_some()
                        && value["project"]["id"] == self.selected
                    {
                        self.detail = value;
                    }
                }
            }
        }
        changed
    }
    fn go(&mut self, page: usize) {
        self.setup.page = page.min(7);
        if let Ok(bytes) = serde_json::to_vec(&self.setup) {
            let _ = herdr_feishu_bridge::config::write_private(
                &self.backend.dir.join("desktop.json"),
                &bytes,
            );
        }
    }
    fn api(&mut self, method: &'static str, path: String, body: Option<Value>, message: &str) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = false;
        self.notice = "Working...".into();
        if self
            .backend
            .tx
            .send(CommandRequest::Api {
                method,
                path,
                body,
                success: message.into(),
            })
            .is_err()
        {
            self.busy = false;
            self.error = true;
            self.notice = "The background service exited. Reopen the app.".into();
        }
    }
    fn pick(&mut self, database: bool, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: database,
            directories: !database,
            multiple: false,
            prompt: Some(
                if database {
                    "Choose a SQLite file to check"
                } else {
                    "Choose a project folder"
                }
                .into(),
            ),
        });
        cx.spawn_in(window, async move |entity, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.first()
            {
                let path = path.clone();
                let _ = entity.update_in(cx, |this, window, cx| {
                    if !database {
                        this.path.update(cx, |input, cx| {
                            input.set_value(path.to_string_lossy().to_string(), window, cx)
                        });
                    }
                    this.access_result = format!("Checking: {}", path.display());
                    let command = if database {
                        CommandRequest::Database(path.clone())
                    } else {
                        CommandRequest::Folder(path.clone())
                    };
                    if !database {
                        this.picked_path = path.to_string_lossy().into();
                    }
                    let _ = this.backend.tx.send(command);
                    cx.notify();
                });
            }
        })
        .detach();
    }
    fn btn(
        &self,
        id: &'static str,
        label: &'static str,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .label(label)
            .disabled(self.busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.notify();
            }))
    }
    fn title(kicker: &str, title: &str, description: &str) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .mb_6()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(ACCENT))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(kicker.to_owned()),
            )
            .child(
                div()
                    .text_3xl()
                    .font_weight(FontWeight::BOLD)
                    .child(title.to_owned()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .line_height(relative(1.7))
                    .child(description.to_owned()),
            )
    }
    fn card(title: &str, text: &str) -> Div {
        div()
            .min_w_0()
            .p_5()
            .rounded_xl()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(LINE))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_sm()
                    .child(title.to_owned()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .line_height(relative(1.6))
                    .child(text.to_owned()),
            )
    }
    fn field(label: &str, input: &Entity<InputState>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(label.to_owned()),
            )
            .child(Input::new(input))
    }
    fn welcome(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("YOUR AGENTS, A LITTLE CLOSER", "Keep working.\nTake the conversation with you.", "Talk to your project's main session in Feishu, then continue on your Mac. Connect your first bot to get started."))
            .child(div().flex().gap_4()
                .child(Self::card("01  Connect Feishu", "Create an enterprise app bot. Credentials stay on your Mac." ).flex_1())
                .child(Self::card("02  Choose a project", "Connect an installed CLI to a persistent HERDR session.").flex_1())
                .child(Self::card("03  Continue the conversation", "Messages, tasks, and files reach the same main session.").flex_1()))
            .child(Self::card("You control access", "Choose the project folder yourself. Protected files may require additional access; ordinary projects do not need Full Disk Access, Accessibility, or Screen Recording."))
            .child(self.btn("begin", "Start setup →", |this, _, _| this.go(1), cx).primary())
            .child(self.btn("local-first", "Try locally and connect Feishu later", |this, _, _| this.go(5), cx))
            .child(div().mt_6().text_sm().font_weight(FontWeight::SEMIBOLD).child("Local environment"))
            .children(self.environment.iter().map(|(name, status)| div().flex().gap_4().text_sm().child(div().w_32().child(name.clone())).child(div().text_color(rgb(MUTED)).child(status.clone()))))
            .child(self.btn("rescan", "Check environment again", |this, _, _| { let _ = this.backend.tx.send(CommandRequest::Environment); }, cx))
            .into_any_element()
    }
    fn bot(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 01", "Create your Feishu bot", "Use an enterprise custom app with bot capability. Group Webhook bots cannot receive the messages needed for two-way conversations."))
            .child(Self::card("1. Create an app", "Open the Feishu developer console and create an enterprise custom app. Give it a name and description. Your organization must allow you to create apps."))
            .child(self.btn("console", "Open Feishu developer console ↗", |_, _, cx| cx.open_url("https://open.feishu.cn/app"), cx).primary())
            .child(Self::card("2. Enable bot capability", "Open the app and enable Bot under Add App Capabilities. You do not need a Webhook or a public server."))
            .child(Self::card("3. Find the credentials", "Copy the App ID and App Secret from Credentials and Basic Information. Paste them into the next step on your Mac; do not send them in a chat."))
            .child(Self::card("Already have a bot?", "Use a dedicated app for Dauerbot. If another program listens to the same bot, Feishu may deliver messages to that program instead."))
            .child(self.btn("bot-next", "Bot enabled. Add credentials →", |this, _, _| this.go(2), cx).primary()).into_any_element()
    }
    fn credentials(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 02", "Connect the bot to this Mac", "The local service connects after you save. Credential validation checks app identity; message permissions and subscriptions come next."))
            .child(Self::field("App ID", &self.app_id))
            .child(Self::field("App Secret", &self.secret))
            .child(div().text_xs().text_color(rgb(MUTED)).child("The secret stays masked and saved values are never shown. Leave it blank to keep the current secret; enter a new one when changing App ID. Credentials stay in a local file readable only by your user."))
            .child(self.btn("save-credentials", "Save and connect", |this, _, cx| {
                let id = this.app_id.read(cx).value().to_string();
                if !id.starts_with("cli_") { this.notice = "Enter an App ID starting with cli_".into(); this.error = true; return; }
                let secret = this.secret.read(cx).value().to_string();
                this.pending_secret = secret.clone();
                this.credentials_checked = false;
                this.api("PUT", "/settings".into(), Some(json!({"app_id":id,"app_secret":secret})), "Credentials saved. Validate them to continue.");
            }, cx).primary())
            .child(self.btn("check-credentials", "Validate credentials", |this, _, _| this.api("POST", "/settings/check".into(), None, "App identity verified. Configure message events next."), cx))
            .child(Self::card("Connection status", &format!("{} · {}", self.state["runtime"]["feishu"].as_str().unwrap_or("Waiting for service"), self.state["runtime"]["feishu_error"].as_str().unwrap_or(""))))
            .child(self.btn("credentials-next", "Configure message permissions →", |this, _, _| this.go(3), cx).primary().disabled(!self.credentials_checked || self.busy))
            .into_any_element()
    }
    fn events(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 03", "Receive messages and send replies", "Complete these steps in the Feishu developer console. Manual confirmations are separate from automatic checks."))
            .child(Self::card("1. Import app permissions", "In Permissions, import the JSON below to send bot messages, receive direct messages and group mentions, read message resources, upload files, and read group context."))
            .child(self.btn("copy-scopes-full", "Copy full permission JSON (all group messages)", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(true)).unwrap())), cx).primary())
            .child(self.btn("copy-scopes", "Copy basic permission JSON (group mentions)", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(false)).unwrap())), cx))
            .child(Self::card("2. Receive events over a long connection", "Under Events and Callbacks, choose long connections. Save valid credentials in Dauerbot and keep it running before saving the subscription. Add im.message.receive_v1 to receive messages."))
            .child(Self::card("Need every group reply?", "The full JSON includes im:message.group_msg for all user messages in groups; your administrator may need to approve it. The basic set only receives bot mentions. All messages this bot receives reach its fixed project."))
            .child(Self::card("3. Publish the app and add the bot", "Create a version in Version Management and Release, choose its availability, and publish it. Wait for administrator approval if required. Add the bot to a group or use direct messages. Publish a new version after changing permissions or subscriptions."))
            .child(self.btn("confirmed-events", if self.setup.events_confirmed { "✓ Events and permissions confirmed manually" } else { "I configured permissions and event subscriptions" }, |this, _, _| { this.setup.events_confirmed = !this.setup.events_confirmed; this.go(3); }, cx))
            .child(self.btn("confirmed-release", if self.setup.published { "✓ Publishing and bot access confirmed manually" } else { "I published the app and made the bot available" }, |this, _, _| { this.setup.published = !this.setup.published; this.go(3); }, cx))
            .child(self.btn("events-next", "Check local permissions →", |this, _, _| this.go(4), cx).primary().disabled(!self.setup.events_confirmed || !self.setup.published))
            .into_any_element()
    }
    fn permissions(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 04", "Grant access where you need it", "Checks reflect this process's actual access. The desktop, daemon, and HERDR agent may have different permissions."))
            .child(Self::card("Project folder · required for project work", "Select a folder with the macOS picker to check reading and temporary-file creation. macOS may request access to Desktop, Documents, or Downloads. You can decline and choose another folder."))
            .child(self.btn("choose-access-folder", "Choose and check a project folder...", |this, window, cx| this.pick(false, window, cx), cx).primary())
            .child(Self::card("Databases · when a task needs them", "Dauerbot's SQLite database lives in its private data directory. Select another database only when a task needs it. Checks read schema metadata without reading business records or scanning private app databases."))
            .child(self.btn("choose-db", "Choose and check a SQLite database...", |this, window, cx| this.pick(true, window, cx), cx))
            .child(Self::card("Full Disk Access · optional", "Protected app data may require Full Disk Access. In System Settings, add the app that actually runs the task, such as HERDR. Granting Dauerbot access does not grant it to existing HERDR processes. Reopen the relevant app and check again after granting access."))
            .child(div().flex().gap_3()
                .child(self.btn("files-settings", "Open Files and Folders ↗", |_, _, cx| cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders"), cx))
                .child(self.btn("disk-settings", "Open Full Disk Access ↗", |_, _, cx| cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"), cx)))
            .child(self.btn("reveal-app", "Reveal this app in Finder", |_, _, cx| {
                if let Ok(exe) = std::env::current_exe()
                    && let Some(bundle) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
                { cx.reveal_path(bundle); }
            }, cx))
            .child(Self::card("Accessibility, Screen Recording, and Automation", "Dauerbot manages sessions through HERDR's protocol, without screen capture or system input simulation. These permissions are not required."))
            .when(!self.access_result.is_empty(), |view| view.child(Self::card("Access check result", &self.access_result)))
            .child(self.btn("access-next", "Add a project →", |this, _, _| this.go(5), cx).primary())
            .into_any_element()
    }
    fn project_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let picked = self.picked_path.clone();
        let agents = ["Codex", "Claude Code", "Pi", "Antigravity"];
        let mut choices = div().flex().gap_2();
        for (index, name) in agents.into_iter().enumerate() {
            choices = choices.child(
                Button::new(("agent", index))
                    .label(name)
                    .selected(self.agent == index)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agent = index;
                        cx.notify();
                    })),
            );
        }
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 05", "Create your first main session", "One bot belongs to one project. Direct and group messages reach its active agent, which you can switch in the workspace."))
            .child(Self::field("Project name", &self.name))
            .child(Self::field("Project folder (or enter an absolute path)", &self.path))
            .when(!picked.is_empty(), |v| v.child(div().text_sm().text_color(rgb(ACCENT)).child(format!("Selected: {picked}"))))
            .child(self.btn("pick-project", "Choose a folder...", |this, window, cx| this.pick(false, window, cx), cx))
            .child(choices)
            .child(Self::card("Automatic binding", "Your first project becomes this bot's fixed owner. You do not need to select chats or enter IDs. Direct and group messages both reach this project."))
            .child(Self::card("Try it locally first", "Create a project, start an agent, and send local tasks before connecting Feishu. The CLI may require sign-in or project trust in HERDR on first use."))
            .child(self.btn("create-project", "Create project →", |this, _, cx| {
                let manual = this.path.read(cx).value().to_string();
                let path = if manual.trim().is_empty() { this.picked_path.clone() } else { manual };
                let name = this.name.read(cx).value().to_string();
                if name.trim().is_empty() || path.is_empty() { this.notice = "Enter a project name and choose a folder".into(); this.error = true; return; }
                let agent = ["codex","claude","pi","agy"][this.agent];
                this.api("POST", "/projects".into(), Some(json!({"name":name,"path":path,"agent":agent,"chat_id":""})), "Project created. Start the main session to begin.");
            }, cx).primary())
            .into_any_element()
    }
    fn workspace(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let projects = self.state["projects"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let selected = projects.iter().find(|p| p["id"] == self.selected).cloned();
        let mut body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(Self::title(
                "WORKSPACE",
                "Your projects, within reach",
                "Feishu conversations and local tasks meet here. Closing this window leaves the daemon and main session running.",
            ))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        self.btn(
                            "add-project",
                            "＋ Add project",
                            |this, _, _| {
                                this.go(5);
                            },
                            cx,
                        )
                        .primary(),
                    )
                    .child(self.btn(
                        "return-setup",
                        "Reopen setup guide",
                        |this, _, _| this.go(0),
                        cx,
                    )),
            )
            .child(Self::card(
                "Live connections",
                &format!(
                    "HERDR: {}    Feishu: {}",
                    self.state["runtime"]["core"].as_str().unwrap_or("Waiting for service"),
                    self.state["runtime"]["feishu"].as_str().unwrap_or("Disconnected")
                ),
            ));
        if projects.is_empty() {
            body = body.child(Self::card(
                "No projects yet",
                "Add a project to get started. You can try a local main session before connecting Feishu.",
            ));
        }
        for project in &projects {
            let id = project["id"].as_str().unwrap_or_default().to_owned();
            let label = format!(
                "{}  ·  {}  ·  {}",
                project["name"].as_str().unwrap_or("Project"),
                project["agent"].as_str().unwrap_or(""),
                if project["terminal"]
                    .as_str()
                    .is_some_and(|t| self.state["runtime"]["terminals"][t]["state"] == "finished")
                {
                    "Exited"
                } else if project["terminal"].as_str().is_some_and(|t| {
                    self.state["runtime"]["terminals"][t]["status"]["state"] == "blocked"
                }) {
                    "Needs input"
                } else if project["terminal"].as_str().is_some_and(|t| {
                    self.state["runtime"]["terminals"][t]["status"]["state"] == "error"
                }) {
                    "CLI error"
                } else if project["ready"] == true {
                    "Ready"
                } else if !project["terminal"].is_null() {
                    "Waiting for session"
                } else {
                    "Stopped"
                }
            );
            body = body.child(
                Button::new(SharedString::from(format!("project-{id}")))
                    .label(label)
                    .selected(id == self.selected)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.selected = id.clone();
                        this.detail = Value::Null;
                        let _ = this.backend.tx.send(CommandRequest::Watch(id.clone()));
                        this.api("GET", format!("/projects/{id}"), None, "Project selected");
                    })),
            );
        }
        if let Some(project) = selected {
            let id = self.selected.clone();
            let id2 = id.clone();
            let id3 = id.clone();
            let id4 = id.clone();
            let id5 = id.clone();
            let id7 = id.clone();
            let messages = self.detail["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let terminal = project["terminal"].as_str().unwrap_or("Not created");
            let mut agent_controls = div().flex().flex_wrap().gap_2();
            for (index, (agent, label)) in [
                ("codex", "Codex"),
                ("claude", "Claude Code"),
                ("pi", "Pi"),
                ("agy", "Antigravity"),
            ]
            .into_iter()
            .enumerate()
            {
                let project_id = id.clone();
                agent_controls = agent_controls.child(
                    Button::new(("switch-agent", index))
                        .label(label)
                        .selected(project["agent"] == agent)
                        .disabled(self.busy || project["agent"] == agent)
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.api(
                                "POST",
                                format!("/projects/{project_id}/agent"),
                                Some(json!({"agent":agent})),
                                "Agent switched. The bot and project binding are preserved.",
                            );
                        })),
                );
            }
            let issue = self.state["runtime"]["view_errors"][&id]
                .as_str()
                .unwrap_or("")
                .to_owned();
            if !issue.is_empty() {
                body = body.child(Self::card("Herdr App notice", &issue));
            }
            if let Some(t) = project["terminal"].as_str()
                && self.state["runtime"]["terminals"][t]["state"] == "finished"
            {
                body = body.child(Self::card("The main session exited", "Start the main session again. Its terminal will appear in Herdr App, including sign-in, project trust, and approval prompts."));
            }
            if let Some(t) = project["terminal"].as_str()
                && self.state["runtime"]["terminals"][t]["status"]["state"] == "blocked"
            {
                let reason = self.state["runtime"]["terminals"][t]["status"]["msg"]
                    .as_str()
                    .unwrap_or("The CLI needs input");
                body = body.child(Self::card("The main session needs input", &format!("{reason}\nRecognized Antigravity project-trust prompts are confirmed automatically. Handle other sign-in, question, or approval prompts in Herdr. Queued messages and the current process remain intact.")));
            }
            if let Some(t) = project["terminal"].as_str()
                && self.state["runtime"]["terminals"][t]["status"]["state"] == "error"
            {
                let reason = self.state["runtime"]["terminals"][t]["status"]["msg"]
                    .as_str()
                    .unwrap_or("The CLI reported an error. Check Herdr for details.");
                body = body.child(Self::card("CLI error", reason));
            }

            body = body
                .child(Self::card(project["name"].as_str().unwrap_or("Project"), &format!("{}\nTerminal: {}", project["path"].as_str().unwrap_or(""), terminal)))
                .child(Self::card("Feishu bot", if self.state["bot_project"] == project["id"] {
                    "Bound to this project. All direct and group messages this bot receives reach the active agent without per-chat binding."
                } else {
                    "This is a local project. The current Feishu bot belongs to another project."
                }))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Active agent · click to switch"))
                .child(agent_controls)
                .child(div().text_xs().text_color(rgb(MUTED)).child("Switching stops the current agent and starts a new main session. The project, bot binding, and message queue stay intact."))
                .child(div().flex().flex_wrap().gap_2()
                    .child(self.btn("start", "Start main session", move |this, _, _| this.api("POST", format!("/projects/{id}/start"), None, "Start requested. Waiting for sign-in, project trust, and MCP registration."), cx).primary())
                    .child(self.btn("resume", "Resume session", move |this, _, _| this.api("POST", format!("/projects/{id2}/resume"), None, "Resume requested"), cx))
                    .child(self.btn("show", "Monitor in HERDR", move |this, _, _| this.api("POST", format!("/projects/{id3}/show"), None, "Main session opened in a Herdr App tab"), cx))
                    .child(self.btn("refresh-project", "Refresh messages", move |this, _, _| this.api("GET", format!("/projects/{id4}"), None, "Messages refreshed"), cx))
                    .child(self.btn("stop", "Stop session...", move |_this, window, cx| {
                        let answer = window.prompt(PromptLevel::Warning, "Stop this project's main session?", Some("This ends the agent and its child terminals. Project settings and message history stay intact."), &["Cancel", "Stop"], cx);
                        let id = id5.clone();
                        cx.spawn(async move |entity, cx| {
                            if answer.await.ok() == Some(1) { let _ = entity.update(cx, |this, _| this.api("POST", format!("/projects/{id}/stop"), None, "Session stopped")); }
                        }).detach();
                    }, cx)))
                    .child(Self::field("Local task", &self.message))
                .child(self.btn("inject", "Send to main session", move |this, _, cx| {
                    let text = this.message.read(cx).value().to_string();
                    if text.trim().is_empty() { return; }
                    this.pending_message = text.clone();
                    this.api("POST", format!("/projects/{id7}/messages"), Some(json!({"content":text})), "Task queued. Check message history for acknowledgment and replies.");
                }, cx).primary())
                .child(Self::card("Check message delivery", "Send a direct message to the bot or post in its group. Check the incoming record, agent acknowledgment, and outgoing reply. The agent uses to to select a chat or user; Feishu permissions determine which group messages arrive."))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Message history"));
            if messages.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("No messages yet. Refresh to check again."),
                );
            }
            for item in messages.iter().rev().take(30) {
                body = body.child(Self::card(
                    &format!(
                        "{} · {}",
                        item["direction"].as_str().unwrap_or(""),
                        item["status"].as_str().unwrap_or("")
                    ),
                    &format!(
                        "{}\n{}",
                        item["content"].as_str().unwrap_or(""),
                        item["error"].as_str().unwrap_or("")
                    ),
                ));
            }
        }
        body.into_any_element()
    }
    fn updates_page(&self, cx: &mut Context<Self>) -> Div {
        let phase = self.update_status["phase"].as_str().unwrap_or("idle");
        let running = matches!(phase, "checking" | "downloading" | "installing");
        let automatic = self.update_status["preferences"]["automatic"]
            .as_bool()
            .unwrap_or(true);
        let channel = self.update_status["preferences"]["channel"]
            .as_str()
            .unwrap_or("stable");
        let message = self.update_status["message"]
            .as_str()
            .unwrap_or("Starting the update service...");
        div().flex().flex_col().gap_5()
            .child(Self::title("UPDATES", "App Updates", "Signed updates download in the background and take effect next time you open the app. Project files, bot settings, and active sessions stay intact."))
            .child(Self::card("Current version", &format!("Dauerbot {} · {} channel", env!("CARGO_PKG_VERSION"), if channel == "preview" { "preview" } else { "stable" })))
            .child(Self::card("Update status", message))
            .child(div().flex().gap_3().flex_wrap()
                .child(Button::new("update-check").label("Check for updates").disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Check); cx.notify();
                })))
                .when(phase == "available" || phase == "error", |row| row.child(Button::new("update-install").label("Download and install").disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Install); cx.notify();
                }))))
                .when(phase == "installed", |row| row.child(Button::new("update-restart").label("Restart desktop now").on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Restart); cx.notify();
                }))))
                .when(self.update_status["rollback_available"].as_bool().unwrap_or(false), |row| row.child(Button::new("update-rollback").label("Roll back").disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Rollback); cx.notify();
                }))))
            )
            .child(div().flex().gap_3()
                .child(Button::new("update-auto").label(if automatic { "Disable automatic updates" } else { "Enable automatic updates" }).on_click(cx.listener(move |this, _, _, cx| {
                    let channel = this.update_status["preferences"]["channel"].as_str().unwrap_or("stable").to_owned();
                    let _ = this.backend.tx.send(CommandRequest::UpdatePreferences { automatic: !automatic, channel }); cx.notify();
                })))
                .child(Button::new("update-channel").label(if channel == "stable" { "Switch to preview channel" } else { "Switch to stable channel" }).on_click(cx.listener(|this, _, _, cx| {
                    let channel = if this.update_status["preferences"]["channel"] == "preview" { "stable" } else { "preview" }.into();
                    let automatic = this.update_status["preferences"]["automatic"].as_bool().unwrap_or(true);
                    let _ = this.backend.tx.send(CommandRequest::UpdatePreferences { automatic, channel }); cx.notify();
                })))
            )
            .child(Self::card("Automatic updates", "Checks run every six hours by default. Manual updates remain available when automatic updates are off. Stable excludes prereleases; preview includes them. Installation requires a verified signature from the same developer."))
            .when(!self.update_status["notes"].as_str().unwrap_or("").is_empty(), |page| page.child(Self::card("Release notes", self.update_status["notes"].as_str().unwrap_or(""))))
    }
}
impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.setup.page;
        let mut sidebar = div()
            .w(px(226.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf1f3f2))
            .border_r_1()
            .border_color(rgb(LINE))
            .px_5()
            .py_8()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .mb_1()
                    .child("Dauerbot"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .mb_8()
                    .child("Your projects, always available"),
            );
        for (i, step) in STEPS.iter().enumerate().take(7) {
            sidebar = sidebar.child(
                div()
                    .id(("step", i))
                    .px_3()
                    .py_3()
                    .rounded_lg()
                    .text_sm()
                    .cursor_pointer()
                    .bg(rgb(if i == page { 0xe0ebe6 } else { 0xf1f3f2 }))
                    .text_color(rgb(if i == page { ACCENT } else { MUTED }))
                    .child(format!(
                        "{}   {}",
                        if i == 6 {
                            "↗".into()
                        } else {
                            format!("{:02}", i + 1)
                        },
                        step
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.go(i);
                        cx.notify();
                    })),
            );
        }
        sidebar = sidebar
            .child(div().flex_1())
            .child(
                div()
                    .id("updates-navigation")
                    .px_3()
                    .py_3()
                    .mb_4()
                    .rounded_lg()
                    .text_sm()
                    .cursor_pointer()
                    .bg(rgb(if page == 7 { 0xe0ebe6 } else { 0xf1f3f2 }))
                    .text_color(rgb(ACCENT))
                    .child(if self.update_status["phase"] == "installed" {
                        "App Updates · restart to apply"
                    } else {
                        "App Updates"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.go(7);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .line_height(relative(1.8))
                    .child(format!(
                        "NATIVE ON YOUR MAC\nRust · GPUI\n{}",
                        env!("CARGO_PKG_VERSION")
                    )),
            );
        let content = match page {
            0 => self.welcome(cx),
            1 => self.bot(cx),
            2 => self.credentials(cx),
            3 => self.events(cx),
            4 => self.permissions(cx),
            5 => self.project_form(cx),
            6 => self.workspace(cx),
            _ => self.updates_page(cx).into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .font_family(".AppleSystemUIFont")
            .text_color(rgb(INK))
            .bg(rgb(0xfafbf9))
            .child(sidebar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(56.))
                            .px_10()
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_b_1()
                            .border_color(rgb(LINE))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("Workspace  /  {}", STEPS[page]))
                            .child(if self.independent_data {
                                "●  Independent data directory"
                            } else if self.busy {
                                "●  Working"
                            } else {
                                "●  Private local workspace"
                            }),
                    )
                    .child(
                        div()
                            .id("content-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_10()
                            .child(
                                div()
                                    .max_w(px(850.))
                                    .when(self.independent_data, |view| {
                                        view.child(
                                            div()
                                                .mb_6()
                                                .p_4()
                                                .rounded_lg()
                                                .bg(rgb(0xfff1ce))
                                                .text_sm()
                                                .child("Independent instance · This window uses separate data from your everyday workspace.")
                                                .child(div().mt_2().text_xs().child(
                                                    self.backend.dir.display().to_string(),
                                                )),
                                        )
                                    })
                                    .child(content),
                            ),
                    )
                    .child(
                        div()
                            .px_8()
                            .py_3()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .text_xs()
                            .text_color(rgb(if self.error { 0xb44343 } else { MUTED }))
                            .child(self.notice.clone()),
                    ),
            )
    }
}
fn independent_data_dir(dir: &std::path::Path) -> bool {
    let default = herdr_feishu_bridge::config::default_dir();
    dir.canonicalize().unwrap_or_else(|_| dir.to_owned())
        != default.canonicalize().unwrap_or(default)
}
fn main() {
    let dir = std::env::args()
        .skip_while(|v| v != "--data-dir")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(herdr_feishu_bridge::config::default_dir);
    let title = if independent_data_dir(&dir) {
        "Dauerbot · Independent instance"
    } else {
        "Dauerbot"
    };
    let backend = Backend::start(dir);
    Application::new().run(move |cx| {
        gpui_component::init(cx);
        Theme::change(ThemeMode::Light, None, cx);
        Theme::global_mut(cx).colors.primary = rgb(ACCENT).into();
        Theme::global_mut(cx).colors.primary_hover = rgb(0x23685e).into();
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-,", ShowUpdates, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![Menu {
            name: "Dauerbot".into(),
            items: vec![
                MenuItem::action("App Updates…", ShowUpdates),
                MenuItem::action("Quit Dauerbot", Quit),
            ],
        }]);
        let bounds = Bounds::centered(None, size(px(1180.), px(860.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(980.), px(680.))),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| Desktop::new(backend, window, cx));
                let updates_view = view.clone();
                cx.on_action(move |_: &ShowUpdates, cx| {
                    updates_view.update(cx, |this, cx| {
                        this.go(7);
                        cx.notify();
                    });
                });
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .expect("Cannot create the desktop window");
        cx.activate(true);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
