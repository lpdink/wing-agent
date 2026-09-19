# wing-probe：确定性集成测试

> 契约与背景：`openspec/changes/wing-probe/`（`design.md` D1–D12、`specs/probe-scenarios/spec.md`）。
> 本页是**使用说明**：怎么跑、怎么加场景、断言能用什么、哪些红线被守。

`libs/wing-probe/` 是一个**不调真实模型、不碰用户 `~/.wing`** 的整机测试台：每个场景自举一个临时 `WING_HOME` + 一个真网关子进程 + 一个进程内假 Provider（按 model 名消费剧本），断言锚定在四个面上——**事件时间线**（WS 实时帧）、**LLM 请求上下文**（假 Provider 留档的请求体）、**落盘 history**（`history.jsonl` 独立解析）、**workspace 文件**。

为什么要有它：上下文红线（compact / rewind / fork 的链语义、tool 配对、瞬态不落盘）属于"改坏了不一定报错、报错了也看不出"的那一类；既有测试要么是 loopback 单测（`libs/core/tests/`），要么依赖真实模型与网络（`e2e/`）。probe 提供可重复、可断言、离线跑的整机证据。

硬约束：**probe 不得 `import wing`**（`wing_probe/guard.py` AST 门禁 + `tests/test_no_wing_imports.py` 每次运行强制）——它是"外部实现"，只通过 HTTP / WS 公开协议观测产品。

## 怎么跑

| 命令 | 做什么 |
|------|--------|
| `make test-probe` | 全部：基础设施自测 + 场景（= `uv run pytest libs/wing-probe/ --timeout=120`） |
| `uv run pytest libs/wing-probe/tests/` | 只跑基础设施自测（不起网关，秒级） |
| `uv run pytest libs/wing-probe/scenarios/test_rewind.py::test_rewind_skips_event_ancestors -v --timeout=120` | 单个场景 |
| `make test` | 三组（python / probe / rust）**并行**跑完，末尾统一给结论（失败详情在结论之前）——见 `scripts/collect_output.sh` |
| CI | `probe-check` job（与 `python-check` / `rust-check` 并列） |

前提：仓库根跑过 `uv sync`（产出 `.venv/bin/wing-gateway`）；全程离线、无外部 API key、不读写 `~/.wing`。

环境变量：

| 变量 | 默认 | 作用 |
|------|------|------|
| `WING_GATEWAY_BIN` | 空 | 显式指定网关可执行文件（覆盖解析顺序）；指向不存在的文件直接报错，不静默回落 |
| `PROBE_DUMP` | `on-fail` | 现场转储策略：`on-fail` / `always` / `never`（非法值报错） |

网关二进制解析顺序（`wing_probe.env.resolve_gateway_bin`）：`$WING_GATEWAY_BIN` → 与 `sys.executable` 同目录 → 仓库 `.venv/bin/wing-gateway` → `PATH`。

失败（含 teardown 不变量失败）时自动转储到 `<tmp>/probe/artifacts/`：`timeline.jsonl`（全事件）/ `frames.jsonl`（原始帧，含 `_chunk` 重组前的样子）/ `http.jsonl` / `requests.json`（LLM 请求留档）/ `sessions/*`（各会话的 `history.jsonl` + `metadata.json` 拷贝）/ `gateway.log` / `dump.txt`（逃生舱理由等）。失败报告的末行与终端汇总都打印该路径；本地排查"绿场景长什么样"用 `PROBE_DUMP=always`。

## 目录

```
libs/wing-probe/
├── wing_probe/            # 基础设施（不得 import wing）
│   ├── env.py             # ProbeEnv：临时 WING_HOME + 端口 + 网关子进程生命周期
│   ├── probe.py           # Probe 门面：会话工厂 / 视图 / 现场转储 / 逃生舱
│   ├── driver/            # http（动作封装 + 留档）· ws（订阅 + 信封重组）· session（会话句柄）
│   ├── provider/          # script（剧本模型）· server（假 Provider）· sse（编码）· request_log · context（ContextView）
│   ├── watch/             # timeline（事件缓存 + 游标）· expect（五原语）· report（失败报告）
│   ├── history/           # view（history.jsonl 解析）· invariants（三条不变量 + 红线过渡断言）
│   ├── files.py           # workspace 文件断言
│   └── guard.py           # import 门禁（AST）
├── tests/                 # 基础设施自测（不起网关）
└── scenarios/             # 整机场景（conftest.py 是唯一的 fixture 定义处）
```

## 新增一个场景

**场景 = 代码，不写配置文件**：剧本（假 Provider 每一轮吐什么）写在测试函数里；模型名按场景私有（`probe/<主题>-<角度>`），因为剧本按 model 名路由、场景之间零共享。

最小模板（`scenarios/test_<主题>.py`，下面这段是实跑验证过的）：

```python
from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_bash_write_then_history(probe: Probe) -> None:
    """一句话覆盖点（spec 场景名）。"""
    probe.register(                      # ① 先注册剧本：按序消费，多一次请求就 5xx
        "probe/doc-example",
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "printf ok > out.txt"})]),
        Turn.of(text="written"),
    )
    session = await probe.session(model="probe/doc-example", yolo=True)  # ② 建会话（yolo = Bash 免确认）

    result = await session.chat("please write out.txt")   # ③ send + 等本轮 turn_result（error 立即失败）
    assert result.data["subtype"] == "success", result.data

    session.watch.assert_ordered(                          # ④ 事件（since=0 从时间线起点扫）
        ["tool_call", "tool_call_result", "turn_result"], since=0
    )
    session.watch.assert_never("error")

    probe.files.assert_content("out.txt", equals="ok")     # ⑤ 文件效果

    follow_up = probe.context("probe/doc-example", index=1)  # ⑥ 第 2 次请求的上下文
    follow_up.assert_tool_pairing()

    history = probe.history(session)                       # ⑦ 落盘（history.jsonl 独立解析）
    assert [message["role"] for message in history.messages()] == [
        "user", "assistant", "tool", "assistant",
    ]
    history.assert_chain_invariants()
    history.assert_no_transient_records()
```

三条要点：

1. `probe.register(model, Turn...)` 支持关键字形式 `Turn.of(text=…, thinking=…, tool_calls=[…], usage=…, chunk=…, delay=…)`，也支持 `Script(...)`（分片粒度 / 切断点 / 延迟）；
2. 剧本**按序消费**，请求次数超出即返回 5xx（`ScriptExhaustedError` 附可读报告）——"不该发生的额外调用"会立刻变红，这是特性不是噪音；
3. 失败报告与场景断言并列：teardown 的三条内置不变量对**场景创建／挂载的全部会话**（含 fork 出来的子会话）自动运行，失败抛 `ProbeInvariantError`，报告标注 `built-in invariants` + session id + 不变量名。

模型名建议加注释锚定用途；`session.watch` 的游标随 `chat()` 推进会消耗已消费事件——需要回看整轮顺序时用 `since=0` 或 `since=<进入本轮前的 cursor>`。

**环境旋钮**：需要非标准网关配置的场景（例如把逐出 TTL 压到秒级、把上下文窗口调小），用 `@pytest.mark.probe_env(...)` 给 `ProbeEnv` 传启动参数（kwargs 原样透传给 `Probe.start`；缺省不打标记＝标准 probe 配置）：

```python
FAST_EVICTION = {"eviction": {"idle_ttl_seconds": 1.0, "sweep_interval_seconds": 0.5}}

@pytest.mark.probe_env(sessions=FAST_EVICTION)   # → config.yaml 的 sessions: 段
```

确定性来自配置而不是等待运气：把阈值压到秒级、断言仍走"轮询到状态翻转（带超时）"。

## 断言原语速查

### 事件时间线：`session.watch`（游标模型）

| 原语 | 语义 | 游标 |
|------|------|------|
| `await watch.expect(type, where=?, within=?)` | 游标后**首个**匹配（无则等到 `within`） | 命中即推进 |
| `await watch.expect_none(types, where=?, within=?)` | **未来**窗口内不得出现 | 不动 |
| `watch.assert_never(type, where=?)` | **全时间线**（含已消费）从未出现 | 不动 |
| `watch.assert_ordered(types, where=?, since=?)` | 游标后按序出现（允许夹杂其他事件） | 推进到最后命中处 |
| `watch.events(type=?, where=?)` | 全量查询（自定义断言的入口） | 不动 |

- `type` 可以是单个类型名或一组（或语义）；`where` 是谓词 `lambda event: …` 或"字段子集相等"映射 `{"tool_name": "Bash"}`；
- **时间只能是上限**：`within` 缺省 5s，显式值必须是有限数（`inf` / `None` 被拒绝），`within=0` = 只看当下；断言禁止用 `sleep` 驱动；
- 事件对象：`event.type` / `event.data`（原始 JSON，不做类型化镜像——防漂移）/ `event.index` / `event.at`（相对 env 启动的单调时钟）/ `event.raw`（重组后的原始载荷）/ `event.frames`（传输层帧数，`>1` 表示经 `_chunk` 信封重组）；
- 失败报告：期望描述 → 游标后实际时间线 → 第一处分叉 → 原始帧尾部；
- `session.chat(text, within=…)` = 发送 + 等本轮 `turn_result`；若本轮先出现 `error` 事件则**立即失败**（不等满 `within`）——报告复用 `expect` 的渲染：聚焦 error 事件 + 失败前一个事件起的时间线 + 原始帧尾部 + 转储路径。"模型报错导致轮次没结束"因此是零等待的失败，而不是 30s 超时。

### LLM 请求上下文：`probe.context(model=None, index=0)`

白盒读假 Provider 留档的请求体（`probe.request(...)` 拿原始留档，`probe.requests_for(model)` 拿某 model 全部）。

| API | 用途 |
|-----|------|
| `view.system` / `view.messages`（非 system）/ `view.role_sequence()` / `view.texts(role)` | 请求构成 |
| `view.tool_names` | 本轮声明的工具集 |
| `view.assert_tool_pairing(require_json_args=True)` | 每个 `tool_calls[].id` 都有配对 tool 消息、参数可解析 |
| `view.assert_prefix_like([...])` | 前缀语义等价（KV cache 保护断言） |
| `view.assert_tail_from([...])` | 尾部（保留区）未丢 |

消息规格（`MsgSpec`）三种写法：`{"role": "user", "content": "hi"}` / `"user: hi"` / `MessageView` 对象；映射键允许 `role` / `content` / `tool_call_id` / `name` / `reasoning_content` / `has_tool_calls` / `tool_calls`（拼错的键会报错，不静默通过）。

请求的 system 段来自生成配置里的默认模板 `system_prompt`（`wing_probe.env.DEFAULT_SYSTEM_PROMPT`，**非空**；可用 `render_config_yaml(system_prompt=…)` / `ProbeEnv(system_prompt=…)` 覆盖，`probe.env.system_prompt` 是场景里的对照值）。"system + 摘要"这类前缀断言因此作用在真实的 system 段上，而不是空串上退化。

### 落盘：`session.history` / `probe.history(session)`

每次访问都**重新解析** `history.jsonl`，所以"操作前后两个视图"就是 `before = session.history` / `await op()` / `after = session.history`。

| API | 用途 |
|-----|------|
| `records` / `by_uuid` / `record_lines` | 全量记录（含损坏行跳过）、uuid 索引、行号 |
| `active_chain()` / `full_chain()` / `messages()` / `events()` | 活跃链（tip 回溯）/ 完整链 / 消息层 / 事件层 |
| `tip_uuid` / `last_record_uuid` / `parent_of(uuid)` / `find(uuid)` / `metadata()` | 拓扑与元数据 |
| `reload()` / `describe(limit=24)` | 重读 / 人类可读摘要（贴进断言消息） |
| `assert_chain_invariants()` / `assert_tool_pairing()` / `assert_no_transient_records(types=?)` | 三条内置不变量（单个视图） |
| `assert_compact_transition(before, after, …)` / `assert_rewind_transition(before, after, target_uuid, …)` / `assert_fork_of(source, child, at_uuid, …)` | 红线过渡断言（返回结构化 `material` 供进一步断言） |
| `fork_metadata(child_session_dir, require=FORK_METADATA_FIELDS)` | 子会话 `metadata.json` 独立解析 + 必需字段 |

### 文件：`probe.files`（workspace 作用域）

| API | 用途 |
|-----|------|
| `assert_exists(path, kind="file"\|"dir")` / `assert_missing(path)` | 存在性 |
| `assert_content(path, contains=… \| equals=… \| matches=…)` | 内容（三者选一；`matches` 走 `re.search` + `MULTILINE`） |
| `read_text(path)` / `paths(ignore=…)` / `snapshot(ignore=…)` / `assert_snapshot(expected)` | 读取 / 清单 / 快照对账 |

路径按 workspace 解析；越界路径由 `resolve()` 拒绝。`probe.files` 用 probe 的默认 workspace（`<root>/workspace`，`probe.session()` 不带 `workspace=` 时的目录）；`probe.files_of(session | session_id)` 用该会话自己的目录——只给 session id（或 resume 出来的、句柄不知道目录的会话）时回读 `metadata.json.workspace`，不会静默落到默认 workspace 上。

## 红线清单与口径声明

**内置不变量**（每个场景 teardown 自动跑；不通过即场景失败）：

1. `chain_topology`（`assert_chain_invariants`）：链拓扑自洽——uuid 存在且唯一、记录种类可识别（Message 或 `role="event"` + 非空 `type`）、每条 `parent_uuid` 都在记录集中、活跃链自 tip 回溯可达根且逐节点衔接（无环）、活跃链末端 == tip == 末条记录；
2. `tool_pairing`（`assert_tool_pairing`）：每个带 `tool_calls` 的 assistant 消息，其每个 `call_id` 都有配对 tool 消息（未终结的半截参数块一律剔除）；
3. `no_transient_records`（`assert_no_transient_records`）：流式 delta / 瞬态事件不得出现在 `history.jsonl`。

**红线过渡断言**（场景里显式调用）：`assert_compact_transition`（手动/后台压缩的链形状）、`assert_rewind_transition`（复制行 / 事件节点跳过 / 回退到根）、`assert_fork_of`（uuid 重映射 + 事件随行 + metadata 快照）。会话逐出（`scenarios/test_session_eviction.py`）不需要专用 helper——它断言的是"什么都不该变"（逐出/水合前后记录集指纹一致），红线直接由场景内的指纹对比 + 内置不变量承担。

**口径**（与 spec 一致，比 spec 严的部分在此声明）：

- 只保 **`history` 链上的上下文事实**：链拓扑、记录配对、瞬态记录不落盘。**不做**"全事件类型 × `persist` 布尔矩阵"对账（二批）；
- `assert_no_transient_records` 检查**全量记录**而非仅活跃链：瞬态记录一旦落盘，无论之后是否被 rewind / compact 移出链，都是持久化语义被破坏；
- 黑名单 `TRANSIENT_EVENT_TYPES` = 产品中 `persist=False` 的**全集**（`text` / `reasoning` / `tool_call_stream` / `tool_call` / `tool_call_result` / `assistant_turn` / `llm_call_metrics` / `sync_session` / `session_state_changed` / `session_init` / `context_stats` / `branch_targets` / `delivered` / `notice` …），是 spec 最低要求（流式 delta 三类）的超集；新增 `persist=False` 事件类型时应同步该集合（`wing_probe/history/invariants.py`）；
- 事件断言锚定**事件序**，时间只作上限；并行工具用集合语义断言，不依赖完成顺序。

## 逃生舱

`probe.without_invariants(reason="...")` 关闭本场景的 teardown 不变量——**必须给理由**：理由写进 `artifacts/dump.txt` 并在终端汇总回显，空理由直接报错。用于"场景有意制造非法中间态"（例如断言失败路径的中间产物），不得用于掩盖真实失败。

## 已知取舍（与二批范围）

- **env 是 function 级**（design D10）：每个场景一个新 tmp + 新网关进程 + 新假 Provider，正确性优先；实测 14 场景约 8s（约 0.25s/场景启动），暂不构成压力。场景数量显著增长后再走优化路径（session 级共享网关 + 按 model 名分域的剧本），`Probe` 与 `ProbeEnv` 分离就是为了那时只改 fixture 作用域；
- **二批**：Anthropic 协议路径、远程工具宿主（`wing-sdk` 对接）、中断（interrupt）提交语义、性能/时延断言、后台自动压缩（`_apply_pending_compact`）、全事件类型 × `persist` 矩阵；
- 兜底：`make test-probe` 传 `--timeout=120`（另有场景级 `@pytest.mark.timeout(120)`），网关进程在 teardown 走 `/api/shutdown` → `terminate` → `kill`；
- 网关重试 / 退避不进首批（探测配置显式 `max_retries=0`），保证"一次请求 = 一次剧本消费"可数。

## 附：变异验证怎么复跑

本 change 的验收含三项变异验证（人为在产品代码引入回归 → 对应场景必须变红 → 立即还原）。复跑纪律：`git status` 干净起步 → 改 1 处产品代码 → 跑目标场景看红 → `git checkout -- <file>` 还原 → 重跑确认绿。

| 变异 | 改动点 | 期望变红 |
|------|--------|----------|
| transient 落盘 | `libs/core/wing/event/react.py`：`TextEvent.persist = True` | `scenarios/test_react_basics.py::test_text_turn_event_order_and_persistence`（场景内断言 + teardown 不变量 `no_transient_records`） |
| rewind 跳过事件节点 | `libs/core/wing/context_manager.py::rewind` 中沿链回溯跳过事件节点的 `while` 循环 | `scenarios/test_rewind.py::test_rewind_skips_event_ancestors` |
| fork metadata 快照 | `libs/core/wing/session_manager.py::fork_session` 的 `save_metadata(..., forked_from=session_id)` | `scenarios/test_fork.py::test_fork_metadata_snapshot`（`assert_fork_of` 连带 `test_fork_chain_integrity_and_uuid_remap`） |

**变异绝不入库**：验证完必须还原，工作区只允许 probe 侧改动。
