# wing-agent

<p align="center">
  <strong>迈向通用 agent 运行时。</strong>
</p>

**[English](../../README.md)**

> **⚠️ 实验阶段** — v1.0 之前可能包含 breaking change。

## 为什么选择 wing？

**不在上下文中施加魔法。** 我们从不注入隐藏的系统提示词。你看到的就是模型看到的——你的 system prompt、你的工具、你的对话。没有多余的东西。

**理论最高缓存命中率。** 我们承诺达到理论最高的 prompt 缓存命中率。除了压缩，绝不主动破坏缓存前缀。

**极简工具 schema。** 内置工具使用最简化的 schema。上下文窗口初始工具开销不超过 2K tokens——而不是 10K。

## 快速开始

```bash
pip install wing-agent
wing
```

首次运行时，wing 会在 `~/.wing/core/config.yaml` 创建配置模板。编辑其中的 `ChangeHere` 占位符，填入你的 API 端点和密钥，然后再次运行 `wing`。

## 配置

后端配置：`~/.wing/core/config.yaml`
前端配置：`~/.wing/tui/config.yaml`

完整参考：**[docs/zh/config.md](config.md)**

## 内置工具

| 工具 | 说明 |
|------|------|
| `Bash` | 执行 shell 命令（含安全审查） |
| `Read` | 读取文件内容（支持行范围） |
| `Write` | 创建或覆写文件 |
| `Edit` | 精准字符串替换 |
| `Glob` | 按模式查找文件 |
| `Grep` | 正则搜索文件内容 |
| `AskUserQuestion` | 向用户提问 |
| `TodoWrite` | 跟踪任务进度 |
| `Explorer` | 自主代码探索 agent |

自定义工具：**[docs/zh/custom-tools.md](custom-tools.md)**

## 魔术命令

在 TUI 中输入 `/` 查看可用命令。

完整参考：**[docs/zh/magic-commands.md](magic-commands.md)**

## 文档

| 文档 | English | 中文 |
|------|---------|------|
| 配置 | [docs/en/config.md](../en/config.md) | [docs/zh/config.md](config.md) |
| 自定义工具 | [docs/en/custom-tools.md](../en/custom-tools.md) | [docs/zh/custom-tools.md](custom-tools.md) |
| 魔术命令 | [docs/en/magic-commands.md](../en/magic-commands.md) | [docs/zh/magic-commands.md](magic-commands.md) |

## 许可证

[Apache-2.0](../../LICENSE)
