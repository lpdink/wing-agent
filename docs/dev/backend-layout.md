# 后端分层规范（libs/core/wing）

本页是 `libs/core/wing/**` 的**目标分层规范**：谁在哪一层、能依赖谁、东西该放哪、迁移怎么走。
[`architecture.md`](architecture.md) 讲**机制与数据流**（数据怎么流动、会话怎么跑、事件怎么送）；
本页只讲**静态结构与归属**（回答「这个模块放哪、能不能 import 那个包」）。

规范不是口头约定：由 [`libs/core/tests/test_layering.py`](../../libs/core/tests/test_layering.py)
以 AST 解析 import 关系强制执行（见 §4 守门机制）。新增代码违反分层会直接把测试打红。

## 1. 目标分层图

依赖方向**只能向下**（↓）；同层之间按 §4 的规则各论。

```
                                           依赖方向 ↓
┌──────────────────────────────────────────────────────────────────────────┐
│ L4 编排与传输   runtime · background · gateway/*                         │
├──────────────────────────────────────────────────────────────────────────┤
│ L3 核心域       config · context · session · agent · tools · provider    │
│                 audit · commands · diagnostics                           │
├──────────────────────────────────────────────────────────────────────────┤
│ L2 领域设施     store/* · event/* · event_bus · hook_registry(hooks)     │
│                 request_context · tool_registry                          │
├──────────────────────────────────────────────────────────────────────────┤
│ L1 领域模型     schema · media · chain（现 common/tracked_list）         │
├──────────────────────────────────────────────────────────────────────────┤
│ L0 基础         common/*（logger / fs / process / with_retry /           │
│                 token_counter / utils）· build_info                      │
└──────────────────────────────────────────────────────────────────────────┘
```

| 层 | 包（目标路径） | 允许依赖 | 备注 |
|---|---|---|---|
| L0 基础 | `wing.common.*`、`wing.build_info` | 标准库 / L0 | 通用无领域依赖 |
| L1 领域模型 | `wing.schema.*`、`wing.media.*`、`wing.chain` | L0–L1 | 叶子组，另有 R5 约束 |
| L2 领域设施 | `wing.store.*`、`wing.event*`、`wing.event_bus`、`wing.hooks`、`wing.request_context`、`wing.tool_registry` | L0–L2 | 可被任意上层复用 |
| L3 核心域 | `wing.config.*`、`wing.context.*`、`wing.session.*`、`wing.agent.*`、`wing.tools.*`、`wing.provider.*`、`wing.audit.*`、`wing.commands`、`wing.diagnostics.*` | L0–L3 | `tools` / `provider` 另有更严规则（R2/R3） |
| L4 编排与传输 | `wing.runtime`、`wing.background`、`wing.gateway.*` | L0–L4 | 唯一允许依赖 gateway 的位置 |

两处与「只向下」看似冲突、实为设计批准的依赖（写在这里以免后人误当违规）：

- **`chain → store.base.MessageLog`、`chain → event.EVENT_TYPES`**：链拓扑引擎把 I/O 全部委托
  `MessageLog` 抽象，并按 `role` 分发事件记录——这两条上向边是 chain 的设计依赖。
  `chain` 在层次表里标 L1，但**不受 R5 叶子约束**（07 迁出 `wing/common/` 后自然解除）。
- **`runtime → gateway.protocol.AgentOverride`**：L4 内部依赖，R1 对 runtime 豁免；
  `session` / `session_manager` 的同款依赖是**违规**（见 §5 白名单），07 或 10 必须清。

另外 **§6 的两类规则内例外**（叶子组 `common ↔ media ↔ schema` 互依、3 条函数内懒加载）
同样是刻意批准的横向 / 向上依赖，一并登记在那里。

## 2. 每个包的职责一句话

| 包 / 模块 | 层 | 职责一句话 |
|---|---|---|
| `common/` | L0 | 基础库：日志（按日切分 + 轮转）、原子写、进程组管理、重试、token 估算、路径与 id 工具 |
| `build_info.py`（+`_build_info`、`_version`） | L0 | 构建信息读取口：版本 + commit hash（构建期注入，运行期零 git） |
| `schema.py` | L1 | 领域模型：Message / Tool / ToolParam / ToolError / MediaRef 等公共类型与校验 |
| `media.py` | L1 | 图片媒体纯函数层：id / 格式 / 尺寸 / 信封 / 请求期投影 |
| `common/tracked_list.py`（→`chain.py`） | L1 | 链拓扑引擎 TrackedList：uuid/parentUuid 链模型，I/O 全部委托 MessageLog |
| `store/` | L2 | 会话持久状态唯一所有者：SessionStore / MessageLog / SessionMetadata（file / memory 后端） |
| `event/`、`event_bus.py` | L2 | 事件类型 + 注册表 + 序列化边界（WingEvent / EVENT_TYPES / wire_dump）与全局 EventBus 路由 |
| `hook_registry.py` | L2 | Hook 扩展点注册表（before_session_start / before_user_message / before_tool_call / after_tool_call） |
| `request_context.py` | L2 | 每请求上下文（request_id / session_id / client_id，单 ContextVar） |
| `tool_registry.py` | L2 | 工具注册表：命名空间感知注册 + ToolRef 解析 |
| `config.py`（+`default_config`） | L3 | 配置模型 + `WING_HOME` 解析 + 手写默认模板（事实来源） |
| `context_manager.py`、`compactor.py` | L3 | 上下文窗口跟踪 + 压缩（LLM 摘要）+ rewind |
| `session.py`、`session_manager.py`、`session_reaper.py`、`agent_template.py` | L3 | 会话生命周期：Session 状态、多会话与 fork/resume、空闲逐出、agent 模板 |
| `agent/` | L3 | WingAgent 运行时：ReAct 主循环、工具并发执行、事件发射、取消取证、未提交投影 |
| `tools/` | L3 | 内置工具（Bash / Read / Write / Edit / Glob / Grep / ReadImage / AskUserQuestion / TodoWrite） |
| `provider/` | L3 | 模型调用协议层：OpenAI 兼容 / Anthropic 隔离、SSE 传输、provider registry |
| `metrics_registry/`（→`audit/`） | L3 | 指标 / 审计注册中心（EventBus 订阅，原子写 JSON） |
| `magic_command/`（→`commands.py`） | L3 | prompt 命令：registry 元数据 + `$ARGUMENTS` 展开 |
| `diagnostics/`（现 `agent/cancel_watch.py`） | L3 | 中断取证：cancel 快照、不死看门狗、锁争用告警 |
| `runtime.py` | L4 | WingRuntime：service 层协调者，`post()` 唯一入站，路由到 Session / ContextManager |
| `background.py` | L4 | BackgroundScheduler：周期任务宿主（逐出、未来的 dreaming 等） |
| `gateway/` | L4 | FastAPI 网关：HTTP 路由 + WS 事件流 + 鉴权 + 远程工具宿主 |
| `__init__.py` | — | 包入口；**不得 import 任何 wing 子模块**（R6：顶层无副作用，组合根负责显式装配） |

## 3. 迁移映射表（旧路径 → 新路径）

基线 `develop@83751e0` 的路径 → 目标路径；「步骤」列对应任务书编号（03–13），
括号内是重构阶段说明。

| 旧路径（基线） | 新路径（目标） | 步骤 |
|---|---|---|
| `context_manager.py` | `context/manager.py` | 05 |
| `compactor.py` | `context/compaction.py` | 05 |
| `session.py` / `session_manager.py` / `session_reaper.py` / `agent_template.py` | `session/*`（session / manager / reaper / template） | 07 |
| `common/tracked_list.py` | `chain.py` | 07 |
| `media.py` | `media/*` | 08 |
| `schema.py` | `schema/*` | 08 |
| `config.py` | `config/*`（`default_config.py` 并入） | 08 |
| `provider/anthropic.py` / `provider/openai_compat.py` | `provider/anthropic/*` / `provider/openai/*`；provider registry 抽出为独立模块 | 09 |
| `gateway/protocol.py` | `gateway/protocol/*`（领域需要的类型**移出** gateway） | 10 |
| `agent/cancel_watch.py` | `diagnostics/*` | 11 |
| `magic_command/` | `commands.py` | 11 |
| `metrics_registry/` | `audit/*` | 11 |
| `config.load_hooks` | `hooks/*`（与 `hook_registry` 归位一处） | 11 |
| `tools/*.py` | `tools/builtin/*`（一工具一文件） | 11 |
| `runtime.reload_system` | 从 runtime 抽出（拆分） | 11 |
| `AgentOverride`（住 `gateway/protocol.py`） | 领域层（session 侧或独立协议模型模块） | 07 / 10（必须清 R1 白名单） |
| `session.py` / `session_manager.py` 的 `TYPE_CHECKING` 反向依赖 | 随上一条一并清除 | 07 / 10 |

守门测试的族表已**预登记目标路径**（`wing/chain`、`wing/context`、`wing/session`、
`wing/audit`、`wing/commands`、`wing/diagnostics`、`wing/hooks`…）——迁移后同一套规则自动生效，
测试代码无需跟着改。

## 4. 守门机制（`libs/core/tests/test_layering.py`）

**违规检测方式**：解析 `libs/core/wing/**` 全部 `.py` 的 AST，收集每一条 import：

- `ast.Import` 与 `ast.ImportFrom` 都记为「依赖目标模块」；
- **`if TYPE_CHECKING:` 块与函数内懒加载一样计入**——import 即依赖，懒加载只是延迟代价；
- 相对 import 按文件所在包换算成绝对模块名（`wing/agent/core.py` 的 `from .inbox import X`
  → `wing.agent.inbox`；`from ..schema import Y` → `wing.schema`）；包属性形态
  `from wing import store` / `from . import store` 先用**文件系统判定**（`wing/store/`
  存在，不 import）解析成子模块 `wing.store`，只有名字不是子模块时才退回包根
  （`from wing import execute_shell` 记 `wing`，对应 `wing/__init__` re-export 的现状）；
- 每个模块与每个 `wing.*` 目标都必须映射到一个「包族」；**未登记 = 测试失败**（新包必须登记，
  不允许绕过守门）；扫描到的 import 边过少也会失败（防扫描器失效假绿）。

**规则**（每条一个测试，失败信息给出 `规则 + 文件:行 + import 源码 + 修复提示`）：

| 规则 | 名称 | 断言 |
|---|---|---|
| R1 | 传输隔离 | 除 `gateway` / `runtime` / `background` 外，任何模块不得 import `wing.gateway.*` |
| R2 | 工具不越层 | `tools` 不得 import `gateway` / `runtime` / `background` / `session` / `context` / `store`；agent 侧只允许 `wing.agent` 包根的**窄接口符号**（`ToolContext` / `current_tool_call_id`）与 `wing.agent.tool_context`，不得 import `wing.agent.*` 其它子模块 |
| R3 | provider 独立 | `provider` 不得 import `agent` / `session` / `context` / `runtime` / `gateway` / `tools` |
| R4 | 存储不反向 | `store` 不得 import L3 / L4 任一族 |
| R5 | 叶子纯净 | `wing/common/**`、`wing/media*`、`wing/schema*` 不得 import 叶子组 `{common, media, schema}` 之外的 wing 包（例外见 §6） |
| R6 | 顶层无副作用 | `wing/__init__.py` 不得 import 任何 `wing.*` 子模块 |

**断言的子集**：R1–R6 是机器强制的**断言子集**；§1 层表「允许依赖」列的其余单元格
（如 L2 各包不向上 import）是目标约定，靠评审把关，不是守门测试的一条断言。

**已知边界**：本守门只做**静态 AST** 判定——动态导入（`importlib.import_module("wing…")`、
`__import__`、运行时拼接的模块名）不猜（口径对齐
[`libs/wing-probe/wing_probe/guard.py`](../../libs/wing-probe/wing_probe/guard.py) 的
「明确接受的残余风险」声明）；运行时才能定名的形态由 review 兜底。
仓内有**两个** AST import 扫描器，分工与覆盖差异如下（互补，不共用代码——两个包的依赖方向相反）：

| | 本页守门（`libs/core/tests/test_layering.py`） | probe 门禁（`libs/wing-probe/wing_probe/guard.py`） |
|---|---|---|
| 禁用面 | `wing` **内部的分层方向**（按包族判 R1–R6） | probe 代码**不得 import `wing`**（任何形态） |
| 扫描面 | `libs/core/wing/**` | `libs/wing-probe/**` |
| 形态覆盖 | 相对 import 展开、包属性形态（文件系统判定）、函数体位置维度 | 字符串导入入口（`importlib.import_module` / `__import__`）与别名追踪 |
| 动态导入 | 不检（显式边界） | 检字面量形态，其余显式残余风险 |

**白名单机制**：现状违规锁定在测试的 `KNOWN_VIOLATIONS`（规则名 → 条目集合），条目格式与
失败信息中的违规一致：

```
"wing/session.py:36 → wing.gateway.protocol"
```

每条必须注明由哪个步骤清除。判定是**集合严格相等**：未登记的新违规 → 红；已清除/已迁移的
条目（stale，含行号漂移）→ 也红（附「删除条目 / 更新行号」提示）。
**白名单是待办清单，不是豁免开关**——13 步骤（分层白名单清零）终结时必须为空。

## 5. 白名单基线（develop@83751e0，10 条）

| 规则 | 条目 | 清除步骤（预期） |
|---|---|---|
| R1 | `wing/session.py:36 → wing.gateway.protocol` | 07（或 10）：`AgentOverride` 移出 gateway |
| R1 | `wing/session_manager.py:44 → wing.gateway.protocol` | 07（或 10） |
| R2 | `wing/tools/explorer.py:21 → wing.context_manager` | 03：删除 Explorer |
| R2 | `wing/tools/explorer.py:25 → wing.store` | 03 |
| R2 | `wing/tools/explorer.py:29 → wing.agent.core` | 03 |
| R2 | `wing/tools/explorer.py:81 → wing.agent.core` | 03 |
| R5 | `wing/common/tracked_list.py:28 → wing.store.base` | 07：迁出 `wing/common/`（chain 不受 R5 约束） |
| R5 | `wing/common/tracked_list.py:171 → wing.event` | 07 |
| R6 | `wing/__init__.py:1 → wing.tools` | 06：顶层无副作用化 |
| R6 | `wing/__init__.py:2 → wing.metrics_registry` | 06 |

## 6. 规则内例外（不是白名单）

两类**经确认的设计依赖**直接写进规则（`test_layering.py` 的 `LEAF_GROUP` /
`LEAF_LAZY_EXCEPTIONS`），新出现其它跨组依赖照常变红：

1. **叶子组内互依允许**：`common` ↔ `media` ↔ `schema` 相互 import 不算违规。现状：
   `media → schema`、`schema → common.token_counter`（`Message.estimate_tokens` 懒加载）、
   `common.token_counter → schema/media`（token 估算需要消息与图片类型）。
2. **3 条精确到「源文件 → 目标模块」的懒加载例外（仅函数体内生效）**：
   - `wing/common/logger.py → wing.config`：`setup_logging()` 需要 `get_wing_home()` 决定日志目录；
   - `wing/common/with_retry.py → wing.event`、`→ wing.event_bus`：仅在重试真正发生时发 NoticeEvent。

   三条都是函数内懒加载（import 期零结构依赖、无加载副作用），且没有重构步骤负责它们；
   登记为规则的一部分，而不是「待清理违规」。**位置维度是强制的**：同样的
   `文件 → 目标模块` 若写成模块级 import 照常变红（`in_function` 由扫描器跟踪，并有
   `test_leaf_exceptions_are_function_local` 守护例外条目本身）。若未来要收紧
   （例如依赖注入化），改的是规则与测试常量，不是绕过守门。

## 7. 怎么加新代码

1. **新工具** → `tools/`（11 后 `tools/builtin/`）。工具**不得** import `session` / `context` /
   `store` / `runtime` / `gateway` / `background`，以及 `wing.agent.*` 子模块与 `wing.agent`
   包根的非窄接口符号（只放行 `ToolContext` / `current_tool_call_id`）——这就是 R2 的全部禁令；
   其余依赖（`schema` / `media` / `common` / `event` / `tool_registry` / `config` 等）由评审
   判断：**允许清单是评审期望，守门只拦上述禁令**。远程工具走 `wing-sdk`，不在本包内。
2. **新 provider（协议）** → `provider/` 下独立子包，实现 `ModelProvider`；协议差异收敛在子包内，
   不得 import `agent` / `session` / `context` / `runtime` / `gateway` / `tools`（R3）。
3. **跨层类型** → 领域层需要的类型放 `schema`（模型）或 `event`（事件）；只在网关边界出现的
   传输模型放 `gateway/protocol/`。域层**不得**反向 import `gateway`（R1）——所以写在
   `gateway/protocol.py` 里的领域类型（如 `AgentOverride`）必须迁往域层。
4. **新包** → 在 `test_layering.py` 的 `FAMILY_RULES` 登记包族，并到本页补一行职责——
   未登记会让守门测试直接失败（这是刻意的）。

## 8. 与 architecture.md 的分工

- **本页**：静态结构——分层、职责归属、允许的依赖方向、迁移映射、「东西放哪」；
- **[architecture.md](architecture.md)**：运行机制——三层架构数据流、前端形态、会话生命周期、
  持久化与压缩、事件系统。

改结构（移动 / 新增 / 删除模块）时同步本页与 `test_layering.py` 的族表；改机制时同步
architecture.md。
