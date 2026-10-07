# 在 omnigent 里使用 wing

> 受众：omnigent 用户。适用版本：带 `acp.agents` 配置块的 omnigent（自定义 ACP agent harness）与带 `wing acp` 子命令的 wing（ACP v1）。
> 能力矩阵与共同限制见 [README.md](README.md)，前置条件（wing 安装 / 配置）也在那里。

## 前置条件

1. wing 已安装并配置好（见 [README](README.md) 的「前置条件」一节）；
2. 按 README 的「先验证 `wing acp`」自检能拿到 `protocolVersion: 1` 的响应；
3. omnigent 已安装：`omni`（等价命令 `omnigent`）可用。

## 配置（`~/.omnigent/config.yaml`）

omnigent 用 `acp.agents` 这个顶层块登记「一条命令」形式的 ACP agent。加一个 wing 条目：

```yaml
acp:
  agents:
    - name: Wing
      command: wing acp
```

- `name`：显示名，同时决定 slug —— `Wing` → `wing`，所以 harness id 是 **`acp:wing`**（名字里的非字母数字会被折成 `-`，同 slug 冲突时加 `-2`、`-3` 后缀）；
- `command`：**一条可带参数的完整命令**，用 omnigent 所在主机的 PATH 解析。找不到 `wing` 时写绝对路径（例如 `/Users/you/.local/bin/wing acp`）。

### 自定义 `WING_HOME`（或其它环境变量）必须声明

omnigent 启动 ACP 命令时，环境变量是 **deny-by-default** 的：只有 `HOME` / `PATH` / `TERM` / `TMPDIR` 和代理、`XDG_*` 等少数在白名单里，其它变量默认不会传给命令。所以只要你依赖了默认路径之外的东西（自定义 `WING_HOME`、指向 venv 的变量……），都要在条目里按**变量名**声明：

```yaml
acp:
  agents:
    - name: Wing
      command: wing acp
      env_passthrough: [WING_HOME]
```

只写名字，不写值（值从 omnigent 进程自己的环境里读）。用默认的 `~/.wing` 且 `wing` / `wing-gateway` 在 PATH 里时，可以省略这一行。

### 建议：关掉 MCP 中转

wing 没有 MCP 支持（客户端转发的 `mcpServers` 会被忽略），omnigent 的 MCP 中转对 wing 没有意义：

```yaml
acp:
  agents:
    - name: Wing
      command: wing acp
      omnigent_mcp: false
```

### 可选字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `model` | 无 | 固定这个 wing agent 的初始模型（形如 `provider:model`，取值看 wing 的 `wing models` 输出 / 模型选择器）；不填则用 wing 侧的默认模型 |
| `session_id_mode` | `server` | 保持默认：会话 id 由 wing 生成 |
| `send_model` | `false` | 不需要（那是给把模型塞进 `session/new` 的 agent 用的） |
| `omnigent_mcp` | `true` | 建议设 `false`，见上 |
| `inject_system_prompt` | `true` | 是否把 omnigent 的 system prompt 折进第一轮；wing 自带 system prompt，若不希望混入可设 `false` |
| `env_passthrough` | `[]` | 需要透传给 wing 的环境变量名列表（如 `WING_HOME`） |

也可以走向导：`omni setup` 的 harness 配置页里有「Add custom ACP agent」入口，会依次问 name / command /（可选）model 并写进 `acp.agents`——`env_passthrough`、`omnigent_mcp` 这些字段仍需要按上面的格式手改配置文件。

## 用法

```bash
# 命令行直接跑
omni run --harness acp:wing

# 或在 Web / TUI 的新会话里，从列表里选「Wing」（显示名就是条目里的 name）
```

`--harness acp:wing` 中的 `wing` 就是上面 `name` 推导出的 slug。

## 一期能力（omnigent 视角）

| 功能 | 在 omnigent 里的样子 |
|------|----------------------|
| 流式输出 | 助手文本与思考过程逐段出现 |
| 工具调用 | 工具卡片（标题、类型、参数），完成后附结果文本 |
| 权限审批 | wing 侧需要确认的命令（如 Bash）变成审批卡片，选项就是 wing 给出的：**允许一次 / 总是允许 / 拒绝**（「总是允许」等价于给该会话打开 wing 的 yolo 模式，慎用） |
| Ask 提问 | omnigent 暂不支持 ACP 的 elicitation（agent 发的表单请求会被回「方法未支持」），wing 会自动**降级**：`AskUserQuestion` 的问题变成一张张**权限卡片**逐题呈现，每张卡片的选项就是该题的预定义答案 |
| 模型切换 | 会话输入框里的 `/model` 选择器：omnigent 读 `session/new` 返回的配置项（id 为 `model`），切换时调用 `session/set_config_option`，wing 热切换模型且保留会话历史 |
| 取消 | 中断当前轮次 → wing 以「已取消」收尾 |

模型候选从哪来：默认不受限制（你选/输入什么 id 就传什么）；如果 agent spec 里显式绑定了某个 provider（`executor.auth: {type: provider, name: …}`），候选项会受该 provider 的 `models:` 列表约束。细节见 omnigent 官方文档的 "Custom ACP agents" 一节。

## 一期不支持（omnigent 视角）

- **会话列表 / 恢复**：omnigent 不调用 `session/list`、`session/load`、`session/resume`，所以没有「导入历史会话」「接着上次聊」的入口；wing 侧的会话仍然存在，可以用 `wing ps` / `wing tail` 看；
- **关闭会话**：omnigent 不调用 `session/close`；会话会留在 wing 侧（可 `wing release <id>` 逐出内存，磁盘保留）；
- **标题同步 / 斜杠命令补全**：更新类型不在 omnigent 的消费集合里，不展示；
- **工具 diff 视图**：工具结果按内容原样展示，没有 Zed 那样的 diff 渲染。

## 排障

1. **agent 起不来**：确认 `command` 的第一段能在 omnigent 进程的 PATH 里找到（PATH 在白名单里，但 GUI / 服务化部署的 PATH 可能与终端不同）→ 写绝对路径；
2. **wing 用了错误的目录 / 配置**：自定义了 `WING_HOME` 却没加 `env_passthrough`，wing 会退回默认的 `~/.wing`。另外如果你在 omnigent 侧设置过 `OMNIGENT_ACP_ENV_UNSET`，检查它没有把 wing 需要的变量清掉；
3. **超时**：omnigent 对一轮 prompt 设了「无进展空闲超时」（默认 300 秒；`HARNESS_ACP_PROMPT_TIMEOUT_S`，在运行 omnigent / harness 的进程环境里设置，远程 runner 部署则设在 runner 宿主上）——它管的是 agent **长时间不出帧**（例如一条跑很久、中间不吐事件的命令）。等你回答权限 / Ask 卡片时轮次是阻塞等待的，不会被它掐断；但也别把卡片无限晾着：omnigent 服务端等待裁决默认一天，单轮硬上限默认 3 小时（`HARNESS_TURN_ABSOLUTE_TIMEOUT_S`），而 wing 自己会在约 100 分钟（`FEEDBACK_TIMEOUT`，6000 秒）把该次询问判超时（工具调用以超时失败收尾）；
4. **看 wing 侧发生了什么**：`wing ps`（会话在不在）、`wing tail <session-id>`；日志在 `~/.wing/core/logs/`（网关）与 `~/.wing/tui/logs/`（`wing acp` 进程）。omnigent 自身的日志位置见其官方文档；
5. **最小复现**：`omni run --harness acp:wing` 直接跑一句话，比在界面里排查更快。

## 最小验证清单（接好之后跑一遍）

1. `omni run --harness acp:wing` 发一句普通问题 → 应看到流式输出；
2. 让它「在项目里跑一条命令」→ 默认配置下（`safe_command_patterns` 为空）**任何** Bash 命令都会弹审批卡片；选「允许一次」后命令执行；
3. 用 `/model` 换个模型 → 再发消息仍正常继续（历史保留）；
4. 让它「用 AskUserQuestion 问我一个问题」→ 应出现逐题权限卡片（不是表单），选一个选项后 agent 收到答案。

<!--
对账（integration 阶段核对后删除）：
- Ask 降级为「逐题权限卡片」的具体形态（选项来源、自由文本是否可用、取消语义）以 03 步实现为准。
- `model` 字段的取值格式（`provider:model`）与「warm switch 应用」细节以 04 步实现与 omnigent 实际行为为准。
- `HARNESS_ACP_PROMPT_TIMEOUT_S` 的设置位置与生效方式（在 omnigent 服务进程环境里 export）以 omnigent 实际行为为准。
- `omni run --harness acp:wing`、`omni setup` 向导文案以 omnigent 当期版本为准。
-->
