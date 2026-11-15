# HERDR Feishu Bridge

一个本机运行的项目 daemon，把飞书机器人对话连接到 HERDR 托管的持久 CLI 主会话。Rust 后端、CLI 和 MCP 共用同一套业务逻辑；Svelte 管理界面作为静态资源打包进可执行文件。

关闭管理页面不影响后台工作。重启 Bridge 不重建 Agent 会话；关闭 Bridge 也不会主动结束 HERDR 中的终端。

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

在 UI 新建项目，填写已有的绝对代码目录、Agent 类型和飞书群 `chat_id`。也可通过 CLI：

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

一个项目对应一个主会话，一个飞书聊天只能绑定到一个项目。暂未绑定飞书也能创建项目、启动会话并从 UI/CLI 加入本地测试消息：

```sh
hfb message inject my-project '检查这个项目的目录结构，完成后回复飞书'
```

Agent 启动后必须完成当前 CLI 的信任或登录流程。遇到权限问题，在 HERDR 中接管处理；Bridge 不强制回答 blocked 对话框。

`session attach` 使用项目所属的 Core session 交互接管，按 `Ctrl+\` 脱离。使用独立 Core session 时，「在 HERDR 打开」会拒绝向默认 App 发送同号终端地址，以免打开其它会话；此时使用 CLI 接管。

原生会话恢复的处理方式：

| Agent | MCP 接入 | 原生恢复 |
|---|---|---|
| Codex | 启动参数注入独立 MCP 配置，强制 `--no-daemon`；向 MCP 显式转发 `HC_TERMINAL_ID` / `HC_TERMINAL_SOCKET` | 从 `session_ready` 的 MCP `threadId` 元信息登记；也可手动填写，然后使用 `codex resume` |
| Claude Code | `--mcp-config` 加项目专用配置 | 新会话指定 UUID，恢复时使用 `--resume` |
| Pi | 合并项目 `.pi/mcp.json` 中的 `herdr_feishu` 项 | 新会话指定 UUID，恢复时使用 `--session` |
| Antigravity `agy` | 合并项目 `.agents/mcp_config.json` | 暂未提供专用恢复参数；可用自定义 argv |
| 自定义 CLI | 用生成的 stdio MCP 配置接入 | 通过自定义 argv 指定 CLI 自己的恢复方式 |

Pi/Antigravity 的配置写入保留其他 MCP 项；遇到非法 JSON 或不属于本项目的同名项会拒绝覆盖。将这些本机配置加入对应项目的忽略规则，避免提交个人路径。

自动投递要求终端报告 OSC 7501 的 `idle` 或 `done` 状态，并且 MCP 已确认会话身份。无法报告状态的 CLI 会保留消息，不按屏幕文字猜测是否能输入。Antigravity 启动配置已实现，本机未安装该 CLI，状态与恢复行为尚待实机验证。

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
hfb project bind my-project oc_your_project_chat
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
| `send_markdown` | `text`, `request_id` | 向本项目绑定的聊天发送 Markdown |
| `send_file` | `path`, `request_id` | 流式发送本地文件，最大 100 MB |

同一发送请求重试复用 `request_id`；不同内容须使用不同 ID。工具只有返回 `status=sent` 才表示 API 已确认接收。MCP 不能管理其他项目、读取管理设置或启动/停止会话。

附件须位于当前项目目录或 Bridge 缓存内。路径会做 canonicalize 校验，符号链接不能绕出允许范围。入站消息资源和出站文件均使用流式 I/O。

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

Bridge 核对 Core 标签、会话代次、PID 和程序类型；程序返回 shell 或更换进程后拒绝自动投递。主终端不启用 `hc keep`，因为它恢复的是 shell。主机重启或 Agent 退出后，需按原生会话 ID 恢复。

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
