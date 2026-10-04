<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="../../assets/banner-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="../../assets/banner-light.svg">
    <img src="../../assets/banner-dark.svg" alt="wing —— 像素海鸥与 WING 字标" width="620">
  </picture>
</p>

# wing-agent

<p align="center">
  <strong>迈向通用 agent 运行时。</strong>
</p>

**[English](../../README.md)**

> **⚠️ 实验阶段** — v1.0 之前可能包含 breaking change。

<p align="center">
  <img src="../../assets/demo.gif" alt="wing TUI 干活的完整过程：思考流式输出、Edit 长出彩色 diff、测试由红转绿、任务清单逐项打勾" width="920">
</p>

## 为什么选择 wing？

**上下文是你自己的。** 没有隐藏的系统提示词，没有背着你注入的脚手架。模型看到的就是你写的东西——你的 prompt、你的工具、你的历史。跑歪了的时候，你能读到到底发生了什么。

**缓存优先是构造出来的。** 除了压缩，我们从不重写 provider 能缓存的前缀：对话只会增长。长会话始终快而便宜，而不是每轮被从头重算一遍。

**极简工具 schema。** 内置工具用"够用就好"的最小 schema——上下文里的工具开销不到 2K tokens，而不是 10K。少花在描述工具上的 token，就是多留给你的代码。

## 为机器速度而建

TUI 从不重画整段回答：markdown 块在闭合时一次性定型，之后每一帧只重画仍在生长的尾部——
一帧的开销由**屏幕上有什么**决定，而不是由回答已经长了多久决定。

同一段文字、同一个终端，四种喂入速率——流是脚本造的，渲染是真的：

**30 tok/s** —— 顶级推理模型。
<p align="center"><img src="../../assets/speed-30.gif" alt="wing 以 30 tokens/s 流式输出" width="900"></p>

**60 tok/s** —— 当前旗舰。
<p align="center"><img src="../../assets/speed-60.gif" alt="wing 以 60 tokens/s 流式输出" width="900"></p>

**240 tok/s** —— 快档 "flash" 模型。
<p align="center"><img src="../../assets/speed-240.gif" alt="wing 以 240 tokens/s 流式输出" width="900"></p>

**3,000 tok/s** —— 约为今天最快模型的 10 倍。界面不在乎。
<p align="center"><img src="../../assets/speed-3000.gif" alt="wing 以 3000 tokens/s 流式输出" width="900"></p>

端到端显示延迟——Provider → 网关 → WebSocket → TUI → 终端，一 token 一帧（M6 Mac mini）：

| 喂入速率 | p50 | p99 |
|---|---|---|
| 3,000 tok/s | 17 ms | 28 ms |
| 30,000 tok/s | 13 ms | 22 ms |
| 45,000 tok/s | 10 ms | 30 ms |

复算：`uv run python scripts/demo/latency.py --steps 3000,30000,45000`。

## 快速开始

```bash
pip install wing-agent
wing
```

首次运行时，wing 会在 `~/.wing/core/config.yaml` 创建配置模板并退出。打开它，填入你的 **provider、密钥与模型**：

```yaml
providers:
  - name: default
    protocol: openai                         # openai | anthropic
    base_url: "https://your-api-endpoint/v1" # ← 你的 API 地址
    api_key: "sk-xxx"                        # ← 你的密钥

agents:
  - name: default
    model: "gpt-4o"                          # ← 你的模型
    default: true
    tools: [Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, TodoWrite, Explorer]
```

然后启动 wing：

```bash
wing stop    # 如果 Gateway 已在运行，先停掉
wing         # 重新启动
```

> **注意：** Gateway 仅在启动时加载配置。修改 `config.yaml` 后，在 TUI 中执行 `/reload`（或 `wing stop` 再 `wing`）以加载新配置。

## 配置

后端配置：`~/.wing/core/config.yaml` —— 首次运行自动生成，模板带完整注释（见 `wing/default_config.py`）。
前端配置：`~/.wing/tui/config.yaml`

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
| `Explorer` | 自主代码探索子 agent（可阻塞或后台运行） |
| `BetterEdit` | 锚定 `[upto]` 编辑（实验性） |

自定义工具：**[docs/zh/custom-tools.md](custom-tools.md)**

工具调用就在它发生的位置渲染：`Edit` 对着真文件长出一份带语法高亮的 diff，`Bash` 显示真实输出，`TodoWrite` 把计划一直摆在眼前——三者在页面顶部的演示里都能看到。

## 魔术命令

在 TUI 中输入 `/` 查看可用命令。

完整参考：**[docs/zh/magic-commands.md](magic-commands.md)**

## 无头模式（stdio）

`wing` 也能以兼容 Claude Code 的 stdio 协议无头运行——把 `wing` alias 为 `claude` 即可接入外部编排层，或在脚本中直接驱动：

```bash
wing -p "列出当前目录的文件"                    # text（默认）：仅输出最终结果
wing -p "列出文件" --output-format json         # 单个 result JSON 对象
wing -p "列出文件" --output-format stream-json  # 实时 NDJSON 流
```

常用参数：`-m/--model`、`-r/--resume`、`--system-prompt`、`--append-system-prompt`、`--max-turns`、`--effort`、`--input-format`、`--yolo`。为兼容 Claude，未识别的 `--xxx` 参数会被静默忽略。

## 文档

| 文档 | English | 中文 |
|------|---------|------|
| 自定义工具 | [docs/en/custom-tools.md](../en/custom-tools.md) | [docs/zh/custom-tools.md](custom-tools.md) |
| 魔术命令 | [docs/en/magic-commands.md](../en/magic-commands.md) | [docs/zh/magic-commands.md](magic-commands.md) |

## 开发

从 **[AGENTS.md](../../AGENTS.md)** 开始（高信息密度的项目总览）。需要机制级细节（数据流、完整 HTTP API、术语表）请读 **[docs/dev/](../dev/)**。

## 许可证

[Apache-2.0](../../LICENSE)
