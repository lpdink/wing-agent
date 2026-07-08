# 已知问题

## 配置修改后需要手动重启 Gateway

**影响版本:** 0.1.x

**现象：** 修改 `~/.wing/core/config.yaml`（如 `base_url`、`api_key`、`model`）后，直接运行 `wing` 进入 TUI，Gateway 仍然使用旧配置，导致请求发到错误的 endpoint 或认证失败。

**原因：** Gateway 进程仅在启动时加载一次配置，运行期间不会监听配置文件变更。如果 Gateway 已在后台运行，`wing` 命令会直接复用现有 Gateway 连接，不会重新读取配置。

**临时解决方案：**

```bash
wing stop    # 停掉旧的 Gateway
wing         # 重新启动（会加载最新配置）
```

**计划修复：** 下一版本实现配置热重载（file watcher）或在 TUI 启动时检测配置变更并自动重启 Gateway。

## WebSocket 巨量消息导致断连循环

**发现时间**: 2026-07-09

**现象**: 当工具调用结果包含大量数据时（如 Bash 工具读取二进制文件），
Gateway 尝试通过 WebSocket 推送巨量消息，导致客户端断连。TUI 自动
重连后，SyncSession 重放历史消息（包含同一条巨量消息），再次断连，
形成无限断连-重连循环。

**根因**: 传输层缺少消息大小保护。目前依赖可选的 `truncate_tool_result`
hook 做内容截断，但 hook 不是必装的，传输层本身没有兜底机制。

**待办**:

- [ ] 实测 WebSocket 各层（uvicorn / starlette / tungstenite）的最大消息
  大小，确定安全阈值
- [ ] 在传输层（`GatewayServer._send_text`）添加消息大小 guard，
  超限时降级处理（截断或替换为摘要事件），确保不依赖可选 hook
- [ ] SyncSession 重放时对历史消息做同样的大小保护，防止重连循环
- [ ] 考虑在 agent 层面增加二进制内容检测（Bash / Read 等工具），
  从源头阻止二进制数据进入消息流

**设计考量**: 是否将 `truncate_tool_result` 从可选 hook 提升为内置逻辑
尚未决定——需要权衡"核心层不假设 hook 存在"的设计哲学与"传输层必须
自我保护"的鲁棒性需求。
