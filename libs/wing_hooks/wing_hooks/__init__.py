"""wing_hooks — 官方 hook 实现。

五个 hook:
1. prefix_timestamp: before_user_message — 给用户消息前缀时间戳
2. truncate_tool_result: after_tool_call — 截断过长的 tool result
3. audit_edit_write: after_tool_call — 审计 Edit/Write 代码变更
4. workspace_env_inject: before_session_start — 注入 workspace 和 OS 信息
5. claude_session_mirror: EventBus 订阅 — 把 wing 会话投影成 Claude 转录
   JSONL（``~/.claude/projects/<cwd 编码>/<session-id>.jsonl``，只读，供
   CloudCLI 等 Claude 生态工具消费）

用户可以 cp -r 到 ~/.wing/hooks/ 目录启用。
也可以直接 import 后调用 register_* 函数注册到自己的 HookRegistry
（claude_session_mirror 额外提供 install()/uninstall() 做 EventBus 订阅）。
"""
