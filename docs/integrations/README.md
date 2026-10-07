# 集成 ACP 客户端（Integrations）

> 受众：已经（或准备）安装 wing 的最终用户。适用版本：带 `wing acp` 子命令的 wing（该子命令自本次交付起引入；ACP 协议 v1）。
> 本目录只讲「怎么接、怎么用、出问题先看哪里」；实现机制在 [docs/dev/](../dev/architecture.md)。

`wing acp` 是 wing 的第四种前端形态（TUI / stdio / 编排 CLI / ACP）：一个跑在标准输入输出上的 **ACP agent 服务端**，把这台机器上 wing 网关的能力（模型、会话、工具、权限）交给任意 ACP 客户端使用。客户端负责界面，wing 负责干活：

- **[Zed](zed.md)** — 在 Agent Panel 里像内置 agent 一样用 wing：流式输出、工具卡片、内联 diff、权限弹窗、模型下拉、会话导入与恢复；
- **[omnigent](omnigent.md)** — 注册成 `acp:wing` harness，`omni run` 或 Web / TUI 界面上直接选。

## 目录

| 文档 | 内容 |
|------|------|
| [zed.md](zed.md) | Zed 接入：`settings.json` 配置、打开线程、一期能力、排障 |
| [omnigent.md](omnigent.md) | omnigent 接入：`acp.agents` 配置、`omni run`、一期能力（含降级说明）、排障 |

## `wing acp` 是什么

ACP（Agent Client Protocol）是一套基于 JSON-RPC 的客户端 ↔ agent 协议：客户端（编辑器 / harness）提供聊天面板，agent（这里就是 wing）通过 `session/new` 建会话、`session/prompt` 收指令，用 `session/update` 流式汇报进展，并在需要用户拍板时发 `session/request_permission`（权限询问）或 `elicitation/create`（表单提问）。

几个后面会反复出现的词：

- **session（会话）**：一次对话的容器；ACP 会话 id 与 wing 会话 id 相同；
- **config option（会话配置项）**：会话级可切换的设置，目前只有模型（id 为 `model`），在客户端里表现为模型选择器；
- **permission（权限询问）**：agent 执行敏感操作前，请客户端弹出确认卡片；
- **elicitation（表单提问）**：agent 请客户端弹出一个表单来收集用户输入（wing 的 `AskUserQuestion` 走这条路）。

```text
Zed / omnigent（ACP 客户端）
        │   ACP over stdio（JSON-RPC，进程的标准输入输出）
        ▼
   wing acp（本机进程，第四前端）
        │   HTTP + WebSocket（与 TUI 完全相同的网关接口）
        ▼
   wing 网关（守护进程）──► LLM provider
        会话持久化：~/.wing/core/sessions/
```

- 一个 `wing acp` 进程服务多个会话；每个 ACP 会话就是一个 wing 会话；
- 网关没在跑时，`wing acp` 会自动拉起（与 `wing start` 同一套逻辑）；已在跑则直接连接；
- `wing acp` 的标准输出只承载 ACP 帧，日志一律走文件（见「排障」）。

## 前置条件

1. **wing 已安装**：`wing` 命令可用（`pip install wing-agent`，或下载 Release 二进制）。**注意版本门槛**：必须包含 `wing acp` 子命令——若 `wing acp --help` 报 `unrecognized subcommand 'acp'`，说明装的是旧版本，升级到带 ACP 前端的版本即可；
2. **已配置过 wing**：`~/.wing/core/config.yaml` 里有可用的 provider / model / agent 模板（首次运行 `wing` 会生成带注释的模板，填好后重启网关或 `/reload`）；
3. **网关能被拉起**：`wing status` 显示 running，或交给 `wing acp` 自动拉起（需要能找到 `wing-gateway` 可执行文件，见「排障」）。

## 先验证 `wing acp`（不装任何客户端）

```bash
# 1) 子命令存在、参数可读
wing acp --help
```

`--agent` / `--model` 可选：`--agent` 指定 wing 的 agent 模板名（`~/.wing/core/config.yaml` 里 `agents:` 的 `name`），`--model` 指定初始模型。

```bash
# 2) 握手自检：给 stdin 喂一帧 ACP initialize，看 stdout 上的 JSON-RPC 响应
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' | wing acp
```

期望：标准输出出现**一行** JSON-RPC 响应。下面是真机捕获的完整一行（`version` 串随构建变化，其余字段逐字一致）：

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},"mcpCapabilities":{"http":false,"sse":false},"sessionCapabilities":{"list":{},"resume":{},"close":{}},"auth":{}},"authMethods":[],"agentInfo":{"name":"wing","version":"0.x.y (构建 commit)"}}}
```

判定标准（只需看这三条；其余字段随版本变化，不必逐字一致）：

- `result.protocolVersion` 为 `1`；
- `result.agentInfo.name` 为 `wing`（`version` 与 `wing --version` 同口径，形如 `0.x.y (构建 commit)`）；
- stdin 关闭后进程正常退出。

> 这一步会按需自动拉起本机网关（已在跑则直接复用），不会改动你已有的会话。

## 能力矩阵（Zed vs omnigent）

| 能力 | Zed | omnigent | 说明 |
|------|-----|----------|------|
| 流式文本 / 思考 | ✅ | ✅ | `agent_message_chunk` / `agent_thought_chunk` |
| 工具卡片（名称 / 类型 / 位置 / 参数） | ✅ | ✅ | `tool_call` + `tool_call_update`，含 kind、文件位置、原始入参 |
| 工具结果 | ✅ | ✅ | 结果文本随卡片展示；超长结果会被截断 |
| Edit / Write 内联 diff | ✅ | ❌ | Zed 渲染 `Diff` 内容块；omnigent 只把结果内容原样展示 |
| 上下文用量（token） | ✅ | ✅ | `usage_update` |
| 会话标题同步 | ✅ | ❌ | `session_info_update`；omnigent 侧不展示 |
| wing 的 prompt 命令（`/` 补全） | ✅ | ❌ | `available_commands_update`（wing 命令来自 `commands.paths` 配置） |
| 权限询问（Bash 危险命令确认） | ✅ | ✅ | `session/request_permission`；客户端原样渲染 wing 的三个选项 |
| Ask 提问（AskUserQuestion 工具） | ✅ 表单提问（elicitation） | ⚠️ 降级为逐题权限卡片 | omnigent 不支持 elicitation，见 [omnigent.md](omnigent.md) |
| 模型切换 | ✅ 会话内模型选择器 | ✅ 会话内 `/model` | config option `model`（值形如 `provider:model`），热切换、不丢会话 |
| 会话导入（历史列表） | ✅ Thread History → Import Threads | ❌ | 需要 `session/list`；omnigent 不调用 |
| 打开旧会话 | ✅ 优先回放历史（`session/load`），否则 `session/resume` | ❌ | 同上 |
| 关闭会话 | ✅（关闭线程） | ❌ | omnigent 不调用 `session/close`；会话留在 wing 侧，可 `wing release` 逐出内存 |
| 取消进行中的轮次 | ✅ | ✅ | `session/cancel`，轮次以 `cancelled` 结束 |
| 工具集 / 权限模型 / system prompt | 来自 wing 配置 | 来自 wing 配置 | 客户端不提供工具，也不改变 wing 的配置 |

## 共同限制

- **MCP**：客户端随 `session/new` 发来的 `mcpServers` 会被 wing 忽略（wing 目前没有 MCP 支持），客户端里配置的 MCP server 不会出现在 wing 会话中；
- **图片 / 音频 / 嵌入式内容**：wing 不声明 ACP 的 image / audio / embeddedContext 能力，随 prompt 发送的图片会被跳过。要看图，请让 agent 用 wing 的 `ReadImage` 工具读本地文件（需要模型声明 vision 能力）；
- **文件 @提及**：客户端把文件引用转成 ACP 资源链接，wing 侧只会把它拍平成一行路径文本附在 prompt 后——文件内容不会自动读入，需要内容时让 agent 用 `Read` 工具读；
- **同一个会话，多个前端**：ACP 的 sessionId 就是 wing 会话 id，可以用其它前端观察同一个会话：
  `wing ps`（列表）、`wing info <id>`（模型 / 工具 / 用量）、`wing tail <id>`（最近消息）、`wing release <id>`（逐出内存，磁盘保留）；
- **配置真值在 wing 侧**：agent 模板、工具集、system prompt、权限白名单（`safe_command_patterns`）、yolo 等都由 `~/.wing/core/config.yaml` 决定，客户端改不了（客户端只能改「本会话的模型」）；
- **一期不含**：ACP Registry 一键安装（规划中，需要发布流水线）、`session/delete`（会话删除）、todo（plan）映射、thought_level / yolo 之类的附加 config option、ACP v2 草案。

## 排障

**先分层定位：客户端 → `wing acp` → 网关。**

1. **客户端里 agent 起不来 / 立刻退出**：先在终端按上面的自检手跑一次（把 `initialize` 帧喂给 `wing acp`）看有没有报错；再看日志与客户端侧的 ACP 帧（Zed：命令面板跑 `dev: open acp logs`）。
2. **提示找不到 `wing-gateway`**：`wing acp` 按 `$WING_GATEWAY_CMD` → `~/.wing/venv/bin/wing-gateway` → `PATH` 的顺序查找网关可执行文件。`pip install wing-agent` 通常把它装在环境里；GUI 客户端（Zed）的 PATH 与终端不同，必要时在客户端配置里用 `env` 把 `WING_GATEWAY_CMD` 指到绝对路径。
3. **网关没在跑**：`wing status` 看状态，`wing start` 手动拉起（`wing acp` 也会自动拉起）；端口被非网关进程占用时会明确报错。
4. **会话行为不对**（模型 / 工具 / 权限不像预期）：`wing info <session-id>` 看运行时信息，`wing tail <session-id>` 看消息；再查网关日志。
5. **日志位置**（按本地日期，一天一个文件，保留 7 天）：
   - `wing acp`（前端进程，与 TUI / 其它子命令同一份）：`$WING_HOME/tui/logs/wing_YYYY-MM-DD.log`，默认 `~/.wing/tui/logs/`；加 `RUST_LOG=wing=debug`（或 `wing=trace`）提高详细度；
   - 网关（后端）：`$WING_HOME/core/logs/new.log`（活跃日志的符号链接）；守护进程的 stdout / stderr 在 `~/.wing/core/logs/gateway.log`。
   目录布局与按日期 grep 的技巧见 [docs/dev/config-logging.md](../dev/config-logging.md)。

