# HTTP API 与 WebSocket 协议

Gateway 是一个 FastAPI 服务。**HTTP 负责生命周期 / 查询 / 状态变更（RPC 风格），WebSocket 只负责实时事件流。** 会话创建与 WS 握手解耦：客户端先经 HTTP 创建会话，再订阅事件。

默认监听 `127.0.0.1:32523`（`gateway.host` / `gateway.port`）。OpenAPI 文档由 `wing/gateway/openapi.py` 提供元数据。

## 典型客户端流程

```
1. WS  GET/WS  /ws                 → ConnectResponse { type:"connected", client_id }
2. HTTP POST  /api/session/create   → { session_id }
3. HTTP POST  /api/session/subscribe { session_id, client_id }
4. HTTP POST  /api/session/send      { session_id, content }   → agent loop 启动
5. WS   接收 ReAct 事件流（text / tool_call / … / turn_result）
```

查询（弹窗候选、系统信息等）走 GET 端点；状态变更（切模型、压缩、回退等）走 POST 端点。TUI 中的斜杠命令多数被前端拦截并转换为这些 HTTP 调用（见 [glossary.md](glossary.md) 中「命令」条目）。

## HTTP 端点

### Session（`routes/session.py`，15 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/session/create` | 创建新 session（可选 `backend: file\|memory`，默认 file；`workspace`、`template` 等） |
| POST | `/api/session/resume` | 恢复已有 session（还原 template_name、workspace 与模型绑定；模型记录优先于模板默认；也是被逐出会话的显式水合入口） |
| POST | `/api/session/fork` | 从指定消息 uuid 分叉；新 session 含该消息及之前全部消息，继承源 backend |
| POST | `/api/session/subscribe` | 将某 client 订阅到 session 事件（触发 SyncSession 重放；不在内存的会话先按需水合） |
| POST | `/api/session/unsubscribe` | 取消订阅 |
| POST | `/api/session/send` | 发送用户消息，驱动 agent loop（不在内存的会话先按需水合，磁盘上也没有才 404） |
| GET | `/api/session/list` | 列出所有 session（跨 store 聚合）；`status: inactive` = 不在内存（未加载 / 已逐出） |
| GET | `/api/session/get` | 获取 session 详情 |
| GET | `/api/session/info` | 运行时状态，含 `context_stats`、`skills_info`、`reasoning_effort` |
| GET | `/api/session/branches` | 可回退 / 分叉的消息节点 |
| POST | `/api/session/update` | 更新状态：model / agent / title / thinking / reasoning_effort / yolo / workspace / tools（`tools` 全量替换，ref 格式，PR #50） |
| POST | `/api/session/compact` | 手动压缩上下文，可带 `instruction` 侧重指令（条件插入压缩 prompt，无指令时 prompt 不变） |
| POST | `/api/session/interrupt` | 中断当前任务（Esc 键） |
| POST | `/api/session/rewind` | 回退到指定消息 uuid |
| POST | `/api/session/release` | 逐出 session 内存态（只回收内存，磁盘不动）：忽略空闲时长，不忽略钉住条件——忙碌 / 被订阅 / 非持久后端以 409 拒绝；本就不在内存返回 `released: false`（幂等） |

> **会话逐出（eviction）**：空闲会话（无 turn 在跑、无后台任务、无人订阅且超过
> `sessions.eviction.idle_ttl_seconds`）会被后台周期任务逐出内存——只回收 worker
> 与 provider client，`history.jsonl` / `metadata.json` 一概不动。逐出对外唯一
> 可见痕迹是 `/api/session/list` 里的 `status: inactive`；`resume` / `subscribe` /
> `send`（以及 `wing run -r`、`wing tail`）都按需水合。`wing release <sid>` 是
> 对应的显式操作。

### System（`routes/system.py`，6 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/commands` | 命令列表（仅返回 `source == "prompt"` 的命令） |
| GET | `/api/models` | 可用模型列表 |
| GET | `/api/agents` | 可用 agent 模板列表 |
| POST | `/api/system/reload` | 重载配置 / hooks / provider / skills / auth（无需重启） |
| POST | `/api/shutdown` | Gateway 优雅自关闭（返回 200 后延迟自送 SIGTERM） |
| GET | `/api/tools` | 全局工具列表（内置 + 远程，平铺；ref / namespace / name / llm_name / description）（PR #50） |

### Health（`routes/health.py`，1 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/health` | 健康检查，返回 `service: "wing-gateway"`（身份标识）+ `version` + `commit`（构建时注入的短 hash）+ `uptime`。**鉴权豁免**，供 `wing status` 探活。 |

> `wing start/stop/status` 完全基于 HTTP：`stop` → `POST /api/shutdown` 后轮询 health 直至不可达；`start` → 探活 health，无响应则拉起再轮询；`status` → 读 health 的 version + commit + uptime。已无 PID / state.json（PR #10）。

### Tools（`routes/tools.py`，1 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/tools/register` | 注册远程工具。需 `X-Client-Id` header，且该 client 已持有活跃 WS（否则 400）。工具以 client_id 为 namespace 落入核心 registry，引用形如 `<client_id>.<name>`。 |

**远程工具注册**（实验性）：tool host 建立 WS 连接并经 `?client_id=<id>` 自选 client_id（作为工具 namespace），再经此端点注册工具。client_id 自定义**与权限解耦**——任何角色都可声明（面向未来 UX：用户需知道"远程是谁"以分配工具），先来先得到、全量唯一性校验（冲突拒连）；`tool_runtime` 必须声明，admin 可选。声明 client_id 的连接即具备注册资格。注册后：

- 调用走 WS：agent 调用 `<client_id>.<name>` 时，Gateway 经该 client 的 WS 发 `tool_call_request` 帧，tool host 执行后回 `tool_call_result` 帧（同 `call_id`）。
- 工具规格可携带可选 `llm_name`（LLM 可见名，须符合 provider 文法 `^[a-zA-Z0-9_-]{1,64}$`）；未提供退化为裸 name。运行时不自动以 client_id 限定 llm_name——当前不允许一个 agent 同时持有两个同名工具，绑定时撞名会在 create session 失败（预期行为）。
- `client_id="default"` 为保留字（内置工具命名空间），连接即拒。
- 核心网络无关：核心只看到一个普通 `Tool`（schema + 可执行体），远程性封装在 Gateway 注入的 dispatch 闭包里。
- 断连即失败并注销：WS 断开时在途调用立即失败、工具从 registry 移除。**已加载 agent 持有的工具引用不受影响**（保护 KV cache）——再次调用时清晰返回 "tool unavailable / not connected"。运行期工具集切换已由 `POST /api/session/update`（`tools`）支持（PR #50，见 [architecture.md](architecture.md) 远程工具与编排一节）。
- 总超时：`gateway.remote_tool_timeout`（默认 1800s）仅为安全网，断连是首要失败信号。

## WebSocket 协议（`/ws`）

握手成功后服务端推送 `ConnectResponse { type: "connected", client_id }`。之后是**双向**通道：

- **服务端 → 客户端**：推送 `WingEvent` 事件流，事件均为 `WingEvent` 子类，按 `type` 字段区分。
- **客户端 → 服务端**：发送 `ClientRequest` 帧 `{ request_id, session_id, content, tool_call_id? }`，用于投递用户消息，或携带 `tool_call_id` 定向回复某个 `ask` 事件（resolve 对应的 feedback waiter）。Gateway 注入 `client_id` 后转给 `WingRuntime.post()`。

> 投递消息有两条等价路径：WS `ClientRequest`（TUI 实际所用，便于与 Ask 回复复用同一连接）或 HTTP `POST /api/session/send`。

**远程工具帧**（tool host 专用，按 `call_id` 字段与 `ClientRequest` 区分，向后兼容）：

- **服务端 → tool host**：`tool_call_request { type, call_id, name, arguments }` —— 发起一次远程工具调用。
- **tool host → 服务端**：`tool_call_result { type, call_id, result, is_error }` —— 回传调用结果，Gateway 据此 resolve 在途调用。

`tool_runtime` 角色连接时须经 `?client_id=<id>` 自选 client_id（admin 也可自选，未声明者服务端分配）；该 WS 是纯工具执行通道，不参与事件订阅。

**ReAct 事件**（`event/react.py`）：`turn_started` · `user_message_accepted` · `text` · `reasoning` · `tool_call_stream` · `tool_call` · `tool_call_result` · `diff_content` · `ask` · `assistant_turn` · `tool_result_turn` · `turn_result`（subtype: success / error_during_execution / error_max_turns）· `done` · `llm_call_metrics`。

> `tool_call_stream`：LLM 生成工具参数期间的流式渲染事件，携带**增量原始 args 文本碎片**（`args_fragment`，首个事件含完整前缀）。后端不解析 partial JSON，前端自行累积 buffer 并容错解析渲染；参数生成结束后由 `tool_call` 事件携带权威解析结果。

> `user_message_accepted`：用户消息被消费进模型上下文的确认（`content` + `origin_request_id`，后者即客户端提交时的 `request_id`）。两个发射点：`run_turn` 入口的 drain-and-merge（先于 `turn_started`）与工具执行后的 steer 注入。产品语义：TUI 把已发送消息挂在底部排队区，收到本事件才上移进聊天历史——消息"往上走"当且仅当它真的被发给了模型。无 `request_id` 的内部投递（如 Explorer 回传）不发射。

**状态事件**（`event/state_change.py`）：`session_init` · `sync_session`（订阅时重放历史）· `session_state_changed`（update / think / yolo 后统一发出）· `interrupted` · `compact_done`。

**其他**（`event/base.py`、`query_response.py`）：`error` · `notice` · `delivered` · `context_stats` · `branch_targets`。

> `notice`：一次性提醒（`level` + `message`，可带 `attempt` / `max_attempts` / `retry_in_s`），`persist=false` 不落盘、不重放。与 `error` 的边界：`error` 是"真错误"（前端终结 turn / 渲染错误 / 通知），`notice` 不终结 turn（如"LLM 调用失败，N 秒后重试"）。

## 慢消费者回收（写超时）

网关对"不再读取的客户端"有界反制（实测背景：一个读任务死亡的客户端让连接与路由表项永久泄漏，且每个事件都变成一次失败投递 + 一行日志，最高 12,290 行/分钟）：

- 每次投递（`GatewayServer._send_text`，每事件一次 `create_task`，无发送队列）等待写完成的时间上界为 **60s**（`WRITE_TIMEOUT_SECONDS`，硬编码；关闭连接另有 5s 上界 `CLOSE_TIMEOUT_SECONDS`）。
- 超时或写失败 → **回收该客户端**：从 `clients` / `ws_to_clients` 注销、`event_bus.route_detach_client` 清订阅路由、主动 `close(code=1013)`、attached 的远程工具宿主 `fail_client`（在途调用立即失败）。
- 回收与 `handle_ws` 的正常断连收尾共用**同一个幂等入口** `GatewayServer.drop_client(ws, reason=…)`；`pop` 语义保证每个客户端最多一次副作用（一行回收日志）。
- 回收立即生效：投递列表就是路由表，注销后该 client 不再收到任何事件——失败投递与日志洪泛随之停止。
- 边界：uvicorn 的 WS 写走用户态缓冲，`send_text` 往往立即返回；此时触发回收的是异常分支（连接已死 / ASGI 已关闭），两条分支走同一条回收路径。

## 单帧上界与分片（`_chunk`）

客户端单帧上限是 tungstenite 默认的 **16 MiB**（`max_frame_size`，不改），而 `sync_session` 载荷可以更大（2026-09-13 实测 22,151,988 B → 读任务 `Message too long` 死亡 → 重连重同步再死，会话在 TUI 里永久打不开）。**上界由发送方保证**：超过软上限的载荷在唯一 wire 出口（`GatewayServer._on_event` → `_send_frames`）切分，客户端在读任务内合并还原——应用层零感知（不引入半成品事件、不做增量渲染）。

| 常量 | 值 | 含义 |
|------|-----|------|
| `SOFT_LIMIT_BYTES` | 8 MiB | 序列化载荷超过它即切分；每帧（含信封与 JSON 转义开销）≤ 它 |
| `HARD_LIMIT_BYTES` | 16 MiB | 任何出网帧的上界，= 客户端默认 `max_frame_size`；仍超限的帧被拦截丢弃 + 一行 WARN（含 client / 类型 / 字节数 / 阈值），应用层照常 emit，不新增计数指标 |

切分按 **UTF-8 字符边界**，每帧尽量撞软上限（帧数最小化）；同一事件的所有帧在**同一个发送任务内**按 `index` 升序发出（每事件一次 `asyncio.create_task`，无发送队列；每帧仍是一次 `send_text`，逐帧受 60s 写超时约束）。载荷小于软上限时原样单帧，零额外开销。

### 信封

```json
{"type":"_chunk","id":"7","index":0,"count":3,"of_type":"sync_session","data":"{…"}
```

| 字段 | 说明 |
|------|------|
| `type` | 恒为 `"_chunk"`；以 `_` 开头的 `type` 是**传输层保留命名**，应用事件（含未来新增）MUST NOT 使用 |
| `id` | 同一事件的所有帧共享，事件间由网关保证唯一（进程内递增计数器） |
| `index` | 0-based 帧序号，同一事件内连续 |
| `count` | 该事件的总帧数（≥ 2） |
| `of_type` | 原始事件的 `type`（诊断用，不参与路由） |
| `data` | 原始载荷 JSON 文本的一段；所有帧按 `index` 顺序拼接后与原始载荷逐字节相等 |

字段语义不依赖出现顺序；`_chunk` 不是 `WingEvent`（不进事件注册表），Rust 侧也是独立类型（`gateway::chunk::ChunkEnvelope`）。**接收方契约**：客户端读任务的正常路径不变（先按 `WingEvent` 解析；未知类型 `WingEvent::Unknown` 才尝试信封解析，热路径无额外成本）。

### 客户端重组与防护

重组发生在读任务内（`crates/wing/src/gateway/chunk.rs`），`recv_event()` 只产出**完整事件**：

- **保序**：窗口打开期间（收到某事件首片、未闭合）所有其他帧原样缓冲、不解析、不投递；闭合时先投递完整事件、再按到达序放行缓冲帧。否则 `sync_session` 与其后的 live delta 交错会让应用的 `chat.clear()` 吞掉先到的 delta。
- **防护**（违反任一条即断开并记录原因，由既有重连路径重新订阅重放）：

| 约束 | 值 |
|------|-----|
| `count` 合法区间 | 2..=1024（`MAX_CHUNKS`） |
| 首帧 `index` | 必须为 0；后续帧必须严格连续（重复 / 空洞 / 越界即失败） |
| 窗口内换 `id` | 失败（同一时刻至多一个重组窗口；并发切分事件 → 断开重连） |
| 缓冲上限（未闭合分片 + 窗口内缓冲帧） | 64 MiB |
| 分片不闭合超时 | 30s（收到任一属于该事件的分片即重置——静默才是异常信号） |

- 失败分类复用 `CloseReason`（新增 `ReassemblyFailed { detail }`）：TUI 的重连提示与 `wing wait` 的失败信息都能说出原因，不静默降级、不空转。
- **边界**：远程工具帧（`tool_call_request` / `tool_call_result`，走 `RemoteToolManager` 独立的 `ws.send_text`）不在本机制覆盖范围内——tool host 不参与事件订阅，其超大载荷（如大文件 Write）是已知限制，另行立项。

## 鉴权与 RBAC（opt-in，PR #35）

默认关闭，完全向后兼容。配置于后端 `gateway.auth`：

```yaml
gateway:
  auth:
    enabled: false
    keys:
      - key: "my-secret"
        role: admin        # admin = 全量；tool_runtime = 仅注册远程工具
```

前端在 `~/.wing/tui/config.yaml` 设 `api_key: "my-secret"`，随每个请求发送。

**角色强制（RBAC）**：`role` 现已强制（不再仅存储）。

- `admin`：全量访问所有端点与 WS 事件订阅。
- `tool_runtime`：纯工具执行远端，仅允许 `POST /api/tools/register`（及豁免的 `/api/health`）与其工具 WS 连接；访问其他端点返回 **403**（`ErrorResponse` 形状，`error: "forbidden"`）。既要注册工具又要订阅事件的客户端应持 admin 身份。
- 强制在 `AuthMiddleware` 集中完成（allowlist），新增端点对 `tool_runtime` 默认关闭。鉴权关闭时不做角色强制。

| 通道 | 接受形式 | 优先级 |
|------|----------|--------|
| HTTP | `Authorization: Bearer <key>` | 1 |
| HTTP | `X-API-Key: <key>` | 2 |
| WS | 同 HTTP 请求头 | 1 |
| WS | 查询参数 `?api_key=<key>` | 2 |

- `/api/health` 始终豁免。
- key 用 `hmac.compare_digest` 常量时间比较；须为 ASCII 可打印字符（配置解析时校验）。
- `auth_config` 每次请求读取最新配置单例，`/api/system/reload` 可即时生效。
- **加密（TLS）由外部反向代理（nginx 等）负责**，应用层只做身份验证。
