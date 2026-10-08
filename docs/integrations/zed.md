# 在 Zed 里使用 wing

> 受众：Zed 用户。适用版本：带 External Agents（自定义 ACP agent）的 Zed，与带 `wing acp` 子命令的 wing（ACP v1）。
> 能力矩阵与共同限制见 [README.md](README.md)，前置条件（wing 安装 / 配置）也在那里。

## 前置条件

1. wing 已安装并配置好（见 [README](README.md) 的「前置条件」一节）；
2. 按 README 的「先验证 `wing acp`」自检能拿到 `protocolVersion: 1` 的响应；
3. Zed 侧不需要扩展——「自定义 agent」是 Zed 自带的 ACP 接入方式。

## 配置（`settings.json`）

Zed 的设置文件：macOS 和 Linux（未改 `XDG_CONFIG_HOME`）是 `~/.config/zed/settings.json`；Windows 的路径以 Zed 文档为准。往里加一个 `agent_servers` 条目（已有该字段就并入）：

```json
{
  "agent_servers": {
    "Wing": {
      "type": "custom",
      "command": "wing",
      "args": ["acp"]
    }
  }
}
```

字段含义：

| 字段 | 说明 |
|------|------|
| `Wing` | 这个 agent 的名字（键），也是新线程菜单里显示的名字 |
| `type` | 固定 `"custom"`：自定义 agent（不是从 ACP Registry 安装的） |
| `command` | 要启动的可执行文件；写成 `wing` 或绝对路径都行 |
| `args` | 启动参数；`acp` 即让 wing 以 ACP 前端运行 |
| `env` | 可选：补充环境变量，例如自定义的 `WING_HOME` |
| `default_config_options` | 可选：给会话配置项预置默认值，例如 `{ "model": "<model_id>" }`（值 = `providers[].models` 声明的 **model_id**，取值见线程里的模型选择器 / `wing models`；旧的 `provider:model` 拼串不再接受） |

### PATH 陷阱（最常踩的坑）

Zed 是 GUI 程序，启动 agent 时用的环境变量来自 **Zed 进程本身**（macOS 上通常不包含你在 `~/.zshrc` / `~/.bashrc` 里追加的 PATH 条目）。「终端里 `wing` 能用」不代表 Zed 里也能用。如果 Zed 报 agent 启动失败，先跑 `which wing` 拿到绝对路径，然后：

```json
{
  "agent_servers": {
    "Wing": {
      "type": "custom",
      "command": "/Users/you/.local/bin/wing",
      "args": ["acp"],
      "env": { "WING_HOME": "/Users/you/.wing" }
    }
  }
}
```

（`env` 里的 `WING_HOME` 只在你有自定义目录时需要；用默认的 `~/.wing` 可以省掉。）

### 也可以从 GUI 添加

（以下菜单文案来自 Zed 官方文档，Zed 大版本更新后可能变化。）

命令面板打开 **Agent Settings** → **External Agents** 页面 → `Add Agent` → `Add Custom Agent`，Zed 会打开设置文件并生成一条 `agent_servers` 条目，再把上面的字段补全即可。Zed 会热加载设置，不需要重启。

## 打开线程

- 打开 **Agent Panel**，在新线程菜单里选 `Wing`（Threads Sidebar 同理）；
- 你也可以给 `agent::NewExternalAgentThread` 配快捷键（把 `agent` 参数设为 `Wing` 即可直接开 wing 线程，键位写法见 Zed 的 keymap 文档）；
- 新会话的工作目录 = 当前项目根目录：wing 以它作为会话 workspace，agent 的文件 / 命令工具都在这个目录下动作。

## 一期能力（Zed 视角）

| 功能 | 在 Zed 里的样子 |
|------|------------------|
| 流式输出 | 助手文本与思考过程逐段出现 |
| 工具调用 | 工具卡片（标题、类型、涉及文件、参数），Bash / Read / Edit / Write / Glob / Grep 等按类型呈现 |
| 文件改动 | Edit / Write 的结果以**内联 diff**（改动前后对照）展示 |
| 上下文用量 | 线程里的上下文占用指示（token 用量） |
| 会话标题 | wing 自动生成的会话标题会同步成线程标题 |
| 权限询问 | wing 侧需要确认的命令（如 Bash）弹出权限卡片，选项由 wing 给出：**允许一次 / 总是允许 / 拒绝**（「总是允许」等价于给该会话打开 wing 的 yolo 模式，之后不再询问，慎用） |
| Ask 提问 | agent 用 `AskUserQuestion` 提问时，Zed 以**表单**呈现（Zed 支持 ACP 的 elicitation），题目与选项直接可点 |
| 模型切换 | 线程里的模型选择器（config option `model`），切换后当前会话继续，历史不丢 |
| 会话导入 | Thread History → **Import Threads**：wing 的会话会列出来供导入（没有工作目录的会话会被跳过） |
| 打开旧线程 | 直接打开已导入的线程：wing 声明了回放能力时回放完整历史（`session/load`），否则退回 `session/resume`（不重放历史，但可继续对话） |
| 关闭线程 | 关闭 Zed 线程会让 wing 侧释放该会话的内存态（磁盘保留） |
| 取消 | 停止按钮 / 快捷键 → wing 中断当前轮次，以「已取消」收尾 |

## Zed 侧的限制

- **MCP**：Zed 里配置的 MCP server 不会被 wing 使用（wing 忽略客户端转发的 `mcpServers`），MCP 工具不会出现在 wing 会话里；
- **图片 / @提及**：随消息附带的图片不会送达模型（wing 不声明图片能力）；`@文件` 只作为一行路径文本传给模型，内容不会自动读入；
- **只能改模型**：会话的模型是唯一可以在 Zed 里改的会话配置；工具集 / system prompt / 权限白名单等都要改 wing 配置（`~/.wing/core/config.yaml`），改完 `/reload` 或重启网关后对新会话生效；
- **多根工作区**：会话以当前项目的**主工作目录**作为 wing 会话的 workspace，Zed 传来的其它根目录不会被附加进会话。

## 排障

1. **看 Zed 与 agent 之间的 ACP 帧**：命令面板运行 `dev: open acp logs`，能看到 `initialize` / `session/new` / update 的原始报文；
2. **agent 加载失败**：按顺序排除——`command` 是否在 Zed 进程的 PATH（→ 写绝对路径）；终端能否按 README 的自检拿到握手响应；`wing-gateway` 是否找得到（→ 在 `env` 里设 `WING_GATEWAY_CMD` 指向绝对路径）；
3. **wing 侧日志**：`wing acp` 进程写在 `~/.wing/tui/logs/wing_YYYY-MM-DD.log`（`RUST_LOG=wing=debug` 提高详细度）；网关写在 `~/.wing/core/logs/`（`new.log` / `gateway.log`）；
4. **导入线程为空 / 提示不支持**：说明当前 agent 没有声明 `session/list` 能力，请确认 wing 与 Zed 都是较新版本；
5. **导入后打不开旧线程**：先在终端确认会话还在（`wing ps`，落盘位置 `~/.wing/core/sessions/`）——会话被删除或存储被清理后无法恢复。

## 最小验证清单（接好之后跑一遍）

1. 新开一个 wing 线程，发一句普通问题 → 应看到流式输出；
2. 让它「在项目里跑一条命令」→ 默认配置下（`safe_command_patterns` 为空）**任何** Bash 命令都会弹权限卡片；选「允许一次」后命令执行、卡片变为完成；
3. 让它「改一个文件」→ 应看到内联 diff；
4. 让它「用 AskUserQuestion 问我一个问题」→ 应弹出表单；
5. 用线程里的模型选择器换个模型 → 再发消息仍正常继续（历史保留）；
6. Thread History → Import Threads → 应能列出 wing 会话。

