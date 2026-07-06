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
