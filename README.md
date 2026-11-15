# HERDR Feishu Bridge

原生 macOS 项目工作台，把飞书机器人对话连接到 HERDR 托管的持久 CLI 主会话。桌面界面由 Rust / GPUI 绘制，Rust daemon、CLI 和 MCP 共用同一套业务逻辑。Svelte 页面保留为辅助管理入口。

关闭桌面窗口不影响后台工作。重启 Bridge 不重建 Agent 会话；退出桌面应用也不会主动结束 HERDR 中的终端。

## 桌面应用

在 macOS 上安装 Xcode 和 Rust 后构建：

```sh
./scripts/build-desktop.sh
open "dist/Herdr Bridge.app"
```

应用直接打开原生窗口，自动启动或连接本机后台服务，无需浏览器或前端开发服务器。可将 `.app` 拖入 Applications 再运行。构建产物使用 ad-hoc 本地签名；对外分发仍需 Developer ID 签名与 Apple 公证。

首次引导包括创建企业自建应用、启用机器人、填写并验证凭证、复制权限导入 JSON、配置长连接与消息订阅、发布应用和加群、本机权限检测、选择代码目录与 CLI。可以选择「先在本机试用」跳过飞书，稍后补齐。引导进度保存在本机，随时可重新打开；发布和订阅的手动确认不会显示为自动检测成功。

在「添加项目」中通过 macOS 系统选择器选择目录，填写名称并选择 Codex / Claude Code / Pi / Antigravity。当前机器人会自动固定绑定首个项目；私聊和群聊都是消息来源，无需查找或关联聊天 ID。工作台直接显示当前智能体，可点击切换为其他 CLI；切换会停止旧智能体、清除不兼容的原生会话 ID 和旧启动参数，再启动新主会话，机器人、项目和消息队列保持。默认 Core 的主会话会自动展示在 Herdr App 的「项目名 · 主会话」标签页。用户选择目录并启动，即授权信任该项目目录；当前会自动确认已识别的 Antigravity 目录信任提示。项目消息与状态自动刷新。

本机访问采用按需引导：

- 项目目录：只检测用户选择的目录，尝试枚举目录和创建、清理临时文件。桌面、文稿、下载等文件夹的用途说明已加入应用 Info.plist。
- 应用数据库：位于本机私有数据目录，无需完全磁盘访问。可选的外部 SQLite 检测只读打开用户选择的文件并查询 schema 元数据，不读取业务记录。
- 完全磁盘访问：可选，通过按钮进入系统设置，由用户为实际执行任务的应用手动开启。没有使用私有 TCC 接口，不会把“选中文件能打开”误报为全盘权限已开启。
- Bridge、后台服务和既有 HERDR/Agent 进程的权限可能不同。Bridge 目录检测通过不等于 HERDR 权限已经通过；需要在实际主会话验证。当前功能不要求辅助功能、屏幕录制或自动化权限。

默认共享 CLI 的用户数据目录。桌面首次创建配置时选择可用的本机端口，后续固定使用该配置中的端口；CLI 会读取同一份配置。需要隔离预览时：

```sh
open "dist/Herdr Bridge.app" --args --data-dir /absolute/path/to/private-data
```

## 运行

需要 Rust 1.89+、Node.js 22.12+、pnpm 12.6，以及已安装的 `hc`。建议使用已核查的 HERDR Core 0.1.20 或兼容版本。只有查看原始交互终端时才需要 Herdr App。

```sh
./scripts/build.sh

# 终端一：运行 daemon
./target/release/hfb serve

# 终端二：打开管理界面，令牌通过 URL fragment 交给当前浏览器会话
./target/release/hfb ui
```

默认地址是 `http://127.0.0.1:49321`。程序只监听 IPv4 loopback。管理 API 使用随机 bearer token，写入本机配置文件；UI 接收后立即清除 URL fragment，仅在当前 sessionStorage 中保存。

需要隔离数据或 Core 时：

```sh
./target/release/hfb --data-dir /absolute/path/to/private-data serve \
  --port 49322 --core-session my-bridge
./target/release/hfb --data-dir /absolute/path/to/private-data ui
```

`--data-dir` 是所有 CLI 子命令的全局参数。更换 Core session 时应使用独立数据目录，避免已有终端地址指向另一实例。

## 项目与主会话

在原生工作台新建项目，选择代码目录、Agent 类型和接收聊天。私聊与群聊可以同时进入同一主会话。也可通过 CLI：

```sh
hfb project add my-project --path /absolute/path/to/repo --agent codex \
  --chat-id oc_your_project_chat
hfb project list
hfb session start my-project
hfb session screen my-project
hfb session attach my-project
hfb project pause my-project
hfb project unpause my-project
hfb session stop my-project
hfb session resume my-project
```

一个飞书机器人固定对应一个项目，项目只运行一个当前智能体。所有私聊、群聊消息直接进入该项目，切换 CLI 不改变绑定。来源聊天的类型、ID、成员等信息由 XML 提供，Agent 通过 MCP 的 `to` 自行决定回复目标。机器人收到消息时项目尚未创建，会先保存，创建后自动按顺序接入。额外本机项目不会分享当前机器人的消息或发信权限。未配置飞书也能先在本机试用：

```sh
hfb message inject my-project '检查这个项目的目录结构，完成后回复飞书'
```

Bridge 沿用 CLI 已有的账号、模型、provider、推理强度和执行模式，不自动注入 `--model`、`--provider`、`--settings` 或 `--mode`。用户自填的启动参数原样保留。Herdr Core 是常驻进程，可能早于桌面应用启动；Bridge 会通过 `terminal.run.env` 补传当前已有的标准代理变量（含大小写及 `NO_PROXY`），避免 CLI 在 Core 中丢失网络环境。不会把 Bridge 的终端身份或模型环境变量覆盖到 Agent。按用户要求，Codex 使用 `--dangerously-bypass-approvals-and-sandbox`，Claude Code / Antigravity 使用 `--dangerously-skip-permissions`，Pi 使用 `--approve` 信任当前项目的本地文件；Pi 的这个参数不代表能覆盖扩展自行实现的授权逻辑。MCP 配置、初始化提示和原生恢复参数仍按各 CLI 支持的方式传递；Antigravity 的 `-i` 仅用于提交首次交互提示。

Agent 启动后必须完成当前 CLI 的信任或登录流程。Bridge 自动确认已识别的 Antigravity 项目目录信任：必须匹配当前项目目录、终端 PID/代次、结构化 trust 状态以及屏幕上已选中的 Yes；每代会话最多发送一次确认。其他 blocked 状态在桌面显示“等待交互”，可点击“在 HERDR 监控”查看；队列保留，继续使用原进程。

「启动主会话」会创建一个 HERDR 托管的真实 CLI 进程，后续飞书消息持续进入同一终端。在 Herdr App 中可以查看完整实时输出、处理权限提示和直接输入；Bridge 的「在 HERDR 监控」也可以重新定位到该终端。展示失败会在项目中提示，进程启动后立即退出则返回实际错误，不误报为正在等待就绪。

`session attach` 使用项目所属的 Core session 交互接管，按 `Ctrl+\` 脱离。使用独立 Core session 时，「在 HERDR 打开」会拒绝向默认 App 发送同号终端地址，以免打开其它会话；此时使用 CLI 接管。

原生会话恢复的处理方式：

| Agent | MCP 接入 | 原生恢复 |
|---|---|---|
| Codex | 启动参数注入独立 MCP 配置，强制 `--no-daemon`；向 MCP 显式转发 `HC_TERMINAL_ID` / `HC_TERMINAL_SOCKET` | 从 `session_ready` 的 MCP `threadId` 元信息登记；也可手动填写，然后使用 `codex resume` |
| Claude Code | `--mcp-config` 加项目专用配置 | 新会话指定 UUID，恢复时使用 `--resume` |
| Pi | 合并项目 `.pi/mcp.json` 中的 `herdr_feishu` 项 | 新会话指定 UUID，恢复时使用 `--session` |
| Antigravity `agy` | 合并项目 `.agents/mcp_config.json`，使用 `-i` 传入交互启动提示 | `--conversation <原生会话 ID>` |
| 自定义 CLI | 用生成的 stdio MCP 配置接入 | 通过自定义 argv 指定 CLI 自己的恢复方式 |

Pi/Antigravity 的配置写入保留其他 MCP 项；遇到非法 JSON 或不属于本项目的同名项会拒绝覆盖。将这些本机配置加入对应项目的忽略规则，避免提交个人路径。

自动投递要求终端报告 OSC 7501 的 `idle` 或 `done` 状态，并且 MCP 已确认会话身份。无法报告状态的 CLI 会保留消息，不按屏幕文字猜测是否能输入。Antigravity 使用本机 `agy --help` 核对过的 `-i` 交互启动形式。模型登录、项目授权及恢复行为仍受当前安装版本约束。

可在项目高级设置中填写额外启动参数。例如 CLI 的 `--model` 参数通过字符串数组保存。自定义 Agent 的数组则是完整 argv，不经过 shell，支持以下占位符：

```json
["my-agent", "--mcp-config", "{mcp_config}", "{instructions}"]
```

占位符包括 `{mcp_config}`、`{credentials}`、`{bridge_exe}`、`{instructions}`。运行数据目录下的 `sessions/<project>/<generation>/` 保存当次配置；生成的 MCP 凭证只允许当前项目、当前会话代次的操作。

## 飞书配置

使用企业自建应用，在飞书后台启用机器人。选择「使用长连接接收事件」，订阅 `im.message.receive_v1`，开通机器人发送消息和图片/文件资源权限。接收群内所有普通消息需要相应全量消息权限；只有 @机器人 权限时只能收到相应事件。发布配置，并将机器人加入项目群。

通过 UI「连接设置」保存 App ID 和 App Secret，再点击检查凭证。Secret 不会由管理 API 回传。更换 App ID 时会清除旧 Secret，避免误用。

也可以从 stdin 读取 Secret：

```sh
hfb feishu set --app-id cli_your_app
# 输入 Secret，然后 EOF；推荐在 UI 中配置，避免终端回显
hfb feishu check
hfb session switch my-project --agent agy
```

同一个飞书应用的长连接事件采用集群分发。不要在已有生产机器人同时使用此应用建立第二套收件监听；应为 Bridge 选用专用应用，或让现有接收端统一路由。真实发送测试可以单独运行，不建立新的监听。

飞书监听器状态的含义：

- `unconfigured`：尚无完整凭证。
- `connecting` / `awaiting_events`：正在运行 SDK 连接流程，首条实际事件尚未确认；不等于已证明事件权限正确。
- `receiving`：实际收到过消息事件。
- `reconnecting`：连接结束或失败，daemon 正在退避重试。

## MCP 工具

受管理的主 CLI 会自动启动 `hfb mcp --credentials <private-file>`。

| 工具 | 参数 | 行为 |
|---|---|---|
| `session_ready` | 可选 `native_session` | 核对终端、进程与会话代次，登记就绪和原生会话 ID |
| `ack_message` | `message_id` | 确认已接收指定入站消息，解除后续队列阻塞 |
| `send_markdown` | `to`, `text`, `request_id` | 向 Agent 指定的聊天或用户发送 Markdown |
| `send_file` | `to`, `path`, `request_id` | 向 Agent 指定的聊天或用户流式发送本地文件，最大 100 MB |

`to` 是必填字段：`oc_…` 是聊天 ID（群聊或私聊），`ou_…` 是用户 Open ID。Agent 按用户意图决定目的地，可与来源聊天不同，无须 `reply_to`，后端不自动锁定回复来源；最终可达范围由飞书机器人的权限和应用可用范围决定。CLI 的 `message send` / `message file` 支持 `--to`；旧 CLI 调用省略时兼容项目原有主聊天。

同一发送请求重试复用 `request_id`；不同内容或目的地须使用不同 ID。工具只有返回 `status=sent` 才表示 API 已确认接收。MCP 不能管理其他项目、读取管理设置或启动/停止会话。

附件须位于当前项目目录或 Bridge 缓存内。路径会做 canonicalize 校验，符号链接不能绕出允许范围。入站消息资源和出站文件均使用流式 I/O。

## 入站 XML 上下文

主会话收到统一的 XML 信封，包含平台、聊天类型及 ID、message_id、发送者各类 ID、群名、成员快照、@ 提及、话题根消息/父消息、正文和附件路径。所有字段与正文均做 XML 转义，正文无法闭合信封伪造平台字段。XML 只是上下文格式，用户文本仍按用户输入处理。

```xml
<feishu_message version="1" message_id="om_example">
  <source platform="feishu" chat_type="group" chat_id="oc_team"
          root_id="" parent_id="" create_time=""/>
  <sender open_id="ou_person" user_id="" union_id="" name="小明"/>
  <chat name="产品群"/>
  <members status="snapshot" truncated="false" includes_bots="false">
    <member id="ou_person" id_type="open_id" name="小明"/>
  </members>
  <mentions/>
  <body type="text" trust="user_input">请把结果私聊给我</body>
</feishu_message>
```

Agent 可以据此调用 `send_markdown(to="ou_person", text="结果…", request_id="…")`。私聊使用同样的结构，`chat_type="p2p"`，无需群成员快照。未知姓名和 ID 留空；群成员 API 只返回人类成员，最多获取一页（100 个分页额度，平台可能返回更多同批加入成员），超过时标记 `truncated=true`。快照缓存五分钟，权限不足或超时时标记 `unavailable`，不阻止正文投递，也不虚构成员。首次配置权限时可导入工作台提供的群成员读取权限。

## 持久化与交付状态

SQLite 保存项目和收发队列。入站事件在返回 SDK ACK 前先入库，按项目和飞书 message_id 去重。自动投递使用每个项目的 FIFO 队列；已提交的消息需 Agent 明确确认，下一条才会继续。

| 状态 | 含义 |
|---|---|
| `queued` | 已保存，等待主会话就绪或结束当前工作 |
| `submitting` / `submitted` | 正在写入终端 / 已写入，等待 Agent 确认 |
| `acknowledged` | Agent 调用了确认工具；不代表任务已完成 |
| `pending` / `sending` / `sent` | 出站待发 / 发送中 / 飞书 API 已确认 |
| `failed` | 已观察到明确错误，可处理后重试 |
| `uncertain` | 网络或进程中断导致结果不能确定；先核对再手动重试 |
| `dismissed` | 用户手动忽略，不再阻塞后续入站消息 |

程序重启会将操作中的记录转为 `uncertain`，不盲目重发。PTY 和飞书之间没有原子提交，因此不承诺严格 exactly-once。`hc send` 的 started 也不作为某条业务消息的最终确认。

Bridge 核对 Core 标签、会话代次、PID 和程序类型；程序返回 shell 或更换进程后拒绝自动投递。通过 Herdr App 接入标签页时，App 会临时启用 keep；Bridge 在确认标签页已出现后将其关闭，避免 Agent 退出时自动恢复成 shell。主机重启或 Agent 退出后，需按原生会话 ID 恢复。

## 用户服务

`hfb serve` 在前台运行，支持 SIGINT/SIGTERM。`hfb service` 打印当前平台的用户服务配置，不会自行修改系统服务。

macOS：

```sh
mkdir -p ~/Library/LaunchAgents
hfb service > ~/Library/LaunchAgents/dev.herdr.feishu-bridge.plist
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/dev.herdr.feishu-bridge.plist
```

Linux：

```sh
mkdir -p ~/.config/systemd/user
hfb service > ~/.config/systemd/user/herdr-feishu-bridge.service
systemctl --user daemon-reload
systemctl --user enable --now herdr-feishu-bridge
```

服务模板记录可执行文件和数据目录的绝对路径，移动文件后需重新生成。macOS 的 `service.log` 由使用者定期清理或配置轮转。

## 开发与验证

```sh
cd frontend
pnpm install --frozen-lockfile
pnpm check
pnpm build
cd ..
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
python3 scripts/smoke.py --browser
```

前端开发使用 `pnpm dev`，API 代理到默认端口 49321。生产构建无需 Node.js 或 Vite 常驻。

验证分层：

- Rust 测试覆盖去重、异常恢复、项目级凭证、跨项目拒绝、来源校验、终端身份和附件目录边界。
- `scripts/smoke.py` 使用真实 SDK WebSocket、真实隔离 Core 终端、真实 MCP stdio 和本地飞书 HTTP fixture，验证多轮往返、附件、重连、daemon/Core 重启。
- `--browser` 额外运行桌面/手机 Playwright 测试，并在 `test-results` 保存截图。优先使用本机 Chrome；没有时执行 `pnpm --dir frontend exec playwright install chromium`。
- `--real-pi` 使用本机已配置的 Pi 账号执行一个真实模型回合，可能产生模型费用；飞书端仍为本地模拟服务。它仅对测试脚本自己生成的工作目录使用 `--approve`，不修改全局信任配置。
- `--real-agent codex` / `--real-agent claude` 可验证相应本机 CLI；首次运行时可能需要在隔离 HERDR 终端确认已知测试目录的信任，脚本不会跳过工具权限提示。
- `tests/live_feishu.rs` 是显式启用的真实发信测试。它读取被忽略的 `.local/live.json`，包含 `app_id`、`app_secret`、`chat_id`，只发送一条测试 Markdown 和一个小文件，不启动 WebSocket：

```sh
cargo test --test live_feishu -- --ignored
```

真实测试后应删除该临时凭证文件。真实飞书收件权限、各厂商 CLI 登录、信任与授权对话框仍需按实际账号验证。本项目未调用模型反向代理。

## 目录

```text
src/             Rust daemon、HTTP API、CLI、MCP、数据库和连接器
frontend/        Svelte + TypeScript + Vite
tests/           有回归价值的状态与边界测试、可选真实飞书测试
scripts/         构建脚本、无模型的端到端协议 fixture
test-results/    本机测试证据，Git 忽略
```

默认数据目录使用操作系统的本地应用数据目录；运行 `hfb init` 可查看。配置文件权限为 0600、目录为 0700（Unix）。凭证以受文件权限保护的本机明文保存，当前未接入系统钥匙串。管理接口面向同一台机器的受信任用户，不应直接转发到公网。
