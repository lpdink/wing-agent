# wing-orch

Wing 编排 CLI —— 后台 Goal 循环（executor/checker）。

在本地启动一个 tool host（六个标准工具，绑定 workspace），在 Gateway 上
创建 executor 与 checker 两个 session，自动跑"执行 → 校验 → 反馈"循环
直到 checker 判定目标完成。状态机与 TUI 的 Goal 模式同构
（port 自 `crates/wing/src/app/goal.rs`）。

## 用法

```bash
# 无限轮次直到 checker 判定完成（默认）
wing-orch goal "重构 foo 模块并通过所有测试" --workspace /path/to/repo

# 长目标从文件读取；限制轮次
wing-orch goal --task-file goal.md --max-rounds 5

# 中断后恢复（状态持久化在 .wing-orch.json）
wing-orch goal "..." --resume

# 连接远程 Gateway / 鉴权
wing-orch goal "..." --gateway http://10.0.0.5:32523 --api-key <key>
```

## 角色配置

- `--executor-model` / `--checker-model`：分角色指定模型。
- `--executor-tools` / `--checker-tools`：逗号分隔工具引用。
  默认 executor 持有全部六个标准工具，checker 持有 Bash/Read/Glob/Grep
  （只校验、不修改）。
- `--executor-append-system-prompt` / `--checker-append-system-prompt`：
  追加提示词；`--checker-system-prompt` 整体覆盖 checker 系统提示词。

## 行为约定

- 远程工具没有审批路径，executor/checker 均以 **yolo** 模式运行。
- `Ctrl+C`（SIGINT/SIGTERM）优雅退出，exit code 130，状态落盘可 `--resume`。
- Resume：executor 必须可恢复；checker 尝试恢复旧 session，不可用时
  （Gateway 重启等）自动新建；统一从当前 round 的 executor 阶段重新开始。
- checker 连续 3 次输出格式错误（无 `<goal_finish>` 标签）→ stall 退出。
- 开启鉴权时 `--api-key` 需要 **admin** 角色（`tool_runtime` 角色只能
  注册工具，无法创建 session）。

## 日志

默认输出到 stderr；`--log-file` 同时写文件。
