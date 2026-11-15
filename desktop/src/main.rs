#![allow(non_snake_case)]
mod access;
mod backend;

use backend::{Backend, CommandRequest, Reply};
use gpui::{prelude::*, *};
use gpui_component::{
    button::*,
    input::{Input, InputEvent, InputState},
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

const INK: u32 = 0x202a35;
const MUTED: u32 = 0x74808c;
const LINE: u32 = 0xe6e9ec;
const ACCENT: u32 = 0x1c796c;
const STEPS: [&str; 7] = [
    "欢迎使用",
    "创建飞书机器人",
    "连接机器人",
    "订阅消息事件",
    "本机访问权限",
    "添加首个项目",
    "项目工作台",
];
actions!(bridge, [Quit]);

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Setup {
    page: usize,
    events_confirmed: bool,
    published: bool,
}
struct Desktop {
    backend: Backend,
    setup: Setup,
    state: Value,
    detail: Value,
    selected: String,
    picked_path: String,
    chats: Vec<Value>,
    name: Entity<InputState>,
    path: Entity<InputState>,
    chat: Entity<InputState>,
    chat_search: Entity<InputState>,
    show_advanced_chat: bool,
    _subscriptions: Vec<Subscription>,
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
}
impl Desktop {
    fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut setup: Setup = std::fs::read(backend.dir.join("desktop.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        setup.page = setup.page.min(6);
        let mut input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("例如：个人网站", cx);
        let path = input("选择项目代码所在的文件夹", cx);
        let chat = input("高级：oc_…（普通使用请从上方列表选择）", cx);
        let chat_search = input("搜索群名、私聊或最近消息…", cx);
        let app_id = input("cli_…", cx);
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("只在你的 Mac 本地保存")
                .masked(true)
        });
        let message = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("给这个项目的主会话发送任务…")
                .multi_line(true)
                .rows(3)
        });
        let search_subscription = cx.subscribe_in(&chat_search, window, |_, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
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
            setup,
            state: Value::Null,
            detail: Value::Null,
            selected: String::new(),
            picked_path: String::new(),
            chats: vec![],
            name,
            path,
            chat,
            chat_search,
            show_advanced_chat: false,
            _subscriptions: vec![search_subscription],
            app_id,
            secret,
            message,
            agent: 0,
            notice: "正在启动本地服务…".into(),
            error: false,
            busy: false,
            access_result: String::new(),
            environment: vec![],
            credentials_checked: false,
            pending_message: String::new(),
            pending_secret: String::new(),
        }
    }
    fn receive(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.backend.rx.try_recv() {
            changed = true;
            match reply {
                Reply::State(value) => {
                    if self.state.is_null() {
                        self.notice = "本地服务已就绪，可以开始设置，也可以先创建本机项目".into();
                        if let Some(id) = value["settings"]["app_id"].as_str()
                            && self.app_id.read(cx).value().is_empty()
                        {
                            self.app_id
                                .update(cx, |input, cx| input.set_value(id.to_owned(), window, cx));
                        }
                    }
                    self.state = value;
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
                    } else if path == "/onboarding/chats" {
                        self.chats = value["items"].as_array().cloned().unwrap_or_default();
                        if value["truncated"] == true {
                            self.notice =
                                "群列表超过 2000 项，仅显示前 2000 项；可在高级设置中补充聊天"
                                    .into();
                        }
                    }
                }
            }
        }
        changed
    }
    fn go(&mut self, page: usize) {
        self.setup.page = page.min(6);
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
        self.notice = "正在处理…".into();
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
            self.notice = "后台服务已退出，请重新打开应用".into();
        }
    }
    fn pick(&mut self, database: bool, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: database,
            directories: !database,
            multiple: false,
            prompt: Some(
                if database {
                    "选择需要检测的 SQLite 文件"
                } else {
                    "选择项目文件夹"
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
                    this.access_result = format!("正在检测：{}", path.display());
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
            .child(Self::title("YOUR AGENTS, A LITTLE CLOSER", "让工作继续，\n让对话随行。", "在飞书里和项目的主会话交谈，回到 Mac 后接着做。先花几分钟，连接你的第一个机器人。"))
            .child(div().flex().gap_4()
                .child(Self::card("01  连接飞书", "创建企业自建机器人。凭证保留在本机。" ).flex_1())
                .child(Self::card("02  选择项目", "连接已有 CLI 与 HERDR 持久会话。").flex_1())
                .child(Self::card("03  继续对话", "文本、任务和附件，回到同一主会话。").flex_1()))
            .child(Self::card("你掌握每一次授权", "项目文件夹由你选择。访问受保护文件时再申请对应权限；普通项目无需完全磁盘访问、辅助功能或屏幕录制权限。"))
            .child(self.btn("begin", "开始设置 →", |this, _, _| this.go(1), cx).primary())
            .child(self.btn("local-first", "先在本机试用，稍后连接飞书", |this, _, _| this.go(5), cx))
            .child(div().mt_6().text_sm().font_weight(FontWeight::SEMIBOLD).child("本机环境"))
            .children(self.environment.iter().map(|(name, status)| div().flex().gap_4().text_sm().child(div().w_32().child(name.clone())).child(div().text_color(rgb(MUTED)).child(status.clone()))))
            .child(self.btn("rescan", "重新检测环境", |this, _, _| { let _ = this.backend.tx.send(CommandRequest::Environment); }, cx))
            .into_any_element()
    }
    fn bot(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 01", "创建你的飞书机器人", "使用企业自建应用，而不是群里的自定义 Webhook 机器人。后者无法完成双向消息交互。"))
            .child(Self::card("1. 创建应用", "打开飞书开放平台 → 开发者后台 → 创建企业自建应用。名称可填「项目助手」，描述写明用途。需要有创建应用的企业权限。"))
            .child(self.btn("console", "打开飞书开发者后台 ↗", |_, _, cx| cx.open_url("https://open.feishu.cn/app"), cx).primary())
            .child(Self::card("2. 添加机器人能力", "进入应用，在「添加应用能力」中启用机器人。先不要创建 Webhook，也不需要公网服务器。"))
            .child(Self::card("3. 找到凭证", "进入「凭证与基础信息」，复制 App ID 和 App Secret。下一步粘贴到本机应用，不要发到群里。"))
            .child(Self::card("已有机器人？", "建议为 Bridge 创建独立应用。同一机器人同时被其他程序通过长连接监听，可能导致消息由其他程序收到。"))
            .child(self.btn("bot-next", "已启用机器人，填写凭证 →", |this, _, _| this.go(2), cx).primary()).into_any_element()
    }
    fn credentials(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 02", "把机器人连接到这台 Mac", "保存后，本地服务会连接飞书。凭证验证只证明身份有效，消息权限和事件订阅将在下一步配置。"))
            .child(Self::field("App ID", &self.app_id))
            .child(Self::field("App Secret", &self.secret))
            .child(div().text_xs().text_color(rgb(MUTED)).child("Secret 使用密码输入框，不从已保存配置回显。留空保存会保留原 Secret；更换 App ID 时需要重新填写。凭证保存在仅当前用户可读的本地文件。"))
            .child(self.btn("save-credentials", "保存并建立连接", |this, _, cx| {
                let id = this.app_id.read(cx).value().to_string();
                if !id.starts_with("cli_") { this.notice = "请输入以 cli_ 开头的 App ID".into(); this.error = true; return; }
                let secret = this.secret.read(cx).value().to_string();
                this.pending_secret = secret.clone();
                this.credentials_checked = false;
                this.api("PUT", "/settings".into(), Some(json!({"app_id":id,"app_secret":secret})), "凭证已保存，请点击验证");
            }, cx).primary())
            .child(self.btn("check-credentials", "验证应用凭证", |this, _, _| this.api("POST", "/settings/check".into(), None, "应用身份验证成功；继续配置事件订阅"), cx))
            .child(Self::card("连接状态", &format!("{} · {}", self.state["runtime"]["feishu"].as_str().unwrap_or("等待服务"), self.state["runtime"]["feishu_error"].as_str().unwrap_or(""))))
            .child(self.btn("credentials-next", "继续配置消息权限 →", |this, _, _| this.go(3), cx).primary().disabled(!self.credentials_checked || self.busy))
            .into_any_element()
    }
    fn events(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 03", "让消息能来，也能回", "请在飞书开发者后台完成下面的配置。手动确认项不会被当作已经通过的自动检测。"))
            .child(Self::card("1. 导入应用权限", "在「权限管理」批量导入下方清单：发送机器人消息、读取单聊和群内 @ 消息、读取消息资源、上传附件，以及列出机器人所在群。"))
            .child(self.btn("copy-scopes-full", "复制完整权限 JSON（接收群内全部用户回复）", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(true)).unwrap())), cx).primary())
            .child(self.btn("copy-scopes", "复制基础权限 JSON（仅接收群内 @ 消息）", |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&backend::scopes(false)).unwrap())), cx))
            .child(Self::card("2. 使用长连接接收事件", "进入「事件与回调」→「事件配置」，选择「使用长连接接收事件」。先在上一步保存有效凭证，保持应用运行，再在后台保存。添加事件 im.message.receive_v1（接收消息）。"))
            .child(Self::card("群内所有回复都要转发？", "请选择完整权限 JSON，其中的 im:message.group_msg 用于接收群内全部用户消息，属于敏感权限，通常需要管理员批准。基础权限清单只接收群内 @ 机器人的消息。Bridge 只将已绑定聊天的消息送入项目。"))
            .child(Self::card("3. 发布应用并添加到群", "在「版本管理与发布」创建版本，设置可用范围并发布；如企业要求审核，等待管理员通过。将机器人加入用于此项目的群。权限或订阅改动后也需要发布新版本。"))
            .child(self.btn("confirmed-events", if self.setup.events_confirmed { "✓ 已手动确认事件与权限" } else { "我已配置权限和事件订阅" }, |this, _, _| { this.setup.events_confirmed = !this.setup.events_confirmed; this.go(3); }, cx))
            .child(self.btn("confirmed-release", if self.setup.published { "✓ 已手动确认发布与加群" } else { "我已发布应用并添加机器人到群" }, |this, _, _| { this.setup.published = !this.setup.published; this.go(3); }, cx))
            .child(self.btn("events-next", "继续检查本机权限 →", |this, _, _| this.go(4), cx).primary().disabled(!self.setup.events_confirmed || !self.setup.published))
            .into_any_element()
    }
    fn permissions(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 04", "只在需要的地方，打开权限", "检测结果以当前进程的实际访问为准。应用窗口、后台服务和 HERDR 主会话可能拥有不同的权限。"))
            .child(Self::card("项目文件夹 · 使用时需要", "通过 macOS 文件选择器选定目录，再检测读取和创建临时文件的能力。若项目位于桌面、文稿或下载，系统可能询问文件夹访问权限。你可以随时拒绝并选择其他目录。"))
            .child(self.btn("choose-access-folder", "选择项目文件夹并检测…", |this, window, cx| this.pick(false, window, cx), cx).primary())
            .child(Self::card("数据库 · 按任务需要", "应用自己的 SQLite 数据库在私有数据目录，无需额外系统权限。只有任务需要访问其他应用的数据库时，才选择该文件进行只读检测；不读取业务记录，不扫描邮件、聊天、浏览器或系统权限数据库。"))
            .child(self.btn("choose-db", "选择 SQLite 数据库并检测…", |this, window, cx| this.pick(true, window, cx), cx))
            .child(Self::card("完全磁盘访问权限 · 可选", "受系统保护的第三方应用数据可能需要此权限。请在系统设置中手动添加真正执行任务的应用（如 HERDR）；授权 Bridge 不保证已有 HERDR 进程能访问。开启后可能需要退出并重新打开对应应用，再重新检测。"))
            .child(div().flex().gap_3()
                .child(self.btn("files-settings", "打开「文件与文件夹」↗", |_, _, cx| cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders"), cx))
                .child(self.btn("disk-settings", "打开「完全磁盘访问权限」↗", |_, _, cx| cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"), cx)))
            .child(self.btn("reveal-app", "在 Finder 中显示此应用", |_, _, cx| {
                if let Ok(exe) = std::env::current_exe()
                    && let Some(bundle) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
                { cx.reveal_path(bundle); }
            }, cx))
            .child(Self::card("辅助功能、屏幕录制与自动化", "当前 Bridge 通过 HERDR 协议管理会话，不模拟键盘和读取屏幕，因此不会要求这些权限。未来启用对应功能时再说明用途并申请。"))
            .when(!self.access_result.is_empty(), |view| view.child(Self::card("本次检测结果", &self.access_result)))
            .child(self.btn("access-next", "继续添加项目 →", |this, _, _| this.go(5), cx).primary())
            .into_any_element()
    }
    fn conversation_picker(&mut self, project: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let project = project.map(str::to_owned);
        let mut choices = std::collections::BTreeMap::new();
        for chat in &self.chats {
            if let Some(id) = chat["chat_id"].as_str() {
                let mut chat = chat.clone();
                chat["kind"] = json!("group");
                choices.insert(id.to_owned(), chat);
            }
        }
        if let Some(observed) = self.state["conversations"]["items"].as_array() {
            for chat in observed {
                if let Some(id) = chat["chat_id"].as_str() {
                    let mut merged = chat.clone();
                    if merged["name"].as_str().unwrap_or("").is_empty()
                        && let Some(known) = choices.get(id)
                    {
                        merged["name"] = known["name"].clone();
                        if merged["kind"] == "unknown" {
                            merged["kind"] = known["kind"].clone();
                        }
                    }
                    choices.insert(id.to_owned(), merged);
                }
            }
        }
        if let Some(projects) = self.state["projects"].as_array() {
            for existing in projects {
                if let Some(id) = existing["chat_id"].as_str().filter(|id| !id.is_empty()) {
                    let chat = choices
                        .entry(id.to_owned())
                        .or_insert_with(|| json!({"chat_id":id,"kind":"unknown","name":""}));
                    chat["project_id"] = existing["id"].clone();
                }
            }
        }
        let filter = self.chat_search.read(cx).value().to_lowercase();
        let selected_id = self.chat.read(cx).value().to_string();
        let mut view = div().flex().flex_col().gap_3().p_5().bg(rgb(0xffffff)).rounded_xl().border_1().border_color(rgb(LINE))
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(if project.is_some() { "接收聊天 · 可关联多个私聊和群聊" } else { "接收聊天 · 可稍后设置" }))
            .child(Self::card("先发一条消息，就能找到私聊", "用飞书给机器人发送一条私聊，这个聊天会自动出现在下面。群聊先把机器人加到群，再更新群列表。选中聊天即可关联，无需查找 ID。"))
            .child(self.btn("refresh-conversations", "从飞书更新群列表", |this, _, _| this.api("GET", "/onboarding/chats".into(), None, "群列表已更新；私聊会在收到首条消息后自动出现"), cx))
            .child(Input::new(&self.chat_search));
        let mut count = 0;
        for (id, chat) in &choices {
            let kind = chat["kind"].as_str().unwrap_or("unknown");
            let label_kind = match kind {
                "p2p" => "私聊",
                "group" => "群聊",
                _ => "聊天",
            };
            let name = chat["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("名称暂未获取");
            let preview = chat["preview"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(42)
                .collect::<String>();
            let label = format!(
                "{label_kind} · {name}{}",
                if preview.is_empty() {
                    String::new()
                } else {
                    format!(" · {preview}")
                }
            );
            if !label.to_lowercase().contains(&filter) {
                continue;
            }
            count += 1;
            if count > 50 {
                continue;
            }
            let owner = chat["project_id"].as_str();
            let owned = project.as_deref().is_some_and(|p| owner == Some(p));
            let unavailable = owner.is_some() && !owned;
            let id_copy = id.clone();
            let kind = kind.to_owned();
            let name = chat["name"].as_str().unwrap_or("").to_owned();
            let destination = project.clone();
            let button = Button::new(SharedString::from(format!("conversation-{id}"))).label(format!("{}{}",if owned { "✓ " } else { "" },label))
                .selected(owned || (project.is_none() && selected_id==*id)).disabled(self.busy || unavailable || owned)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.chat.update(cx, |input,cx|input.set_value(id_copy.clone(),window,cx));
                    if let Some(project) = &destination { this.api("PUT", "/conversations".into(), Some(json!({"chat_id":id_copy,"project_id":project,"kind":kind,"name":name})), "聊天已关联；等待中的消息将送入这个项目"); }
                }));
            let mut row = div().flex().gap_2().items_center().child(button);
            if unavailable {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("已关联其他项目"),
                );
            }
            if owned {
                let id = id.clone();
                row = row.child(
                    Button::new(SharedString::from(format!("unbind-{id}")))
                        .label("解除关联")
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.api(
                                "PUT",
                                "/conversations".into(),
                                Some(json!({"chat_id":id,"project_id":null})),
                                "聊天已解除关联，消息记录保留",
                            )
                        })),
                );
            }
            view = view.child(row);
        }
        if count == 0 {
            view = view.child(Self::card(if choices.is_empty() { "还没有可选择的聊天" } else { "没有匹配聊天" }, if choices.is_empty() { "请先在连接设置保存机器人凭证。私聊发送首条消息后即可出现；群列表读取失败时，检查应用是否发布、机器人是否加群，以及群信息读取权限。" } else { "换一个群名或消息关键词试试。" }));
        } else if count > 50 {
            view = view.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("仅显示前 50 个匹配聊天，请输入更具体的名称。"),
            );
        }
        if let Some(project) = &project {
            let default = self.state["conversations"]["private_default"].as_str();
            if default == Some(project) {
                view = view.child(Self::card("本项目正在接收未关联私聊", "机器人收到的、未单独关联其他项目的私聊，会进入本项目。Agent 仍通过 to 选择回复对象。"))
                    .child(self.btn("disable-private-default","停止默认接收私聊",|this,_,_|this.api("PUT","/conversations/private-default".into(),Some(json!({"project_id":null})),"已停止默认接收私聊"),cx));
            } else {
                let project = project.clone();
                view = view.child(self.btn("enable-private-default","将未关联私聊默认接入本项目…",move |_,window,cx| {
                    let answer = window.prompt(PromptLevel::Info,"将未关联的私聊接入这个项目？",Some("此机器人可用范围内的用户，其未单独关联其他项目的私聊将进入本项目主会话。已有其他默认项目时会被替换。Agent 仍自行选择 to 发送目标。"),&["取消","接入本项目"],cx);
                    let project = project.clone();
                    cx.spawn(async move |entity,cx| { if answer.await.ok()==Some(1) { let _ = entity.update(cx, |this,_|this.api("PUT","/conversations/private-default".into(),Some(json!({"project_id":project})),"本项目已设为未关联私聊的接收项目")); } }).detach();
                },cx));
            }
        }
        view = view.child(self.btn(
            "advanced-chat",
            "高级设置：手动填写聊天 ID",
            |this, _, _| this.show_advanced_chat = !this.show_advanced_chat,
            cx,
        ));
        if self.show_advanced_chat {
            view = view.child(Self::field("聊天 ID（可选）", &self.chat));
            if let Some(project) = project {
                view = view.child(self.btn("bind-manual","关联手动填写的聊天",move |this,_,cx|this.api("PUT","/conversations".into(),Some(json!({"chat_id":this.chat.read(cx).value().to_string(),"project_id":project})),"聊天已关联"),cx));
            }
        }
        view.into_any_element()
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
        let picker = self.conversation_picker(None, cx);
        div().flex().flex_col().gap_4()
            .child(Self::title("SETUP / 05", "给第一个项目，一个主会话", "一个代码目录、一种 CLI，可接入多个私聊和群聊。Agent 通过 to 自主选择回复目的地。"))
            .child(Self::field("项目名称", &self.name))
            .child(Self::field("项目目录（可手动输入绝对路径）", &self.path))
            .when(!picked.is_empty(), |v| v.child(div().text_sm().text_color(rgb(ACCENT)).child(format!("已选择：{picked}"))))
            .child(self.btn("pick-project", "用系统选择器选择文件夹…", |this, window, cx| this.pick(false, window, cx), cx))
            .child(choices)
            .child(picker)
            .child(Self::card("可以先在本机试用", "没有飞书凭证或群聊也能创建项目、启动 Agent，并从工作台发送任务。首次使用 CLI 可能需要在 HERDR 中完成登录或项目目录信任。"))
            .child(self.btn("create-project", "创建项目 →", |this, _, cx| {
                let manual = this.path.read(cx).value().to_string();
                let path = if manual.trim().is_empty() { this.picked_path.clone() } else { manual };
                let name = this.name.read(cx).value().to_string();
                if name.trim().is_empty() || path.is_empty() { this.notice = "请填写项目名称并选择目录".into(); this.error = true; return; }
                let agent = ["codex","claude","pi","agy"][this.agent];
                this.api("POST", "/projects".into(), Some(json!({"name":name,"path":path,"agent":agent,"chat_id":this.chat.read(cx).value().to_string()})), "项目已创建。点击「启动主会话」开始工作");
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
                "所有项目，触手可及",
                "飞书对话和本机任务在这里汇合。关闭窗口后，已启动的后台服务与主会话继续运行。",
            ))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        self.btn(
                            "add-project",
                            "＋ 添加项目",
                            |this, window, cx| {
                                this.chat
                                    .update(cx, |input, cx| input.set_value("", window, cx));
                                this.go(5);
                            },
                            cx,
                        )
                        .primary(),
                    )
                    .child(self.btn(
                        "return-setup",
                        "重新查看使用引导",
                        |this, _, _| this.go(0),
                        cx,
                    )),
            )
            .child(Self::card(
                "实时连接",
                &format!(
                    "HERDR：{}    飞书：{}",
                    self.state["runtime"]["core"].as_str().unwrap_or("等待服务"),
                    self.state["runtime"]["feishu"].as_str().unwrap_or("未连接")
                ),
            ));
        if projects.is_empty() {
            body = body.child(Self::card(
                "还没有项目",
                "先添加一个项目。可以跳过飞书连接，在本机体验主会话。",
            ));
        }
        for project in &projects {
            let id = project["id"].as_str().unwrap_or_default().to_owned();
            let chat_id = project["chat_id"].as_str().unwrap_or_default().to_owned();
            let label = format!(
                "{}  ·  {}  ·  {}",
                project["name"].as_str().unwrap_or("项目"),
                project["agent"].as_str().unwrap_or(""),
                if project["terminal"]
                    .as_str()
                    .is_some_and(|t| self.state["runtime"]["terminals"][t]["state"] == "finished")
                {
                    "已退出"
                } else if project["terminal"].as_str().is_some_and(|t| {
                    self.state["runtime"]["terminals"][t]["status"]["state"] == "blocked"
                }) {
                    "等待交互"
                } else if project["ready"] == true {
                    "已就绪"
                } else if !project["terminal"].is_null() {
                    "会话待就绪"
                } else {
                    "未启动"
                }
            );
            body = body.child(
                Button::new(SharedString::from(format!("project-{id}")))
                    .label(label)
                    .selected(id == self.selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = id.clone();
                        this.detail = Value::Null;
                        this.chat
                            .update(cx, |input, cx| input.set_value(chat_id.clone(), window, cx));
                        let _ = this.backend.tx.send(CommandRequest::Watch(id.clone()));
                        this.api("GET", format!("/projects/{id}"), None, "已选择项目");
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
            let terminal = project["terminal"].as_str().unwrap_or("尚未创建");
            let picker = self.conversation_picker(Some(&id), cx);
            let issue = self.state["runtime"]["view_errors"][&id]
                .as_str()
                .unwrap_or("")
                .to_owned();
            if !issue.is_empty() {
                body = body.child(Self::card("Herdr App 展示提示", &issue));
            }
            if let Some(t) = project["terminal"].as_str()
                && self.state["runtime"]["terminals"][t]["state"] == "finished"
            {
                body = body.child(Self::card("主会话已经退出", "点击「启动主会话」重新启动。启动成功后会自动在 Herdr App 展示；首次登录、项目信任和权限确认也会显示在那里。"));
            }
            if let Some(t) = project["terminal"].as_str()
                && self.state["runtime"]["terminals"][t]["status"]["state"] == "blocked"
            {
                let reason = self.state["runtime"]["terminals"][t]["status"]["msg"]
                    .as_str()
                    .unwrap_or("CLI 正在等待交互");
                body = body.child(Self::card("主会话正在等待交互", &format!("{reason}\n已识别的 Antigravity 项目目录信任会自动确认。其他登录、提问或权限提示可在 Herdr 中查看；排队消息会保留，原进程继续运行。")));
            }

            body = body
                .child(Self::card(project["name"].as_str().unwrap_or("项目"), &format!("{}\n终端：{}", project["path"].as_str().unwrap_or(""), terminal)))
                .child(div().flex().flex_wrap().gap_2()
                    .child(self.btn("start", "启动主会话", move |this, _, _| this.api("POST", format!("/projects/{id}/start"), None, "启动请求已完成。等待主会话完成登录、信任和 MCP 登记"), cx).primary())
                    .child(self.btn("resume", "恢复会话", move |this, _, _| this.api("POST", format!("/projects/{id2}/resume"), None, "恢复请求已完成"), cx))
                    .child(self.btn("show", "在 HERDR 监控", move |this, _, _| this.api("POST", format!("/projects/{id3}/show"), None, "主会话已在 Herdr App 标签页中展示"), cx))
                    .child(self.btn("refresh-project", "刷新消息", move |this, _, _| this.api("GET", format!("/projects/{id4}"), None, "消息已刷新"), cx))
                    .child(self.btn("stop", "停止会话…", move |_this, window, cx| {
                        let answer = window.prompt(PromptLevel::Warning, "停止这个项目的主会话？", Some("将结束 Agent 及关联子终端。项目配置和消息记录会保留。"), &["取消", "停止"], cx);
                        let id = id5.clone();
                        cx.spawn(async move |entity, cx| {
                            if answer.await.ok() == Some(1) { let _ = entity.update(cx, |this, _| this.api("POST", format!("/projects/{id}/stop"), None, "会话已停止")); }
                        }).detach();
                    }, cx)))
                .child(picker)
                .child(Self::field("本机任务", &self.message))
                .child(self.btn("inject", "发送到主会话", move |this, _, cx| {
                    let text = this.message.read(cx).value().to_string();
                    if text.trim().is_empty() { return; }
                    this.pending_message = text.clone();
                    this.api("POST", format!("/projects/{id7}/messages"), Some(json!({"content":text})), "任务已加入队列。请在消息记录中确认处理状态");
                }, cx).primary())
                .child(Self::card("验证消息链路", "给机器人发送一条私聊，或在已关联的群中发送消息。确认入站记录、Agent 确认与出站回复。Agent 使用 to 选择接收聊天或用户，不强制回复消息来源。群消息的接收范围由飞书权限决定。"))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("消息记录"));
            if messages.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("暂无消息，或尚未刷新。"),
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
                    .child("herdr / bridge"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .mb_8()
                    .child("你的项目，始终在线"),
            );
        for (i, step) in STEPS.iter().enumerate() {
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
        sidebar = sidebar.child(div().flex_1()).child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .line_height(relative(1.8))
                .child("NATIVE ON YOUR MAC\nRust · GPUI\n0.2.0 / 本地预览"),
        );
        let content = match page {
            0 => self.welcome(cx),
            1 => self.bot(cx),
            2 => self.credentials(cx),
            3 => self.events(cx),
            4 => self.permissions(cx),
            5 => self.project_form(cx),
            _ => self.workspace(cx),
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
                            .child(format!("工作空间  /  {}", STEPS[page]))
                            .child(if self.busy {
                                "●  正在处理"
                            } else {
                                "●  本机私有工作台"
                            }),
                    )
                    .child(
                        div()
                            .id("content-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_10()
                            .child(div().max_w(px(850.)).child(content)),
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
fn main() {
    let dir = std::env::args()
        .skip_while(|v| v != "--data-dir")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(herdr_feishu_bridge::config::default_dir);
    let backend = Backend::start(dir);
    Application::new().run(move |cx| {
        gpui_component::init(cx);
        Theme::change(ThemeMode::Light, None, cx);
        Theme::global_mut(cx).colors.primary = rgb(ACCENT).into();
        Theme::global_mut(cx).colors.primary_hover = rgb(0x23685e).into();
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![Menu {
            name: "Herdr Bridge".into(),
            items: vec![MenuItem::action("退出 Herdr Bridge", Quit)],
        }]);
        let bounds = Bounds::centered(None, size(px(1180.), px(860.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(980.), px(680.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Herdr Bridge".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| Desktop::new(backend, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .expect("无法创建桌面窗口");
        cx.activate(true);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
