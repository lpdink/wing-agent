# HTTP API 与 WebSocket 协议

Gateway 是一个 FastAPI 服务。**HTTP 负责生命周期 / 查询 / 状态变更（RPC 风格），WebSocket 只负责实时事件流。** 会话创建与 WS 握手解耦：客户端先经 HTTP 创建会话，再订阅事件。

默认监听 `127.0.0.1:32523`（`gateway.host` / `gateway.port`）。OpenAPI 文档由 `wing/gateway/openapi.py` 提供元数据。

## 典型客户端流程

```
1. WS  GET/WS  /ws                 → ConnectResponse { type:"connected", client_id }
2. HTTP POST  /api/session/create   → { session_id }
3. HTTP POST  /api/session/subscribe { session_id, client_id }
4. HTTP POST  /api/session/send      { session_id, content, tool_call_id?, media? } → agent loop 启动
   （`tool_call_id` = 定向回答一个在挂 Ask 的 feedback waiter；CLI 侧：`wing asks` 拿 id，
   `wing run --tool-call-id` 回答）
5. WS   接收 ReAct 事件流（text / tool_call / … / turn_result）
```

查询（弹窗候选、系统信息等）走 GET 端点；状态变更（切模型、压缩、回退等）走 POST 端点。TUI 中的斜杠命令多数被前端拦截并转换为这些 HTTP 调用（见 [glossary.md](glossary.md) 中「命令」条目）。

带图消息是两步：先 `POST /api/session/media` 上传字节、拿回 `media` 引用，再把该引用放进消息的 `media`（见下表与 [media-images.md](media-images.md)「用户贴图」）。

## HTTP 端点

### Session（`routes/session.py`，19 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/session/create` | 创建新 session（可选 `backend: file\|memory`，默认 file；`workspace`、`template` 等；可选 `tags` 创建即打标，校验语义同 `/api/session/tag`；可选 `session_id` = **create-or-adopt**：不存在则以该 id 建会话，已存在则收养既有会话——语义同 `/api/session/resume`，`agent` 覆盖只应用 resume 子集。**收养路径忽略创建参数**：`template_name` / `workspace` / `backend` 一律以 metadata 为准、且不做校验，因此同一个请求体可能"id 存在 → 200（参数被忽略）/ id 不存在 → 400（如 `backend` 非法）"；**例外**是"该 id 已命中内存里的**空会话**"（同 FS 的变体）：那条路径走的是认领之后的校验分支，`backend` / `template_name` 非法仍会 **400**） |
| POST | `/api/session/resume` | 恢复已有 session（还原 template_name、workspace 与模型绑定；模型绑定按恢复链还原：`metadata.model_id` 命中用当前映射 → 快照兜底 → 模板默认；也是被逐出会话的显式水合入口）。可选 `agent` 覆盖：**只应用 `model_id` / `effort` / `tools`**——`system_prompt` / `append_system_prompt` / `max_turns` / `yolo` 一律不应用（它们会改请求前缀或会话既有限额，属创建期语义），被忽略的字段会记 warning |
| POST | `/api/session/fork` | 从指定消息 uuid 分叉；新 session 含该消息及之前全部消息，继承源 backend |
| POST | `/api/session/subscribe` | 将某 client 订阅到 session 事件（触发 SyncSession 重放；不在内存的会话先按需水合） |
| POST | `/api/session/unsubscribe` | 取消订阅 |
| POST | `/api/session/send` | 发送用户消息，驱动 agent loop（不在内存的会话先按需水合，磁盘上也没有才 404）。`media` 是消息携带的媒体引用（`MediaRef` 数组，缺省 = 无媒体；id 必须是该会话已存在的对象、mime 必须在支持列表内——幽灵引用 / 非法 id / 非法 mime 回 400，消息不进历史）。`tool_call_id` 非空时定向 resolve 该 id 的 feedback waiter（回答在挂 Ask；该通道**只承载文本**，同时带 `media` 时记 WARN 后按文本 resolve）；无该 id / id 已失效则回退普通入队 |
| POST | `/api/session/media` | 上传媒体（**原始字节体**：`?session_id=<sid>[&name=<n>]`，body = 图片字节，非 JSON / 非 multipart）。收包顺序：会话存在（否则 404，且**不隐式建会话**）→ `Content-Length` 预检（声明值 > `images.max_bytes` 直接 400，**不收包**）→ 收包 → 字节校验：body 非空 → `≤ images.max_bytes`（真实字节兜底）→ magic bytes ∈ {png, jpeg, webp, gif} → 头部尺寸可解析；失败一律 400 且**零存储副作用**（不建目录、不写对象）。id = 字节的 sha256（内容寻址），同字节重复上传返回**同一 id** 且不重写对象（幂等）；响应 `{"ok":true,"media":{id,mime,bytes,width,height,name?}}`，`media` 就是消息 `media` 数组的元素形状（可直接回投 `/api/session/send`）。`name` 是展示素材（缺省 / 空串 = 不记录，建议给 basename；长度上限 255 字符，超限 400；id 才是寻址键）。**不水合**已逐出会话（与 `/api/session/tag` 同款）；写入只落 `<sessions-root>/<sid>/media/<id>`，老共享池只读不写。鉴权 / setup mode 行为与其它 session 端点一致 |
| GET | `/api/session/list` | 列出所有 session（跨 store 聚合）；`status: inactive` = 不在内存（未加载 / 已逐出）。每个条目携带 `tags`（插入序；无标签为空数组）与 `tag_meta`（每个标签的记录，当前含 `added_at` 打标时间；键集 ⊆ tags），以及生效模型的四件套 `model_id`（引用词）/ `model_name`（调用名）/ `provider_name` / `model_display_name`（口径同 `/api/session/info`；在内存的会话取 live agent，未加载的按 resume 链解析盘上记录，解析不出的降级路径为 null，展示层回落 展示名 → 调用名 → 引用词）；**带标签的会话即使还没有首条消息也列出**（"创建即打标"窗口期可查，name 为 null）。**顺序是契约**：活跃（`status != inactive`，= 已在内存的工作集）在前，组内按 `last_interaction` 降序（缺失 / 不可解析时回退 session id 前缀 `YYYYMMDD-HHMMSS`，都没有按 0），完全并列则按 session id 升序（全序，避免顺序随 store 枚举漂移）。后端只定这个基准序；前端在其上加**自己的语义叠加**——当前是「`pin` 标签置顶」（组内按 `tag_meta.pin.added_at` 降序，后 pin 的更靠前；缺时间的**仍然置顶**，只在置顶组内排在有时间者之后——时间口径：写者只产出 naive 本地 ISO；比较时 naive 值按固定基准、带偏移值按瞬时——两类混排（手改数据）是已知边界，且前后端对 naive 取的基准不同、不互为一致性合约），以及 `/ss <args>` 的 exact > prefix > contains 分层（层内保持上述顺序）；后端不感知 `pin`（它只是普通标签），状态优先级（`waiting` 不提前）与 workspace 不参与排序。可选**子集过滤**（查询参数，逗号分隔）：`status=idle,working,waiting` 只保留状态命中者（取值域 = 状态全集）；`tag=pin,crew` 只保留**携带全部**给定标签的行（AND，与 `wing ps --tag` 的既有标签口径一致；`/ss <args>` 的 args 是 id / 标题 / workspace 的子串匹配，与标签无关）；两者同时给出即 AND。**过滤 = 子集**：结果是全量列表的**子序列**（同样的行、同样的相对顺序，只是少了几行），聚合成本与不过滤时相同——省下的是响应体积与调用方的清洗 / 合并成本。「在内存的会话」（能在跑的那批，含完整行）即 `status=idle,working,waiting`：会话列表 / rail 的状态轮询走它，不必每次拉全量——注意可见性规则先于过滤生效（无名且无标的会话本就不在列表里，过滤不会让它出现）；旧网关不识别这两个参数时会静默返回全量（仍是正确的超集——消费方按 id 处理多出来的行即可）。非法取值一律 400（未知状态 / 空段 / **同一参数重复给出**——多值用单个逗号分隔参数表达；标签取值按写入侧同一套规则校验（`wing.session.tags.validate_tag`），词法非法（空 / 超长 / 空白 / 控制符 / `-` 开头）即 400）；**语法合法但不存在**的标签是空结果（词表开放，无从枚举——字面匹配，不是错误） |
| GET | `/api/session/get` | 获取 session 详情 |
| GET | `/api/session/info` | 运行时状态，含 `model`（调用名）+ `model_id`（引用词；不可用时 null）+ `provider_name`（运行期事实）+ `model_display_name`（展示名；未声明 = null，前端回落 `model`）、`context_stats`、`skills_info`、`reasoning_effort`、`tags`（会话标签）、`tag_meta`（标签记录，含 `added_at`）与 `usage`（会话用量账本：计数 `user_messages` / `llm_calls` / `tool_calls` / `turns` + token `prompt_tokens` / `completion_tokens` / `cached_tokens` + 时长 `llm_time_ms` / `tool_time_ms`，跨 release / 网关重启延续；旧会话 / 无记录读出即全零，不回填。口径：`llm_*` 只计 ReAct 循环内的调用（压缩等旁路不计），tool 只计真实执行的调用（参数非法 / 未知工具的短路与中断合成不计）。命中率 = `cached_tokens / prompt_tokens`、工作时长 = `llm_time_ms + tool_time_ms` 由客户端推导——线格式不含派生值） |
| GET | `/api/session/branches` | 可回退 / 分叉的消息节点 |
| POST | `/api/session/update` | 更新状态：model_id / agent / title / thinking / reasoning_effort / yolo / workspace / tools（`tools` 全量替换，ref 格式，PR #50）。`model_id` 是模型**引用词**（∈ `providers[].models` 的 id），未命中回 400（错误含 available ids + 调用名提示）；旧字段 `model` / `provider` 已删除——发了被**静默忽略**（不报错、不生效） |
| POST | `/api/session/tag` | 读取或原子增删会话标签。body `{session_id, add?, remove?}`：两者皆缺省 = 纯读取；同时给出时服务端一次原子应用（幂等，remove 胜出）。响应 `{ok, session_id, tags, added, removed, tag_meta}`。**打标时间**：实际加入的标签记 `added_at`（本地 naive ISO，与 `last_interaction` 同口径；幂等 no-op 不刷新），移除即删记录，清空后 `tag_meta` 与 `tags` 一起从 metadata 消失。**不水合**已逐出会话（标签与记录属持久 metadata，读写都不把会话换入内存）；标签为不透明字符串（1..64 字符，禁空白 / 逗号 / 控制符，不以 `-` 开头，单会话上限 64；建议小写、`k=v` 作命名空间）；非法 400 / 未知会话 404 |
| POST | `/api/session/compact` | 手动压缩上下文，可带 `instruction` 侧重指令（条件插入压缩 prompt，无指令时 prompt 不变） |
| POST | `/api/session/interrupt` | 中断当前任务（Esc 键） |
| POST | `/api/session/rewind` | 回退到指定消息 uuid |
| POST | `/api/session/release` | 逐出 session 内存态（只回收内存，磁盘不动）：忽略空闲时长，不忽略钉住条件——忙碌 / inbox 有待处理输入 / 被订阅 / 非持久后端以 409 拒绝；本就不在内存返回 `released: false`（幂等） |
| POST | `/api/session/archive` | **设置会话的归档状态**（幂等）。body `{session_id, archived?}`：`archived` 缺省 `true` = 归档（会话目录移入 `<sessions root>/.archive/`）；`false` = 恢复（移回 live 区）。响应 `{ok, session_id, archived, changed, detail}`——`archived` 是操作后的状态、`changed` 说明本次是否真的移动（`detail` ∈ `archived` / `already archived` / `restored` / `not archived`）。**归档是纯移动**（不改任何字节）；**恢复会刷新会话的 `last_interaction`**——恢复是显式兴趣信号，否则自动 sweep 会按旧年龄在下一轮立刻把它收回去。钉住条件同 release（忙碌 / 排队输入 / 被订阅 / 非归档后端（memory）以 409 拒绝）；空闲可收回的会话先逐出再移动。未知 id 404（归档区未命中即普通 `not_found`） |
| GET | `/api/session/archive` | 列出归档区条目（**读网关侧索引** `.archive/index.json`，不扫会话目录；索引缺失 / 损坏时按目录重建，`archived_at` 为 null）。条目 = 归档时刻事实快照（`id` / `name`（标题回退同 ps）/ `workspace` / `template_name` / `last_interaction` / `archived_at` / `tags`）+ 投影期补全的模型四件套（与 `/api/session/list` 同一读法）。按最后交互时间降序。`wing archive --list` 对应的端点 |

> **会话逐出（eviction）**：空闲会话（无 turn 在跑、inbox 无待处理输入、无人订阅、
> 非 memory 后端，且超过 `sessions.eviction.idle_ttl_seconds`）会被后台周期任务逐出内存——
> 只回收 worker 与 provider client，`history.jsonl` / `metadata.json` 一概不动。
> `resume` / `subscribe` / `send` 按需水合；`session/get`、`info`、`branches` 不
> 自动水合，对已逐出会话仍回 404（`wing tail` / `head` / `info` 内部按 404→resume
> 惯例处理；`/api/session/list` 的 `status: inactive` 是逐出的可观测痕迹）。
> `wing release <sid>` 是对应的显式操作。
>
> **会话归档（archive）**：归档区是**冷存储**——不在 `/api/session/list` 里
> （这正是 `wing ps` 性能的解法），也不参与任何解析：对归档会话调 resume /
> send / tag / create-or-adopt 一律 404，但带结构化 `error="session_archived"`
> （与"真不存在"的 `not_found` 区分），调用方可给出"先恢复"的指引。恢复必须
> 显式（`archived: false` / `wing archive --restore`）——没有隐式恢复路径。
> 自动归档（`sessions.archive`，默认 90 天）由后台周期任务执行：带 `pin`
> 标签的会话豁免（后端感知的唯一保留标签，见 `session.tags.PIN_TAG`），
> `idle_days <= 0` 关闭。`wing archive <sid>` / `wing archive --restore <sid>` /
> `wing archive --list` 是对应的 CLI。
>
> **session id 是闸门（只防穿越与卫生）**：id 由后端生成（默认形态
> `YYYYMMDD-HHMMSS-<8 位小写 hex>`）**或由编排方自带**（`/api/session/create`
> 的 `session_id` = create-or-adopt）。校验只拒绝危险值：含路径分隔符 / `..`、
> 点开头（`<sessions root>/.media` 是遗留媒体池，存储的点命名空间）、ASCII
> 控制字符、不可编码为 UTF-8（孤立代理字符）、空串、超过 **128 字节**
> （`common.utils.is_valid_session_id`；字节而非字符——Linux/macOS 的
> `NAME_MAX` 是 255 字节，`"收" * 128` 是 384 字节，会在首次写入炸
> `ENAMETOOLONG`）。所有端点的 session_id 先过闸门再触达存储——不过闸门的值
> 与"不存在"同价（一律 404，不给探测反馈），绝不进入文件路径拼接（防路径穿越；
> file 后端在拼接处还有最终防线）；create 端点则回 400（请求非法，而非"找不到"）。
>
> **一个 id 一个会话（同文件系统内）**：大小写 / Unicode 归一化不敏感的文件系统
> 上，`team-a` 与 `Team-A` 解析到同一个目录——会话层一律按**磁盘真名**建索引并在
> 响应里回报它（别名留 warning），因此"两个 id 共用一份 `history.jsonl`"不可能
> 发生；大小写敏感的 FS（Linux）上两者是各自独立的会话。读端点（`get` / `info` /
> `branches` / `interrupt`）只认内存里的精确键，别名请求回 404。
>
> 边界：**空会话**（从未发言 → 磁盘无痕迹）一旦被逐出即不可恢复——它没有可水合
> 的状态，此后 `release` / `send` / `subscribe` 都回 404（不是幂等 `not loaded`）。
> 空会话只来自"TUI 启动即建会话"这类场景，回收它正是逐出的目的。

### System（`routes/system.py`，6 个）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/commands` | 命令列表（仅返回 `source == "prompt"` 的命令） |
| GET | `/api/models` | 可用模型目录（按 provider 分组嵌套；目录 = **配置静态声明的同步投影**——远端 `/models` 发现已退役、无网络请求）。`providers[].models` 是**对象数组**：`{id, name, display_name, description, context_window_tokens, keep_recent_tokens, capabilities: {vision}}`——`id` 是全局唯一**引用词**（一切请求 / 协议引用它），`name` 是发给上游的调用名，`display_name` 是展示名（可空，前端回落 `name`），`context_window_tokens` / `keep_recent_tokens` 是压缩双阈值的**生效值**（模型声明唯一来源，未声明 = 声明层默认），`capabilities` 是能力声明（见 [media-images.md](media-images.md)）。旧的 `model_details` 平行数组已删除（消灭「两数组逐项对齐」的脆弱契约） |
| GET | `/api/agents` | 可用 agent 模板列表 |
| POST | `/api/system/reload` | 热重载（无需重启），**逐项名字序是对外契约**（probe `test_system_reload` 钉住，只许在末尾追加）：`config.yaml → hooks → prompt commands → provider → skills & rules → log level`。config 项失败立即中止（后续项不再尝试），其余项失败继续；逐项 `ok` / `detail` 如实上报，**失败不回滚文件**。保存事务（`/api/settings/set` 的第 ⑦ 步）走同一条管道 |
| POST | `/api/shutdown` | Gateway 优雅自关闭（返回 200 后延迟自送 SIGTERM） |
| GET | `/api/tools` | 全局工具列表（内置 + 远程，平铺；ref / namespace / name / llm_name / description）（PR #50） |

> `/api/models` 的响应形状（对象数组；`model_details` 平行数组已不存在）：

```json
{"providers": [{"provider": "qoder", "models": [
  {"id": "dfmodel", "name": "dfmodel", "display_name": "DeepSeek-Flash",
   "description": null, "context_window_tokens": 256000, "keep_recent_tokens": 50000,
   "capabilities": {"vision": false}}]}]}
```

### Settings（`routes/settings.py`，4 个）

RPC 风格（不是 RESTful）：目录 / 取值 / 状态 / 保存各一个端点。**读端点不经过 `server.runtime`**
（`config.document` 的纯函数 + 投影组合），写路径住在 `runtime.apply_settings`。机制细节
（声明层 / 稀疏文档 / 保存事务 / 密文 / 生效域）见 [settings.md](settings.md)。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/settings/schema` | 设置目录树（`SettingNodeProto`：默认值 / 约束 / 枚举 / 生效域 `apply` / 密文标记 / 分组 / 列表元素形态）。纯静态，可长缓存；`{version, root, config_path, groups}`，根节点的 `key` / `path` 恒为 `"config"`；`groups[]`（`{id, title, doc, members}`，顺序即界面顺序）是设置面板左列锚点的唯一来源，声明在后端 `config/groups.py` |
| GET | `/api/settings/get` | 稀疏文档（只有用户显式写下的键；密文叶子恒为 `null`）+ `secrets` 状态表（`set` / `empty` / `absent` + 末 4 位 hint）+ `fingerprint`（文件 sha256；不存在 = `"absent"`）+ 全部 `problems` + `setup_mode` + `config_path`。文件坏掉（YAML 语法错）也照常应答：`values={}` + 一条文档级 problem（`path=null`） |
| GET | `/api/settings/status` | `{valid, setup_mode, problems, fingerprint}`——启动路径上的最便宜预检。`valid == (not setup_mode and 无 problem)`：降级态**恒** `false` |
| POST | `/api/settings/set` | 保存事务。body `{base, document}`：`base` = 客户端持有的指纹（显式 `null` / 缺键 = 不做并发检查，CLI `--force`）；`document` = 整份稀疏文档。**密文三态**：`null` = 保留磁盘现值 / 字符串 = 设为该值（`""` = 显式清空）/ 键缺席 = 从文件移除。**鉴权：admin**（`tool_runtime` 403） |

`POST /api/settings/set` 的响应（`SettingsSetResponse`）：

```json
{"ok": true, "fingerprint": "…", "problems": [], "changed": ["providers[0].extra_body"],
 "restart_required": [], "reload": {"ok": true, "results": [{"name": "config.yaml", "ok": true}, …]},
 "setup_mode_exited": false, "backup_path": "…/config.yaml.bak", "warnings": []}
```

> **校验失败走 HTTP 200 + `ok=false` + `problems`（不是 4xx）**——刻意的取舍，别"修好"它：
> 请求本身完全合法，是**用户填的内容**不合法；把它当业务结果返回，前端保存路径永远拿到同一个响应类型
> （`ok` 决定成败，`problems` 逐条驱动标红）。HTTP 错误码只留给**协议级**失败：**409**（指纹不匹配，
> `error: "conflict"`，`detail` 带当前指纹）、**401 / 403**（鉴权）、**500**（写盘 `OSError`）。
>
> 其他语义：**全有或全无**（有 problem 时文件一个字节都不写）；写前把现有文件字节级复制到
> `config.yaml.bak`（覆盖式，只留最近一份）；`changed` 是相对保存前的变更路径（列表按下标 diff，
> 删除元素会把后续项记成 modified）；`restart_required` 是其中 `apply == restart` 的**叶子**路径，
> **不做假热更**（`gateway.port` 改了也不会换端口监听）；`warnings` 是非致命告知
> （当前唯一产出：原文件不可解析时"其中的密钥无法保留"——语法错的文件也能经这个端点修好）。
> 保存成功会广播 `settings_changed` 事件（见下）。

#### 配置不可用时的降级面（setup mode）

配置缺失 / 非法时网关**不再崩溃退出**：`boot_config()` 永不抛，网关以 **setup mode** 降级启动——
只服务设置端点，其余一律 **503**，`error == "setup_mode"`（`detail` = 前 10 条 problem + 修复指引）。
经 API 保存出一份合法配置后**就地转入正常模式**（不重启进程，`setup_mode_exited: true`）。

**可用路径 = 九条 allowlist**（`gateway/setup_guard.py::SETUP_ALLOWED_PATHS`）：

```
/api/health · /api/settings/{schema,get,status,set} · /api/shutdown · /openapi.json · /docs · /redoc
```

- 其余一切（session / models / agents / commands / tools / system reload …）一律 503。
  注意 `HTTP_ERROR_TYPES[503]` 是**通用**的 `"service_unavailable"`——setup 语义由守门**显式覆盖**
  `error="setup_mode"`（客户端结构化判定的唯一依据，不要嗅探 `detail` 文案）。
- `/ws` 在 **accept 之前**以 1013 关闭（客户端看到握手失败，而不是"连上就断"）。
- **修复模式鉴权 = loopback-only 免 key**：配置坏掉 ⇒ auth 配置本身不可信，所以只接受
  `127.0.0.1` / `::1` / `localhost` 来源且**不要求 key**；非 loopback 一律 **403**。
  **这是收紧不是放松**（正常模式 `auth.enabled=false` 时任何人都能访问）——把网关暴露到 `0.0.0.0`
  且配置坏掉的部署**无法远程修复**，是刻意的安全姿态。中间件顺序（`app.py`）：
  `AuthMiddleware` 在外层，非 loopback 在 setup mode 下任何路径都先吃 403（而不是 503）。

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
- 断连即失败并注销：WS 断开时在途调用立即失败、工具从 registry 移除。**已加载 agent 持有的工具引用不受影响**（保护 KV cache）——再次调用时清晰返回 "tool unavailable / not connected"。运行期工具集切换已由 `POST /api/session/update`（`tools`）支持（PR #50，见 [architecture.md](architecture.md) 远程工具一节）。
- 总超时：`gateway.remote_tool_timeout`（默认 1800s）仅为安全网，断连是首要失败信号。

### Workspace（`gateway/workspace/`，4 个）

**只读**的工作区文件浏览（异机 Web 前端的接入面）：目录树浏览 / 工作区选择 / 文件预览
/@ 引用补全的素材。落位是「文件浏览器」的**过渡形态**——终态由远程工具宿主暴露"它所在
环境的文件系统"；本实现即「workspace provider 的本地实现」（网关宿主机文件系统），
是 Gateway 下的**自含子包**（只依赖 stdlib + FastAPI，拆 Gateway 时原样带走）。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/workspace/roots` | 可浏览的起点：`home`（进程用户家目录）与 `cwd`（网关进程工作目录）——「工作区选择」的入口，去重、只含真实存在的目录（会话自己的 workspace 在 session 面端点） |
| GET | `/api/workspace/list` | 列举目录。`path`（绝对路径，必填）、`offset`（≥0，默认 0）、`limit`（1..1000，默认 200）。响应 `{path, entries[], total, next_offset}`：`total` 是扫描到的有效条目数，`next_offset=null` 表示已到末尾。**排序是契约**：目录在前，组内按 `name.casefold()` 再按 `name`（全序，翻页稳定）；隐藏文件（点开头）不做过滤。**每页成本与 offset 无关**（整目录扫描 + 排序；条目数以文件系统为界），超大目录由前端自行限幅展示 |
| GET | `/api/workspace/stat` | 单个条目元数据（形状同 list 元素）。**lstat 口径**：符号链接不解析目标（`type: "symlink"`，附 `symlink_target` 与 `broken`） |
| GET / HEAD | `/api/workspace/read` | 读取**常规文件**：流式 + `Range`（206 / 416）+ `Content-Disposition: inline`；MIME 按扩展名猜（猜不出 = `application/octet-stream`，`text/*` 附 `charset=utf-8`）。符号链接跟随一次；目录 / FIFO / 设备一律 400（FIFO 会挂住读线程，设备是无界流）；`HEAD` 只回头（大小 / 类型 / Range 支持） |

不变量与取舍（实现见 `service.py`，单测 + probe `test_workspace_files` 钉住）：

- **只读**：没有写 / 上传 / 删除路径（写继续只走 agent）；全部操作是 scandir / lstat /
  readlink / 常规文件 read。
- **路径是提供方命名空间的绝对路径**：相对路径 400——不把「网关进程 cwd」这类本地细节
  写进协议（远程实现里没有对应物）；`.` / `..` / 重复分隔符做**词法归一**（`abspath`，
  不触盘、不解析符号链接），响应回显归一后的路径。
- **条目类型**（`type`）：`file` / `dir` / `symlink` / `other`（lstat 口径）；`size` /
  `mime` 只对常规文件有意义（其余为 `null`）；`mtime` 是 lstat 的 epoch 秒。
- **错误面**：404（不存在）/ 403（无权限）/ 400（相对路径、NUL 字节、对非目录列举、
  读非常规文件）/ 422（参数校验）；统一 `ErrorResponse` 形状。
- **`read` 的安全响应头**：`X-Content-Type-Options: nosniff` 恒在；`text/html` /
  `application/xhtml+xml` / `image/svg+xml` / XML 这些**浏览器会执行**的文档类型额外带
  `Content-Security-Policy: sandbox`——渲染照常（HTML / SVG 可预览），但脚本被禁、资源
  降成唯一源，防止「直接导航 / iframe 预览文件」把任意文件读升级成同源 XSS（能读
  localStorage / 驱动 API 的那个源）。
- **安全口径**：整流在鉴权面之后（反代 + HTTPS + 应用层鉴权 + 内网 VPC，"能过鉴权即
  全权"），不做过度加固。本端点族没有"根约束"——它浏览的就是宿主文件系统，`..` 只是
  普通导航，因此不存在"越出根"的穿越面；基本卫生落在：绝对路径校验、NUL 拒绝、词法
  归一、列举 / 分页限幅（单页 ≤1000）、符号链接策略显式、错误文案克制（不带堆栈 /
  不读文件内容）。RBAC：对 `tool_runtime` 默认关闭（allowlist 之外，403）；setup mode
  下随其余端点一律 503。

## WebSocket 协议（`/ws`）

握手成功后服务端推送 `ConnectResponse { type: "connected", client_id }`。之后是**双向**通道：

- **服务端 → 客户端**：推送 `WingEvent` 事件流，事件均为 `WingEvent` 子类，按 `type` 字段区分。
- **客户端 → 服务端**：发送 `ClientRequest` 帧 `{ request_id, session_id, content, tool_call_id?, media? }`，用于投递用户消息，或携带 `tool_call_id` 定向回复某个 `ask` 事件（resolve 对应的 feedback waiter）。Gateway 注入 `client_id` 后转给 `WingRuntime.post()`。`media` 是消息携带的媒体引用（`MediaRef` 数组，缺省 / null = 无媒体；每个 id 必须是该会话已存在的对象、mime 必须在支持列表内，否则整条消息被拒——客户端收 `error` 帧（携带该帧的 `session_id` 与客户端自己的 `request_id`，供「拒绝 → 丢弃 pending」关联）、消息不进历史、也没有 `delivered`）。

> 投递消息有两条等价路径：WS `ClientRequest`（TUI 实际所用，便于与 Ask 回复复用同一连接）或 HTTP `POST /api/session/send`。

**远程工具帧**（tool host 专用，按 `call_id` 字段与 `ClientRequest` 区分，向后兼容）：

- **服务端 → tool host**：`tool_call_request { type, call_id, name, arguments }` —— 发起一次远程工具调用。
- **tool host → 服务端**：`tool_call_result { type, call_id, result, is_error }` —— 回传调用结果，Gateway 据此 resolve 在途调用。

`tool_runtime` 角色连接时须经 `?client_id=<id>` 自选 client_id（admin 也可自选，未声明者服务端分配）；该 WS 是纯工具执行通道，不参与事件订阅。

**ReAct 事件**（`event/react.py`）：`turn_started` · `user_message_accepted` · `text` · `reasoning` · `tool_call_stream` · `tool_call` · `tool_call_result` · `diff_content` · `ask` · `assistant_turn` · `tool_result_turn` · `turn_result`（subtype: success / error_during_execution / error_max_turns）· `done` · `llm_call_metrics`。

> `tool_call_stream`：LLM 生成工具参数期间的流式渲染事件，携带**增量原始 args 文本碎片**（`args_fragment`，首个事件含完整前缀）。后端不解析 partial JSON，前端自行累积 buffer 并容错解析渲染；参数生成结束后由 `tool_call` 事件携带权威解析结果。

> `user_message_accepted`：用户消息被消费进模型上下文的确认（`content` + `origin_request_id`，后者即客户端提交时的 `request_id`）。两个发射点：`run_turn` 入口的 drain-and-merge（先于 `turn_started`）与工具执行后的 steer 注入。产品语义：TUI 把已发送消息挂在底部排队区，收到本事件才上移进聊天历史——消息"往上走"当且仅当它真的被发给了模型。无 `request_id` 的内部投递不发射。

**状态事件**（`event/state_change.py`）：`session_init` · `sync_session`（订阅时重放历史）· `session_state_changed`（update / think / yolo 后统一发出）· `interrupted` · `compact_done`。

> **`sync_session.usage`（用量基线）**：与 `status` / `turn_started_at` 同刻的会话用量账本快照（形状同 `/api/session/info.usage`），是前端累计的**唯一**基线来源——快照与快照之后的 live 事件走同一条有序 WS 通道，"基线 = 快照值、增量 = 其后 `llm_call_metrics` 之和"精确成立。**不要**拿 HTTP `/api/session/info.usage` 做基线：两个通道无跨通道定序，快照可能已包含尚在途的事件（重复计数）或在它之后才到（丢增量）。旧网关不带这个字段——按零基线显示。

> **模型四元组**（`model` / `model_id` / `provider_name` / `model_display_name`）：会话 agent 快照（`sync_session.agent`、`GET /api/session/get.agent`）、`session_state_changed`（模型变更时四者同刻下发）与 `GET /api/session/info` 携带同一组字段。`model_id` 是配置声明 `providers[].models[].id` 的投影（可空——旧会话的 id 已删除且反查不中时为 null），是**身份**：前端的选择态匹配、状态展示的「当前模型」判定与一切变更请求都以它为准；未命中任何候选即视为未知，绝不按调用名反查。`model_display_name` 是**展示层素材**（未声明 / 空串 = 缺失或 null）：前端渲染展示名、缺省回落调用名，不参与匹配。

**其他**（`event/base.py`、`query_response.py`）：`error` · `notice` · `delivered` · `context_stats` · `branch_targets`。

**网关级事件**（`event/state_change.py`）：`settings_changed` —— `{changed, restart_required, setup_mode_exited, fingerprint}`，`target = global`（广播给所有已连接客户端）；`persist=false` 且**不进 `FACT_EVENTS`**：它是**时点通知**（没有 `session_id`、不进任何会话链），客户端重连后应重新 `GET /api/settings/status`，重放一条旧通知只会误导。消费侧**用指纹比对**判断是不是别人改的（自己的保存会更新本地指纹，相同即忽略；不同则提示"配置已被其它客户端修改"并作废本地缓存）。

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
- **修复模式（setup mode）另有一条前置分支**：配置不可用时只接受 loopback 来源且**不要求 key**（非 loopback 一律 403）——见上面「配置不可用时的降级面」。
- key 用 `hmac.compare_digest` 常量时间比较；须为 ASCII 可打印字符（配置解析时校验）。
- `auth_config` 每次请求读取最新配置单例，`/api/system/reload` 可即时生效。
- **加密（TLS）由外部反向代理（nginx 等）负责**，应用层只做身份验证。

## CORS（跨源访问）

装配在 `gateway/cors.py`（中间件最外层——**预检不带凭据，必须就地应答**，不能先被鉴权拦下）。

**部署口径：推荐同源部署**——前端静态文件与 API 走**同一个反向代理**（同源），该形态下
CORS 完全不被用到、网关零配置（也是唯一的推荐形态；跨源只服务本机开发与边缘场景）。
WebSocket（`/ws`）不受 CORS 约束，不需要也不提供任何跨源处理。

| 来源 | 是否放通 | 说明 |
|------|----------|------|
| loopback（`http(s)://localhost` / `127.0.0.1` / `[::1]`，**任意端口**） | **默认放通**（网关自身绑 loopback 时） | CORS 规范不支持端口通配，实现用 host 谓词匹配、按请求回声具体 origin（`allow_origin_regex`）。只服务本机页面 / 开发服务器；注意 `file://` 页面的 Origin 是字面量 `null`，**不在**放通范围 |
| 其它来源 | 默认不放通 | 写进 `gateway.cors.origins` 才生效（如 `https://ui.example.com`；写 `*` = 放开全部来源） |

**loopback 回声有个前提**：网关自身绑在 loopback 地址（`gateway.host`）上。绑 `0.0.0.0` /
外部地址是**远端可达**的部署，本机任意页面的跨源读权限随之关闭（那不是「本机开发」形态；
需要就显式写 `origins`，loopback 来源也可以写进去）。

- `allow_credentials` 恒 `false`（API key 走 header，不用 cookie）；允许的方法 / 头按前端
  最小需求：`GET / POST / HEAD / OPTIONS` + `Authorization / Content-Type / X-API-Key / X-Client-Id`；
  `Access-Control-Expose-Headers` 放通 `Content-Range / Content-Disposition / ETag / Accept-Ranges`
  （Fetch 的 safelist 不含它们，跨源前端要读 Range / 文件名素材必须显式 expose）。
  401 / 403 响应同样带 CORS 头，前端能读到错误体。
- **生效域 = `restart`**：中间件在网关启动时装配，改 `gateway.cors.origins` 要重启网关
  （保存回执的 `restart_required` 会点名 `gateway.cors.origins[i]`）。**setup 修复也一视同仁**：
  配置坏掉后经保存事务**就地**转入正常模式（不重启进程）时，origins 仍是装配时的空值——
  运行期重建中间件在 Starlette 里不可行，修复流程里刚写的外部来源仍需重启才生效。
- 安全性：CORS 不是鉴权边界（能过鉴权即全权）。对**本机进程**这层默认不构成新权限
  （不受同源策略约束，本就能直连 API）；对**页面**成立的边界变化与部署指引见下。

**风险与部署指引（如实声明）**：默认放通 loopback + `auth.enabled=false` 的组合下，
**本机 http(s) 服务上的页面**（任意端口的本地 dev server / 本地工具里的页面）可以 `fetch`
读走宿主文件与 `~/.wing/core/config.yaml` 里的密钥——这是「本机开发便利」换来的代价
（`file://` 打开的本地 HTML 不受影响：其 Origin 是 `null`，不在放通范围）。缓解：
loopback 回声要求网关自身绑 loopback（见上），远端可达的部署默认就没有这层。因此：

- **推荐同源部署**（前端与 API 同一个反向代理 + HTTPS + 鉴权），该形态下 CORS 不参与；
- 网关**绑定非 loopback 地址**（`gateway.host: 0.0.0.0`）或**多用户 / 非本人可控的机器**上，
  必须开 `gateway.auth`（或由反代在鉴权后面转发），否则 `/api/workspace/read` 是免鉴权的
  任意文件读原语；
- 网关不校验 `Host`（既有现状），DNS rebinding 类攻击不靠 CORS 挡住——别把裸网关
  暴露到不可信网络。
