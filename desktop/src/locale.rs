use serde::{Deserialize, Serialize};

/// English remains the default for existing preferences and new installations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    En,
    #[serde(rename = "zh-CN", alias = "zh")]
    Zh,
}
impl Language {
    pub fn other(self) -> Self {
        match self {
            Self::En => Self::Zh,
            Self::Zh => Self::En,
        }
    }
    pub fn text(self, source: &str) -> String {
        if self == Self::En {
            return source.to_owned();
        }
        let translated = match source {
            "Welcome" => "欢迎使用",
            "Create a Feishu bot" => "创建飞书机器人",
            "Connect your bot" => "连接机器人",
            "Message events" => "消息事件",
            "Local permissions" => "本机权限",
            "Add your first project" => "添加首个项目",
            "Workspace" => "工作区",
            "App Updates" => "应用更新",
            "Environment" => "本地环境",
            "For example: personal website" => "例如：个人网站",
            "Choose the project folder" => "选择项目文件夹",
            "Stored only on this Mac" => "仅保存在本机",
            "Starting the local service..." => "正在启动本地服务…",
            "Local service ready. Connect a bot or create a local project." => {
                "本地服务已就绪，可以连接机器人或添加项目。"
            }
            "Local service connection restored" => "已重新连接本地服务",
            "Local service connection failed" => "无法连接本地服务",
            "Working..." => "正在处理…",
            "The background service exited. Reopen the app." => "后台服务已退出，请重新打开应用。",
            "Choose a SQLite file to check" => "选择要检查的 SQLite 文件",
            "Choose a project folder" => "选择项目文件夹",
            "YOUR AGENTS, A LITTLE CLOSER" => "随时连接你的智能体",
            "Keep working.\nTake the conversation with you." => "让工作继续，\n让对话随行。",
            "Talk to your project's main session in Feishu, then continue on your Mac. Connect your first bot to get started." => {
                "在飞书里与项目主会话交流，回到 Mac 后接着做。连接你的第一个机器人即可开始。"
            }
            "01  Connect Feishu" => "01  连接飞书",
            "Create an enterprise app bot. Credentials stay on your Mac." => {
                "创建企业自建机器人，凭据保存在本机。"
            }
            "02  Choose a project" => "02  选择项目",
            "Connect an installed CLI to a persistent HERDR session." => {
                "连接已有 CLI 与 Herdr 持久会话。"
            }
            "03  Continue the conversation" => "03  继续对话",
            "Messages, tasks, and files reach the same main session." => {
                "文本、任务和附件，回到同一主会话。"
            }
            "You control access" => "由你决定授权",
            "Choose the project folder yourself. Protected files may require additional access; ordinary projects do not need Full Disk Access, Accessibility, or Screen Recording." => {
                "自行选择项目文件夹。访问受保护文件时再申请权限；普通项目无需完全磁盘访问、辅助功能或屏幕录制权限。"
            }
            "Start setup →" => "开始设置 →",
            "Try locally and connect Feishu later" => "先添加本地项目，稍后连接飞书",
            "SETUP / 01" => "机器人设置 / 1",
            "Create your Feishu bot" => "创建飞书机器人",
            "Use an enterprise custom app with bot capability. Group Webhook bots cannot receive the messages needed for two-way conversations." => {
                "使用带机器人能力的企业自建应用。群 Webhook 机器人无法接收双向对话所需的消息。"
            }
            "1. Create an app" => "1. 创建应用",
            "Open the Feishu developer console and create an enterprise custom app. Give it a name and description. Your organization must allow you to create apps." => {
                "打开飞书开发者后台，创建企业自建应用并填写名称与描述。需要组织允许创建应用。"
            }
            "Open Feishu developer console ↗" => "打开飞书开发者后台 ↗",
            "2. Enable bot capability" => "2. 开启机器人能力",
            "Open the app and enable Bot under Add App Capabilities. You do not need a Webhook or a public server." => {
                "进入应用，在“添加应用能力”中开启机器人。无需 Webhook 或公网服务器。"
            }
            "3. Find the credentials" => "3. 获取凭据",
            "Copy the App ID and App Secret from Credentials and Basic Information. Paste them into the next step on your Mac; do not send them in a chat." => {
                "在“凭证与基础信息”中复制 App ID 和 App Secret，在下一步填入本机应用。"
            }
            "Already have a bot?" => "已有机器人？",
            "Use a dedicated app for Dauerbot. If another program listens to the same bot, Feishu may deliver messages to that program instead." => {
                "建议为 Dauerbot 使用专用应用。同一个机器人被其他程序监听时，消息可能被其他程序接收。"
            }
            "Bot enabled. Add credentials →" => "已开启机器人，填写凭据 →",
            "SETUP / 02" => "机器人设置 / 2",
            "Connect the bot to this Mac" => "将机器人连接到这台 Mac",
            "The local service connects after you save. Credential validation checks app identity; message permissions and subscriptions come next." => {
                "保存后本地服务将连接飞书。先验证应用身份，再配置消息权限和事件订阅。"
            }
            "The secret stays masked and saved values are never shown. Leave it blank to keep the current secret; enter a new one when changing App ID. Credentials stay in a local file readable only by your user." => {
                "密钥仅保存在本机，不会回显。留空可保留当前密钥；修改 App ID 时请填写新密钥。"
            }
            "Save and connect" => "保存并连接",
            "Enter an App ID starting with cli_" => "请输入以 cli_ 开头的 App ID",
            "Credentials saved. Validate them to continue." => "凭据已保存，请验证后继续。",
            "Validate credentials" => "验证凭据",
            "App identity verified. Configure message events next." => {
                "应用身份验证成功，接下来配置消息事件。"
            }
            "Connection status" => "连接状态",
            "Configure bot" => "配置机器人",
            "Feishu connection needs attention" => "飞书连接需要处理",
            "Configure message permissions →" => "配置消息权限 →",
            "SETUP / 03" => "机器人设置 / 3",
            "Receive messages and send replies" => "接收消息与发送回复",
            "Complete these steps in the Feishu developer console. Manual confirmations are separate from automatic checks." => {
                "在飞书开发者后台完成以下步骤，然后在此确认。"
            }
            "1. Import app permissions" => "1. 导入应用权限",
            "In Permissions, import the JSON below to send bot messages, receive direct messages and group mentions, read message resources, upload files, and read group context." => {
                "在“权限管理”中导入权限 JSON，允许机器人发送消息、接收私聊与群内 @ 消息、读取消息资源、上传文件和读取群成员信息。"
            }
            "Copy full permission JSON (all group messages)" => "复制完整权限 JSON（所有群消息）",
            "Copy basic permission JSON (group mentions)" => "复制基础权限 JSON（群内 @ 消息）",
            "2. Receive events over a long connection" => "2. 使用长连接接收事件",
            "Under Events and Callbacks, choose long connections. Save valid credentials in Dauerbot and keep it running before saving the subscription. Add im.message.receive_v1 to receive messages." => {
                "在“事件与回调”中选择长连接。先在 Dauerbot 保存有效凭据并保持运行，再添加 im.message.receive_v1 消息事件并保存订阅。"
            }
            "Need every group reply?" => "需要接收所有群回复？",
            "The full JSON includes im:message.group_msg for all user messages in groups; your administrator may need to approve it. The basic set only receives bot mentions. All messages this bot receives reach its fixed project." => {
                "完整权限包含 im:message.group_msg，可接收群内所有用户消息，可能需要管理员审批。基础权限仅接收 @ 机器人消息。机器人收到的所有消息都会进入绑定项目。"
            }
            "3. Publish the app and add the bot" => "3. 发布应用并添加机器人",
            "Create a version in Version Management and Release, choose its availability, and publish it. Wait for administrator approval if required. Add the bot to a group or use direct messages. Publish a new version after changing permissions or subscriptions." => {
                "在“版本管理与发布”中创建版本，设置可用范围并发布。如需审批，请等待管理员通过。可以将机器人加入群聊，也可以直接私聊。修改权限或订阅后需发布新版本。"
            }
            "✓ Events and permissions confirmed manually" => "✓ 已确认权限与事件订阅",
            "I configured permissions and event subscriptions" => "我已配置权限和事件订阅",
            "✓ Publishing and bot access confirmed manually" => "✓ 已确认应用发布与可用范围",
            "I published the app and made the bot available" => "我已发布应用并设置机器人可用范围",
            "Check local permissions →" => "检查本机权限 →",
            "SETUP / 04" => "本机权限",
            "Grant access where you need it" => "按需授予访问权限",
            "Checks reflect this process's actual access. The desktop, daemon, and HERDR agent may have different permissions." => {
                "检查当前应用的实际访问能力。桌面端、后台服务和 Herdr 中的 Agent 可能具有不同权限。"
            }
            "Project folder · required for project work" => "项目文件夹",
            "Select a folder with the macOS picker to check reading and temporary-file creation. macOS may request access to Desktop, Documents, or Downloads. You can decline and choose another folder." => {
                "通过 macOS 文件选择器选择文件夹，检查读取和临时文件创建能力。访问桌面、文稿或下载目录时，系统可能请求授权。"
            }
            "Choose and check a project folder..." => "选择并检查项目文件夹…",
            "Databases · when a task needs them" => "数据库 · 任务需要时检查",
            "Dauerbot's SQLite database lives in its private data directory. Select another database only when a task needs it. Checks read schema metadata without reading business records or scanning private app databases." => {
                "Dauerbot 的数据库位于本机数据目录。任务需要访问其他数据库时，可主动选择文件检查。检查只读取表结构，不读取业务数据。"
            }
            "Choose and check a SQLite database..." => "选择并检查 SQLite 数据库…",
            "Full Disk Access · optional" => "完全磁盘访问 · 按需开启",
            "Protected app data may require Full Disk Access. In System Settings, add the app that actually runs the task, such as HERDR. Granting Dauerbot access does not grant it to existing HERDR processes. Reopen the relevant app and check again after granting access." => {
                "受保护的应用数据可能需要“完全磁盘访问”。请在系统设置中添加实际执行任务的应用，例如 Herdr。授权 Dauerbot 不会同时授权 Herdr。授权后重新打开相应应用再检查。"
            }
            "Open Files and Folders ↗" => "打开“文件与文件夹”设置 ↗",
            "Open Full Disk Access ↗" => "打开“完全磁盘访问”设置 ↗",
            "Reveal this app in Finder" => "在 Finder 中显示此应用",
            "Accessibility, Screen Recording, and Automation" => "辅助功能、屏幕录制与自动化",
            "Dauerbot manages sessions through HERDR's protocol, without screen capture or system input simulation. These permissions are not required." => {
                "Dauerbot 通过 Herdr 协议管理会话，无需辅助功能、屏幕录制或自动化权限。"
            }
            "Access check result" => "访问检查结果",
            "Add a project →" => "添加项目 →",
            "ADD PROJECT" => "添加项目",
            "One bot belongs to one project. Direct and group messages reach its active agent, which you can switch in the workspace." => {
                "一个机器人绑定一个项目。私聊与群聊消息都进入当前 Agent，可随时在工作区切换。"
            }
            "Selected:" => "已选择：",
            "Project name" => "项目名称",
            "Project folder (or enter an absolute path)" => "项目文件夹（也可输入绝对路径）",
            "Choose a folder..." => "选择文件夹…",
            "Automatic binding" => "自动绑定",
            "Your first project becomes this bot's fixed owner. You do not need to select chats or enter IDs. Direct and group messages both reach this project." => {
                "首个项目会自动绑定机器人，无需选择聊天或填写 ID。私聊与群聊消息均进入该项目。"
            }
            "Continue in Herdr App" => "在 Herdr App 中继续",
            "Start a session and open it in Herdr App. Conversations happen in Feishu; the live CLI stays in Herdr." => {
                "启动会话后，在 Herdr App 中查看执行情况。在飞书里交流，在 Herdr 中操作 CLI。"
            }
            "Create project →" => "创建项目 →",
            "Enter a project name and choose a folder" => "请输入项目名称并选择文件夹",
            "Project created. Start the main session to begin." => "项目已创建，可以启动主会话了。",
            "WORKSPACE" => "工作区",
            "Projects" => "项目",
            "Talk in Feishu. Follow the live session in Herdr App." => {
                "在飞书里交流，在 Herdr App 中查看执行情况。"
            }
            "Disconnected" => "未连接",
            "Not configured" => "未配置",
            "Feishu" => "飞书",
            "＋ Add project" => "＋ 添加项目",
            "No projects yet" => "还没有项目",
            "Add a project, choose an agent, and start your main session." => {
                "添加项目，选择 Agent，然后启动主会话。"
            }
            "Delivery paused" => "已暂停接收",
            "Exited" => "已退出",
            "Needs input" => "等待操作",
            "CLI error" => "CLI 出错",
            "Ready" => "已就绪",
            "Starting" => "启动中",
            "Stopped" => "未运行",
            "Feishu bot bound · Direct and group messages reach this project" => {
                "已绑定飞书机器人 · 私聊与群聊消息均进入此项目"
            }
            "Local project" => "本地项目",
            "Could not open Herdr App" => "无法打开 Herdr App",
            "The main session needs input" => "主会话需要你操作",
            "Open Herdr App to continue sign-in or answer the CLI prompt." => {
                "打开 Herdr App，继续登录或处理 CLI 提示。"
            }
            "Open Herdr App to see the error and continue." => "打开 Herdr App 查看错误并继续。",
            "Monitor in Herdr App" => "在 Herdr App 中查看",
            "Main session opened in a Herdr App tab" => "已在 Herdr App 标签页中打开主会话",
            "Stop session..." => "停止会话…",
            "Stop this project's main session?" => "确定停止此项目的主会话？",
            "This ends the CLI process. Project settings stay intact." => {
                "这将结束 CLI 进程，项目配置会保留。"
            }
            "Cancel" => "取消",
            "Stop" => "停止",
            "Session stopped" => "会话已停止",
            "Start main session" => "启动主会话",
            "Starting the main session..." => "正在启动主会话…",
            "Resume session" => "恢复上次会话",
            "Resume requested" => "正在恢复会话",
            "Resume Feishu delivery" => "恢复接收飞书消息",
            "Pause Feishu delivery" => "暂停接收飞书消息",
            "Feishu delivery resumed" => "已恢复接收飞书消息",
            "Feishu delivery paused" => "已暂停接收飞书消息",
            "Active agent" => "当前 Agent",
            "Agent switched. The bot and project binding are preserved." => {
                "Agent 已切换，项目与机器人绑定保持不变。"
            }
            "Switching restarts the main session. Project and bot bindings stay intact." => {
                "切换会重启主会话，项目和机器人绑定保持不变。"
            }
            "ENVIRONMENT" => "本地环境",
            "Local environment" => "本地环境",
            "Installed command-line tools. Check sign-in inside each CLI." => {
                "检查本机已安装的命令行工具。登录状态请在各 CLI 中确认。"
            }
            "Check environment again" => "重新检查环境",
            "Checking for updates..." => "正在检查更新…",
            "New version available:" => "有新版本：",
            "Downloading and verifying update" => "正在下载并验证更新",
            "Update installed. Restart the desktop to apply." => {
                "更新已安装，重新打开桌面应用后生效。"
            }
            "You have the latest version on this channel" => "当前已是此通道的最新版本",
            "Update failed" => "更新失败",
            "Automatic checks run every six hours when enabled." => {
                "开启自动更新后，每六小时检查一次。"
            }
            "UPDATES" => "应用更新",
            "Signed updates download in the background and take effect next time you open the app. Project files, bot settings, and active sessions stay intact." => {
                "更新将在后台下载，下次打开应用时生效。项目文件、机器人设置和运行中的会话会保留。"
            }
            "Current version" => "当前版本",
            "Preview channel" => "预览通道",
            "Stable channel" => "稳定通道",
            "Update status" => "更新状态",
            "Check for updates" => "检查更新",
            "Download and install" => "下载并安装",
            "Restart desktop now" => "立即重启桌面应用",
            "Roll back" => "回退版本",
            "Disable automatic updates" => "关闭自动更新",
            "Enable automatic updates" => "开启自动更新",
            "Switch to preview channel" => "切换到预览通道",
            "Switch to stable channel" => "切换到稳定通道",
            "Automatic updates" => "自动更新",
            "Checks run every six hours by default. Manual updates remain available when automatic updates are off. Stable excludes prereleases; preview includes them. Installation requires a verified signature from the same developer." => {
                "默认每六小时检查一次。关闭自动更新后仍可手动检查。稳定通道只提供正式版，预览通道也包含预发布版本。安装前会验证开发者签名。"
            }
            "Release notes" => "更新说明",
            "Project & Bot Manager" => "项目与机器人管理",
            "Add project" => "添加项目",
            "Feishu bot setup" => "飞书机器人配置",
            "macOS permissions" => "macOS 权限",
            "App Updates · restart to apply" => "应用更新 · 重启生效",
            "Independent instance · This window uses separate data from your everyday workspace." => {
                "独立实例 · 此窗口的数据与日常工作区分开保存。"
            }
            "Create bot" => "创建机器人",
            "Connect bot" => "连接机器人",
            "Permissions & events" => "权限与事件",
            "App Updates…" => "应用更新…",
            "Quit Dauerbot" => "退出 Dauerbot",
            "connected" => "已连接",
            "disconnected" => "未连接",
            "connecting" => "连接中",
            "configured" => "已配置",
            "not_configured" => "未配置",
            "Connected" => "已连接",
            "Connecting" => "连接中",
            "Connected · Waiting for messages" => "已连接 · 等待消息",
            "Waiting for service" => "等待服务启动",
            "Application database" => "应用数据库",
            "Not found · install it and check again" => "未找到 · 安装后重新检查",
            "Readable · Dauerbot's SQLite database does not need Full Disk Access" => {
                "可读取 · Dauerbot 数据库无需完全磁盘访问权限"
            }
            "Unreadable · check data-directory permissions" => "无法读取 · 请检查数据目录权限",
            "The selected folder is readable and writable. Verify the HERDR agent's own permissions in its session." => {
                "所选文件夹可读写。请在 Herdr 会话中确认 Agent 的实际访问权限。"
            }
            "The selected SQLite database opens read-only; no business records were read. This checks this file's current access, not Full Disk Access." => {
                "所选 SQLite 数据库可只读打开，未读取业务记录。此结果仅代表该文件当前可访问。"
            }
            _ => {
                if let Some(path) = source
                    .strip_prefix("Found · ")
                    .and_then(|s| s.strip_suffix(" (check sign-in in the CLI)"))
                {
                    return format!("已找到 · {path}（请在 CLI 中确认登录）");
                }
                if let Some(path) = source.strip_prefix("Selected: ") {
                    return format!("已选择：{path}");
                }
                if let Some(path) = source.strip_prefix("Checking: ") {
                    return format!("正在检查：{path}");
                }
                if let Some(detail) = source.strip_prefix("macOS or the filesystem denied access. ")
                {
                    let detail = detail.split("Details: ").last().unwrap_or(detail);
                    return format!(
                        "访问被拒绝。请重新选择文件夹，检查系统“文件与文件夹”权限；受保护数据库可能需要完全磁盘访问。\n{detail}"
                    );
                }
                if let Some(detail) = source.strip_prefix("Check failed: ") {
                    return format!("检查失败：{detail}");
                }
                return source.to_owned();
            }
        };
        translated.to_owned()
    }
}
