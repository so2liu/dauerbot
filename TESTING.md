# 本机验证记录

日期：2026-11-15。环境：macOS，Rust 1.96.0，Node.js 22.23.2，pnpm 12.6.0，HERDR Core 0.1.20。这是本机开发验证，不等同于跨平台或长期生产压测。

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

- Antigravity `agy` 本机未安装，尚无实机结果。
- 用户真实飞书入站事件的权限和生产路由，未通过启动第二个监听器进行验证。
- 原生会话恢复参数与 ID 登记已实现；每种厂商 CLI 的退出后恢复、授权窗口、上下文压缩尚未覆盖所有组合。
- Linux/Windows 构建、整机重启、数小时运行和接近 100 MB 附件的压力场景。
