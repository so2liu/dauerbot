#![allow(non_snake_case)]
mod access;
mod backend;
mod locale;
mod terminal;
mod updater;

use backend::{Backend, CommandRequest, Reply};
use gpui::{prelude::*, *};
use gpui_component::{
    button::*,
    input::{Input, InputState},
    resizable::{h_resizable, resizable_panel},
    *,
};
use locale::Language;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

const INK: u32 = 0x202a35;
const MUTED: u32 = 0x74808c;
const LINE: u32 = 0xe6e9ec;
const ACCENT: u32 = 0x1c796c;
const STEPS: [&str; 9] = [
    "Welcome",
    "Create a Feishu bot",
    "Connect your bot",
    "Message events",
    "Local permissions",
    "Add your first project",
    "Workspace",
    "App Updates",
    "Environment",
];
actions!(bridge, [Quit, ShowUpdates]);

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Setup {
    page: usize,
    events_confirmed: bool,
    published: bool,
    language: Language,
    terminal_hidden: bool,
    dismissed_core_projects: Vec<String>,
}
struct Desktop {
    backend: Backend,
    independent_data: bool,
    setup: Setup,
    state: Value,
    selected: String,
    picked_path: String,
    name: Entity<InputState>,
    path: Entity<InputState>,
    app_id: Entity<InputState>,
    secret: Entity<InputState>,
    agent: usize,
    notice: String,
    error: bool,
    busy: bool,
    access_result: String,
    environment: Vec<(String, String)>,
    credentials_checked: bool,
    connection_error: Option<String>,
    pending_secret: String,
    update_status: Value,
    terminal: Option<terminal::Bridge>,
    terminal_key: String,
    terminal_error: String,
    tasks: Value,
    timers: Value,
    work_checked: std::time::Instant,
    decision_answer: Entity<InputState>,
    task_title: Entity<InputState>,
    task_prompt: Entity<InputState>,
    schedule: Entity<InputState>,
    timezone: Entity<InputState>,
    recipient: Entity<InputState>,
    task_kind: usize,
    task_form: bool,
    editing_timer: Option<String>,
    quitting: bool,
    job_reason: Entity<InputState>,
    takeover_requested: bool,
}
impl Desktop {
    fn tr(&self, text: &str) -> String {
        self.setup.language.text(text)
    }
    fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let independent_data = independent_data_dir(&backend.dir);
        let mut setup: Setup = std::fs::read(backend.dir.join("desktop.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        setup.page = setup.page.min(8);
        let mut input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(setup.language.text(placeholder)))
        };
        let name = input("For example: personal website", cx);
        let path = input("Choose the project folder", cx);
        let app_id = input("cli_…", cx);
        let decision_answer = input("Your answer", cx);
        let job_reason = input("What did you verify?", cx);
        let task_title = input("Task name", cx);
        let schedule = input("Schedule", cx);
        let recipient = input("Reply destination (optional)", cx);
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(setup.language.text("Stored only on this Mac"))
                .masked(true)
        });
        let task_prompt = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder(setup.language.text("What should the agent do?"))
        });
        let timezone = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()))
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
            selected: String::new(),
            picked_path: String::new(),
            name,
            path,
            app_id,
            secret,
            agent: 0,
            notice: "Starting the local service...".into(),
            error: false,
            busy: false,
            access_result: String::new(),
            environment: vec![],
            credentials_checked: false,
            connection_error: None,
            pending_secret: String::new(),
            update_status: Value::Null,
            terminal: None,
            terminal_key: String::new(),
            terminal_error: String::new(),
            tasks: Value::Null,
            timers: Value::Null,
            work_checked: std::time::Instant::now() - Duration::from_secs(30),
            decision_answer,
            task_title,
            task_prompt,
            schedule,
            timezone,
            recipient,
            task_kind: 0,
            task_form: false,
            editing_timer: None,
            quitting: false,
            job_reason,
            takeover_requested: false,
        }
    }
    fn receive(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.backend.rx.try_recv() {
            changed = true;
            match reply {
                Reply::Terminal {
                    project,
                    value,
                    error,
                } => {
                    if project == self.selected
                        && self.setup.page == 6
                        && !self.setup.terminal_hidden
                    {
                        self.terminal_error = error;
                        if let Ok(descriptor) =
                            serde_json::from_value::<terminal::Descriptor>(value)
                            && descriptor.project_id == self.selected
                            && self.state["projects"].as_array().is_some_and(|projects| {
                                projects.iter().any(|project| {
                                    project["id"] == self.selected
                                        && project["generation"] == descriptor.generation
                                })
                            })
                        {
                            self.terminal = Some(terminal::Bridge::build(descriptor, cx));
                        }
                    }
                }
                Reply::Takeover {
                    project,
                    generation,
                    active,
                } => {
                    if project == self.selected
                        && let Some(bridge) = &mut self.terminal
                        && bridge.descriptor.generation == generation
                    {
                        self.takeover_requested = false;
                        bridge.set_control(active, cx);
                    }
                }
                Reply::Work {
                    project,
                    tasks,
                    timers,
                } => {
                    if project == self.selected {
                        self.tasks = tasks;
                        self.timers = timers;
                    }
                }
                Reply::Update(status) => self.update_status = status,
                Reply::Restart => self.quit(cx),
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
                    }
                    if self.state.is_null()
                        && value["projects"]
                            .as_array()
                            .is_some_and(|projects| !projects.is_empty())
                    {
                        self.go(6);
                    }
                    if self.selected.is_empty()
                        && let Some(id) = value["projects"]
                            .as_array()
                            .and_then(|projects| projects.first())
                            .and_then(|project| project["id"].as_str())
                    {
                        self.selected = id.to_owned();
                    }
                    self.state = value;
                    if let Some(tasks) = self.state["work"][&self.selected]["tasks"].as_array() {
                        self.tasks = Value::Array(tasks.clone());
                    }
                    if let Some(timers) = self.state["work"][&self.selected]["timers"].as_array() {
                        self.timers = Value::Array(timers.clone());
                    }
                    if let Some(bridge) = &mut self.terminal
                        && bridge.controlling
                        && (self.state["work"][&self.selected]["lease"]["active"] != true
                            || self.state["work"][&self.selected]["lease"]["generation"]
                                != bridge.descriptor.generation)
                    {
                        bridge.set_control(false, cx);
                    }
                }
                Reply::Connection(reason) => {
                    self.connection_error = reason;
                    if self.connection_error.is_none() {
                        self.notice = "Local service connection restored".into();
                        self.error = false;
                    }
                }
                Reply::Environment(value) => self.environment = value,
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
                    if !message.is_empty() {
                        self.notice = message;
                    }
                    if path == "/settings/check" {
                        self.credentials_checked = true;
                    }
                    if path == "/projects" {
                        self.selected = value["id"].as_str().unwrap_or_default().into();
                        self.go(6);
                    }
                }
            }
        }
        if let Some(terminal) = &mut self.terminal {
            changed |= terminal.receive();
        }
        self.sync_terminal(cx);
        if !self.selected.is_empty() && self.work_checked.elapsed() >= Duration::from_secs(10) {
            self.work_checked = std::time::Instant::now();
            let _ = self.backend.tx.send(CommandRequest::Work {
                project: self.selected.clone(),
            });
        }
        changed
    }
    fn release_terminal(&mut self) {
        if let Some(bridge) = self.terminal.take()
            && (bridge.controlling || self.takeover_requested)
        {
            let _ = self.backend.tx.send(CommandRequest::Takeover {
                project: bridge.descriptor.project_id.clone(),
                generation: bridge.descriptor.generation.clone(),
                active: false,
            });
        }
        self.takeover_requested = false;
    }
    fn quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        let pending = self
            .terminal
            .as_ref()
            .filter(|bridge| bridge.controlling || self.takeover_requested)
            .map(|bridge| {
                self.backend.release_before_exit(
                    bridge.descriptor.project_id.clone(),
                    bridge.descriptor.generation.clone(),
                )
            });
        self.release_terminal();
        if let Some(pending) = pending {
            cx.spawn(async move |_, cx| {
                let _ = pending.recv_async().await;
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        } else {
            cx.quit();
        }
    }
    fn sync_terminal(&mut self, _: &mut Context<Self>) {
        if self.setup.page != 6 || self.setup.terminal_hidden {
            return;
        }
        let project = self.state["projects"]
            .as_array()
            .and_then(|items| items.iter().find(|p| p["id"] == self.selected));
        let key = project
            .map(|p| {
                format!(
                    "{}/{}/{}",
                    self.selected,
                    p["generation"].as_str().unwrap_or(""),
                    p["terminal"].as_str().unwrap_or("")
                )
            })
            .unwrap_or_default();
        let has_terminal = project.is_some_and(|p| p["terminal"].as_str().is_some());
        if key != self.terminal_key {
            self.release_terminal();
            self.terminal_key = key;
            self.terminal_error.clear();
            if has_terminal {
                let _ = self.backend.tx.send(CommandRequest::Terminal {
                    project: self.selected.clone(),
                });
            }
        }
    }
    fn select_project(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected != id {
            self.release_terminal();
            self.selected = id;
            self.terminal_key.clear();
            self.tasks = self.state["work"][&self.selected]["tasks"].clone();
            self.timers = self.state["work"][&self.selected]["timers"].clone();
            self.work_checked = std::time::Instant::now() - Duration::from_secs(30);
            self.sync_terminal(cx);
        }
        cx.notify();
    }
    fn resolve_event(
        &mut self,
        event_id: String,
        retry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = if retry {
            "Retry this task?"
        } else {
            "Dismiss this task?"
        };
        let description = if retry {
            "The agent may already have changed files or sent a reply. Continue after reviewing its work."
        } else {
            "The task will stop waiting. Existing files and sent replies remain."
        };
        let action = if retry { "Retry task" } else { "Dismiss task" };
        let answer = window.prompt(
            PromptLevel::Warning,
            &self.tr(title),
            Some(&self.tr(description)),
            &[self.tr("Cancel").as_str(), self.tr(action).as_str()],
            cx,
        );
        cx.spawn(async move |entity, cx| {
            if answer.await.ok() == Some(1) {
                let _ = entity.update(cx, |this, _| {
                    this.api(
                        "POST",
                        format!("/events/{event_id}/resolve"),
                        Some(json!({"action": if retry { "retry" } else { "dismiss" }})),
                        if retry {
                            "Task queued"
                        } else {
                            "Task dismissed"
                        },
                    );
                    this.work_checked = std::time::Instant::now() - Duration::from_secs(30);
                });
            }
        })
        .detach();
    }
    fn go(&mut self, page: usize) {
        if self.setup.page == 6 && page != 6 {
            self.release_terminal();
            self.terminal_key.clear();
        }
        self.setup.page = page.min(8);
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
                self.tr(if database {
                    "Choose a SQLite file to check"
                } else {
                    "Choose a project folder"
                })
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
            .label(self.tr(label))
            .disabled(self.busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.notify();
            }))
    }
    fn title(&self, kicker: &str, title: &str, description: &str) -> Div {
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
                    .child(self.tr(kicker)),
            )
            .child(
                div()
                    .text_3xl()
                    .font_weight(FontWeight::BOLD)
                    .child(self.tr(title)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .line_height(relative(1.7))
                    .child(self.tr(description)),
            )
    }
    fn card(&self, title: &str, text: &str) -> Div {
        self.card_detail(title, &self.tr(text))
    }
    fn card_detail(&self, title: &str, text: &str) -> Div {
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
                    .child(self.tr(title)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .line_height(relative(1.6))
                    .child(text.to_owned()),
            )
    }
    fn field(&self, label: &str, input: &Entity<InputState>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(self.tr(label)),
            )
            .child(Input::new(input))
    }
    fn welcome(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(self.title("YOUR AGENTS, A LITTLE CLOSER", "Keep working.\nTake the conversation with you.", "Talk to your project's main session in Feishu, then continue on your Mac. Connect your first bot to get started."))
            .child(div().flex().gap_4()
                .child(self.card("01  Connect Feishu", "Create an enterprise app bot. Credentials stay on your Mac." ).flex_1())
                .child(self.card("02  Choose a project", "Connect an installed CLI to a persistent HERDR session.").flex_1())
                .child(self.card("03  Continue the conversation", "Messages, tasks, and files reach the same main session.").flex_1()))
            .child(self.card("You control access", "Choose the project folder yourself. Protected files may require additional access; ordinary projects do not need Full Disk Access, Accessibility, or Screen Recording."))
            .child(self.btn("begin", "Start setup →", |this, _, _| this.go(1), cx).primary())
            .child(self.btn("local-first", "Try locally and connect Feishu later", |this, _, _| this.go(5), cx))
            .into_any_element()
    }
    fn bot(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(self.title("SETUP / 01", "Create your Feishu bot", "Use an enterprise custom app with bot capability. Group Webhook bots cannot receive the messages needed for two-way conversations."))
            .child(self.card("1. Create an app", "Open the Feishu developer console and create an enterprise custom app. Give it a name and description. Your organization must allow you to create apps."))
            .child(self.btn("console", "Open Feishu developer console ↗", |_, _, cx| cx.open_url("https://open.feishu.cn/app"), cx).primary())
            .child(self.card("2. Enable bot capability", "Open the app and enable Bot under Add App Capabilities. You do not need a Webhook or a public server."))
            .child(self.card("3. Find the credentials", "Copy the App ID and App Secret from Credentials and Basic Information. Paste them into the next step on your Mac; do not send them in a chat."))
            .child(self.card("Already have a bot?", "Use a dedicated app for Dauerbot. If another program listens to the same bot, Feishu may deliver messages to that program instead."))
            .child(self.btn("bot-next", "Bot enabled. Add credentials →", |this, _, _| this.go(2), cx).primary()).into_any_element()
    }
    fn credentials(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(self.title("SETUP / 02", "Connect the bot to this Mac", "The local service connects after you save. Credential validation checks app identity; message permissions and subscriptions come next."))
            .child(self.field("App ID", &self.app_id))
            .child(self.field("App Secret", &self.secret))
            .child(div().text_xs().text_color(rgb(MUTED)).child(self.tr("The secret stays masked and saved values are never shown. Leave it blank to keep the current secret; enter a new one when changing App ID. Credentials stay in a local file readable only by your user.")))
            .child(self.btn("save-credentials", "Save and connect", |this, _, cx| {
                let id = this.app_id.read(cx).value().to_string();
                if !id.starts_with("cli_") { this.notice = "Enter an App ID starting with cli_".into(); this.error = true; return; }
                let secret = this.secret.read(cx).value().to_string();
                this.pending_secret = secret.clone();
                this.credentials_checked = false;
                this.api("PUT", "/settings".into(), Some(json!({"app_id":id,"app_secret":secret})), "Credentials saved. Validate them to continue.");
            }, cx).primary())
            .child(self.btn("check-credentials", "Validate credentials", |this, _, _| this.api("POST", "/settings/check".into(), None, "App identity verified. Configure message events next."), cx))
            .child(self.card("Connection status", &format!("{} · {}", self.tr(connection_status(self.state["runtime"]["feishu"].as_str().unwrap_or("Waiting for service"))), self.state["runtime"]["feishu_error"].as_str().unwrap_or(""))))
            .child(self.btn("credentials-next", "Configure message permissions →", |this, _, _| this.go(3), cx).primary().disabled(!self.credentials_checked || self.busy))
            .into_any_element()
    }
    fn events(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(self.title("SETUP / 03", "Receive messages and send replies", "Complete these steps in the Feishu developer console. Manual confirmations are separate from automatic checks."))
            .child(self.card("1. Import app permissions", "In Permissions, import the JSON below to send bot messages, receive direct messages and group mentions, read message resources, upload files, and read group context."))
            .child(self.btn("copy-scopes-full", "Copy full permission JSON (all group messages)", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(true)).unwrap())), cx).primary())
            .child(self.btn("copy-scopes", "Copy basic permission JSON (group mentions)", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(false)).unwrap())), cx))
            .child(self.card("2. Receive events over a long connection", "Under Events and Callbacks, choose long connections. Save valid credentials in Dauerbot and keep it running before saving the subscription. Add im.message.receive_v1 to receive messages."))
            .child(self.card("Need every group reply?", "The full JSON includes im:message.group_msg for all user messages in groups; your administrator may need to approve it. The basic set only receives bot mentions. All messages this bot receives reach its fixed project."))
            .child(self.card("3. Publish the app and add the bot", "Create a version in Version Management and Release, choose its availability, and publish it. Wait for administrator approval if required. Add the bot to a group or use direct messages. Publish a new version after changing permissions or subscriptions."))
            .child(self.btn("confirmed-events", if self.setup.events_confirmed { "✓ Events and permissions confirmed manually" } else { "I configured permissions and event subscriptions" }, |this, _, _| { this.setup.events_confirmed = !this.setup.events_confirmed; this.go(3); }, cx))
            .child(self.btn("confirmed-release", if self.setup.published { "✓ Publishing and bot access confirmed manually" } else { "I published the app and made the bot available" }, |this, _, _| { this.setup.published = !this.setup.published; this.go(3); }, cx))
            .child(self.btn("events-next", "Check local permissions →", |this, _, _| this.go(4), cx).primary().disabled(!self.setup.events_confirmed || !self.setup.published))
            .into_any_element()
    }
    fn permissions(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(self.title("SETUP / 04", "Grant access where you need it", "Checks reflect this process's actual access. The desktop, daemon, and HERDR agent may have different permissions."))
            .child(self.card("Project folder · required for project work", "Select a folder with the macOS picker to check reading and temporary-file creation. macOS may request access to Desktop, Documents, or Downloads. You can decline and choose another folder."))
            .child(self.btn("choose-access-folder", "Choose and check a project folder...", |this, window, cx| this.pick(false, window, cx), cx).primary())
            .child(self.card("Databases · when a task needs them", "Dauerbot's SQLite database lives in its private data directory. Select another database only when a task needs it. Checks read schema metadata without reading business records or scanning private app databases."))
            .child(self.btn("choose-db", "Choose and check a SQLite database...", |this, window, cx| this.pick(true, window, cx), cx))
            .child(self.card("Full Disk Access · optional", "Protected app data may require Full Disk Access. In System Settings, add the app that actually runs the task, such as HERDR. Granting Dauerbot access does not grant it to existing HERDR processes. Reopen the relevant app and check again after granting access."))
            .child(div().flex().gap_3()
                .child(self.btn("files-settings", "Open Files and Folders ↗", |_, _, cx| cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders"), cx))
                .child(self.btn("disk-settings", "Open Full Disk Access ↗", |_, _, cx| cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"), cx)))
            .child(self.btn("reveal-app", "Reveal this app in Finder", |_, _, cx| {
                if let Ok(exe) = std::env::current_exe()
                    && let Some(bundle) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
                { cx.reveal_path(bundle); }
            }, cx))
            .child(self.card("Accessibility, Screen Recording, and Automation", "Dauerbot manages sessions through HERDR's protocol, without screen capture or system input simulation. These permissions are not required."))
            .when(!self.access_result.is_empty(), |view| view.child(self.card("Access check result", &self.access_result)))
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
            .child(self.title("ADD PROJECT", "Add project", "One bot belongs to one project. Direct and group messages reach its active agent, which you can switch in the workspace."))
            .child(self.field("Project name", &self.name))
            .child(self.field("Project folder (or enter an absolute path)", &self.path))
            .when(!picked.is_empty(), |v| v.child(div().text_sm().text_color(rgb(ACCENT)).child(format!("{} {picked}", self.tr("Selected:")))))
            .child(self.btn("pick-project", "Choose a folder...", |this, window, cx| this.pick(false, window, cx), cx))
            .child(choices)
            .child(self.card("Automatic binding", "Your first project becomes this bot's fixed owner. You do not need to select chats or enter IDs. Direct and group messages both reach this project."))
            .child(self.card("Your agent, inside Dauerbot", "Watch and operate the live terminal in this app. Closing the window leaves tasks running."))
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
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.tr("Projects")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        self.connection_pill(
                            "HERDR",
                            self.state["runtime"]["core"]
                                .as_str()
                                .unwrap_or("Disconnected"),
                        ),
                    )
                    .child(
                        self.connection_pill(
                            "Feishu",
                            self.state["runtime"]["feishu"]
                                .as_str()
                                .unwrap_or("Not configured"),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.btn("add-project", "＋ Add project", |this, _, _| this.go(5), cx))
                    .child(self.btn(
                        "toggle-terminal",
                        if self.setup.terminal_hidden {
                            "Show terminal"
                        } else {
                            "Hide terminal"
                        },
                        |this, _, _| {
                            this.setup.terminal_hidden = !this.setup.terminal_hidden;
                            this.release_terminal();
                            this.terminal_key.clear();
                            this.go(6);
                        },
                        cx,
                    )),
            );
        if projects.is_empty() {
            body = body.child(self.card(
                "No projects yet",
                "Add a project, choose an agent, and start your main session.",
            ));
        }
        for project in &projects {
            let id = project["id"].as_str().unwrap_or_default().to_owned();
            let bound = self.state["bot_project"] == project["id"];
            let status =
                project["lifecycle"]
                    .as_str()
                    .unwrap_or(if project["terminal"].is_null() {
                        "Sleeping"
                    } else {
                        "Ready"
                    });
            body = body.child(
                Button::new(SharedString::from(format!("project-{id}")))
                    .label(format!(
                        "{}{} · {}",
                        if bound { "● " } else { "" },
                        project["name"].as_str().unwrap_or("Project"),
                        self.tr(status)
                    ))
                    .selected(id == self.selected)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_project(id.clone(), cx)),
                    ),
            );
        }
        if let Some(project) = selected {
            let id = self.selected.clone();
            body = body
                .child(
                    div()
                        .mt_2()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(project["name"].as_str().unwrap_or("Project").to_owned()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(project["path"].as_str().unwrap_or("").to_owned()),
                );
            if let Some(message) = project["migration_notice"]
                .as_str()
                .filter(|text| !text.is_empty())
                && !self.setup.dismissed_core_projects.contains(&id)
            {
                let project_id = id.clone();
                body = body
                    .child(self.card("Session preserved", message))
                    .child(self.btn(
                        "dismiss-migration",
                        "Got it",
                        move |this, _, _| {
                            this.setup.dismissed_core_projects.push(project_id.clone());
                            this.go(6);
                        },
                        cx,
                    ));
            }
            if let Some(issue) = self.state["runtime"]["issues"][&id]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                body = body.child(self.card_detail("Needs attention", issue));
            }
            let work = self.state["work"][&id].clone();
            if project["terminal"].is_null()
                && work["execution"]["status"] == "exited"
                && let Some(error) = work["execution"]["error"]
                    .as_str()
                    .filter(|error| error.contains("\n\n"))
            {
                body = body.child(self.card_detail("Startup output", error));
            }
            for attention in work["attention"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["status"] == "open")
                .take(2)
            {
                if let Some(message) = attention["message"].as_str() {
                    body = body.child(self.card("Needs attention", message));
                }
            }
            let unfinished = work["attention"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["status"] == "open" && item["reason"] == "unfinished_run");
            for event in work["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|event| {
                    ["uncertain", "failed"].contains(&event["status"].as_str().unwrap_or(""))
                        || (unfinished
                            && ["submitted", "acknowledged"]
                                .contains(&event["status"].as_str().unwrap_or("")))
                })
                .take(2)
            {
                let event_id = event["id"].as_str().unwrap_or("").to_owned();
                let retry_id = event_id.clone();
                body = body
                    .child(self.card("Review task result", "Check the terminal and saved work before retrying or dismissing this task."))
                    .child(
                        div().id(SharedString::from(format!("resolve-{event_id}"))).flex().gap_2()
                            .child(self.btn("retry-event", "Retry task", move |this, window, cx| {
                                this.resolve_event(retry_id.clone(), true, window, cx);
                            }, cx))
                            .child(self.btn("dismiss-event", "Dismiss task", move |this, window, cx| {
                                this.resolve_event(event_id.clone(), false, window, cx);
                            }, cx)),
                    );
            }
            if let Some(decision) = work["decisions"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["status"] == "pending"))
            {
                let decision_id = decision["id"].as_str().unwrap_or("").to_owned();
                body = body
                    .child(self.card_detail(
                        "Your answer is needed",
                        decision["question"].as_str().unwrap_or(""),
                    ))
                    .child(Input::new(&self.decision_answer))
                    .child(self.btn(
                        "answer-decision",
                        "Send answer",
                        move |this, _, cx| {
                            let answer = this.decision_answer.read(cx).value().to_string();
                            this.api(
                                "POST",
                                format!("/decisions/{decision_id}/answer"),
                                Some(json!({"answer":answer})),
                                "Answer saved",
                            );
                        },
                        cx,
                    ));
            }
            let jobs: Vec<_> = work["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|job| {
                    ["active", "uncertain"].contains(&job["status"].as_str().unwrap_or(""))
                })
                .collect();
            if !jobs.is_empty() {
                body = body
                    .child(self.card(
                        "Background work needs review",
                        "Stop the parent session, verify any remaining child work, and record what you checked.",
                    ))
                    .child(Input::new(&self.job_reason));
                for job in jobs.into_iter().take(3) {
                    let job_id = job["id"].as_str().unwrap_or("").to_owned();
                    let label = job["label"]
                        .as_str()
                        .or(job["name"].as_str())
                        .unwrap_or("Background task")
                        .to_owned();
                    body = body.child(
                        div().flex().items_center().justify_between().gap_2()
                            .child(label)
                            .child(Button::new(SharedString::from(format!("abandon-{job_id}")))
                                .label(self.tr("Abandon reviewed job"))
                                .disabled(!project["terminal"].is_null() || self.busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let reason = this.job_reason.read(cx).value().to_string();
                                    if reason.trim().is_empty() {
                                        this.notice = "Describe what you checked first".into();
                                        this.error = true;
                                        cx.notify();
                                        return;
                                    }
                                    let answer = window.prompt(
                                        PromptLevel::Warning,
                                        &this.tr("Record this job as abandoned?"),
                                        Some(&this.tr("This records your review. It does not terminate an external process.")),
                                        &[this.tr("Cancel").as_str(), this.tr("Abandon reviewed job").as_str()],
                                        cx,
                                    );
                                    let job_id = job_id.clone();
                                    cx.spawn(async move |entity, cx| {
                                        if answer.await.ok() == Some(1) {
                                            let _ = entity.update(cx, |this, _| {
                                                this.api("POST",format!("/jobs/{job_id}/abandon"),Some(json!({"reason":reason})),"Review saved");
                                            });
                                        }
                                    }).detach();
                                }))),
                    );
                }
            }
            if work["lease"]["active"] == true
                && !self
                    .terminal
                    .as_ref()
                    .is_some_and(|bridge| bridge.controlling)
            {
                let project = id.clone();
                let generation = work["lease"]["generation"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                body = body.child(self.card("Manual control is reserved", "Release the reservation to let queued tasks continue, or take control in the terminal."))
                    .child(self.btn("release-reservation", "Release reservation", move |this, _, _| { let _ = this.backend.tx.send(CommandRequest::Takeover { project:project.clone(),generation:generation.clone(),active:false }); }, cx));
            }
            let start_action = if project["native_session"].as_str().unwrap_or("").is_empty() {
                "start"
            } else {
                "resume"
            };
            let start = id.clone();
            let stop = id.clone();
            let pause = id.clone();
            let paused = project["paused"].as_bool().unwrap_or(false);
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        self.btn(
                            "wake-project",
                            "Start / resume",
                            move |this, _, _| {
                                this.api(
                                    "POST",
                                    format!("/projects/{start}/{start_action}"),
                                    None,
                                    "Starting the main session...",
                                )
                            },
                            cx,
                        )
                        .primary(),
                    )
                    .child(self.btn(
                        "pause-project",
                        if paused {
                            "Resume delivery"
                        } else {
                            "Pause delivery"
                        },
                        move |this, _, _| {
                            this.api(
                                "POST",
                                format!("/projects/{pause}/pause"),
                                Some(json!({"paused":!paused})),
                                "Delivery settings saved",
                            )
                        },
                        cx,
                    ))
                    .child(self.btn(
                        "stop-project",
                        "Stop",
                        move |this, _, _| {
                            this.api(
                                "POST",
                                format!("/projects/{stop}/stop"),
                                None,
                                "Session stopped",
                            )
                        },
                        cx,
                    )),
            );
            body = body.child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.tr("Active agent")),
            );
            let mut agents = div().flex().flex_wrap().gap_2();
            for (index, agent) in ["codex", "claude", "pi", "agy"].into_iter().enumerate() {
                let project_id = id.clone();
                agents = agents.child(
                    Button::new(("agent", index))
                        .label(agent_name(agent))
                        .selected(project["agent"] == agent)
                        .disabled(self.busy || project["agent"] == agent)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.release_terminal();
                            this.api(
                                "POST",
                                format!("/projects/{project_id}/agent"),
                                Some(json!({"agent":agent})),
                                "Agent switched. The bot and project binding are preserved.",
                            );
                            cx.notify();
                        })),
                );
            }
            body = body.child(agents);
            if let Some(model) = project["model"]
                .as_str()
                .filter(|s| !s.is_empty())
                .filter(|_| project["model_source"] == "native_observation")
            {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(format!("{}: {model}", self.tr("Model"))),
                );
            } else {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(self.tr("Model follows the CLI's own settings")),
                );
            }
            body = body.child(
                div()
                    .mt_3()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.tr("Tasks & schedules")),
                    )
                    .child(self.btn(
                        "new-task",
                        "＋ New task",
                        |this, _, _| {
                            this.task_form = !this.task_form;
                            this.editing_timer = None;
                        },
                        cx,
                    )),
            );
            if self.task_form {
                body = body.child(self.task_editor(cx));
            }
            for task in records(&self.tasks, "tasks")
                .into_iter()
                .filter(|task| task["status"] != "imported")
                .take(6)
            {
                let task_id = task["id"].as_str().unwrap_or("").to_owned();
                let state = task["status"]
                    .as_str()
                    .or(task["state"].as_str())
                    .unwrap_or("Queued");
                let mut row = div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_1()
                            .child(task["title"].as_str().unwrap_or("Task").to_owned())
                            .child(div().text_xs().text_color(rgb(MUTED)).child(self.tr(state))),
                    );
                if !["completed", "cancelled", "failed", "done"].contains(&state) {
                    row = row.child(
                        Button::new(SharedString::from(format!("cancel-{task_id}")))
                            .label(self.tr("Cancel"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.api(
                                    "POST",
                                    format!("/tasks/{task_id}/cancel"),
                                    None,
                                    "Task cancelled",
                                );
                                this.work_checked =
                                    std::time::Instant::now() - Duration::from_secs(30);
                                cx.notify();
                            })),
                    );
                }
                body = body.child(row);
            }
            for timer in records(&self.timers, "timers") {
                let timer_id = timer["id"].as_str().unwrap_or("").to_owned();
                let edit = timer.clone();
                let toggle_id = timer_id.clone();
                let enabled = timer["enabled"].as_bool().unwrap_or(false);

                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .text_sm()
                        .child(
                            div()
                                .flex_1()
                                .child(
                                    timer["name"]
                                        .as_str()
                                        .unwrap_or("Scheduled task")
                                        .to_owned(),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(timer_description(&timer)),
                                ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("toggle-{timer_id}")))
                                .label(self.tr(if enabled { "Pause" } else { "Enable" }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.api(
                                        "POST",
                                        format!("/timers/{toggle_id}/pause"),
                                        Some(json!({"enabled":!enabled})),
                                        "Schedule saved",
                                    );
                                    this.work_checked =
                                        std::time::Instant::now() - Duration::from_secs(30);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("edit-{timer_id}")))
                                .label(self.tr("Edit"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_timer(edit.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("remove-{timer_id}")))
                                .label(self.tr("Remove"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.api(
                                        "DELETE",
                                        format!("/timers/{timer_id}"),
                                        None,
                                        "Schedule removed",
                                    );
                                    this.work_checked =
                                        std::time::Instant::now() - Duration::from_secs(30);
                                    cx.notify();
                                })),
                        ),
                );
            }
            let remove_id = self.selected.clone();
            body = body.child(self.btn(
                "remove-project",
                "Remove project…",
                move |this, window, cx| {
                    let answer = window.prompt(
                        PromptLevel::Warning,
                        &this.tr("Remove this project?"),
                        Some(&this.tr("Stop its session first. Project files remain on disk.")),
                        &[this.tr("Cancel").as_str(), this.tr("Remove").as_str()],
                        cx,
                    );
                    let project = remove_id.clone();
                    cx.spawn(async move |entity, cx| {
                        if answer.await.ok() == Some(1) {
                            let _ = entity.update(cx, |this, _| {
                                this.release_terminal();
                                this.api(
                                    "DELETE",
                                    format!("/projects/{project}"),
                                    None,
                                    "Project removed",
                                );
                            });
                        }
                    })
                    .detach();
                },
                cx,
            ));
        }
        body.into_any_element()
    }
    fn task_editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut kinds = div().flex().flex_wrap().gap_2();
        for (index, label) in ["Run now", "Once", "Interval", "Cron"]
            .into_iter()
            .enumerate()
        {
            kinds = kinds.child(
                Button::new(("task-kind", index))
                    .label(self.tr(label))
                    .selected(self.task_kind == index)
                    .disabled(self.editing_timer.is_some() && index == 0)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.task_kind = index;
                        cx.notify();
                    })),
            );
        }
        div()
            .p_3()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(LINE))
            .rounded_lg()
            .flex()
            .flex_col()
            .gap_3()
            .child(kinds)
            .child(self.field("Task name", &self.task_title))
            .child(div().h(px(100.)).child(Input::new(&self.task_prompt)))
            .when(self.task_kind > 0, |form| {
                form.child(self.field(
                    match self.task_kind {
                        1 => "Start time (RFC 3339)",
                        2 => "Repeat every (seconds)",
                        _ => "Cron expression (five fields)",
                    },
                    &self.schedule,
                ))
                .child(self.field("Time zone", &self.timezone))
            })
            .child(self.field("Reply destination (optional)", &self.recipient))
            .child(
                self.btn(
                    "save-task",
                    if self.editing_timer.is_some() {
                        "Save schedule"
                    } else {
                        "Create task"
                    },
                    |this, _, cx| this.save_task(cx),
                    cx,
                )
                .primary(),
            )
            .into_any_element()
    }
    fn save_task(&mut self, cx: &mut Context<Self>) {
        let title = self.task_title.read(cx).value().to_string();
        let instructions = self.task_prompt.read(cx).value().to_string();
        if title.trim().is_empty() || instructions.trim().is_empty() {
            self.notice = "Enter a task name and instructions".into();
            self.error = true;
            return;
        }
        let to = self.recipient.read(cx).value().to_string();
        let destination = (!to.trim().is_empty()).then_some(to);
        if self.task_kind == 0 {
            self.api(
                "POST",
                format!("/projects/{}/tasks", self.selected),
                Some(json!({"title":title,"instructions":instructions,"to":destination})),
                "Task queued",
            );
        } else {
            let spec = self.schedule.read(cx).value().to_string();
            let mut body = json!({"name":title,"prompt":instructions,"kind":(["","once","interval","cron"][self.task_kind]),"timezone":self.timezone.read(cx).value().to_string(),"to":destination});
            match self.task_kind {
                1 => {
                    body["at"] = json!(spec);
                }
                2 => {
                    let Ok(seconds) = spec.trim().parse::<u64>() else {
                        self.error = true;
                        self.notice = "Enter a positive interval in seconds".into();
                        return;
                    };
                    body["interval_seconds"] = json!(seconds);
                }
                _ => {
                    body["cron"] = json!(spec);
                }
            }
            if let Some(id) = self.editing_timer.clone() {
                self.api("PUT", format!("/timers/{id}"), Some(body), "Schedule saved");
            } else {
                self.api(
                    "POST",
                    format!("/projects/{}/timers", self.selected),
                    Some(body),
                    "Schedule created",
                );
            }
        }
        self.task_form = false;
        self.editing_timer = None;
        self.work_checked = std::time::Instant::now() - Duration::from_secs(30);
    }
    fn edit_timer(&mut self, timer: Value, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_timer = timer["id"].as_str().map(str::to_owned);
        self.task_kind = match timer["kind"].as_str().unwrap_or("") {
            "once" => 1,
            "interval" => 2,
            _ => 3,
        };
        self.task_title.update(cx, |input, cx| {
            input.set_value(timer["name"].as_str().unwrap_or("").to_owned(), window, cx)
        });
        self.task_prompt.update(cx, |input, cx| {
            input.set_value(
                timer["prompt"].as_str().unwrap_or("").to_owned(),
                window,
                cx,
            )
        });
        self.schedule.update(cx, |input, cx| {
            input.set_value(
                match self.task_kind {
                    1 => timer_time(&timer["at"]),
                    2 => timer["interval_seconds"].to_string(),
                    _ => timer["cron"].as_str().unwrap_or("").to_owned(),
                },
                window,
                cx,
            )
        });
        self.timezone.update(cx, |input, cx| {
            input.set_value(
                timer["timezone"].as_str().unwrap_or("UTC").to_owned(),
                window,
                cx,
            )
        });
        self.recipient.update(cx, |input, cx| {
            input.set_value(timer["to"].as_str().unwrap_or("").to_owned(), window, cx)
        });
        self.task_form = true;
    }
    fn execution_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let control = self
            .terminal
            .as_ref()
            .is_some_and(|bridge| bridge.controlling);
        let mut header = div()
            .h(px(52.))
            .flex_shrink_0()
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .bg(rgb(0x202a35))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xe8eeeb))
                    .child(self.tr(if control {
                        "You are in control"
                    } else {
                        "Live terminal"
                    })),
            );
        if let Some(bridge) = &self.terminal {
            let project = bridge.descriptor.project_id.clone();
            let generation = bridge.descriptor.generation.clone();
            header = header.child(
                Button::new("terminal-control")
                    .label(self.tr(if control {
                        "Release control"
                    } else {
                        "Take control"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.takeover_requested = !control;
                        let _ = this.backend.tx.send(CommandRequest::Takeover {
                            project: project.clone(),
                            generation: generation.clone(),
                            active: !control,
                        });
                        cx.notify();
                    })),
            );
        }
        let mut panel = div()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e1e))
            .child(header);
        if let Some(bridge) = &self.terminal {
            panel = panel.child(div().flex_1().min_h_0().child(bridge.view.clone()));
            if !bridge.notice.is_empty() {
                panel = panel.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_xs()
                        .text_color(rgb(0xffcf89))
                        .child(self.tr(&bridge.notice)),
                );
            }
            if !control {
                panel = panel.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_xs()
                        .text_color(rgb(0xa7b5c2))
                        .child(self.tr("Viewing only · Take control to type or scroll history")),
                );
            }
        } else {
            let lifecycle = self.state["projects"]
                .as_array()
                .and_then(|items| items.iter().find(|project| project["id"] == self.selected))
                .and_then(|project| project["lifecycle"].as_str())
                .unwrap_or("");
            let message = if self.selected.is_empty() {
                "Choose a project to view its agent"
            } else if matches!(lifecycle, "starting" | "connecting") {
                "Connecting to the live terminal…"
            } else if lifecycle == "stopped" {
                "Project is stopped. Start it to run tasks."
            } else if matches!(lifecycle, "needs_attention" | "error") {
                "Agent needs attention. Review the project notice."
            } else {
                "Agent is sleeping. Tasks and schedules will wake it automatically."
            };
            panel = panel.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_8()
                    .text_color(rgb(0xa7b5c2))
                    .child(self.tr(message)),
            );
        }
        if !self.terminal_error.is_empty() {
            panel = panel.child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(0xffcf89))
                    .child(self.terminal_error.clone()),
            );
        }
        panel
            .child(
                Button::new("reconnect-terminal")
                    .label(self.tr("Reconnect"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.release_terminal();
                        this.terminal_key.clear();
                        this.sync_terminal(cx);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
    fn workspace_layout(&mut self, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        if self.setup.terminal_hidden {
            return div()
                .id("project-controls")
                .size_full()
                .overflow_y_scroll()
                .p_6()
                .child(content)
                .into_any_element();
        }
        let terminal = self.execution_panel(cx);
        h_resizable("workspace-split")
            .child(
                resizable_panel().size(px(360.)).child(
                    div()
                        .id("project-controls")
                        .size_full()
                        .overflow_y_scroll()
                        .p_5()
                        .child(content),
                ),
            )
            .child(resizable_panel().child(terminal))
            .into_any_element()
    }
    fn connection_pill(&self, name: &str, status: &str) -> Div {
        let status = connection_status(status);
        div()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(rgb(0xe8eeeb))
            .text_sm()
            .child(format!("{} · {}", self.tr(name), self.tr(status)))
    }
    fn environment_page(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.title(
                "ENVIRONMENT",
                "Local environment",
                "Installed command-line tools. Check sign-in inside each CLI.",
            ))
            .children(self.environment.iter().map(|(name, status)| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_sm()
                    .child(self.tr(name))
                    .child(div().text_color(rgb(MUTED)).child(self.tr(status)))
            }))
            .child(self.btn(
                "rescan",
                "Check environment again",
                |this, _, _| {
                    let _ = this.backend.tx.send(CommandRequest::Environment);
                },
                cx,
            ))
            .into_any_element()
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
        let message = match phase {
            "checking" => self.tr("Checking for updates..."),
            "available" => format!(
                "{} {}",
                self.tr("New version available:"),
                self.update_status["latest"].as_str().unwrap_or("")
            ),
            "downloading" | "installing" => format!(
                "{} {}%",
                self.tr("Downloading and verifying update"),
                self.update_status["progress"].as_u64().unwrap_or(0)
            ),
            "installed" => self.tr("Update installed. Restart the desktop to apply."),
            "current" => self.tr("You have the latest version on this channel"),
            "error" => format!(
                "{}\n{}",
                self.tr("Update failed"),
                self.update_status["message"].as_str().unwrap_or("")
            ),
            _ => self.tr("Automatic checks run every six hours when enabled."),
        };
        div().flex().flex_col().gap_5()
            .child(self.title("UPDATES", "App Updates", "Signed updates download in the background and take effect next time you open the app. Project files, bot settings, and active sessions stay intact."))
            .child(self.card("Current version", &format!("Dauerbot {} · {}", env!("CARGO_PKG_VERSION"), self.tr(if channel == "preview" { "Preview channel" } else { "Stable channel" }))))
            .child(self.card("Update status", &message))
            .child(div().flex().gap_3().flex_wrap()
                .child(Button::new("update-check").label(self.tr("Check for updates")).disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Check); cx.notify();
                })))
                .when(phase == "available" || phase == "error", |row| row.child(Button::new("update-install").label(self.tr("Download and install")).disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Install); cx.notify();
                }))))
                .when(phase == "installed", |row| row.child(Button::new("update-restart").label(self.tr("Restart desktop now")).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Restart); cx.notify();
                }))))
                .when(self.update_status["rollback_available"].as_bool().unwrap_or(false), |row| row.child(Button::new("update-rollback").label(self.tr("Roll back")).disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.updates.send(updater::Action::Rollback); cx.notify();
                }))))
            )
            .child(div().flex().gap_3()
                .child(Button::new("update-auto").label(self.tr(if automatic { "Disable automatic updates" } else { "Enable automatic updates" })).on_click(cx.listener(move |this, _, _, cx| {
                    let channel = this.update_status["preferences"]["channel"].as_str().unwrap_or("stable").to_owned();
                    let _ = this.backend.tx.send(CommandRequest::UpdatePreferences { automatic: !automatic, channel }); cx.notify();
                })))
                .child(Button::new("update-channel").label(self.tr(if channel == "stable" { "Switch to preview channel" } else { "Switch to stable channel" })).on_click(cx.listener(|this, _, _, cx| {
                    let channel = if this.update_status["preferences"]["channel"] == "preview" { "stable" } else { "preview" }.into();
                    let automatic = this.update_status["preferences"]["automatic"].as_bool().unwrap_or(true);
                    let _ = this.backend.tx.send(CommandRequest::UpdatePreferences { automatic, channel }); cx.notify();
                })))
            )
            .child(self.card("Automatic updates", "Checks run every six hours by default. Manual updates remain available when automatic updates are off. Stable excludes prereleases; preview includes them. Installation requires a verified signature from the same developer."))
            .when(!self.update_status["notes"].as_str().unwrap_or("").is_empty(), |page| page.child(self.card("Release notes", self.update_status["notes"].as_str().unwrap_or(""))))
    }
}
impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.setup.page;
        let mut sidebar = div()
            .w(px(206.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf1f3f2))
            .border_r_1()
            .border_color(rgb(LINE))
            .px_4()
            .py_8()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .mb_1()
                    .child(self.tr("Dauerbot")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .mb_8()
                    .child(self.tr("Project & Bot Manager")),
            );
        for (index, label) in [
            (6, "Workspace"),
            (5, "Add project"),
            (1, "Feishu bot setup"),
            (4, "macOS permissions"),
            (8, "Environment"),
        ] {
            if index == 1 {
                sidebar = sidebar.child(div().mt_4().mb_2().border_t_1().border_color(rgb(LINE)));
            }
            let active = page == index || (index == 1 && matches!(page, 0..=3));
            sidebar = sidebar.child(
                div()
                    .id(("navigation", index))
                    .px_3()
                    .py_3()
                    .rounded_lg()
                    .text_sm()
                    .cursor_pointer()
                    .bg(rgb(if active { 0xe0ebe6 } else { 0xf1f3f2 }))
                    .text_color(rgb(if active { ACCENT } else { MUTED }))
                    .child(self.tr(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.go(index);
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
                    .rounded_lg()
                    .text_sm()
                    .cursor_pointer()
                    .text_color(rgb(ACCENT))
                    .child(self.tr(if self.update_status["phase"] == "installed" {
                        "App Updates · restart to apply"
                    } else {
                        "App Updates"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.go(7);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .px_3()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
            );
        let content = match page {
            0 => self.welcome(cx),
            1 => self.bot(cx),
            2 => self.credentials(cx),
            3 => self.events(cx),
            4 => self.permissions(cx),
            5 => self.project_form(cx),
            6 => self.workspace(cx),
            7 => self.updates_page(cx).into_any_element(),
            _ => self.environment_page(cx),
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
                            .child(self.tr(STEPS[page]))
                            .child(Button::new("language").label(if self.setup.language == Language::En { "简体中文" } else { "English" }).on_click(cx.listener(|this, _, window, cx| {
                                this.setup.language = this.setup.language.other();
                                this.go(this.setup.page);
                                this.name.update(cx, |input, cx| input.set_placeholder(this.setup.language.text("For example: personal website"), window, cx));
                                this.path.update(cx, |input, cx| input.set_placeholder(this.setup.language.text("Choose the project folder"), window, cx));
                                this.secret.update(cx, |input, cx| input.set_placeholder(this.setup.language.text("Stored only on this Mac"), window, cx));
                                for (input, placeholder) in [(&this.task_title, "Task name"), (&this.task_prompt, "What should the agent do?"), (&this.schedule, "Schedule"), (&this.recipient, "Reply destination (optional)"), (&this.decision_answer, "Your answer"), (&this.job_reason, "What did you verify?")] {
                                    input.update(cx, |input, cx| input.set_placeholder(this.setup.language.text(placeholder), window, cx));
                                }
                                set_menus(this.setup.language, cx);
                                cx.notify();
                            }))),
                    )
                    .child(if page == 6 {
                        self.workspace_layout(content, cx)
                    } else {
                        div()
                            .id(("content-scroll", page))
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
                                                .child(self.tr("Independent instance · This window uses separate data from your everyday workspace."))
                                                .child(div().mt_2().text_xs().child(
                                                    self.backend.dir.display().to_string(),
                                                )),
                                        )
                                    })
                                    .when(matches!(page, 1..=3), |view| {
                                        let mut steps = div().flex().flex_wrap().gap_2().mb_6();
                                        for (index, label) in [(1, "Create bot"), (2, "Connect bot"), (3, "Permissions & events")] {
                                            steps = steps.child(Button::new(("setup-stage", index)).label(format!("{}  {}", index, self.tr(label))).selected(page == index).on_click(cx.listener(move |this, _, _, cx| { this.go(index); cx.notify(); })));
                                        }
                                        view.child(steps)
                                    })
                                    .child(content),
                            )
                            .into_any_element()
                    })
                    .child(
                        div()
                            .px_8()
                            .py_3()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .text_xs()
                            .text_color(rgb(if self.error { 0xb44343 } else { MUTED }))
                            .child(if let Some(reason) = &self.connection_error { format!("{}: {reason}", self.tr("Local service connection failed")) } else { self.tr(&self.notice) }),
                    ),
            )
    }
}
fn records(value: &Value, field: &str) -> Vec<Value> {
    value
        .as_array()
        .or_else(|| value[field].as_array())
        .cloned()
        .unwrap_or_default()
}
fn timer_time(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            value
                .as_i64()
                .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                .map(|time| time.to_rfc3339())
        })
        .unwrap_or_default()
}
fn timer_description(timer: &Value) -> String {
    let timezone = timer["timezone"].as_str().unwrap_or("UTC");
    match timer["kind"].as_str().unwrap_or("") {
        "once" => format!("{} · {timezone}", timer_time(&timer["at"])),
        "interval" => format!("{} s · {timezone}", timer["interval_seconds"]),
        _ => format!("{} · {timezone}", timer["cron"].as_str().unwrap_or("")),
    }
}
fn connection_status(status: &str) -> &str {
    match status {
        "connected" | "receiving" => "Connected",
        "connecting" => "Connecting",
        "awaiting_events" => "Connected · Waiting for messages",
        "unconfigured" | "not_configured" => "Not configured",
        "disconnected" | "" => "Disconnected",
        other => other,
    }
}
fn agent_name(agent: &str) -> &'static str {
    match agent {
        "codex" => "Codex",
        "claude" => "Claude Code",
        "pi" => "Pi",
        "agy" => "Antigravity",
        _ => "Agent",
    }
}
fn set_menus(language: Language, cx: &mut App) {
    cx.set_menus(vec![Menu {
        name: "Dauerbot".into(),
        items: vec![
            MenuItem::action(language.text("App Updates…"), ShowUpdates),
            MenuItem::action(language.text("Quit Dauerbot"), Quit),
        ],
    }]);
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
        let language = std::fs::read(backend.dir.join("desktop.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Setup>(&bytes).ok())
            .map(|setup| setup.language)
            .unwrap_or_default();
        set_menus(language, cx);
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
                let quit_view = view.clone();
                cx.on_action(move |_: &Quit, cx| {
                    quit_view.update(cx, |this, cx| this.quit(cx));
                });
                let close_view = view.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    let _ = close_view.update(cx, |this, cx| this.quit(cx));
                    false
                });
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

#[cfg(test)]
mod preference_tests {
    use super::{Language, Setup};

    #[test]
    fn existing_preferences_keep_setup_progress_and_language_round_trips() {
        let old = r#"{"page":3,"events_confirmed":true,"published":false}"#;
        let mut setup: Setup = serde_json::from_str(old).unwrap();
        assert_eq!(setup.language, Language::En);
        assert_eq!(setup.page, 3);
        assert!(setup.events_confirmed);
        setup.language = Language::Zh;
        let encoded = serde_json::to_vec(&setup).unwrap();
        let reloaded: Setup = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(reloaded.language, Language::Zh);
        assert_eq!(reloaded.page, 3);
        assert!(reloaded.events_confirmed);
    }
}
