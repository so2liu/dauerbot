# 本机验证记录

日期：2026-11-15。环境：macOS，Rust 1.96.0，Node.js 22.23.2，pnpm 12.6.0，HERDR Core 0.1.20。这是本机开发验证，不等同于跨平台或长期生产压测。

## 私聊 XML 精简与当前内存采样

私聊与本机消息省略群聊专属 `<chat>` / `<members>`，非群上下文不再产生 `not_applicable`。空 mentions 也省略；群聊成员读取失败仍保留 `unavailable`。在既有 XML 回归中补充私聊携带旧群上下文时不会输出群成员的断言。conversations 的 4 项测试和后端 clippy 通过；新版 hfb 已签名安装，仅重启后台服务，当前 GUI、Agent 与 MCP 进程保留。

一个「双王」项目、agy 待命、桌面窗口打开时，连续 4 秒的 5 次 RSS 快照稳定：GUI 108.5 MiB、daemon 26.4 MiB、MCP 9.6 MiB、Core 连接辅助进程 7.0 MiB、agy 250.8 MiB。Bridge 后台合计约 43.0 MiB，计入 GUI 为 151.5 MiB，再计入 agy 为 402.3 MiB；这些进程采样时 ps CPU 均为 0.0%。不包含用户共享的 Herdr App/Core 及其他 Agent；RSS 相加可能重复计算共享页，不代表峰值、GPU 内存或长期运行保证。原始记录位于 `test-results/memory-current.json`。

## 机器人与项目固定绑定修正

按用户明确的关系实现为“一个机器人 → 一个固定项目 → 一个当前智能体”。首个项目创建时保存机器人归属；从旧版单项目数据升级时自动补齐。飞书真实入站处理统一使用机器人归属，私聊和群聊不再依赖 chat_id 关联，也不会因为新增本机项目而改变目标。创建项目之前收到的消息会保留，创建后按顺序接入。其他本机项目不能借用该机器人发信。

GPUI 移除了聊天选择、逐个关联和私聊默认路由。工作台显示固定绑定，并提供 Codex / Claude Code / Pi / Antigravity 切换按钮；CLI 增加 `hfb session switch <项目> --agent <CLI>`。切换先停止当前智能体，再清除旧 CLI 的会话 ID 与参数并启动新主会话，保留机器人、项目和消息队列。辅助 Svelte 页面也移除聊天绑定步骤，手动发信的目的地仅作用于本次发送。

后端 17 项测试通过，真实飞书发送测试默认忽略；前端 check 为 0 errors / 0 warnings，生产构建、后端及桌面 clippy 通过。回归覆盖首条消息早于项目创建、陌生私聊与群聊自动进入固定项目、旧聊天路由不能覆盖机器人归属、重启与 CLI 切换后关系保持，以及活跃智能体不能被直接修改配置。

现场确认：用户此前的 3 条 hello 和 2 条“1”已全部变为 acknowledged，Agent 自主产生的 5 条飞书回复均为 sent。本轮未额外发送人工测试消息，也未停止用户当前 Agent。

更新后的隔离 SDK / HERDR / MCP / 飞书 fixture 全链路冒烟 12 项通过：创建项目不传 chat_id，新私聊及新群聊无需任何关联调用即进入同一主会话，显式 to、Markdown/附件、去重、断线重连、daemon/Core 重启及停止后的排队行为保持。证据为 `test-results/smoke.json`。安装使用逐文件原子替换，仅重启 Bridge GUI/daemon，保留用户当前 Agent 及 MCP 进程。只读 API 核实固定归属为「双王」、原 t1x8 / PID 17625 仍 ready；原生窗口截图确认“已绑定此项目”和四个智能体切换按钮正常呈现，未在用户主会话上实际切换 CLI。

## GPUI 桌面版追加验证

桌面版本 0.2.0，GPUI 0.2.2、GPUI Component 0.5.1。使用系统 Metal 运行时编译着色器；构建机器无需另行下载离线 Metal Toolchain。桌面与 daemon 分开运行，原生窗口不加载网页或 WebView。

本轮执行：

- `cargo test --locked --release --target-dir desktop/target --tests`：9 项通过，1 项真实飞书发送测试按默认忽略。新增的群列表 HTTP 测试覆盖本机认证、分页合并、上游敏感字段过滤及缺少 scope 时的错误透传。
- `cargo test --locked --release --manifest-path desktop/Cargo.toml`：2 项通过，覆盖所选目录探测临时文件清理、数据库文件内容保持不变，以及“文件不存在”与实际 OS 权限拒绝的错误区分。
- daemon 与桌面 crate 的 `cargo clippy --locked --all-targets -- -D warnings` 通过。GPUI 上游 `block 0.1.6` 依赖存在 Rust future-incompatibility 提示，未产生本项目 lint 错误。
- 前端生产资源重新构建成功；`scripts/build-desktop.sh` 成功生成含 `hfb` sidecar、图标和用途声明的 `.app`，`plutil -lint` 和 `codesign --verify --deep --strict` 通过。
- 实际启动 `.app`，通过 macOS 窗口快照和截图确认 GPUI 欢迎页完成渲染，CLI 路径检测显示正常。后台 API 确认 HERDR `connected`、飞书 `unconfigured`，没有加载既有生产机器人凭证。

交互验收限制：本轮电脑控制工具可观察原生窗口和截图，但坐标点击返回 `noWindowsAvailable`。因此没有将引导按钮、文件选择器、系统设置跳转及项目工作台操作记为已完成的 GUI 自动化测试。相关 OS 权限拒绝、用户在系统设置开启权限后的恢复，以及 HERDR 外部进程的实际访问权限仍需在用户授权场景中验收。未自动授予完全磁盘访问权限，也未扫描任何隐私数据库。

飞书凭证检查只验证已保存应用身份；事件订阅和应用发布为用户手动确认项。完整双向收发需要用户完成配置并向绑定聊天发出测试消息后才能确认。完整权限 JSON 包含 `im:message.group_msg`，用于接收群内全部用户回复；基础清单仅接收群内 @ 消息。群列表由真实 API 获取，不包含单聊。

## HERDR 监控与目录信任追加验证

修复 Antigravity 的启动提示必须通过 `-i` 传递、恢复使用 `--conversation`。默认 Core 中启动的主会话自动展示到 Herdr App 的“项目名 · 主会话”标签页；展示失败单独保留提示，Agent 继续运行。实机确认「双王 · 主会话」展示的是 Bridge 创建的原终端 `t1x4`，PID 46064。

用户明确要求代为确认目录信任后，通过 `hc keys t1x4 enter` 确认了 `/Users/yangliu35/ai-videos/双王`。之后同一 PID 仍运行，状态从 `blocked / permission` 变为 `idle`。终端显示 Antigravity 1.5.9 的 `plan model not specified` 错误，因此该结果仅证明目录信任和原进程继续运行，不能算 MCP 已就绪或飞书已完成收发。

自动目录信任实现限定为已识别的 Antigravity 对话框：同时核对项目目录、PID、代次、结构化 permission/trust 状态及当前选中的 Yes；确认前重新读取终端状态，每代进程最多发一次确认。回归测试覆盖其他目录、未选 Yes、普通工具权限、旧代次、PID 更换、退出进程和 MCP 已就绪后的拒绝路径。其他阻塞在 GPUI 显示“等待交互”，排队消息保留。

本轮 `cargo test --locked --release --target-dir desktop/target --test invariants` 的 10 项全部通过；后端及 GPUI 的 `clippy --all-targets -- -D warnings` 通过（GPUI 上游 `block` 仍有 future-incompatibility 提示）。两个 release 二进制构建通过，安装包通过 ad-hoc 签名验证，已更新并重新打开 `~/Applications/Herdr Bridge.app`。重启 Bridge 后只读验证 Core 连接成功、项目仍指向同一终端/PID，`ready=false`；没有重启 Herdr Core 或 Agent。自动识别与拒绝条件经过回归测试，真实目录信任由本轮显式按键完成，尚未将未来首次启动的自动按键流程记为实机验收完成。

## 原生模型与代理环境适配追加验证

参考本机 Magpie `67f61bd78` 的 `internal/agent/agent.go`、`agy.go`、`pi.go`、`applied.go` 和 `plugged_launch.go`，确认它把 Agent 配置、原生模型与网关模型分开。Bridge 保留原生账号、默认模型及用户启动参数，不引入其网关、模型目录或 `--settings` 覆盖。新增回归覆盖四个 CLI 不自动注入模型/provider/模式参数，以及用户显式模型参数保持一次且原样传递。

按用户明确要求，Codex 追加 `--dangerously-bypass-approvals-and-sandbox`，Claude Code / Antigravity 追加 `--dangerously-skip-permissions`，Pi 追加用于信任本地项目文件的 `--approve`。桌面单独呈现 Core 的 `error` 状态与 CLI 原始错误。

实机对照：同一「双王」工作目录中，仅运行 `agy`，不带 `-i` 或 MCP 初始化提示，发送“仅回复 OK”仍复现 `plan model not specified`。当前 shell 与 Bridge 含本机代理，但默认 Core 子进程没有标准代理变量。只通过 `terminal.run.env` 补传已有代理后，同样的裸 `agy` 自动选中原有 Gemini 4 Carbon (Medium)，真实模型返回 `OK`，Core 报告 `done`。因此该错误已证实与网络环境传递有关，不能归因为必须手工指定模型。新增代理白名单回归确保包含大小写代理和 NO_PROXY，并排除 HC 身份、模型与无关账号变量。

本轮 invariants 共 12 项通过，后端与桌面 clippy 通过，两个 release 二进制构建及安装签名验证通过，已更新并重新打开桌面应用。GPUI 上游仍有 `block` future-incompatibility 提示。完整 AGY MCP 初始化在隔离测试工作目录中遇到 CLI `Sign in required`，本轮未记为通过；测试用 Core、终端和 daemon 已停止。生产「双王」原终端 t1x4 / PID 46064 保留，仍未 ready；原进程需要重启才能继承修复后的环境。本轮未向真实飞书发信，未改全局模型配置。

## 私聊、群聊与 Agent 自选目的地追加验证

本轮后端测试增加 3 项，共 12 项通过、1 项真实飞书发送测试默认忽略；桌面权限逻辑 2 项通过。新增场景覆盖：未关联私聊的首条消息持久化及关联后重放、一个主会话接收多个群与私聊、显式关联优先于默认私聊项目、群消息不被私聊默认值接收、重新关联后全局 message_id 去重、旧主聊天接口无法抢占其他项目的接收聊天、XML 闭合注入与非法控制字符，以及 `to` 映射到飞书 `chat_id` / `open_id` 的真实 HTTP 请求和跨目的地幂等校验。

更新协议 fixture 后运行 `python3 scripts/smoke.py --binary "$PWD/desktop/target/release/hfb"`，11 项检查通过。真实 SDK 接收私聊事件，保留未关联消息；将该私聊关联至已有主会话后，真实 MCP fixture 解析 XML，再通过显式 `to=ou_fixture` 发给指定用户。该验证也覆盖群名、成员元数据的本地 API fixture、Markdown / 文件、重连、daemon 与隔离 Core 重启。证据保存在 `test-results/smoke.json`，不涉及生产飞书收件或真实用户发信。

桌面 UI 以“接收聊天”列表替代日常 ID 输入，支持按群名和最近消息搜索，多聊天关联及可选默认私聊项目；手动 ID 输入只在高级设置显示。原生按钮的自动点击验收限制仍适用。

## 结果

| 层级 | 结果 | 证据 |
|---|---|---|
| Rust 状态与边界测试 | 8 项通过 | `cargo test --locked` |
| Rust 静态检查 | 通过，无警告 | `cargo clippy --locked --all-targets -- -D warnings` |
| Svelte 类型与组件检查 | 0 errors / 0 warnings | `pnpm --dir frontend check` |
| 前端生产构建 | 通过 | `pnpm --dir frontend build` |
| 浏览器操作 | 4 项通过 | `test-results/browser` |
| 完整协议冒烟 | 11 项检查通过 | `test-results/smoke.json` |
| 真实 Pi 1.5.1 | 通过一个模型回合 | `test-results/real-pi.json` |
| 真实 Claude Code 2.1.319 | 通过一个模型回合 | `test-results/real-claude.json` |
| 真实 Codex CLI 0.167.2 | 通过一个模型回合，自动投递无需人工重试 | `test-results/real-codex.json` |
| 真实飞书发送 | 一条 Markdown 和一个小附件，均获得 message_id | `test-results/live-receipts.json` |
| release 构建 | 通过，前端已嵌入 | `target/release/hfb` |

默认 Rust 测试会忽略真实飞书测试，防止常规测试发出消息。本次经用户授权，另行显式运行 `cargo test --test live_feishu -- --ignored`，结果通过。

## 验证范围

Rust 测试覆盖入站去重、出站幂等键、崩溃后的 uncertain 恢复、项目间队列公平性、ACK 归属、机器人消息过滤、终端 PID/标签/代次核对、附件路径和符号链接边界、本机 API 身份/Origin 校验、项目级 MCP 权限，以及明确未输入与结果不确定错误的区分。隔离 Core 的「在 HERDR 打开」也会拒绝向默认 App 传递同号终端。

浏览器测试实际执行创建项目、无效 chat_id 校验、编辑、未启动项目的消息排队、忽略、刷新持久化、手动令牌登录、桌面活动展示、手机布局和密钥不回显。检查手机页面没有横向溢出，并保存：

- `test-results/desktop.png`
- `test-results/mobile.png`
- `test-results/settings.png`

协议冒烟使用真实 `openlark` SDK 连接本地 WebSocket/HTTP fixture，建立真实隔离 HERDR 终端，运行真实 MCP stdio 初始化和工具调用。检查内容包括多轮收发、入站附件落盘、出站流式上传、重复事件不重复执行、WebSocket 断线重连、daemon 重启保留队列和原终端、Core 重启后继续原会话、停止 Agent 后继续保留入站消息。

真实 CLI 测试使用本机账号和默认模型配置，在脚本创建的工作目录中运行。模型实际调用 `session_ready`、`ack_message`、`send_markdown`、`send_file`；飞书端仍为本地 fixture。测试没有配置或实现模型反向代理。

真实飞书测试复用了现成 MBP 机器人的凭证及其已配置会话，只调用发送 API，不启动该应用的第二套长连接监听。两条消息均标记为开发测试；临时凭证文件已删除，源码中没有真实凭证。

所有测试使用独立 HC_SESSION、XDG 状态和短 socket 路径。测试 Core 已停止，临时 socket 目录已清理；最后检查未发现测试 Agent/MCP 遗留进程。用户原有默认 Core/App 未被停止或重启。

## 真实测试发现并修复的问题

1. Codex 对 MCP 子进程使用环境白名单，需要在单次启动配置中显式传递 HERDR 身份。仅 `--no-daemon` 不能替代这一步。
2. Core 的启动 settle 失败可能明确保证「没有输入」。现在保留结构化错误和 hint，只有确认未输入时自动重新排队；未知网络结果仍标记 uncertain。
3. 单个项目积压超过读取批次时可能遮住其他项目。入站读取改为每项目取队首，并补充回归测试。
4. 手工填写管理令牌时不应在输入首字符就切换页面，已修复并通过浏览器验证。
5. 图标按钮的 title 不等于可靠的可访问名称，为编辑项目补充 aria-label。
6. 相对数据目录需转成绝对路径，否则不同工作目录的 CLI 无法加载生成的 MCP 凭证。
7. 不同 Core session 的终端短地址可能相同。独立 Core 使用 `hfb session attach` 接管，避免在默认 App 打开其它终端。

## 资源短时采样

release 可执行文件为 **11,528,688 字节，约 11.0 MiB**，包括前端静态资源。本机对一个未启动项目、已连接 Core、未配置飞书的 release 服务进行空闲短时采样：

- daemon RSS：13,600 KiB，约 **13.3 MiB**。
- `hc connect` 辅助进程 RSS：7,280 KiB，约 **7.1 MiB**。
- 两者 RSS 相加：20,880 KiB，约 **20.4 MiB**。
- `ps` 报告 CPU：0.0%。

该值不包含浏览器、Core 或 Agent 进程，也不是峰值或长时间运行保证；RSS 相加可能重复计算共享页。原始记录及较早的一次单进程采样在 `test-results/runtime.json`。

通过 HTTP 读取 release 中的 HTML、JS、CSS，并逐字节核对当前 `frontend/dist`，确认打包资源一致。前端构建的 JS gzip 约 25.4 kB，CSS gzip 约 4.4 kB。

## 保留的本地预览

- 地址：`http://127.0.0.1:49321`
- 数据目录：`.local/review`
- Core session：`hfb-review`
- 初始项目：Herdr Bridge，指向当前源码目录，未启动 Agent。
- 飞书凭证：未配置，不会与 MBP 机器人的生产收件竞争。

打开：

```sh
./target/release/hfb --data-dir .local/review ui
```

预览未安装成系统自动启动服务。若预览进程已退出，可以重新运行：

```sh
./target/release/hfb --data-dir .local/review serve --port 49321 --core-session hfb-review
```

## 未验证

- Antigravity 1.5.9 已实机启动并通过项目目录信任；当前账号运行时报 `plan model not specified`，尚未完成真实 MCP 模型回合。
- 用户真实飞书入站事件的权限和生产路由，未通过启动第二个监听器进行验证。
- 原生会话恢复参数与 ID 登记已实现；每种厂商 CLI 的退出后恢复、授权窗口、上下文压缩尚未覆盖所有组合。
- Linux/Windows 构建、整机重启、数小时运行和接近 100 MB 附件的压力场景。

## 本地服务恢复修复

修复连接恢复后保留旧红色提示的问题；恢复通知独立于业务状态变化。本机管理请求显式绕过代理，后台连接探测使用较短超时；连接被拒绝时按退避间隔重新拉起 daemon，不重启 HERDR 或 Agent。桌面 3 项测试和前后端 clippy 通过。

隔离实测结束测试 daemon 后，GUI 保持同一进程并自动拉起新 daemon，API 恢复 HTTP 200。生产 GUI 单独更新并重开，原 daemon 和项目主会话保持；原生窗口截图确认旧错误已清除。证据保存在忽略目录 `test-results/connection-recovery`。
