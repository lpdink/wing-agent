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

### Session（`routes/session.py`，14 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/session/create` | 创建新 session（可选 `backend: file\|memory`，默认 file；`workspace`、`template` 等） |
| POST | `/api/session/resume` | 恢复已有 session（默认还原 template_name 与 workspace） |
| POST | `/api/session/fork` | 从指定消息 uuid 分叉；新 session 含该消息及之前全部消息，继承源 backend |
| POST | `/api/session/subscribe` | 将某 client 订阅到 session 事件（触发 SyncSession 重放） |
| POST | `/api/session/unsubscribe` | 取消订阅 |
| POST | `/api/session/send` | 发送用户消息，驱动 agent loop |
| GET | `/api/session/list` | 列出所有 session（跨 store 聚合） |
| GET | `/api/session/get` | 获取 session 详情 |
| GET | `/api/session/info` | 运行时状态，含 `context_stats`、`skills_info`、`reasoning_effort` |
| GET | `/api/session/branches` | 可回退 / 分叉的消息节点 |
| POST | `/api/session/update` | 更新状态：model / agent / title / thinking / reasoning_effort / yolo / workspace |
| POST | `/api/session/compact` | 手动压缩上下文 |
| POST | `/api/session/interrupt` | 中断当前任务（Esc 键） |
| POST | `/api/session/rewind` | 回退到指定消息 uuid |

### System（`routes/system.py`，5 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/commands` | 命令列表（仅返回 `source == "prompt"` 的命令） |
| GET | `/api/models` | 可用模型列表 |
| GET | `/api/agents` | 可用 agent 模板列表 |
| POST | `/api/system/reload` | 重载配置 / hooks / provider / skills / auth（无需重启） |
| POST | `/api/shutdown` | Gateway 优雅自关闭（返回 200 后延迟自送 SIGTERM） |

### Health（`routes/health.py`，1 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/health` | 健康检查，返回 `service: "wing-gateway"`（身份标识）+ `uptime` + version。**鉴权豁免**，供 `wing status` 探活。 |

> `wing start/stop/status` 完全基于 HTTP：`stop` → `POST /api/shutdown` 后轮询 health 直至不可达；`start` → 探活 health，无响应则拉起再轮询；`status` → 读 health 的 version + uptime。已无 PID / state.json（PR #10）。

### Tools（`routes/tools.py`，1 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/tools/register` | 注册远程工具。需 `X-Client-Id` header，且该 client 已持有活跃 WS（否则 400）。工具以 client_id 为 namespace 落入核心 registry，引用形如 `<client_id>.<name>`。 |

**远程工具注册**（实验性）：tool host 建立 WS 连接并经 `?client_id=<id>` 自选 client_id（作为工具 namespace），再经此端点注册工具。client_id 自定义**与权限解耦**——任何角色都可声明（面向未来 UX：用户需知道"远程是谁"以分配工具），先来先得到、全量唯一性校验（冲突拒连）；`tool_runtime` 必须声明，admin 可选。声明 client_id 的连接即具备注册资格。注册后：

- 调用走 WS：agent 调用 `<client_id>.<name>` 时，Gateway 经该 client 的 WS 发 `tool_call_request` 帧，tool host 执行后回 `tool_call_result` 帧（同 `call_id`）。
- 工具规格可携带可选 `llm_name`（LLM 可见名，须符合 provider 文法 `^[a-zA-Z0-9_-]{1,64}$`）；未提供退化为裸 name。运行时不自动以 client_id 限定 llm_name——当前不允许一个 agent 同时持有两个同名工具，绑定时撞名会在 create session 失败（预期行为）。
- `client_id="default"` 为保留字（内置工具命名空间），连接即拒。
- 核心网络无关：核心只看到一个普通 `Tool`（schema + 可执行体），远程性封装在 Gateway 注入的 dispatch 闭包里。
- 断连即失败并注销：WS 断开时在途调用立即失败、工具从 registry 移除。**已加载 agent 持有的工具引用不受影响**（保护 KV cache）——再次调用时清晰返回 "tool unavailable / not connected"。动态工具切换为后续特性。
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

**ReAct 事件**（`event/react.py`）：`turn_started` · `text` · `reasoning` · `tool_call_stream` · `tool_call` · `tool_call_result` · `diff_content` · `ask` · `assistant_turn` · `tool_result_turn` · `turn_result`（subtype: success / error_during_execution / error_max_turns）· `done` · `llm_call_metrics`。

> `tool_call_stream`：LLM 生成工具参数期间的流式渲染事件，携带**增量原始 args 文本碎片**（`args_fragment`，首个事件含完整前缀）。后端不解析 partial JSON，前端自行累积 buffer 并容错解析渲染；参数生成结束后由 `tool_call` 事件携带权威解析结果。

**状态事件**（`event/state_change.py`）：`session_init` · `sync_session`（订阅时重放历史）· `session_state_changed`（update / think / yolo 后统一发出）· `interrupted` · `compact_done`。

**其他**（`event/base.py`、`query_response.py`）：`error` · `delivered` · `context_stats` · `branch_targets`。

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
