# 项目约定

- 这是独立 Rust daemon，前端为 Svelte 静态资源。业务状态集中在 daemon，CLI/UI/MCP 共用接口。
- 不改用户的真实 Core/App 配置或终止其终端。HERDR 测试使用独立 HC_SESSION 和短的 XDG_RUNTIME_DIR。
- 凭证不进入 Git、日志或截图。真实飞书测试须明确选择机器人与聊天，不与已有机器人竞争长连接事件。
- 消息提交不等于处理完成；网络结果不确定时保留 uncertain，不自动重复执行。
- 仅补充能捕获真实业务回归的测试。前端修改运行 pnpm check/build，Rust 修改运行相关测试和 clippy。
- 发布构建先生成 frontend/dist，再构建 Rust。运行时不依赖前端开发服务器。
