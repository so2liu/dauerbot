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
            return match source {
                "disabled" => "Disabled",
                "unavailable" => "Manager unavailable",
                "held" => "Waiting",
                "sleeping" => "Standby",
                "preparing_handoff" => "Preparing handoff",
                "switching_cli" => "Switching CLI",
                "recovery_pending" | "recovery_stopping" | "recovery_starting" => {
                    "Recovering Agent"
                }
                "repairing" => "Repair Agent available",
                "starting" => "Starting",
                "ready" => "Ready",
                "busy" | "running" => "Working",
                "needs_attention" => "Needs attention",
                "update_required" => "Agent update required",
                "taken_over" => "Manual control",
                "offline" => "HERDR unavailable",
                "paused" => "Paused",
                "stopped" => "Stopped",
                "exited" => "Exited",
                "error" => "Error",
                "queued" => "Queued",
                "submitted" | "acknowledged" => "In progress",
                "uncertain" => "Review needed",
                "completed" => "Completed",
                "cancelled" => "Cancelled",
                "waiting" | "deferred" => "Waiting",
                "failed" => "Failed",
                _ => source,
            }
            .to_owned();
        }
        let translated = match source {
            "disabled" => "未启用",
            "unavailable" => "管理智能体暂不可用",
            "The management request timed out" => "管理模型请求超时",
            "The management provider connection failed" => "管理模型连接失败",
            "The management provider rejected access" => "管理模型接口拒绝了访问",
            "The management model was not found" => "未找到配置的管理模型",
            "The management provider is rate limited" => "管理模型接口正在限流",
            "The management provider could not serve the request" => "管理模型接口未能处理请求",
            "The management request exceeded the context limit" => "管理模型请求超过上下文限制",
            "The management provider rejected the request" => "管理模型接口拒绝了请求",
            "The management response was incomplete or invalid" => "管理模型回复不完整或格式不合法",
            "The management context could not be prepared" => "管理上下文未能准备完成",
            "The management run could not continue" => "管理流程未能继续",
            "held" => "等待中",
            "Project Agent" => "项目主智能体",
            "Native CLI" => "原生 CLI",
            "Built-in manager" => "内置管理智能体",
            "Manager model settings" => "管理模型设置",
            "Hide manager settings" => "收起管理模型设置",
            "OpenAI-compatible endpoint" => "OpenAI 兼容接口地址",
            "Manager model" => "管理模型",
            "Manager API key" => "管理模型 API 密钥",
            "API key saved. Leave blank to keep it." => "API 密钥已保存，留空可保留现有密钥。",
            "No manager API key saved" => "尚未保存管理模型 API 密钥",
            "Save manager settings" => "保存管理模型设置",
            "Manager settings saved" => "管理模型设置已保存",
            "Manager settings are unavailable" => "管理模型设置暂不可用",
            "Clear saved API key" => "清除已保存的 API 密钥",
            "Coding CLI" => "编码 CLI",
            "Coding worker model" => "编码智能体模型",
            "Coding worker" => "编码智能体",
            "Show coding terminal" => "查看编码终端",
            "Coding worker terminal" => "编码智能体终端",
            "The manager uses its own model and delegates coding to your native CLI. CLI login and model settings stay separate." => {
                "管理智能体使用独立模型，把编码工作委派给原生 CLI。原生 CLI 的登录和模型设置独立保留。"
            }
            "The terminal belongs to the coding worker. Model and repair controls remain available if the manager cannot connect." => {
                "终端属于编码智能体。管理模型无法连接时，仍可使用模型和修复控制。"
            }
            "No shell access" => "无 Shell 工具权限",
            "Show subagents" => "展开子智能体",
            "View Agent details" => "查看智能体信息",
            "Observed subagent · read only" => "已观测的子智能体 · 只读",
            "Hide subagents" => "收起子智能体",
            "Agent details may be out of date" => "智能体信息可能已过期",
            "Some Agent details are unavailable" => "部分智能体信息不可用",
            "Requested · switching model" => "已请求 · 正在切换模型",
            "Invalid or stale model catalogue response" => "模型目录响应无效或已过期，请刷新",
            "Model unavailable" => "模型信息暂不可用",
            "Model unknown" => "模型信息未知",
            "Native model catalogue could not be loaded" => "无法加载 CLI 模型目录",
            "Refreshing model catalogue…" => "正在刷新模型目录…",
            "Native model selection is unknown" => "尚未获知 CLI 的模型设置",

            "Show sidebar" => "显示侧栏",
            "Hide sidebar" => "隐藏侧栏",
            "Search projects · ⌘F" => "搜索项目 · ⌘F",
            "No matching projects" => "没有匹配的项目",
            "Reveal folder" => "在访达中显示",
            "Copy path" => "复制路径",
            "Path copied" => "路径已复制",
            "Your instructions, one message away." => "发条飞书消息，即可开始。",
            "Managed in Feishu" => "在飞书中管理",
            "Nothing scheduled yet" => "暂无定时安排",

            "Native CLI/session settings" => "CLI 与原生会话设置",
            "Remove project model override" => "移除项目模型覆盖",
            "Choose model" => "选择模型",
            "Confirmed by CLI" => "CLI 已确认",
            "Requested · awaiting native confirmation" => "已选择 · 等待 CLI 确认",
            "Uses native CLI/session settings" => "沿用 CLI 与原生会话设置",
            "This project only. Native login and global CLI settings stay in place." => {
                "仅用于当前项目，保留 CLI 的登录和全局设置。"
            }
            "Model change requested" => "已请求切换模型",
            "Model controls are unavailable" => "模型控件暂不可用",
            "Model ID" => "模型 ID",
            "Provider (optional)" => "提供方（可选）",
            "Use model ID" => "使用模型 ID",
            "Enter a model ID" => "请输入模型 ID",
            "Loading models…" => "正在加载模型…",
            "Refresh" => "刷新",
            "Preparing handoff…" | "preparing_handoff" => "正在准备交接…",
            "Switching CLI…" | "switching_cli" => "正在切换 CLI…",
            "Selected CLI is ready." => "所选 CLI 已就绪。",
            "CLI handoff requested." => "已请求 CLI 交接。",
            "CLI recovery requested." => "已请求恢复 Agent。",
            "Repair Agent" => "修复 Agent",
            "Retry recovery" => "重试恢复",
            "Recovering the previous Agent automatically…"
            | "recovery_pending"
            | "recovery_stopping" => "正在自动恢复原 Agent…",
            "Starting the repair Agent with the saved conversation…" | "recovery_starting" => {
                "正在使用已保存的会话启动修复 Agent…"
            }
            "Repair Agent is available. Messages remain saved for the selected CLI."
            | "repairing" => "修复 Agent 已可用，消息仍保留并等待所选 CLI。",
            "Messages wait for the selected CLI. Stop pauses the project and cancels the switch." => {
                "消息将等待所选 CLI 就绪。停止会暂停项目并取消切换。"
            }
            "Retry startup" => "重试启动",
            "Resume this project and release terminal input before changing its CLI." => {
                "切换 CLI 前，请先恢复项目并释放终端输入。"
            }
            "Project files" => "项目文件",
            "Update pending" => "更新待完成",
            "This project is waiting for a compatible update. Its conversation and saved messages are preserved." => {
                "项目正在等待兼容的更新，会话和已保存的消息均已保留。"
            }
            "Finishing the update. Your projects keep running." => {
                "正在完成更新，你的项目会继续运行。"
            }
            "Signed updates are verified and applied automatically. Your projects and conversations stay in place." => {
                "验证签名后自动完成更新，保留项目和会话。"
            }
            "Background service updated. Compatible Agents keep running; other projects show a recovery action." => {
                "后台服务已更新。兼容的 Agent 会继续运行；其他项目会显示恢复操作。"
            }
            "update_required" | "Agent update required" => "Agent 需要更新",
            "Recover Agent after update" => "更新后恢复 Agent",
            herdr_feishu_bridge::UPDATE_RESUME_NOTICE => {
                "后台服务的 Agent 协议已变更。点击“更新后恢复 Agent”，仅重启此 Agent 并恢复已保存的原生会话。排队消息和结果不确定的操作将保留。"
            }
            herdr_feishu_bridge::UPDATE_FRESH_NOTICE => {
                "后台服务的 Agent 协议已变更。点击“更新后恢复 Agent”，仅重启此 Agent。由于未保存原生会话 ID，将启动全新会话。排队消息和结果不确定的操作将保留。"
            }
            "Agent recovery started with the saved native session. Saved messages continue when automatic activity is enabled." => {
                "正在使用已保存的原生会话恢复 Agent。启用自动运行后，已保存的消息会继续投递。"
            }
            "Agent recovery started with a fresh session because no native session ID was saved. Saved messages continue when automatic activity is enabled." => {
                "未保存原生会话 ID，正在使用全新会话恢复 Agent。启用自动运行后，已保存的消息会继续投递。"
            }
            "Background service" => "后台服务",
            "Service update available" => "后台服务可更新",
            "Update background service" => "更新后台服务",
            "The compatible background service keeps running when the desktop updates. Updating it briefly reconnects messages and tools; Agent sessions keep running." => {
                "桌面更新时兼容的后台服务会继续运行。更新后台服务会短暂重连消息和工具；智能体会话继续运行。"
            }
            "Every" => "每",
            "days" => "天",
            "hours" => "小时",
            "minutes" => "分钟",
            "seconds" => "秒",
            "Daily" => "每天",
            "Weekdays" => "工作日",
            "Independent instance" => "独立实例",
            "Cannot open menu-bar panel" => "无法打开菜单栏面板",
            "Next run" => "下次执行",
            "Connections and occasional project controls." => "连接设置与偶尔使用的项目操作。",
            "Add a project, then send instructions in Feishu." => {
                "添加项目，然后在飞书里交代事情。"
            }
            "Send instructions in Feishu. New messages immediately steer the Agent, even while it is working." => {
                "在飞书里发送指令；Agent 执行中也能立即收到补充消息。"
            }
            "Ask the Agent in Feishu to schedule something when you need it." => {
                "需要时，在飞书里告诉 Agent 安排即可。"
            }
            "No scheduled runs" => "还没有定时任务",
            "Scheduled runs" => "定时任务",
            "Open Dauerbot" => "打开 Dauerbot",
            "+ Add project" => "+ 添加项目",
            "Settings" => "设置",
            "FEISHU BOT" => "飞书机器人",
            "Bot connection" => "机器人连接",
            "Connection lost or authentication failed; check this project's bot credentials and event subscriptions" => {
                "连接已断开或认证失败，请检查此项目的机器人凭据和事件订阅。"
            }
            "Migrate this pre-refactor data directory with Dauerbot 0.4.1 before upgrading" => {
                "请先使用 Dauerbot 0.4.1 迁移此旧版数据目录，再进行升级。"
            }
            "01  Choose a project" => "01  选择项目",
            "Choose a project folder and an installed Agent CLI." => {
                "选择项目文件夹和已安装的智能体 CLI。"
            }
            "02  Connect its bot" => "02  连接专属机器人",
            "Connect a dedicated Feishu bot to this project, or continue locally." => {
                "为此项目连接专属飞书机器人，也可以先在本地使用。"
            }
            "Add your first project →" => "添加第一个项目 →",
            "How to create a Feishu bot" => "如何创建飞书机器人",
            "Go to workspace →" => "进入工作台 →",
            "Choose a project first" => "先选择一个项目",
            "Each project connects to its own bot. Create a project before adding bot credentials." => {
                "每个项目连接自己的机器人。请先创建项目，再填写机器人的凭据。"
            }
            "Connect this project's bot" => "连接这个项目的机器人",
            "Every project has its own bot. Direct messages, bot mentions and verified topic replies reach its project." => {
                "每个项目都有自己的机器人。私聊、明确 @ 机器人及已确认归属的话题回复会进入对应项目。"
            }
            "The secret stays masked. Leave it blank to keep this bot's saved secret. A new App ID needs its own secret. Credentials are stored privately on this Mac." => {
                "密钥始终隐藏。留空会保留这个机器人已保存的密钥；更换 App ID 时需要填写对应的新密钥。凭据仅保存在这台 Mac 上。"
            }
            "Bot connected to this project. Validate its credentials to continue." => {
                "机器人已连接到此项目，请验证凭据后继续。"
            }
            "Use locally for now" => "暂时只在本地使用",
            "Connect a dedicated bot" => "连接专属机器人",
            "After creating this project, connect its own Feishu bot or continue locally. You never need to enter chat IDs." => {
                "创建项目后，连接它自己的飞书机器人，也可以先在本地使用。无需填写聊天 ID。"
            }
            "Project created. Connect its bot or continue locally." => {
                "项目已创建，可以连接它的机器人，也可以先在本地使用。"
            }
            "Bot connected" => "已连接机器人",
            "No bot connected" => "尚未连接机器人",
            "This project's private and group messages reach its active agent." => {
                "这个机器人收到的私聊和群聊消息，都会交给本项目当前的智能体。"
            }
            "This project is local only. Connect a dedicated Feishu bot to receive messages." => {
                "此项目目前仅在本地使用，连接专属飞书机器人后即可接收消息。"
            }
            "Manage bot" => "管理机器人",
            "This bot is already connected to another project" => "此机器人已绑定到另一个项目",
            "Enter this bot's App Secret" => "请输入这个机器人的 App Secret",
            "Connect a Feishu bot to this project first" => "请先为此项目连接飞书机器人",
            "An older or incompatible Dauerbot daemon is using this data directory. Quit that daemon before starting this build, or use --data-dir for an independent instance." => {
                "当前数据目录正在由旧版或不兼容的 Dauerbot 服务使用。请先退出旧服务，或用 --data-dir 打开独立实例。"
            }
            "Startup output" => "启动信息",
            "The CLI exited before it could accept the saved task. Check its installation and login in Environment Check. Dauerbot retries automatically after thirty seconds." => {
                "CLI 在接收任务前退出了。请在“环境检查”中确认安装和登录状态，Dauerbot 会在三十秒后自动重试。"
            }
            "Sign in to the Agent CLI in this project's terminal. Saved messages will continue after login." => {
                "请在项目右侧终端中登录智能体。已保存的消息会在登录后继续。"
            }
            "The Agent needs a permission decision. Continue in this project's terminal." => {
                "智能体正在等待权限确认，请在项目右侧终端中处理。"
            }
            "The Agent is waiting for input. Continue in this project's terminal." => {
                "智能体正在等待输入，请在项目右侧终端中继续。"
            }
            "The Agent reported an error. Open its terminal to see the cause and continue." => {
                "智能体遇到了错误，请在右侧终端中查看原因并继续。"
            }
            "The Agent has not connected to Dauerbot yet. Check login and MCP setup in its terminal." => {
                "智能体还没有连接 Dauerbot，请在右侧终端中检查登录和 MCP 设置。"
            }
            "The Agent exited during a task. Check the saved result before retrying uncertain work." => {
                "智能体在任务执行期间退出了。请先检查已保存的结果，再决定是否重试。"
            }
            "Input may have reached the Agent. Verify its terminal before retrying this input." => {
                "任务可能已经发送给智能体。请先检查终端，再决定是否重试。"
            }
            "Paste is larger than 1 MiB" => "粘贴内容超过 1 MiB",
            "Terminal input is busy; try again." => "终端输入繁忙，请重试。",
            "Terminal frame exceeded the size limit" => "终端画面超过大小限制",
            "Terminal connection closed. Reconnect to view it again." => {
                "终端连接已关闭，请重新连接。"
            }
            "Session preserved" => "已保留原会话",
            "Got it" => "知道了",

            "Review input delivery" => "查看任务结果",
            "Check the terminal and saved work before retrying or dismissing this input." => {
                "重试或忽略任务前，请先查看终端和已保存的工作。"
            }
            "Retry input" => "重试任务",
            "Dismiss input" => "忽略任务",
            "Retry this input?" => "重试这个任务？",
            "Dismiss this input?" => "忽略这个任务？",
            "The agent may already have changed files or sent a reply. Continue after reviewing its work." => {
                "智能体可能已经修改了文件或发出回复。请先检查已完成的工作。"
            }
            "The input will stop waiting. Existing files and sent replies remain." => {
                "任务将停止等待，现有文件和已发出的回复会保留。"
            }
            "Input dismissed" => "输入已忽略",
            "Show terminal" => "显示终端",
            "Hide terminal" => "收起终端",
            "Pause" => "暂停",
            "Enable" => "启用",
            "Remove project…" => "移除项目…",
            "Remove this project?" => "移除这个项目？",
            "Project removed" => "项目已移除",
            "Connecting to the live terminal…" => "正在连接实时终端…",
            "Agent needs attention. Review the project notice." => {
                "智能体需要处理，请查看项目提示。"
            }
            "Your answer" => "你的回答",
            "Send answer" => "提交回答",
            "Manual control is reserved" => "会话已保留人工控制",
            "Release the reservation to let queued messages continue, or take control in the terminal." => {
                "交还控制后，排队任务会继续执行；也可以在右侧接管终端。"
            }
            "Release reservation" => "交还控制",
            "Your agent, inside Dauerbot" => "在 Dauerbot 中操作智能体",
            "Watch and operate the live terminal in this app. Closing the window leaves the Agent running." => {
                "直接在应用内查看和操作终端，关闭窗口后任务继续运行。"
            }
            "Live terminal" => "实时终端",
            "You are in control" => "正在由你操作",
            "Take control" => "接管操作",
            "Release control" => "交还控制",
            "Reconnect" => "重新连接",
            "Viewing only · Take control to scroll app history" => {
                "只读查看 · 接管后可滚动应用历史"
            }
            "Choose a project to view its agent" => "选择项目，查看智能体",
            "Ready for messages. The Agent starts automatically when needed." => {
                "正在待命。收到消息或定时任务触发时，智能体会自动启动。"
            }
            "Messages start the Agent automatically." => "收到消息后会自动启动智能体。",
            "Automatic activity is paused. Messages stay saved until you resume." => {
                "自动运行已暂停。消息会保留，恢复后继续投递。"
            }
            "Pause automatic activity" => "暂停自动运行",
            "Resume automatic activity" => "恢复自动运行",
            "An earlier input delivery is uncertain. New messages continue automatically; retry only after checking its effects." => {
                "之前有一项任务中断了。新消息会自动继续处理；仅在需要重新执行旧任务时重试。"
            }
            "The Agent will stop. Project files remain on disk." => {
                "智能体会停止，项目文件会保留。"
            }
            "Needs attention" => "需要处理",
            "Delivery settings saved" => "消息投递设置已保存",
            "Stop" => "停止",
            "Model" => "模型",
            "Model follows the CLI's own settings" => "模型沿用 CLI 中的设置",
            "Input queued" => "输入已进入队列",
            "Schedule saved" => "计划已保存",
            "Sleeping" => "休眠中",
            "sleeping" => "待命",
            "starting" => "启动中",
            "ready" => "已就绪",
            "busy" => "执行中",
            "needs_attention" => "需要处理",
            "taken_over" => "手动控制中",
            "offline" => "HERDR 暂不可用",
            "stopped" => "已停止",
            "paused" => "已暂停",
            "exited" => "已退出",
            "error" => "发生错误",
            "queued" => "排队中",
            "submitted" | "acknowledged" => "进行中",
            "uncertain" => "需要检查结果",
            "running" => "执行中",
            "completed" => "已完成",
            "cancelled" => "已取消",
            "waiting" => "等待中",
            "deferred" => "等待中",
            "failed" => "失败",
            "Edit" => "编辑",
            "Remove" => "移除",
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
            "Leave empty to create a private project folder" => "留空以创建私有项目文件夹",
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
            "Messages, scheduled runs, and files reach the same main session." => {
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
            "Reply in a topic without @mentions" => "在话题中回复，无需每次 @",
            "Use the Feishu Agent permission preset or the full JSON with im:message.group_msg to receive topic replies without mentioning the bot. Obtain approval and publish if the scope is not already effective. The basic set only receives mentions. Dauerbot admits known project discussions and ignores unrelated group traffic; an ambiguous shared topic needs an explicit bot mention." => {
                "可使用飞书现成的智能体权限组，或包含 im:message.group_msg 的完整 JSON，接收话题中不带 @ 的回复。若权限已生效无需重复导入，否则需审批并发布。基础权限只接收 @ 消息。Dauerbot 仅接收已确认属于该项目的讨论，忽略无关群消息；多人共享且无法确定目标的话题需明确 @ 机器人。"
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
            "Project folder (optional existing folder)" => "项目文件夹（可选已有文件夹）",
            "Private workspace" => "私有工作区",
            "By default, Dauerbot creates a folder in ~/.dauerbot/agents with project instructions. You can also choose an existing folder." => {
                "默认在 ~/.dauerbot/agents 下创建文件夹和项目说明，也可选择已有文件夹。"
            }
            "Choose a folder..." => "选择文件夹…",
            "Automatic binding" => "自动绑定",
            "Your first project becomes this bot's fixed owner. You do not need to select chats or enter IDs. Direct and group messages both reach this project." => {
                "首个项目会自动绑定机器人，无需选择聊天或填写 ID。私聊与群聊消息均进入该项目。"
            }
            "Create project →" => "创建项目 →",
            "Enter a project name" => "请输入项目名称",
            "Project created. Start the main session to begin." => "项目已创建，可以启动主会话了。",
            "WORKSPACE" => "工作区",
            "Projects" => "项目",
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
            "The main session needs input" => "主会话需要你操作",
            "Stop session..." => "停止会话…",
            "Stop this project's main session?" => "确定停止此项目的主会话？",
            "This ends the CLI process. Project settings stay intact." => {
                "这将结束 CLI 进程，项目配置会保留。"
            }
            "Cancel" => "取消",
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
