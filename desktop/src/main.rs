#![allow(non_snake_case)]
mod access;
mod activation;
mod agent_tree;
mod backend;
mod finder;
mod locale;
mod manager_context;
mod manager_settings;
#[cfg(target_os = "macos")]
mod menubar;
mod model_picker;
mod project_tools;
mod sidebar;
mod terminal;
mod ui_theme;
mod updater;

use backend::{Backend, CommandRequest, Reply};
use gpui::{prelude::*, *};
use gpui_component::{
    button::*,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel},
    *,
};
use locale::Language;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

fn control_label(text: impl Into<SharedString>) -> Div {
    let text: SharedString = text.into();
    div()
        .min_w_0()
        .text_size(px(13.))
        .line_height(relative(1.))
        .child(text)
}

#[derive(Clone, Copy, Debug)]
struct WorkspaceGeometry {
    sidebar_hidden: bool,
    available: Pixels,
    center: Pixels,
    sidebar: Pixels,
}
fn workspace_geometry(
    width: Pixels,
    terminal_open: bool,
    sidebar_preference: bool,
    sidebar_width: sidebar::SidebarWidth,
) -> WorkspaceGeometry {
    // Match the reference's compact breakpoint without changing the saved preference.
    let sidebar_hidden = sidebar_preference || terminal_open && width < px(1024.);
    let sidebar = px(sidebar_width.fitted(f32::from(width)));
    let available = width - if sidebar_hidden { px(0.) } else { sidebar };
    let center = px((f32::from(available) * 0.45).clamp(320., 520.));
    WorkspaceGeometry {
        sidebar_hidden,
        available,
        center,
        sidebar,
    }
}

const STEPS: [&str; 9] = [
    "Welcome",
    "Create a Feishu bot",
    "Connect your bot",
    "Message events",
    "Local permissions",
    "Add your first project",
    "Workspace",
    "App Updates",
    "Settings",
];
actions!(
    bridge,
    [
        Quit,
        ShowUpdates,
        ShowSettings,
        FocusProjects,
        NextProject,
        PreviousProject,
        AddProject,
        ToggleTerminal,
        ToggleSidebar,
        QuickFind
    ]
);

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Setup {
    page: usize,
    bots: std::collections::HashMap<String, BotSetup>,
    language: Language,
    terminal_hidden: bool,
    sidebar_hidden: bool,
    sidebar_width: sidebar::SidebarWidth,
    appearance: ui_theme::Appearance,
}
impl Setup {
    fn prepare_startup(&mut self) {
        self.page = self.page.min(8);
        // Debugging is opt-in each time the desktop starts.
        self.terminal_hidden = true;
    }
}

#[derive(Clone)]
struct SidebarDrag {
    offset: std::rc::Rc<std::cell::Cell<Pixels>>,
}
impl Render for SidebarDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct BotSetup {
    events_confirmed: bool,
    published: bool,
}
struct Desktop {
    focus_handle: FocusHandle,
    backend: Backend,
    independent_data: bool,
    setup: Setup,
    state: Value,
    selected: String,
    picked_path: String,
    name: Entity<InputState>,
    project_search: Entity<InputState>,
    finder: Entity<finder::QuickFinder>,
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
    credentials_project: String,
    credentials_app_id: String,
    connection_error: Option<String>,
    pending_secret: String,
    update_status: Value,
    terminal: Option<terminal::Bridge>,
    terminal_key: String,
    terminal_error: String,
    timers: Value,
    project_tools: Entity<project_tools::ProjectTools>,
    model_picker: Entity<model_picker::ModelPicker>,
    manager_settings: Entity<manager_settings::ManagerSettings>,
    manager_context: Entity<manager_context::ManagerContext>,
    work_checked: std::time::Instant,
    quitting: bool,
    takeover_requested: bool,
    folded_agents: std::collections::HashMap<String, std::collections::HashSet<String>>,
    activation_frozen: bool,
    restored: bool,
    sidebar_resizing: bool,
    system_appearance: WindowAppearance,
    _appearance_subscription: Subscription,
}
impl Desktop {
    fn tr(&self, text: &str) -> String {
        self.setup.language.text(text)
    }
    fn palette(&self) -> ui_theme::Palette {
        self.setup.appearance.resolve(Some(self.system_appearance)).palette()
    }
    fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        let independent_data = independent_data_dir(&backend.dir);
        let mut setup: Setup = std::fs::read(backend.dir.join("desktop.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        setup.prepare_startup();
        let system_appearance = window.appearance();
        ui_theme::apply_in_window(setup.appearance, window, cx);
        let appearance_subscription = cx.observe_window_appearance(window, |this, window, cx| {
            let previous = this.setup.appearance.resolve(Some(this.system_appearance));
            this.system_appearance = window.appearance();
            if this.setup.appearance == ui_theme::Appearance::System
                && previous != this.setup.appearance.resolve(Some(this.system_appearance))
            {
                // Reapply our palette after the native mode; changing mode alone
                // restores gpui-component's defaults instead of our semantic colors.
                ui_theme::apply_in_window(this.setup.appearance, window, cx);
                cx.notify();
            }
        });
        let mut input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(setup.language.text(placeholder)))
        };
        let name = input("For example: personal website", cx);
        let project_search = input("Search projects · ⌘F", cx);
        cx.subscribe(&project_search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.move_project(0, cx);
                this.go(6);
            }
            cx.notify();
        })
        .detach();
        let path = input("Leave empty to create a private project folder", cx);
        let app_id = input("cli_…", cx);
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(setup.language.text("Stored only on this Mac"))
                .masked(true)
        });
        let finder = cx.new(|cx| finder::QuickFinder::new(window, cx));
        cx.subscribe_in(
            &finder,
            window,
            |this, _, event: &finder::Navigate, window, cx| {
                if !this.finder_allowed() || !finder::target_available(&event.0, &this.state) {
                    return;
                }
                match &event.0 {
                    finder::Target::Project(id) => {
                        this.select_project(id.clone(), cx);
                        this.go(6);
                    }
                    finder::Target::Session { project, .. } => {
                        this.go(6);
                        this.select_project(project.clone(), cx);
                        this.setup.terminal_hidden = false;
                        this.sync_terminal(cx);
                        this.go(6);
                    }
                    finder::Target::Page(page) => this.go(*page),
                }
                this.focus_handle.focus(window);
                cx.notify();
            },
        )
        .detach();
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
        let project_tools = cx.new(|cx| project_tools::ProjectTools::new(backend.dir.clone(), cx));
        let model_picker =
            cx.new(|cx| model_picker::ModelPicker::new(backend.dir.clone(), window, cx));
        let manager_settings =
            cx.new(|cx| manager_settings::ManagerSettings::new(backend.dir.clone(), window, cx));
        let manager_context = cx.new(|cx| manager_context::ManagerContext::new(backend.dir.clone(), cx));
        Self {
            focus_handle,
            backend,
            independent_data,
            setup,
            state: Value::Null,
            selected: String::new(),
            picked_path: String::new(),
            name,
            project_search,
            finder,
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
            credentials_project: String::new(),
            credentials_app_id: String::new(),
            connection_error: None,
            pending_secret: String::new(),
            update_status: Value::Null,
            terminal: None,
            terminal_key: String::new(),
            terminal_error: String::new(),
            timers: Value::Null,
            project_tools,
            model_picker,
            manager_settings,
            manager_context,
            work_checked: std::time::Instant::now() - Duration::from_secs(30),
            quitting: false,
            takeover_requested: false,
            folded_agents: std::collections::HashMap::new(),
            activation_frozen: false,
            restored: false,
            sidebar_resizing: false,
            system_appearance,
            _appearance_subscription: appearance_subscription,
        }
    }
    fn finder_allowed(&self) -> bool {
        finder::can_open(
            self.takeover_requested,
            self.terminal
                .as_ref()
                .is_some_and(|bridge| bridge.controlling),
            self.activation_frozen,
            self.quitting,
        )
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
                Reply::Work { project, timers } => {
                    if project == self.selected {
                        self.timers = timers;
                    }
                }
                Reply::Update(status) => self.update_status = status,
                Reply::Restart => self.quit(cx),
                Reply::ActivationFreeze { reply, freeze } => {
                    let snapshot = if self.takeover_requested
                        || self.terminal.as_ref().is_some_and(|t| t.controlling)
                    {
                        Err(anyhow::anyhow!(
                            "The update will finish after terminal input is released"
                        ))
                    } else {
                        self.activation_frozen = freeze;
                        self.backend
                            .activation_pause
                            .store(freeze, std::sync::atomic::Ordering::Release);
                        Ok(self.activation_snapshot(window, cx))
                    };
                    let _ = reply.send(snapshot);
                }
                Reply::ActivationResume => {
                    self.activation_frozen = false;
                    self.backend
                        .activation_pause
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                Reply::State(value) => {
                    if self.state.is_null() {
                        self.notice =
                            "Local service ready. Connect a bot or create a local project.".into();
                    }
                    if !self.restored
                        && self.state.is_null()
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
                    let current_app_id = value["projects"]
                        .as_array()
                        .and_then(|projects| projects.iter().find(|p| p["id"] == self.selected))
                        .and_then(|project| project["bot"]["app_id"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    if self.credentials_project != self.selected
                        || (self.credentials_app_id != current_app_id
                            && self.app_id.read(cx).value().as_ref() == self.credentials_app_id
                            && self.secret.read(cx).value().is_empty())
                    {
                        self.app_id.update(cx, |input, cx| {
                            input.set_value(current_app_id.clone(), window, cx)
                        });
                        self.secret
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        self.pending_secret.clear();
                        self.credentials_checked = false;
                        self.credentials_project = self.selected.clone();
                        self.credentials_app_id = current_app_id;
                    }
                    self.state = value;
                    if crate::backend::service_healthy(&self.state)
                        && let Some(ready) = &self.backend.candidate_ready
                    {
                        ready.store(true, std::sync::atomic::Ordering::Release);
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
                    if path == format!("/projects/{}/bot", self.selected)
                        && self.secret.read(cx).value().as_ref() == self.pending_secret
                    {
                        self.secret
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        self.pending_secret.clear();
                        self.credentials_app_id = value["app_id"].as_str().unwrap_or("").to_owned();
                    }
                    if !message.is_empty() {
                        self.notice = message;
                    }
                    if path == format!("/projects/{}/bot/check", self.selected) {
                        self.credentials_checked = true;
                    }
                    if path == "/projects" {
                        self.selected = value["id"].as_str().unwrap_or_default().into();
                        self.credentials_project.clear();
                        self.go(2);
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
    fn activation_snapshot(&self, window: &Window, cx: &Context<Self>) -> Value {
        #[cfg(target_os = "macos")]
        let visible = menubar::window_visible(window);
        #[cfg(target_os = "macos")]
        let frame = menubar::window_frame(window);
        #[cfg(not(target_os = "macos"))]
        let frame: Option<[f64; 4]> = None;
        #[cfg(not(target_os = "macos"))]
        let visible = true;
        serde_json::json!({"setup":self.setup,"selected":self.selected,"picked_path":self.picked_path,
            "name":self.name.read(cx).value().to_string(),"path":self.path.read(cx).value().to_string(),
            "app_id":self.app_id.read(cx).value().to_string(),"secret":self.secret.read(cx).value().to_string(),
            "agent":self.agent,"credentials_project":self.credentials_project,"credentials_app_id":self.credentials_app_id,
            "pending_secret":self.pending_secret,"credentials_checked":self.credentials_checked,"visible":visible,"frame":frame})
    }
    fn restore_activation(&mut self, snapshot: Value, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Ok(frame) = serde_json::from_value::<[f64; 4]>(snapshot["frame"].clone()) {
            menubar::restore_frame(window, frame);
        }

        if let Ok(setup) = serde_json::from_value(snapshot["setup"].clone()) {
            self.setup = setup;
            ui_theme::apply_in_window(self.setup.appearance, window, cx);
        }
        self.selected = snapshot["selected"].as_str().unwrap_or("").into();
        self.picked_path = snapshot["picked_path"].as_str().unwrap_or("").into();
        self.agent = snapshot["agent"].as_u64().unwrap_or(0) as usize;
        self.credentials_project = snapshot["credentials_project"]
            .as_str()
            .unwrap_or("")
            .into();
        self.credentials_app_id = snapshot["credentials_app_id"].as_str().unwrap_or("").into();
        self.pending_secret = snapshot["pending_secret"].as_str().unwrap_or("").into();
        self.credentials_checked = snapshot["credentials_checked"].as_bool().unwrap_or(false);
        for (input, key) in [
            (&self.name, "name"),
            (&self.path, "path"),
            (&self.app_id, "app_id"),
            (&self.secret, "secret"),
        ] {
            input.update(cx, |input, cx| {
                input.set_value(snapshot[key].as_str().unwrap_or("").to_owned(), window, cx)
            });
        }
        self.restored = true;
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
        self.finish_sidebar_resize();
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
        let preferences = self.backend.flush_preferences_before_exit();
        cx.spawn(async move |_, cx| {
            if let Some(pending) = pending {
                let _ = pending.recv_async().await;
            }
            let _ = preferences.recv_async().await;
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
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
    fn finish_sidebar_resize(&mut self) {
        if self.sidebar_resizing {
            self.sidebar_resizing = false;
            self.go(self.setup.page);
        }
    }
    fn toggle_sidebar(&mut self, window: &Window, cx: &mut Context<Self>) {
        let geometry = workspace_geometry(
            window.viewport_size().width,
            self.setup.page == 6 && !self.setup.terminal_hidden,
            self.setup.sidebar_hidden,
            self.setup.sidebar_width,
        );
        if geometry.sidebar_hidden {
            self.setup.sidebar_hidden = false;
            if self.setup.page == 6
                && !self.setup.terminal_hidden
                && window.viewport_size().width < px(1024.)
            {
                self.setup.terminal_hidden = true;
                self.release_terminal();
                self.terminal_key.clear();
            }
        } else {
            self.setup.sidebar_hidden = true;
        }
        self.go(self.setup.page);
        cx.notify();
    }
    fn visible_project_ids(&self, cx: &App) -> Vec<String> {
        let query = self.project_search.read(cx).value().trim().to_lowercase();
        self.state["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|project| {
                query.is_empty()
                    || project["name"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query)
                    || agent_name(project["agent"].as_str().unwrap_or(""))
                        .to_lowercase()
                        .contains(&query)
            })
            .filter_map(|project| project["id"].as_str().map(str::to_owned))
            .collect()
    }
    fn move_project(&mut self, direction: isize, cx: &mut Context<Self>) {
        let ids = self.visible_project_ids(cx);
        if ids.is_empty() {
            return;
        }
        let index = if direction == 0 {
            0
        } else {
            match ids.iter().position(|id| id == &self.selected) {
                Some(current) => {
                    (current as isize + direction).rem_euclid(ids.len() as isize) as usize
                }
                None if direction < 0 => ids.len() - 1,
                None => 0,
            }
        };
        self.select_project(ids[index].clone(), cx);
        self.go(6);
    }
    fn select_project(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected != id {
            self.release_terminal();
            self.selected = id;
            self.terminal_key.clear();
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
            "Retry this input?"
        } else {
            "Dismiss this input?"
        };
        let description = if retry {
            "The agent may already have changed files or sent a reply. Continue after reviewing its work."
        } else {
            "The input will stop waiting. Existing files and sent replies remain."
        };
        let action = if retry {
            "Retry input"
        } else {
            "Dismiss input"
        };
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
                            "Input queued"
                        } else {
                            "Input dismissed"
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
            self.backend.save_preferences(bytes);
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
        id: impl Into<ElementId>,
        label: &'static str,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .small()
            .text_size(px(13.))
            .child(control_label(self.tr(label)))
            .disabled(self.busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.notify();
            }))
    }
    fn title(&self, _kicker: &str, title: &str, description: &str) -> Div {
        let palette = self.palette();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .mb_4()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.tr(title)),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(palette.muted))
                    .line_height(relative(1.5))
                    .child(self.tr(description)),
            )
    }
    fn card(&self, title: &str, text: &str) -> Div {
        self.card_detail(title, &self.tr(text))
    }
    fn card_detail(&self, title: &str, text: &str) -> Div {
        let palette = self.palette();
        div()
            .min_w_0()
            .p_5()
            .rounded_xl()
            .bg(rgb(palette.surface))
            .border_1()
            .border_color(rgb(palette.line))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(13.))
                    .child(self.tr(title)),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(palette.muted))
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
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(self.tr(label)),
            )
            .child(Input::new(input))
    }
    fn welcome(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(self.title("YOUR AGENTS, A LITTLE CLOSER", "Keep working.\nTake the conversation with you.", "Talk to your project's main session in Feishu, then continue on your Mac. Connect your first bot to get started."))
            .child(div().flex().gap_4()
                .child(self.card("01  Choose a project", "Choose a project folder and an installed Agent CLI.").flex_1())
                .child(self.card("02  Connect its bot", "Connect a dedicated Feishu bot to this project, or continue locally.").flex_1())
                .child(self.card("03  Continue the conversation", "Messages, scheduled runs, and files reach the same main session.").flex_1()))
            .child(self.card("You control access", "Choose the project folder yourself. Protected files may require additional access; ordinary projects do not need Full Disk Access, Accessibility, or Screen Recording."))
            .child(self.btn("begin", "Add your first project →", |this, _, _| this.go(5), cx).primary())
            .child(self.btn("bot-guide", "How to create a Feishu bot", |this, _, _| this.go(1), cx))
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
        let palette = self.palette();
        let projects = self.state["projects"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if projects.is_empty() {
            return div().flex().flex_col().gap_4()
                .child(self.title("FEISHU BOT", "Choose a project first", "Each project connects to its own bot. Create a project before adding bot credentials."))
                .child(self.btn("bot-add-project", "Add project →", |this, _, _| this.go(5), cx).primary())
                .into_any_element();
        }
        let mut choices = div().flex().flex_wrap().gap_2();
        for project in &projects {
            let id = project["id"].as_str().unwrap_or("").to_owned();
            choices = choices.child(
                Button::new(SharedString::from(format!("bot-project-{id}")))
                    .child(control_label(
                        project["name"].as_str().unwrap_or("Project").to_owned(),
                    ))
                    .selected(self.selected == id)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_project(id.clone(), cx)),
                    ),
            );
        }
        let selected = self.selected.clone();
        let status = self.state["runtime"]["bots"][&selected].clone();
        div().flex().flex_col().gap_4()
            .child(self.title("FEISHU BOT", "Connect this project's bot", "Every project has its own bot. Direct messages, bot mentions and verified topic replies reach its project."))
            .child(choices)
            .child(self.field("App ID", &self.app_id))
            .child(self.field("App Secret", &self.secret))
            .child(div().text_size(px(11.)).text_color(rgb(palette.muted)).child(self.tr("The secret stays masked. Leave it blank to keep this bot's saved secret. A new App ID needs its own secret. Credentials are stored privately on this Mac.")))
            .child(self.btn("save-credentials", "Save and connect", |this, _, cx| {
                let id = this.app_id.read(cx).value().to_string();
                if !id.starts_with("cli_") { this.notice = "Enter an App ID starting with cli_".into(); this.error = true; return; }
                let secret = this.secret.read(cx).value().to_string();
                this.pending_secret = secret.clone();
                this.credentials_checked = false;
                this.api("PUT", format!("/projects/{}/bot",this.selected), Some(json!({"app_id":id,"app_secret":secret})), "Bot connected to this project. Validate its credentials to continue.");
            }, cx).primary().disabled(self.credentials_project != self.selected || self.busy))
            .child(self.btn("check-credentials", "Validate credentials", |this, _, _| this.api("POST", format!("/projects/{}/bot/check",this.selected), None, "App identity verified. Configure message events next."), cx))
            .child(self.card("Connection status", &format!("{} · {}", self.tr(connection_status(status["status"].as_str().unwrap_or("unconfigured"))), self.tr(status["error"].as_str().unwrap_or("")))))
            .child(self.btn("credentials-next", "Configure message permissions →", |this, _, _| this.go(3), cx).primary().disabled(!self.credentials_checked || self.busy))
            .child(self.btn("bot-local-only", "Use locally for now", |this, _, _| this.go(6), cx))
            .into_any_element()
    }
    fn events(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let app_id = self.state["projects"]
            .as_array()
            .and_then(|projects| projects.iter().find(|p| p["id"] == self.selected))
            .and_then(|p| p["bot"]["app_id"].as_str())
            .unwrap_or("");
        let key = format!("{}:{app_id}", self.selected);
        let confirmed = self.setup.bots.get(&key).cloned().unwrap_or_default();
        let event_key = key.clone();
        let release_key = key.clone();
        div().flex().flex_col().gap_4()
            .child(self.title("SETUP / 03", "Receive messages and send replies", "Complete these steps in the Feishu developer console. Manual confirmations are separate from automatic checks."))
            .child(self.card("1. Import app permissions", "In Permissions, import the JSON below to send bot messages, receive direct messages and group mentions, read message resources, upload files, and read group context."))
            .child(self.btn("copy-scopes-full", "Copy full permission JSON (all group messages)", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(true)).unwrap())), cx).primary())
            .child(self.btn("copy-scopes", "Copy basic permission JSON (group mentions)", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(false)).unwrap())), cx))
            .child(self.card("2. Receive events over a long connection", "Under Events and Callbacks, choose long connections. Save valid credentials in Dauerbot and keep it running before saving the subscription. Add im.message.receive_v1 to receive messages."))
            .child(self.card("Reply in a topic without @mentions", "Use the Feishu Agent permission preset or the full JSON with im:message.group_msg to receive topic replies without mentioning the bot. Obtain approval and publish if the scope is not already effective. The basic set only receives mentions. Dauerbot admits known project discussions and ignores unrelated group traffic; an ambiguous shared topic needs an explicit bot mention."))
            .child(self.card("3. Publish the app and add the bot", "Create a version in Version Management and Release, choose its availability, and publish it. Wait for administrator approval if required. Add the bot to a group or use direct messages. Publish a new version after changing permissions or subscriptions."))
            .child(self.btn("confirmed-events", if confirmed.events_confirmed { "✓ Events and permissions confirmed manually" } else { "I configured permissions and event subscriptions" }, move |this, _, _| { let setup=this.setup.bots.entry(event_key.clone()).or_default(); setup.events_confirmed = !setup.events_confirmed; this.go(3); }, cx))
            .child(self.btn("confirmed-release", if confirmed.published { "✓ Publishing and bot access confirmed manually" } else { "I published the app and made the bot available" }, move |this, _, _| { let setup=this.setup.bots.entry(release_key.clone()).or_default(); setup.published = !setup.published; this.go(3); }, cx))
            .child(self.btn("events-next", "Go to workspace →", |this, _, _| this.go(6), cx).primary().disabled(app_id.is_empty() || !confirmed.events_confirmed || !confirmed.published))
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
        let palette = self.palette();
        let picked = self.picked_path.clone();
        let agents = ["Codex", "Claude Code", "Pi", "Antigravity"];
        let mut choices = div().flex().gap_2();
        for (index, name) in agents.into_iter().enumerate() {
            choices = choices.child(
                Button::new(("agent", index))
                    .child(control_label(name))
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
            .child(self.card("Private workspace", "By default, Dauerbot creates a folder in ~/.dauerbot/agents with project instructions. You can also choose an existing folder."))
            .child(self.field("Project folder (optional existing folder)", &self.path))
            .when(!picked.is_empty(), |v| v.child(div().text_size(px(13.)).text_color(rgb(palette.accent)).child(format!("{} {picked}", self.tr("Selected:")))))
            .child(self.btn("pick-project", "Choose a folder...", |this, window, cx| this.pick(false, window, cx), cx))
            .child(choices)
            .child(self.card("Connect a dedicated bot", "After creating this project, connect its own Feishu bot or continue locally. You never need to enter chat IDs."))
            .child(self.card("Your agent, inside Dauerbot", "Watch and operate the live terminal in this app. Closing the window leaves the Agent running."))
            .child(self.btn("create-project", "Create project →", |this, _, cx| {
                let manual = this.path.read(cx).value().to_string();
                let path = if manual.trim().is_empty() { this.picked_path.clone() } else { manual };
                let name = this.name.read(cx).value().to_string();
                if name.trim().is_empty() { this.notice = "Enter a project name".into(); this.error = true; return; }
                let agent = ["codex","claude","pi","agy"][this.agent];
                this.api("POST", "/projects".into(), Some(json!({"name":name,"path":path,"agent":agent,"chat_id":""})), "Project created. Connect its bot or continue locally.");
            }, cx).primary())
            .into_any_element()
    }
    fn project_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let projects = self.state["projects"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let selected = projects.iter().find(|p| p["id"] == self.selected).cloned();
        let mut body = div().flex().flex_col().gap_4();
        if let Some(project) = selected {
            let id = self.selected.clone();
            self.manager_settings.update(cx, |settings, cx| {
                settings.select(&project, self.setup.language, cx)
            });
            body = body.child(self.manager_settings.clone());
            self.manager_context.update(cx, |context, cx| context.select(&project, self.setup.language, cx));
            body = body.child(self.manager_context.clone());
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
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(project["path"].as_str().unwrap_or("").to_owned()),
                );
            let bound = project["bot"]["has_secret"] == true;
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(if bound { palette.accent } else { palette.muted }))
                            .child(self.tr(if bound {
                                "Bot connected"
                            } else {
                                "No bot connected"
                            })),
                    )
                    .child(self.btn(
                        "project-bot",
                        if bound { "Manage bot" } else { "Connect bot" },
                        |this, _, _| this.go(2),
                        cx,
                    )),
            );
            if !bound {
                body = body.child(div().text_size(px(11.)).text_color(rgb(palette.muted))
                    .child(self.tr("This project is local only. Connect a dedicated Feishu bot to receive messages.")));
            }
            if let Some(error) = self.state["runtime"]["bots"][&id]["error"]
                .as_str()
                .filter(|error| !error.is_empty())
            {
                body = body.child(self.card_detail("Bot connection", error));
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
            for event in work["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|event| {
                    ["uncertain", "failed"].contains(&event["status"].as_str().unwrap_or(""))
                })
                .take(2)
            {
                let event_id = event["id"].as_str().unwrap_or("").to_owned();
                let retry_id = event_id.clone();
                body = body
                    .child(self.card("Review input delivery", "An earlier input delivery is uncertain. New messages continue automatically; retry only after checking its effects."))
                    .child(
                        div().id(SharedString::from(format!("resolve-{event_id}"))).flex().gap_2()
                            .child(self.btn("retry-event", "Retry input", move |this, window, cx| {
                                this.resolve_event(retry_id.clone(), true, window, cx);
                            }, cx))
                            .child(self.btn("dismiss-event", "Dismiss input", move |this, window, cx| {
                                this.resolve_event(event_id.clone(), false, window, cx);
                            }, cx)),
                    );
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
                body = body.child(self.card("Manual control is reserved", "Release the reservation to let queued messages continue, or take control in the terminal."))
                    .child(self.btn("release-reservation", "Release reservation", move |this, _, _| { let _ = this.backend.tx.send(CommandRequest::Takeover { project:project.clone(),generation:generation.clone(),active:false }); }, cx));
            }
            if project["lifecycle"] == "update_required" {
                body = body.child(self.card("Update pending", "This project is waiting for a compatible update. Its conversation and saved messages are preserved."));
            }
            let pause = id.clone();
            let paused = project["paused"].as_bool().unwrap_or(false);
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.btn(
                        "pause-project",
                        if paused {
                            "Resume automatic activity"
                        } else {
                            "Pause automatic activity"
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
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(palette.muted))
                            .child(self.tr(if paused {
                            "Automatic activity is paused. Messages stay saved until you resume."
                        } else {
                            "Messages start the Agent automatically."
                        })),
                    ),
            );
            body = body.child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.tr(if manager_settings::enabled(&project) {
                        "Coding CLI"
                    } else {
                        "Project CLI"
                    })),
            );
            let mut agents = div().flex().flex_wrap().gap_2();
            for (index, agent) in ["codex", "claude", "pi", "agy"].into_iter().enumerate() {
                let project_id = id.clone();
                agents = agents.child(
                    Button::new(("agent", index))
                        .child(control_label(agent_name(agent)))
                        .selected(project["agent"] == agent)
                        .disabled(
                            self.busy
                                || project["paused"] == true
                                || project["lifecycle"] == "taken_over"
                                || project["agent"] == agent,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.release_terminal();
                            this.api(
                                "POST",
                                format!("/projects/{project_id}/agent/transition"),
                                Some(json!({"agent":agent})),
                                "CLI handoff requested. The project binding is preserved.",
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
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(format!("{}: {model}", self.tr("Model"))),
                );
            } else {
                body = body.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(self.tr("Model follows the CLI's own settings")),
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
                        Some(&this.tr("The Agent will stop. Project files remain on disk.")),
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
    fn execution_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let control = self
            .terminal
            .as_ref()
            .is_some_and(|bridge| bridge.controlling);
        let mut header = div()
            .min_h(px(52.))
            .py_2()
            .flex_wrap()
            .flex_shrink_0()
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .bg(rgb(0x202a35))
            .child(
                div().text_size(px(13.)).text_color(rgb(0xe8eeeb)).child(
                    self.tr(if control {
                        "You are in control"
                    } else if self.state["projects"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|p| p["id"] == self.selected && manager_settings::enabled(p))
                    {
                        "Coding worker terminal"
                    } else {
                        "Live terminal"
                    }),
                ),
            );
        if let Some(bridge) = &self.terminal {
            let project = bridge.descriptor.project_id.clone();
            let generation = bridge.descriptor.generation.clone();
            header = header.child(
                Button::new("terminal-control")
                    .child(control_label(self.tr(if control {
                        "Release control"
                    } else {
                        "Take control"
                    })))
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
        header = header.child(self.btn(
            "collapse-terminal",
            "Hide terminal",
            |this, _, cx| {
                this.setup.terminal_hidden = true;
                this.release_terminal();
                this.terminal_key.clear();
                cx.notify();
            },
            cx,
        ));
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
                        .text_size(px(11.))
                        .text_color(rgb(0xffcf89))
                        .child(self.tr(&bridge.notice)),
                );
            }
            if !control {
                panel = panel.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_size(px(11.))
                        .text_color(rgb(0xa7b5c2))
                        .child(self.tr("Viewing only · Take control to scroll app history")),
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
            } else if lifecycle == "paused" {
                "Automatic activity is paused. Messages stay saved until you resume."
            } else if matches!(lifecycle, "needs_attention" | "update_required" | "error") {
                "Agent needs attention. Review the project notice."
            } else {
                "Ready for messages. The Agent starts automatically when needed."
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
                    .text_size(px(11.))
                    .text_color(rgb(0xffcf89))
                    .child(self.terminal_error.clone()),
            );
        }
        panel
            .child(
                Button::new("reconnect-terminal")
                    .child(control_label(self.tr("Reconnect")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.release_terminal();
                        this.terminal_key.clear();
                        this.sync_terminal(cx);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
    fn workspace(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let project = self.state["projects"].as_array().and_then(|projects| {
            projects
                .iter()
                .find(|project| project["id"] == self.selected)
        });
        let Some(project) = project else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_4()
                .p_8()
                .child(
                    div()
                        .text_2xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(self.tr("No projects yet")),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(rgb(palette.muted))
                        .child(self.tr("Add a project, then send instructions in Feishu.")),
                )
                .child(
                    self.btn("first-project", "Add project", |this, _, _| this.go(5), cx)
                        .primary(),
                )
                .into_any_element();
        };
        self.project_tools.update(cx, |tools, cx| {
            tools.select(project, self.setup.language, cx)
        });
        self.model_picker.update(cx, |picker, cx| {
            picker.select(project, self.setup.language, cx)
        });
        let manager_enabled = manager_settings::enabled(project);
        let timers = visible_timers(&self.timers);
        let mut content = div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .truncate()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(
                                        project["name"].as_str().unwrap_or("Project").to_owned(),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(rgb(palette.muted))
                                    .child(self.tr("Your instructions, one message away.")),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(palette.pill))
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(self.tr(project_status(project))),
                    ),
            )
            .child(
                div()
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(palette.line))
                    .bg(rgb(palette.surface))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(self.project_tools.clone())
                    .child(
                        div()
                            .pt_3()
                            .border_t_1()
                            .border_color(rgb(palette.hover))
                            .when(manager_enabled, |view| {
                                view.child(
                                    div()
                                        .mb_2()
                                        .text_size(px(11.))
                                        .text_color(rgb(palette.muted))
                                        .child(self.tr("Coding worker model")),
                                )
                            })
                            .child(self.model_picker.clone()),
                    ),
            );
        let mut schedules = div()
            .rounded_xl()
            .border_1()
            .border_color(rgb(palette.line))
            .bg(rgb(palette.surface))
            .overflow_hidden()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.tr("Scheduled runs")),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(palette.pill))
                                    .text_size(px(11.))
                                    .text_color(rgb(palette.muted))
                                    .child(timers.len().to_string()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(self.tr("Managed in Feishu")),
                    ),
            );
        if timers.is_empty() {
            schedules = schedules.child(
                div()
                    .px_6()
                    .py_8()
                    .border_t_1()
                    .border_color(rgb(palette.hover))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(40.))
                            .rounded_full()
                            .bg(rgb(palette.pill))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xl()
                            .text_color(rgb(palette.muted))
                            .child("◷"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.tr("Nothing scheduled yet")),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(palette.muted))
                            .child(self.tr(
                                "Ask the Agent in Feishu to schedule something when you need it.",
                    )),
                    ),
            );
        }
        for timer in timers {
            let id = timer["id"].as_str().unwrap_or("").to_owned();
            let enabled = timer["enabled"].as_bool().unwrap_or(false);
            let next = timer_time(
                &timer["next_fire"],
                timer["timezone"].as_str().unwrap_or("UTC"),
            );
            let mut details = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(timer["name"].as_str().unwrap_or("Scheduled run").to_owned()),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(timer_description(&timer, self.setup.language)),
                );
            if !next.is_empty() && enabled {
                details = details.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(format!("{} · {next}", self.tr("Next run"))),
                );
            }
            schedules = schedules.child(
                div()
                    .px_4()
                    .py_4()
                    .border_t_1()
                    .border_color(rgb(palette.hover))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(30.))
                            .rounded_lg()
                            .bg(rgb(if enabled {
                                palette.selected
                            } else {
                                palette.pill
                            }))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(if enabled {
                                palette.accent
                            } else {
                                palette.muted
                            }))
                            .child("◷"),
                    )
                    .child(details)
                    .child(
                        Button::new(SharedString::from(format!("timer-{id}")))
                            .small()
                            .text_size(px(13.))
                            .child(control_label(self.tr(if enabled {
                                "Pause"
                            } else {
                                "Enable"
                            })))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.api(
                                    "POST",
                                    format!("/timers/{id}/pause"),
                                    Some(json!({"enabled":!enabled})),
                                    "Schedule saved",
                                );
                                this.work_checked =
                                    std::time::Instant::now() - Duration::from_secs(30);
                                cx.notify();
                            })),
                    ),
            );
        }
        content = content.child(schedules).child(div().text_size(px(11.)).text_color(rgb(palette.muted))
            .child(self.tr("Send instructions in Feishu. New messages immediately steer the Agent, even while it is working.")));
        content.into_any_element()
    }
    fn workspace_layout(
        &mut self,
        content: AnyElement,
        geometry: WorkspaceGeometry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = self.palette();
        let center = div()
            .id("scheduled-work")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .bg(rgb(palette.canvas))
            .child(content);
        if self.setup.terminal_hidden {
            return div()
                .size_full()
                .flex()
                .child(center.flex_1())
                .child(
                    div()
                        .w(px(54.))
                        .h_full()
                        .flex_shrink_0()
                        .border_l_1()
                        .border_color(rgb(palette.line))
                        .bg(rgb(palette.surface))
                        .pt_5()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            Button::new("expand-terminal")
                                .child(control_label("›_"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.setup.terminal_hidden = false;
                                    this.terminal_key.clear();
                                    this.sync_terminal(cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .mt_3()
                                .text_size(px(11.))
                                .text_color(rgb(palette.muted))
                                .child(self.tr("Terminal")),
                        ),
                )
                .into_any_element();
        }
        let terminal = self.execution_panel(cx);
        h_resizable("workspace-split")
            .child(
                resizable_panel()
                    .size(geometry.center)
                    .size_range(px(320.)..geometry.available - px(320.))
                    .child(center),
            )
            .child(
                resizable_panel()
                    .size_range(px(320.)..geometry.available - px(320.))
                    .child(terminal),
            )
            .into_any_element()
    }
    fn settings_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mut links = div().flex().flex_wrap().gap_2();
        for (index, label) in [
            (1, "Feishu bot setup"),
            (4, "macOS permissions"),
            (7, "App Updates"),
        ] {
            links = links.child(self.btn(
                SharedString::from(format!("settings-{index}")),
                label,
                move |this, _, _| this.go(index),
                cx,
            ));
        }
        let appearance = self.setup.appearance;
        let theme_controls = div()
            .flex()
            .items_center()
            .gap_2()
            .child(self.tr("Appearance"))
            .child(
                Button::new("appearance-system")
                    .child(self.tr("Follow system"))
                    .selected(appearance == ui_theme::Appearance::System)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.system_appearance = window.appearance();
                        this.setup.appearance = ui_theme::Appearance::System;
                        ui_theme::apply_in_window(this.setup.appearance, window, cx);
                        this.go(this.setup.page);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("appearance-light")
                    .child(self.tr("Light"))
                    .selected(appearance == ui_theme::Appearance::Light)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.setup.appearance = ui_theme::Appearance::Light;
                        ui_theme::apply_in_window(this.setup.appearance, window, cx);
                        this.go(this.setup.page);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("appearance-dark")
                    .child(self.tr("Dark"))
                    .selected(appearance == ui_theme::Appearance::Dark)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.setup.appearance = ui_theme::Appearance::Dark;
                        ui_theme::apply_in_window(this.setup.appearance, window, cx);
                        this.go(this.setup.page);
                        cx.notify();
                    })),
            );
        let project = self.project_settings(cx);
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.title(
                "SETTINGS",
                "Settings",
                "Connections and occasional project controls.",
            ))
            .child(links)
            .child(theme_controls)
            .child(
                div()
                    .p_5()
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(palette.line))
                    .bg(rgb(palette.surface))
                    .child(project),
            )
            .child(
                div()
                    .p_5()
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(palette.line))
                    .bg(rgb(palette.surface))
                    .child(self.environment_page(cx)),
            )
            .into_any_element()
    }
    fn environment_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
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
                    .text_size(px(13.))
                    .child(self.tr(name))
                    .child(div().text_color(rgb(palette.muted)).child(self.tr(status)))
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
        let running = matches!(
            phase,
            "checking" | "downloading" | "installing" | "activating"
        );
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
            "installed" | "activating" => {
                self.tr("Finishing the update. Your projects keep running.")
            }
            "current" => self.tr("You have the latest version on this channel"),
            "error" => format!(
                "{}\n{}",
                self.tr("Update failed"),
                self.update_status["message"].as_str().unwrap_or("")
            ),
            _ => self.tr("Automatic checks run every six hours when enabled."),
        };
        div().flex().flex_col().gap_5()
            .child(self.title("UPDATES", "App Updates", "Signed updates are verified and applied automatically. Your projects and conversations stay in place."))
            .child(self.card("Current version", &format!("Dauerbot {} · {}", env!("CARGO_PKG_VERSION"), self.tr(if channel == "preview" { "Preview channel" } else { "Stable channel" }))))
            .child(self.card("Update status", &message))
            .child(div().flex().gap_3().flex_wrap()
                .child(Button::new("update-check").child(control_label(self.tr("Check for updates"))).disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.tx.send(CommandRequest::Update(updater::Action::Check)); cx.notify();
                })))
                .when(phase == "available" || phase == "error", |row| row.child(Button::new("update-install").child(control_label(self.tr("Download and install"))).disabled(running).on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.backend.tx.send(CommandRequest::Update(updater::Action::Install)); cx.notify();
                }))))

            )
            .child(div().flex().gap_3()
                .child(Button::new("update-auto").child(control_label(self.tr(if automatic { "Disable automatic updates" } else { "Enable automatic updates" }))).on_click(cx.listener(move |this, _, _, cx| {
                    let channel = this.update_status["preferences"]["channel"].as_str().unwrap_or("stable").to_owned();
                    let _ = this.backend.tx.send(CommandRequest::UpdatePreferences { automatic: !automatic, channel }); cx.notify();
                })))
                .child(Button::new("update-channel").child(control_label(self.tr(if channel == "stable" { "Switch to preview channel" } else { "Switch to stable channel" }))).on_click(cx.listener(|this, _, _, cx| {
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette();
        if self.activation_frozen {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(palette.canvas))
                .child(self.tr("Finishing the update. Your projects keep running."))
                .into_any_element();
        }
        let page = self.setup.page;
        let geometry = workspace_geometry(
            window.viewport_size().width,
            page == 6 && !self.setup.terminal_hidden,
            self.setup.sidebar_hidden,
            self.setup.sidebar_width,
        );
        let visible = self.visible_project_ids(cx);
        let mut project_list = div()
            .id("sidebar-project-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .flex()
            .flex_col()
            .gap_1();
        for project in self.state["projects"].as_array().into_iter().flatten() {
            let id = project["id"].as_str().unwrap_or("").to_owned();
            if !visible.contains(&id) {
                continue;
            }
            let keyboard_id = id.clone();
            let selected = id == self.selected && page == 6;
            let manager_enabled = manager_settings::enabled(project);
            let status = if manager_enabled {
                manager_settings::status(project)
            } else {
                project_status(project)
            };
            let tree = agent_tree::Tree::from_project(project);
            let fold_scope = format!(
                "{}:{}:{}",
                id,
                project["generation"].as_str().unwrap_or(""),
                agent_tree::observed_native(project)
            );
            let folded = self
                .folded_agents
                .get(&fold_scope)
                .cloned()
                .unwrap_or_default();
            let tree_rows = tree.rows(&folded);
            let has_children = manager_enabled || !tree.nodes.is_empty();
            let root_folded = folded.contains("");
            let click_project = id.clone();
            let root_row = div()
                .id(SharedString::from(format!("project-{id}")))
                .focusable()
                .tab_index(0)
                .border_1()
                .border_color(rgb(palette.canvas))
                .focus(|style| style.border_color(rgb(palette.accent)))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.select_project(keyboard_id.clone(), cx);
                        this.go(6);
                        cx.stop_propagation();
                    }
                }))
                .px_2()
                .h(px(32.))
                .rounded_md()
                .cursor_pointer()
                .bg(rgb(if selected {
                    palette.selected
                } else {
                    palette.canvas
                }))
                .hover(|style| style.bg(rgb(palette.hover)))
                .child(
                    div()
                        .flex()
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .h_full()
                        .child(agent_tree::cli_icon(
                            if manager_enabled {
                                "manager"
                            } else {
                                project["agent"].as_str().unwrap_or("custom")
                            },
                            format!("project-cli-{id}"),
                            palette,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.))
                                .font_weight(if selected {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::NORMAL
                                })
                                .child(project["name"].as_str().unwrap_or("Project").to_owned()),
                        )
                        .child(agent_tree::status_dot(
                            status,
                            self.tr(status),
                            format!("project-status-{id}"),
                            palette,
                        )),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_project(click_project.clone(), cx);
                    this.go(6);
                    cx.notify();
                }));
            let root_layout = agent_tree::row_layout(&[], true, has_children, 32., f32::from(geometry.sidebar) - 16., 0.);
            let mut root_toggle = agent_tree::disclosure(&root_layout);
            if root_layout.expandable {
                let scope = fold_scope.clone();
                root_toggle = root_toggle.child(
                    Button::new(SharedString::from(format!("fold-project-{id}")))
                        .w_full()
                        .child(div().text_size(px(12.)).child(if root_folded {
                            "›"
                        } else {
                            "⌄"
                        }))
                        .xsmall()
                        .tooltip(self.tr(if root_folded {
                            "Show subagents"
                        } else {
                            "Hide subagents"
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let folded = this.folded_agents.entry(scope.clone()).or_default();
                            if !folded.remove("") {
                                folded.insert(String::new());
                            }
                            cx.notify();
                        })),
                );
            }
            let root = div().flex().items_center().w_full().min_w_0().child(root_toggle);
            let mut group = div()
                .id(SharedString::from(format!("project-tree-{id}")))
                .flex()
                .flex_col()
                .min_w_0()
                .child(root.child(div().flex_1().min_w_0().child(root_row)));
            if !root_folded {
                let worker_folded = manager_enabled && folded.contains("worker");
                if manager_enabled {
                    let scope = fold_scope.clone();
                    let terminal_project = id.clone();
                    let worker_layout = agent_tree::row_layout(&[], true, !tree.nodes.is_empty(), 28., f32::from(geometry.sidebar) - 16., 28.);
                    let mut worker_toggle = agent_tree::disclosure(&worker_layout);
                    if worker_layout.expandable {
                        worker_toggle = worker_toggle.child(
                            Button::new(SharedString::from(format!("fold-worker-{id}")))
                                .w_full()
                                .xsmall()
                                .child(div().text_size(px(11.)).child(if worker_folded {
                                    "›"
                                } else {
                                    "⌄"
                                }))
                                .tooltip(self.tr(if worker_folded {
                                    "Show subagents"
                                } else {
                                    "Hide subagents"
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let folded = this.folded_agents.entry(scope.clone()).or_default();
                                    if !folded.remove("worker") {
                                        folded.insert("worker".into());
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                })),
                        );
                    }
                    let worker_row = div()
                        .id(SharedString::from(format!("coding-worker-{id}")))
                        .h(px(28.))
                        .pl(px(worker_layout.left_padding))
                        .pr_2()
                        .flex()
                        .items_center()
                        .gap_1()
                        .min_w_0();
                    let worker_row = worker_row
                        .child(agent_tree::branches(&worker_layout, palette))
                        .child(worker_toggle)
                        .child(agent_tree::cli_icon(
                            project["agent"].as_str().unwrap_or("custom"),
                            format!("worker-cli-{id}"),
                            palette,
                        ))
                        .child(
                            div().flex_1().min_w_0().child(
                                Button::new(SharedString::from(format!("open-worker-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .w_full()
                                    .child(
                                        div()
                                            .w_full()
                                            .text_left()
                                            .text_size(px(12.))
                                            .child(self.tr("Coding worker")),
                                    )
                                    .tooltip(self.tr("Show coding terminal"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_project(terminal_project.clone(), cx);
                                        this.go(6);
                                        this.setup.terminal_hidden = false;
                                        this.sync_terminal(cx);
                                        cx.notify();
                                    })),
                            ),
                        )
                        .child(agent_tree::status_dot(
                            project_status(project),
                            self.tr(project_status(project)),
                            format!("worker-state-{id}"),
                            palette,
                        ));
                    group = group.child(worker_row);
                }
                for row in tree_rows.into_iter().filter(|_| !worker_folded) {
                    let node = row.node;
                    let layout = agent_tree::row_layout(&row.branches, row.last, row.has_children, 26., f32::from(geometry.sidebar) - 16., if manager_enabled { 48. } else { 28. });
                    let node_id = node.id.clone();
                    let scope = fold_scope.clone();
                    let target = if tree.status == "observed" {
                        agent_tree::project_target(node, &self.state["projects"])
                    } else {
                        None
                    };
                    let detail = format!(
                        "{} · {} · {}{}",
                        node.name,
                        agent_tree::cli_label(&node.cli),
                        self.tr(&node.state),
                        if node.observation.is_empty() {
                            String::new()
                        } else {
                            format!("\n{}", node.observation)
                        }
                    );
                    let mut child = div()
                        .id(SharedString::from(format!("agent-{id}-{}", node.id)))
                        .h(px(26.))
                        .min_w_0()
                        .pl(px(layout.left_padding))
                        .pr_2()
                        .flex()
                        .items_center()
                        .gap_1()
                        .rounded_md()
                        .hover(|style| style.bg(rgb(palette.hover)))
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(detail.clone()).build(window, cx)
                        })
                        .child(agent_tree::branches(&layout, palette));
                    let mut disclosure = agent_tree::disclosure(&layout);
                    if layout.expandable {
                        let fold_id = node_id.clone();
                        disclosure = disclosure.child(
                            Button::new(SharedString::from(format!("fold-agent-{id}-{node_id}")))
                                .w_full()
                                .child(div().text_size(px(11.)).child(
                                    if folded.contains(&node_id) {
                                        "›"
                                    } else {
                                        "⌄"
                                    },
                                ))
                                .xsmall()
                                .tooltip(self.tr(if folded.contains(&node_id) {
                                    "Show subagents"
                                } else {
                                    "Hide subagents"
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let folded =
                                        this.folded_agents.entry(scope.clone()).or_default();
                                    if !folded.remove(&fold_id) {
                                        folded.insert(fold_id.clone());
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                })),
                        );
                    }
                    child = child.child(disclosure).child(agent_tree::cli_icon(
                        &node.cli,
                        format!("agent-cli-{id}-{node_id}"),
                            palette,
                    ));
                    if target.is_none() {
                        let name = node.name.clone();
                        let summary = format!(
                            "{} · {}",
                            agent_tree::cli_label(&node.cli),
                            self.tr(&node.state)
                        );
                        let observation = node.observation.clone();
                        let read_only = self.tr("Observed subagent · read only");
                        child = child.child(
                            div().flex_1().min_w_0().child(
                                gpui_component::popover::Popover::new(SharedString::from(format!(
                                    "agent-details-{id}-{node_id}"
                                )))
                                .trigger(
                                    Button::new(SharedString::from(format!(
                                        "inspect-agent-{id}-{node_id}"
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .w_full()
                                    .justify_start()
                                    .h(px(24.))
                                    .tooltip(self.tr("View Agent details"))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(12.))
                                            .text_color(rgb(palette.muted))
                                            .child(node.name.clone()),
                                    ),
                                )
                                .content(move |_, _, _| {
                                    div()
                                        .w(px(260.))
                                        .p_3()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .text_size(px(12.))
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(name.clone()),
                                        )
                                        .child(summary.clone())
                                        .child(
                                            div()
                                                .text_color(rgb(palette.muted))
                                                .child(observation.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(rgb(palette.muted))
                                                .child(read_only.clone()),
                                        )
                                }),
                            ),
                        );
                    } else {
                        child = child.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(palette.muted))
                                .child(node.name.clone()),
                        );
                    }
                    child = child.child(agent_tree::status_dot(
                        &node.state,
                        self.tr(&node.state),
                        format!("agent-state-{id}-{node_id}"),
                        palette,
                    ));
                    if let Some(target) = target {
                        let key_target = target.clone();
                        child = child
                            .focusable()
                            .tab_index(0)
                            .cursor_pointer()
                            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.select_project(key_target.clone(), cx);
                                    this.go(6);
                                    cx.stop_propagation();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_project(target.clone(), cx);
                                this.go(6);
                                cx.notify();
                            }));
                    }
                    group = group.child(child);
                }
                if !worker_folded
                    && project["execution"]["agent_tree"].is_object()
                    && tree.status != "observed"
                {
                    group = group.child(
                        div()
                            .pl(px(40.))
                            .pr_2()
                            .py_1()
                            .text_size(px(10.))
                            .text_color(rgb(palette.muted))
                            .child(self.tr(if tree.status == "stale" {
                                "Agent details may be out of date"
                            } else {
                                "Some Agent details are unavailable"
                            })),
                    );
                }
            }
            project_list = project_list.child(group);
        }
        let sidebar = div()
                .relative()
                .w(geometry.sidebar)
                .flex_shrink_0()
                .h_full()
                .flex()
                .flex_col()
            .bg(rgb(palette.canvas))
                .border_r_1()
            .border_color(rgb(palette.line))
                .child(
                    div()
                        .id("sidebar-resize")
                        .absolute()
                        .top_0()
                        .right(px(-4.))
                        .w(px(8.))
                        .h_full()
                        .cursor_col_resize()
                        .on_drag(
                            SidebarDrag {
                                offset: Default::default(),
                            },
                            |drag, position, _, cx| {
                                cx.stop_propagation();
                                drag.offset.set(px(4.) - position.x);
                                cx.new(|_| drag.clone())
                            },
                        )
                        .on_drag_move(cx.listener(
                            |this, event: &DragMoveEvent<SidebarDrag>, window, cx| {
                                let wanted =
                                    f32::from(event.event.position.x + event.drag(cx).offset.get());
                                this.setup.sidebar_width = sidebar::SidebarWidth::from_drag(
                                    wanted,
                                    f32::from(window.viewport_size().width),
                                );
                                this.sidebar_resizing = true;
                                cx.notify();
                            },
                        ))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| this.finish_sidebar_resize()),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| this.finish_sidebar_resize()),
                        ),
                )
                .child(
                    div()
                        .h(px(54.))
                        .px_4()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .size(px(24.))
                                .rounded_md()
                            .bg(rgb(palette.accent))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(palette.on_accent))
                                .child("D"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Dauerbot"),
                        ),
                )
                .child(
                    div()
                        .px_5()
                        .pt_3()
                        .pb_2()
                        .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                        .child(self.tr("Projects")),
                )
                .child(
                    div()
                        .px_3()
                        .pb_3()
                        .child(Input::new(&self.project_search).small().text_size(px(13.))),
                )
                .child(project_list)
                .when(visible.is_empty(), |view| {
                    view.child(
                        div()
                            .px_4()
                            .py_3()
                            .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                            .child(self.tr("No matching projects")),
                    )
                })
                .child(
                    div()
                        .px_3()
                        .py_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                    .child(self.btn("sidebar-add", "+ Add project", |this, _, _| this.go(5), cx))
                    .child(
                        div()
                            .pt_3()
                            .border_t_1()
                            .border_color(rgb(palette.hover))
                        .child(self.btn(
                                "sidebar-settings",
                                "Settings",
                                |this, _, _| this.go(8),
                            cx,
                            )),
                    )
                        .child(
                            div()
                                .px_2()
                                .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                                .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
                        ),
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
            _ => self.settings_page(cx),
        };
        let finder_allowed = self.finder_allowed();
        self.finder.update(cx, |finder, cx| {
            if !finder_allowed { finder.dismiss(window, cx); }
            finder.sync(&self.state, self.setup.language, cx)
        });
        div()
            .id("dauerbot-desktop")
            .relative()
            .track_focus(&self.focus_handle)
            .key_context("Dauerbot")
            .on_action(cx.listener(|this, _: &QuickFind, window, cx| {
                if this.finder_allowed() {
                    this.finder.update(cx, |finder, cx| finder.toggle(this.focus_handle.clone(), window, cx));
                } else { cx.propagate(); }
            }))
            .on_action(cx.listener(|this, _: &FocusProjects, window, cx| {
                if workspace_geometry(window.viewport_size().width, this.setup.page == 6 && !this.setup.terminal_hidden, this.setup.sidebar_hidden, this.setup.sidebar_width).sidebar_hidden {
                    this.toggle_sidebar(window, cx);
                }
                this.project_search.update(cx, |search, cx| search.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &NextProject, _, cx| this.move_project(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousProject, _, cx| this.move_project(-1, cx)))
            .on_action(cx.listener(|this, _: &AddProject, _, cx| { this.go(5); cx.notify(); }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleTerminal, _, cx| {
                this.go(6);
                this.setup.terminal_hidden = !this.setup.terminal_hidden;
                this.release_terminal();
                this.terminal_key.clear();
                this.sync_terminal(cx);
                cx.notify();
            }))
            .size_full()
            .flex()
            .font_family(".AppleSystemUIFont")
            .text_size(px(13.))
            .text_color(rgb(palette.foreground))
            .bg(rgb(palette.canvas))
            .when(!geometry.sidebar_hidden, |view| view.child(sidebar))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(54.))
                            .px_6()
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_b_1()
                            .border_color(rgb(palette.line))
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(div().flex().items_center().gap_3()
                                .child(Button::new("toggle-sidebar").small().text_size(px(13.)).child(control_label(self.tr(if geometry.sidebar_hidden { "Show sidebar" } else { "Hide sidebar" })))
                                    .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx))))
                                .child(self.tr(if page == 6 { "Projects" } else { STEPS[page] })))
                            .child(Button::new("language").child(control_label(if self.setup.language == Language::En { "简体中文" } else { "English" })).on_click(cx.listener(|this, _, window, cx| {
                                this.setup.language = this.setup.language.other();
                                this.go(this.setup.page);
                                this.name.update(cx, |input, cx| input.set_placeholder(this.setup.language.text("For example: personal website"), window, cx));
                                this.project_search.update(cx, |input, cx| input.set_placeholder(this.setup.language.text("Search projects · ⌘F"), window, cx));
                                this.path.update(cx, |input, cx| input.set_placeholder(this.setup.language.text("Leave empty to create a private project folder"), window, cx));
                                this.secret.update(cx, |input, cx| input.set_placeholder(this.setup.language.text("Stored only on this Mac"), window, cx));
                                set_menus(this.setup.language, cx);
                                cx.notify();
                            }))),
                    )
                    .child(if page == 6 {
                        self.workspace_layout(content, geometry, cx)
                    } else {
                        div()
                            .id(("content-scroll", page))
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_6()
                            .child(
                                div()
                                    .max_w(px(850.))
                                    .when(self.independent_data, |view| {
                                        view.child(
                                            div()
                                                .mb_6()
                                                .p_4()
                                                .rounded_lg()
                                                .bg(rgb(palette.warning_surface))
                                                .text_size(px(13.))
                                                .child(self.tr("Independent instance · This window uses separate data from your everyday workspace."))
                                                .child(div().mt_2().text_size(px(11.)).child(
                                                    self.backend.dir.display().to_string(),
                                                )),
                                        )
                                    })
                                    .when(matches!(page, 1..=3), |view| {
                                        let mut steps = div().flex().flex_wrap().gap_2().mb_6();
                                        for (index, label) in [(1, "Create bot"), (2, "Connect bot"), (3, "Permissions & events")] {
                                            steps = steps.child(Button::new(("setup-stage", index)).child(control_label(format!("{}  {}", index, self.tr(label)))).selected(page == index).on_click(cx.listener(move |this, _, _, cx| { this.go(index); cx.notify(); })));
                                        }
                                        view.child(steps)
                                    })
                                    .child(content),
                            )
                            .into_any_element()
                    })
                    .child(
                        div()
                            .px_6()
                            .py_2()
                            .border_t_1()
                            .border_color(rgb(palette.line))
                            .text_size(px(11.))
                            .text_color(rgb(if self.error { palette.error } else { palette.muted }))
                            .child(if let Some(reason) = &self.connection_error { format!("{}: {reason}", self.tr("Local service connection failed")) } else { self.tr(&self.notice) }),
                    ),
            ).child(self.finder.clone()).into_any_element()
    }
}
fn project_status(project: &Value) -> &str {
    if project["paused"] == true {
        "paused"
    } else {
        project["lifecycle"].as_str().unwrap_or_else(|| {
            if project["terminal"].is_null() {
                "sleeping"
            } else {
                "ready"
            }
        })
    }
}
fn records(value: &Value, field: &str) -> Vec<Value> {
    value
        .as_array()
        .or_else(|| value[field].as_array())
        .cloned()
        .unwrap_or_default()
}
fn timer_time(value: &Value, timezone: &str) -> String {
    value
        .as_i64()
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map(|time| match timezone.parse::<chrono_tz::Tz>() {
            Ok(zone) => time.with_timezone(&zone).format("%m/%d %H:%M").to_string(),
            Err(_) => time.format("%m/%d %H:%M UTC").to_string(),
        })
        .or_else(|| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}
/// Missing/empty legacy purpose stays visible. Pause keeps next_fire so future
/// reminders retain their Enable action; exhausted and deleted timers do not.
fn visible_timers(value: &Value) -> Vec<Value> {
    records(value, "timers")
        .into_iter()
        .filter(|timer| timer["purpose"] != "internal" && timer["next_fire"].as_i64().is_some())
        .collect()
}
fn timer_description(timer: &Value, language: Language) -> String {
    let timezone = timer["timezone"].as_str().unwrap_or("UTC");
    let schedule = match timer["kind"].as_str().unwrap_or("") {
        "once" => timer_time(&timer["at"], timezone),
        "interval" => {
            let seconds = timer["interval_seconds"].as_u64().unwrap_or(0);
            let (value, unit) = if seconds > 0 && seconds.is_multiple_of(86400) {
                (seconds / 86400, "days")
            } else if seconds > 0 && seconds.is_multiple_of(3600) {
                (seconds / 3600, "hours")
            } else if seconds > 0 && seconds.is_multiple_of(60) {
                (seconds / 60, "minutes")
            } else {
                (seconds, "seconds")
            };
            format!("{} {value} {}", language.text("Every"), language.text(unit))
        }
        _ => {
            let cron = timer["cron"].as_str().unwrap_or("");
            let parts: Vec<_> = cron.split_whitespace().collect();
            if parts.len() == 5
                && parts[2] == "*"
                && parts[3] == "*"
                && ["*", "1-5"].contains(&parts[4])
                && let (Ok(minute), Ok(hour)) = (parts[0].parse::<u8>(), parts[1].parse::<u8>())
            {
                format!(
                    "{} {hour:02}:{minute:02}",
                    language.text(if parts[4] == "1-5" {
                        "Weekdays"
                    } else {
                        "Daily"
                    })
                )
            } else {
                cron.to_owned()
            }
        }
    };
    format!("{schedule} · {timezone}")
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
            MenuItem::action(language.text("Settings"), ShowSettings),
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
    let activation_id = std::env::args().skip_while(|v| v != "--activation").nth(1);
    let candidate = activation_id.as_deref().map(|id| {
        activation::Candidate::connect(&dir, id).expect("Cannot prepare the updated app")
    });
    let backend = if let Some(candidate) = &candidate {
        Backend::start_mode(
            dir,
            Some((
                candidate.serving.clone(),
                candidate.updates.clone(),
                candidate.backend_ready.clone(),
            )),
        )
    } else {
        Backend::start(dir)
    };
    let frontend_ready = backend.frontend_ready.clone();
    let app = Application::new().with_assets(agent_tree::Assets);
    let published = candidate
        .as_ref()
        .map(|candidate| candidate.serving.clone());
    app.on_reopen(move |cx| {
        if published
            .as_ref()
            .is_some_and(|ready| !ready.load(std::sync::atomic::Ordering::Acquire))
        {
            return;
        }
        #[cfg(target_os = "macos")]
        menubar::open_main(None, 6, cx);
    });
    app.run(move |cx| {
        gpui_component::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-,", ShowSettings, None),
            KeyBinding::new("cmd-k", QuickFind, Some("Dauerbot")),
            KeyBinding::new("cmd-f", FocusProjects, Some("Dauerbot")),
            KeyBinding::new("cmd-b", ToggleSidebar, Some("Dauerbot")),
            KeyBinding::new("cmd-alt-down", NextProject, Some("Dauerbot")),
            KeyBinding::new("cmd-alt-up", PreviousProject, Some("Dauerbot")),
            KeyBinding::new("cmd-n", AddProject, Some("Dauerbot")),
            KeyBinding::new("cmd-alt-t", ToggleTerminal, Some("Dauerbot")),
        ]);
        let saved_setup = std::fs::read(backend.dir.join("desktop.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Setup>(&bytes).ok())
            .unwrap_or_default();
        let language = saved_setup.language;
        ui_theme::apply(saved_setup.appearance, cx);
        if candidate.is_none() {
            set_menus(language, cx);
        }
        let bounds = Bounds::centered(None, size(px(1180.), px(860.)), cx);
        let mut desktop_view = None;
        let main_window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(800.), px(600.))),
                    show: !cfg!(target_os = "macos"),
                    focus: !cfg!(target_os = "macos"),
                    titlebar: Some(TitlebarOptions {
                        title: Some(title.into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| {
                        let mut view = Desktop::new(backend, window, cx);
                        if let Some(candidate) = &candidate {
                            view.restore_activation(candidate.snapshot.clone(), window, cx);
                            view.activation_frozen = true;
                        }
                        view
                    });
                    desktop_view = Some(view.clone());
                    let quit_view = view.clone();
                    cx.on_action(move |_: &Quit, cx| {
                        quit_view.update(cx, |this, cx| {
                            if !this.activation_frozen {
                                this.quit(cx);
                            }
                        });
                    });
                    let close_view = view.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        let _ = close_view.update(cx, |this, cx| {
                            this.release_terminal();
                            this.setup.terminal_hidden = true;
                            this.terminal_key.clear();
                            cx.notify();
                        });
                        #[cfg(target_os = "macos")]
                        menubar::hide_window(window);
                        #[cfg(not(target_os = "macos"))]
                        cx.hide();
                        false
                    });
                    let settings_view = view.clone();
                    cx.on_action(move |_: &ShowSettings, cx| {
                        settings_view.update(cx, |this, cx| {
                            if this.activation_frozen {
                                return;
                            }
                            this.go(8);
                            #[cfg(target_os = "macos")]
                            cx.defer(|cx| menubar::open_main(None, 8, cx));
                            cx.notify();
                        });
                    });
                    let updates_view = view.clone();
                    cx.on_action(move |_: &ShowUpdates, cx| {
                        updates_view.update(cx, |this, cx| {
                            if this.activation_frozen {
                                return;
                            }
                            this.go(7);
                            #[cfg(target_os = "macos")]
                            cx.defer(|cx| menubar::open_main(None, 7, cx));
                            cx.notify();
                        });
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("Cannot create the desktop window");
        cx.defer(move |_| frontend_ready.store(true, std::sync::atomic::Ordering::Release));
        let desktop_view = desktop_view.expect("desktop view");
        if let Some(candidate) = candidate {
            let serving = candidate.serving.clone();
            let ready = candidate.menu_ready.clone();
            let visible = candidate.visible.clone();
            let ready_signal = candidate.ready.clone();
            let refresh = candidate.refresh;
            cx.defer(move |_| {
                let _ = ready_signal.send(());
            });
            cx.spawn(async move |cx| {
                while !serving.load(std::sync::atomic::Ordering::Acquire) {
                    if let Ok((snapshot, ack)) = refresh.try_recv() {
                        visible.store(
                            snapshot["visible"].as_bool().unwrap_or(false),
                            std::sync::atomic::Ordering::Release,
                        );
                        let restored = main_window
                            .update(cx, |_, window, cx| {
                                desktop_view.update(cx, |this, cx| {
                                    this.restore_activation(snapshot.clone(), window, cx);
                                    activation::snapshot_restored(
                                        &snapshot,
                                        &this.activation_snapshot(window, cx),
                                    )
                                })
                            })
                            .unwrap_or(false);
                        let _ = ack.send(restored);
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(25))
                        .await;
                }
                let _ = cx.update(|cx| {
                    set_menus(language, cx);
                    desktop_view.update(cx, |this, cx| {
                        this.activation_frozen = false;
                        cx.notify();
                    });
                    #[cfg(target_os = "macos")]
                    if menubar::install(desktop_view.clone(), main_window, cx).is_err() {
                        return;
                    }
                    if visible.load(std::sync::atomic::Ordering::Acquire) {
                        cx.activate(true);
                        let _ = main_window.update(cx, |_, window, _| window.activate_window());
                    }
                    ready.store(true, std::sync::atomic::Ordering::Release);
                });
            })
            .detach();
        } else {
            #[cfg(target_os = "macos")]
            if let Err(error) = menubar::install(desktop_view, main_window, cx) {
                eprintln!("{error:#}");
                cx.activate(true);
                let _ = main_window.update(cx, |_, window, _| window.activate_window());
            }
            #[cfg(not(target_os = "macos"))]
            cx.activate(true);
        }
    });
}

#[cfg(test)]
mod preference_tests {
    use super::{Language, Setup};

    #[test]
    fn appearance_round_trips_and_bad_fields_preserve_other_preferences() {
        for value in ["null", "false", "42", "{}", "[]", "\"unknown\""] {
            let source =
                format!(r#"{{"page":6,"language":"zh","sidebar_width":280,"appearance":{value}}}"#);
            let setup: Setup = serde_json::from_str(&source).unwrap();
            assert_eq!(setup.appearance, super::ui_theme::Appearance::Light);
            assert_eq!(setup.page, 6);
            assert_eq!(setup.language, Language::Zh);
            assert_eq!(serde_json::to_value(setup.sidebar_width).unwrap(), 280.);
        }
        let mut setup: Setup = serde_json::from_str(r#"{"page":6,"language":"zh"}"#).unwrap();
        assert_eq!(setup.appearance, super::ui_theme::Appearance::Light);
        setup.appearance = super::ui_theme::Appearance::Dark;
        let bytes = serde_json::to_vec(&setup).unwrap();
        assert_eq!(
            serde_json::from_slice::<Setup>(&bytes).unwrap().appearance,
            super::ui_theme::Appearance::Dark
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["appearance"],
            "dark"
        );
        setup.appearance = super::ui_theme::Appearance::System;
        setup.sidebar_width = super::sidebar::SidebarWidth::from_drag(280., 1400.);
        let bytes = serde_json::to_vec(&setup).unwrap();
        let reopened: Setup = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reopened.appearance, super::ui_theme::Appearance::System);
        assert_eq!(reopened.page, 6);
        assert_eq!(reopened.language, Language::Zh);
        assert_eq!(serde_json::to_value(reopened.sidebar_width).unwrap(), 280.);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["appearance"], "system");
    }

    #[test]
    fn sidebar_preferences_reopen_and_old_readers_ignore_the_new_field() {
        let old = r#"{"page":6,"language":"zh","terminal_hidden":false,"sidebar_hidden":true,"bots":{"bot":{"published":true}}}"#;
        let mut setup: Setup = serde_json::from_str(old).unwrap();
        assert_eq!(serde_json::to_value(setup.sidebar_width).unwrap(), 224.);
        setup.sidebar_width = super::sidebar::SidebarWidth::from_drag(280., 1400.);
        let bytes = serde_json::to_vec(&setup).unwrap();
        let mut reopened: Setup = serde_json::from_slice(&bytes).unwrap();
        reopened.prepare_startup();
        assert!(reopened.terminal_hidden);
        assert_eq!(reopened.page, 6);
        assert_eq!(reopened.language, Language::Zh);
        assert!(reopened.sidebar_hidden);
        assert!(reopened.bots["bot"].published);
        assert_eq!(serde_json::to_value(reopened.sidebar_width).unwrap(), 280.);
        #[derive(serde::Deserialize)]
        struct OldSetup {
            page: usize,
            sidebar_hidden: bool,
        }
        let legacy: OldSetup = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(legacy.page, 6);
        assert!(legacy.sidebar_hidden);
    }

    #[test]
    fn malformed_sidebar_width_does_not_discard_other_preferences() {
        for width in [
            "null", "false", "\"wide\"", "{}", "[]", "0", "-1", "167", "321", "1e100",
        ] {
            let input = format!(r#"{{"page":3,"language":"zh","sidebar_width":{width}}}"#);
            let mut setup: Setup = serde_json::from_str(&input).unwrap();
            setup.prepare_startup();
            assert_eq!(setup.page, 3);
            assert_eq!(setup.language, Language::Zh);
            assert_eq!(serde_json::to_value(setup.sidebar_width).unwrap(), 224.);
        }
    }

    #[test]
    fn existing_preferences_keep_setup_progress_and_language_round_trips() {
        let old = r#"{"page":3,"events_confirmed":true,"published":false}"#;
        let mut setup: Setup = serde_json::from_str(old).unwrap();
        assert_eq!(setup.language, Language::En);
        assert_eq!(setup.page, 3);
        assert!(setup.bots.is_empty());
        setup.language = Language::Zh;
        let encoded = serde_json::to_vec(&setup).unwrap();
        let reloaded: Setup = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(reloaded.language, Language::Zh);
        assert_eq!(reloaded.page, 3);
        assert!(reloaded.bots.is_empty());
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::visible_timers;
    use serde_json::json;

    #[test]
    fn rows_and_count_keep_paused_and_legacy_reminders_without_guessing_from_titles() {
        let timers = json!([
            {"id":"legacy-once","name":"Internal follow-up","kind":"once","enabled":true,"next_fire":100},
            {"id":"legacy-empty","purpose":"","kind":"once","enabled":true,"next_fire":100},
            {"id":"paused-user","purpose":"user","kind":"once","enabled":false,"next_fire":200},
            {"id":"user-cron","purpose":"user","kind":"cron","enabled":true,"next_fire":300},
            {"id":"internal","name":"Daily report","purpose":"internal","kind":"interval","enabled":true,"next_fire":100},
            {"id":"internal-paused","purpose":"internal","kind":"once","enabled":false,"next_fire":100},
            {"id":"completed-once","purpose":"user","kind":"once","enabled":false,"next_fire":null},
            {"id":"legacy-completed","kind":"once","enabled":false,"next_fire":null},
            {"id":"deleted-recurring","purpose":"user","kind":"cron","enabled":false,"next_fire":null}
        ]);
        let rows = visible_timers(&timers);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter()
                .map(|t| t["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["legacy-once", "legacy-empty", "paused-user", "user-cron"]
        );
        assert_eq!(
            rows[2]["enabled"], false,
            "The paused future reminder still has a usable Enable action"
        );
    }
}
