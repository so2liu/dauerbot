# 项目约定

- 面向用户的客户端是 `desktop/` 中的 GPUI 原生 macOS 应用，禁止用网页或 WebView 替代最终桌面交付。Svelte 页面保留为辅助管理入口。
- 业务状态集中在独立 Rust daemon，桌面应用、CLI、辅助网页和 MCP 共用接口。后台网络与文件检测不得阻塞 GPUI 主线程。
- macOS 权限以明确选择的路径和实际访问结果为准，不扫描用户隐私数据库、不调用私有 TCC API，不把单个文件访问成功当作完全磁盘访问已授权；说明 Bridge 与 HERDR/Agent 的进程权限差异。
- 不改用户的真实 Core/App 配置或终止其终端。HERDR 测试使用独立 HC_SESSION 和短的 XDG_RUNTIME_DIR。
- 凭证不进入 Git、日志或截图。真实飞书测试须明确选择机器人与聊天，不与已有机器人竞争长连接事件。
- 消息提交不等于处理完成；网络结果不确定时保留 uncertain，不自动重复执行。
- 仅补充能捕获真实业务回归的测试。前端修改运行 pnpm check/build，Rust 修改运行相关测试和 clippy。
- 发布构建先生成 frontend/dist，再构建 Rust。运行时不依赖前端开发服务器。
- 桌面打包使用 `scripts/build-desktop.sh`；本地 ad-hoc 签名只用于开发验证，不代表完成 Developer ID 签名、公证或分发验收。
