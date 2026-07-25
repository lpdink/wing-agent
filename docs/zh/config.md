# 配置参考

wing-agent 从 `~/.wing/core/config.yaml` 读取后端配置。首次运行时自动创建带详细注释的模板。

除 `openai` 和 `agents` 外，所有字段均为可选。

TUI 前端有独立的配置文件 `~/.wing/tui/config.yaml`（颜色、布局、gateway 设置）。

## 最小示例

```yaml
openai:
  base_url: "https://api.openai.com/v1"
  api_key: "sk-your-key"

agents:
  - name: default
    model: "gpt-4"
    tools: [Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, TodoWrite, Explorer]
```

## 完整参考

```yaml
# ── LLM 服务商 ────────────────────────────────────────
openai:
  base_url: "https://api.openai.com/v1"   # OpenAI 兼容端点
  api_key: "sk-xxx"                        # API 密钥
  timeout_first_chunk: 300.0              # 流式首包超时（秒）
  timeout_total: 600.0                     # 非流式总超时（秒）
  explicit_cache_mode: true               # 追加缓存标记以提升 prompt 缓存命中率
  reasoning_effort: null                  # 推理力度: low/medium/high/xhigh/max (null = 服务商默认)

# ── Agent 模板 ────────────────────────────────────────
agents:
  - name: default                          # 模板名称（必须唯一）
    model: "gpt-4"                         # 模型标识
    default: true                          # 未指定时作为默认模板
    system_prompt: ""                      # 系统提示词（空 = 不注入任何系统提示词）
    tools:                                 # 启用的工具
      - Bash
      - Read
      - Write
      - Edit
      - Glob
      - Grep
      - AskUserQuestion
      - TodoWrite
      - Explorer
    context_window_tokens: 256000          # 触发压缩的上下文窗口上限
    keep_recent_tokens: 50000              # 压缩后保留的近期 token 数
    max_turns: null                        # agent loop 最大轮数（null = 不限）
    yolo: null                             # 每 agent 的 yolo 覆盖（null = 继承全局）
    skills:                                # Skills glob 模式
      - ~/.agents/skills/*/SKILL.md
      - .claude/skills/*/SKILL.md
      - .agents/skills/*/SKILL.md
    rules:                                 # Rules glob 模式
      - AGENTS.md

# ── Hooks ─────────────────────────────────────────────
hooks: []                                  # Hook 文件 glob 模式, 如 ~/.wing/hooks/*.py

# ── 安全 ──────────────────────────────────────────────
yolo: false                                # 跳过危险命令安全审查
steer: true                                # 启用 steer 模式
preserved_thinking: true                   # 保留推理内容（不清理）

safe_command_patterns: []                  # 自动放行的 bash 命令正则白名单
                                           # 如 ["^ls ", "^cat "]

# ── 工具结果截断 ──────────────────────────────────────
# 内置的超长工具结果截断。当结果超过 max_length 字符时，
# 完整输出存入临时文件，上下文中仅保留头尾字符。
tool_result_truncate:
  max_length: 100000                       # 触发阈值（字符）。null 或 <0 禁用
  keep_chars: 200                          # 截断时头尾各保留的字符数

# ── 日志 ──────────────────────────────────────────────
log:
  level: "WARNING"                         # 日志级别 (DEBUG/INFO/WARNING/ERROR/CRITICAL)

# ── Gateway ───────────────────────────────────────────
# 仅在直接启动 wing-gateway 时使用（独立部署）。
# Rust TUI 从 ~/.wing/tui/config.yaml 读取 gateway 设置。
gateway:
  host: "127.0.0.1"                        # 监听地址
  port: 32523                              # 监听端口
  auth:                                    # 可选的 API key 鉴权（默认关闭）
    enabled: false                         # 总开关
    keys:
      - key: "my-secret"                   # ASCII 可打印字符；由客户端发送
        role: admin                        # 预留未来 RBAC（当前不强制）

# ── Prompt 命令 ───────────────────────────────────────
commands:
  paths: []                                # 额外的 prompt 命令（.md）文件路径

# ── User Agent ────────────────────────────────────────
user_agent:
  preset: "qwen-code"                      # 客户端身份预设 (opencode | qwen-code)
```

## 字段详解

### openai

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `base_url` | string | *（必填）* | OpenAI 兼容的 API 端点 URL |
| `api_key` | string | *（必填）* | API 密钥 |
| `timeout_first_chunk` | float | 300.0 | 流式首包超时（秒）。某些服务商在涉及工具调用时首包延迟较高。 |
| `timeout_total` | float | 600.0 | 非流式响应的总超时时间。 |
| `explicit_cache_mode` | bool | true | 启用后，在每次请求的最后一个 content 块追加 `cache_control` 标记。支持 prompt 缓存的服务商会利用这些标记；不支持的服务商静默忽略。 |
| `reasoning_effort` | string? | null | 控制推理深度。可选值：`low`、`medium`、`high`、`xhigh`、`max`。为 null 时不发送此参数。 |

### agents

每个条目定义一个 agent 模板。可以有多个模板，通过 `/agents <name>` 切换。

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `name` | string | *（必填）* | 唯一的模板名称 |
| `model` | string | *（必填）* | 发送给 API 的模型标识 |
| `default` | bool | false | 未指定模板时作为默认使用 |
| `system_prompt` | string | "" | 系统提示词。**空字符串表示不注入任何系统提示词** — wing 从不添加隐藏的 prompt。 |
| `tools` | list[string] | [] | 启用的工具名称。参见[内置工具](#内置工具)。 |
| `context_window_tokens` | int | 256000 | 触发上下文压缩的 token 上限。 |
| `keep_recent_tokens` | int | 50000 | 压缩后保留的近期 token 数。 |
| `skills` | list[string] | [] | Skill 文件的 glob 模式（Markdown）。Skills 会被注入系统提示词。 |
| `rules` | list[string] | [] | Rule 文件的 glob 模式（Markdown）。Rules 会被注入系统提示词。 |
| `max_turns` | int? | null | 每次请求的 agent loop 最大轮数（null = 不限）。 |
| `yolo` | bool? | null | 每 agent 的危险命令审查覆盖（null = 继承全局 `yolo`）。 |

### hooks

Python hook 文件的 glob 模式。Hooks 在定义的扩展点扩展 wing 的行为：`before_session_start`、`before_user_message`、`before_tool_call`、`after_tool_call`。

详见[自定义工具与 Hooks](custom-tools.md)。

### safe_command_patterns

自动放行（无需用户确认）的 bash 命令正则模式。示例：

```yaml
safe_command_patterns:
  - "^ls "
  - "^cat "
  - "^git status"
  - "^git log"
```

不匹配任何模式的命令需要用户确认（或设置 `yolo: true`）。

### gateway

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `host` | string | "127.0.0.1" | 直接启动 `wing-gateway` 时的监听地址。 |
| `port` | int | 32523 | 监听端口。 |
| `auth.enabled` | bool | false | API key 鉴权总开关。 |
| `auth.keys` | list | [] | `{key, role}` 条目列表。`key` 须为 ASCII 可打印字符；`role` 预留未来 RBAC（当前不强制）。 |

> **注意：** 使用 Rust TUI（`wing`）时，gateway 设置从 `~/.wing/tui/config.yaml` 读取。在那里设置 `api_key`，客户端会随每个 HTTP/WS 请求发送。`/api/health` 始终豁免。加密（TLS）交由反向代理负责。

### tool_result_truncate

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `max_length` | int? | 100000 | 触发阈值（字符）。工具结果超过此值时，完整输出存入临时文件，上下文中仅保留头尾。`null` 或 `<0` 禁用。 |
| `keep_chars` | int | 200 | 截断时头尾各保留的字符数（须 ≥ 0）。 |

### user_agent

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `preset` | string | "qwen-code" | 客户端身份预设（`opencode` \| `qwen-code`）。决定发给服务商的身份请求头。 |

### commands

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `paths` | list[string] | [] | 除默认发现位置外，额外的 prompt 命令（`.md`）文件路径。 |

## 环境变量

| 变量 | 说明 |
|------|------|
| `WING_HOME` | 覆盖主目录（默认 `~/.wing`）。后端数据在 `$WING_HOME/core/`，前端在 `$WING_HOME/tui/`。 |
| `WING_SESSIONS_PATH` | 覆盖 session 存储目录 |

## 内置工具

| 工具 | 说明 |
|------|------|
| `Bash` | 执行 shell 命令（含安全审查） |
| `Read` | 读取文件内容（支持行范围） |
| `Write` | 创建或覆写文件 |
| `Edit` | 精准的字符串替换 |
| `Glob` | 按 glob 模式查找文件 |
| `Grep` | 正则搜索文件内容 |
| `AskUserQuestion` | 向用户提问 |
| `TodoWrite` | 跟踪任务进度 |
| `Explorer` | 自主代码探索子 agent（可阻塞或后台运行） |
| `BetterEdit` | 锚定 `[upto]` 编辑（实验性） |
