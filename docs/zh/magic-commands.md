# 魔术命令参考

魔术命令是在 TUI 输入区域输入的斜杠命令。它们直接执行，不经过 LLM。

输入 `/` 打开命令面板，支持模糊搜索。

## Session 管理

### `/new [name]`
创建新 session。可选提供名称以便识别。

```
/new my-project
```

### `/session [id]` — 别名: `/ss`
切换到已有 session。不带参数时列出所有 session。

```
/ss                    # 列出所有 session
/ss abc123             # 切换到 session abc123
```

### `/fork <uuid>`
从指定消息处分叉出新 session。新 session 包含截止到该消息的所有内容。

```
/fork a1b2c3d4         # 从 UUID 为 a1b2c3d4 的消息处叉
```

### `/rewind [uuid|list]` — 别名: `/rw`
将当前 session 回退到指定的用户消息。该消息之后的所有内容被丢弃。

```
/rw list               # 列出可回退的消息
/rw a1b2c3d4           # 回退到此消息
```

## 模型与 Agent

### `/model [name]` — 别名: `/m`
查看或切换模型。不带参数时显示当前模型并弹出模型选择面板。

```
/model                 # 弹出模型选择面板
/model gpt-4           # 切换到 gpt-4
```

### `/agents [name]`
查看或切换 agent 模板。模板定义了系统提示词、工具和模型。

```
/agents                # 列出可用模板
/agents coder          # 切换到 "coder" 模板
```

### `/think [on|off|low|medium|high|xhigh|max]` — 别名: `/t`
控制推理/思考模式。

```
/think off             # 关闭思考
/think high            # 设置推理力度为 high
/think                 # 切换思考开关
```

## 上下文与压缩

### `/compact` — 别名: `/cp`
手动触发上下文压缩。压缩器对旧消息进行摘要，保留近期消息完整。在接近上下文窗口上限时有用。

```
/compact
```

### `/context` — 别名: `/ctx`
显示上下文统计信息：消息数量、token 使用量和当前系统提示词。

```
/ctx
```

## 执行控制

### `/interrupt` — 别名: `/int`
中断当前 agent turn。清空 agent 的收件箱并重置事件循环。agent 立即停止处理。

```
/int
```

### `/yolo [on|off]`
切换 YOLO 模式。启用后，所有 bash 命令跳过安全确认直接执行。也可以在安全提示选单中选择 `yolo` 选项来为当前 session 开启。

```
/yolo on               # 启用（危险！）
/yolo off              # 禁用（安全，默认值）
```

## 直接执行

### `/bash <command>` — 别名: `/b`, `/sh`
直接在 shell 中执行命令，不经过 agent。输出显示在聊天中。

```
/bash ls -la
/b git status
/sh cat package.json
```

## 信息查看

### `/help` — 别名: `/h`, `/?`
显示所有可用的魔术命令及其描述。

### `/skills`
列出所有已加载的 skill 文件及其路径。

### `/reload`
从磁盘重新加载配置、hooks、魔术命令、skills 和 rules。编辑配置文件后无需重启 gateway 即可生效。

```
/reload
```

## Prompt 类型命令

除了上述内置命令，wing 还支持 **prompt 类型命令** — 在 Markdown 文件中定义的自定义命令。从配置中指定的路径加载：

```yaml
# ~/.wing/config.yaml
commands:
  paths:
    - "~/.wing/commands/*.md"
```

每个 `.md` 文件定义一个命令。文件内容在命令被调用时作为 prompt 发送给 agent。文件格式参见 skills/rules 系统。
