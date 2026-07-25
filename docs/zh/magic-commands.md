# 魔术命令参考

魔术命令是你在 TUI 输入区输入的斜杠命令。输入 `/` 可打开带模糊搜索和参数提示的命令面板。

## 工作机制

尽管叫「魔术命令」，后端已不再有「magic dispatch」。一个斜杠命令会落到三条路径之一：

| 路径 | 行为 | 例子 |
|------|------|------|
| **前端 → HTTP** | TUI 拦截命令并调用 Gateway 的 HTTP 端点 | `/compact`、`/model`、`/rewind`、`/reload` |
| **前端本地** | 完全在 TUI 内处理，不发给 gateway | `/clear`、`/copy` |
| **Prompt 展开** | 用户 `.md` 文件被展开（替换 `$ARGUMENTS`）后作为普通消息发送 | 自定义 `/plan` 等 |

> **中断不是斜杠命令** —— 按 **Esc** 中断当前 agent 轮次（底层是 `POST /api/session/interrupt`）。

## 命令一览

| 命令 | 别名 | 参数 | 说明 |
|------|------|------|------|
| `/clear` | | | 清空聊天视图 |
| `/copy` | | `[N]` | 复制最近（或第 N 条）assistant 消息到剪贴板 |
| `/new` | | `[name]` | 创建新 session |
| `/session` | `/ss` | `[session_id]` | 切换 session，或列出所有 session |
| `/fork` | | `<uuid>` | 从指定消息分叉出新 session |
| `/model` | `/m` | `[name]` | 查看或切换模型 |
| `/agents` | | `[name]` | 查看或切换 agent 模板 |
| `/title` | | `[name]` | 查看或设置 session 标题 |
| `/workdir` | | `<path>` | 切换工作目录 |
| `/think` | `/t` | `on\|off\|low\|medium\|high\|xhigh\|max` | 开关 thinking / 设置推理力度 |
| `/yolo` | | `on\|off` | 开关 YOLO 模式（跳过危险命令审查） |
| `/compact` | | | 压缩 session 上下文 |
| `/context` | | | 显示上下文统计与系统提示词 |
| `/skills` | | | 显示已加载的 skills |
| `/rewind` | | `<uuid>` | 回退到指定消息（丢弃其后所有消息） |
| `/reload` | | | 重载配置、hooks、provider、skills（无需重启） |
| `/goal` | | `<prompt>` | 启动 Goal 编排（executor + checker 循环） |
| `/goal-exit` | | | 退出 Goal 编排模式 |

本清单的事实来源是 `crates/wing/src/ui/popup/command.rs`（`TUI_ONLY_COMMANDS`）。

## 会话管理

```
/new my-project        # 创建命名 session
/ss                    # 列出所有 session
/ss abc123             # 切换到 session abc123
/fork a1b2c3d4         # 从该 uuid 的消息分叉
/rewind a1b2c3d4       # 回退到该消息（丢弃其后全部内容）
/title refactor-auth   # 重命名当前 session
/workdir ~/code/other  # 切换工作目录
```

## 模型、agent 与行为

```
/model                 # 打开模型选择弹窗
/model gpt-4           # 切换模型
/agents coder          # 切换到 "coder" 模板
/think high            # 设置推理力度
/think off             # 关闭 thinking
/yolo on               # 跳过命令安全审查（危险！）
```

## 上下文与维护

```
/compact               # 摘要旧消息，保留近期消息
/context               # 消息数、token 用量、系统提示词
/skills                # 列出已加载的 skill 文件
/reload                # 无需重启即可生效配置/hooks/skills 变更
```

## Goal 编排

`/goal` 将 **executor**（当前会话）与一个独立的 **checker** 会话配对，循环验证结果——让 agent 不再自评自己的成果。机制详见 [docs/dev/architecture.md](../dev/architecture.md)。

```
/goal 重构 auth 模块并补充测试
/goal-exit             # 退出 Goal 模式
```

## Prompt 命令

除内置命令外，wing 支持 **prompt 命令** —— 以 Markdown 文件定义的自定义命令。文件正文（不含 YAML frontmatter）即为 prompt；`$ARGUMENTS` 会被替换为你在命令后输入的内容。

```yaml
# ~/.wing/core/config.yaml
commands:
  paths:
    - "~/.wing/commands/*.md"
```

示例 `~/.wing/commands/plan.md`：

```markdown
---
description: 规划一次实现
---
为以下内容创建详细的实现计划：$ARGUMENTS
```

随后 `/plan auth feature` 会将展开后的文本作为普通消息发送给 agent。Prompt 命令是 `GET /api/commands` 唯一返回的命令。
