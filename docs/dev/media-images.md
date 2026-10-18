# 媒体与图片（read-image）

「图片文件 → 模型可见」这条链路的机制说明：`ReadImage` 工具、内容寻址的媒体存储
（**会话私有目录** `<sessions-root>/<sid>/media/`，session 自包含）、消息里的媒体引用、
**请求期图片投影**（字节高水位 + 量子批量驱逐；计数驱逐默认关闭）与两条协议线格式。
非图片文件是**另一条通道**（文件系统：`POST /api/session/files` 落 `<sid>/files/` + 路径告知
agent），本文在「用户贴图」末尾与配置 / 整机证据处给出交叉引用，端点级细节见
[http-api.md](http-api.md)。

事实来源：代码 `libs/core/wing/{media/,schema/,store/,agent/,provider/,context/compaction.py}`、
`libs/core/wing/gateway/routes/session.py`（上传 / 读取 / 文件端点）、`libs/core/wing/tools/builtin/read_image.py`；
整机证据 `libs/wing-probe/scenarios/`：`test_read_image.py`（15 条）/ `test_media_layout.py`（3 条存储布局）
/ `test_user_media.py`（8 条用户贴图链路）/ `test_session_media_upload.py`（9 条上传端点）/
`test_session_media_read.py`（7 条读取端点）/ `test_session_files_upload.py`（9 条文件上传），本文末列。
本页只讲 *why* 与不变量，逐行契约以代码为准。

## 链路总览

```
用户贴图（TUI 粘贴 → 上传字节 → composer 引用）           ReadImage(path)（模型发起）
   │ ① POST /api/session/media（原始字节）→ MediaRef      │ magic bytes 判格式（png/jpeg/webp/gif）
   │    校验（非空/上限/格式/尺寸）→ id=sha256 → 落盘     │ 能力门禁：未声明 vision → ToolError
   │ ② 仅引用上行（WS / send 的 media 数组）              │
   ▼                                                      ▼
消息入口闸门（id 格式 + media_exists）                 SessionStore.write_media(sid, id, bytes)
   │ 幽灵引用 → 400 / error 帧，不进历史                   │ <sessions-root>/<sid>/media/<id>，id=sha256
   ▼                                                      ▼
Message(role="user", content=…, media=[MediaRef])      ToolOutput(信封, media=[MediaRef])
   │ drain-and-merge：content 拼接、media 按消息序 concat   │ → tool 消息（history 只落引用）
   │ 纯图消息合法；steer 遇带图消息阻断（D7）               │
   └──────────────────────────┬───────────────────────────┘
                              ▼
plan_request_media(messages, policy, vision, cap)      请求期投影（确定性、不读字节）
   │ kept → 按线格式发射；dropped → 原文本块后追加占位文本（user / tool 文案分流）
   │ text 模型：user 图的 no_vision 占位附**保存路径**（D6）
   ▼
openai: followup(默认) / inline      anthropic: inline(默认) / followup
   ▼
模型请求体里的 data URL / base64 image block      （字节由 provider 按需从存储读取）
```

`ReadImage` 是内置工具，但只有在某个 agent 的 `tools` 列表里才会被它使用（`agents[].tools` 声明；
TUI 设置面板或 `wing config add agents[0].tools ReadImage` 都能加）；未声明 vision 的模型调用它时
按门禁安全拒绝（见下）。

## ReadImage 工具

- 注册名 `ReadImage`，唯一参数 `path`（相对路径按会话 workspace 解析，见 `tools/internal/utils.py`）。
- 支持 **PNG / JPEG / WebP / GIF**，以 magic bytes 判定，**不看扩展名**；尺寸用纯 Python 头部解析
  （PNG IHDR / GIF LSD / JPEG SOFn 扫描 / WebP VP8·VP8L·VP8X），不解码像素。
- **能力门禁在任何文件 I/O 之前**：模型未声明 `capabilities.vision` 时直接拒绝
  （`tool_success=false`），既不 stat 也不读字节，更不写媒体——"拒绝不产生副作用"。
  文案给出开启方法（config 片段）并在归属明确时点名同 provider 的 vision 模型。
- 失败一律 `ToolError`（错误文案回灌模型，面向"下一步怎么做"）：
  文件不存在 / 权限 / 是目录 / 空文件 / 超过 `images.max_bytes`（附 `sips -Z 1568` 降采样示例）/
  不支持的格式（附 `sips -s format png` 转换示例）/ 头部截断损坏 /
  Apple CgBI 变体（IHDR 不在标准偏移，同样是"转换后重读"指引，不误报损坏）/ 会话未挂媒体存储。
- 成功返回单行**信封**（模型唯一可见的文本）：沿用 `format_size` 的人类可读字节数：

  ```
  [image: /abs/path/pic.png | png 800x600 | 2.4 MB | id ab12cd34 | mtime 1759271234]
  ```

- `Read` 撞上图片时保持原有 `Binary file (...)` 错误，仅在识别为支持格式时追加指引
  `— use ReadImage to view images`。

## 媒体引用与存储

```python
class MediaRef(BaseModel):          # 仅元数据，绝无字节
    id: str        # sha256 hex（64 字符）= 存储文件名
    mime: str      # image/png | image/jpeg | image/webp | image/gif
    bytes: int     # 原始字节数
    width: int
    height: int
    name: str | None = None          # basename（展示用；≤ 255 字符，超限的写入侧 400）

class Message(ChainNode):
    media: list[MediaRef] | None = None   # user（贴图）与 tool（ReadImage）携带；assistant/system 不携带
```

- 空列表归一为 `None`：无媒体消息的 `history.jsonl` 记录与引入 media 之前**逐字节一致**。
- `ToolOutput(content, media)` 是工具的结构化返回（`str` 仍兼容）；工具结果截断与
  `after_tool_call` hook **只作用于文本**，media 是引用元数据、原样透传。
- 存储 API（`SessionStore`）——媒体对象是**会话私有**资产（session 自包含，见下）：
  - `write_media(session_id, media_id, data)` / `read_media(session_id, media_id) -> bytes | None`
    / `media_exists(session_id, media_id) -> bool` / `media_path(session_id, media_id) -> str | None`
    / `materialize(session_id, media_id) -> bool`。
  - id 必须是 64 位小写 hex（**防路径穿越的唯一防线**，session id 同样过闸门）；首写校验
    `sha256(data) == id`，已存在即跳过（内容寻址下已存在必然同内容）。
  - file 后端布局 `<sessions-root>/<sid>/media/<id>`（**平铺**：单会话引用对象量级小，
    不需要池时代的两级散列）；memory 后端 per-session 桶，与 store 实例同生命周期。
  - 读取失败（对象缺失 / I/O 错误）**绝不抛**：WARN + 返回 None，序列化层降级为占位文本。
- **会话自包含（为什么 session 私有）**：整个 `<sessions-root>/<sid>/` 目录就是一个可搬运
  单元——拷到另一台机器（新 `WING_HOME`）后 resume，历史引用的图片全部可用。反过来，共享池
  模式下漏拷池目录会让图片集体降级为占位：上下文与源机器不一致、前缀 cache 必然碎裂。
  同图跨会话各存一份（无去重承诺）、fork 复制一份字节——代价被"图片不大"交换掉。
- **老池只读兼容 + 物化**：老布局的共享池 `<sessions-root>/.media/<id[:2]>/<id>` 仍在读路径上
  ——session 目录 miss 时回退老池，命中即**物化**（原子复制进 `<sid>/media/`，幂等；失败只
  WARN，本次读取照常返回字节）。写路径**只写会话目录**，老池永不写入。
  - **加载期物化**：`SessionManager.resume_session`（`ensure_loaded` / create-or-adopt 都收敛
    到它）在加载消息后把活跃链引用的对象逐个物化（先 stat 快路径，miss 才碰老池）；对象通常
    已在目录里，老会话首次加载即完成迁移。失败 WARN 不阻断加载。
  - **fork 复制**：`fork_session` 把**子会话记录整体**（含压缩区间——用户仍可回退到压缩点
    之前）引用的对象复制进子目录（源会话目录优先、老池回退）；失败 WARN 不阻断 fork。
    子会话因此独立自包含：删掉源会话的副本也不影响子会话。
  - `media_path` / `media_exists` 是**纯查询**（不物化）：前者供"无视觉能力"时的占位文案
    附保存路径，后者供消息入口校验引用可兑现。memory 后端 `media_path` 恒 None（没有本地文件）。
- **永不删除**：没有 `delete_media`，不清理老池、不做 GC，也没有按引用的反向索引——对象只增
  不减（孤儿对象保留）。老池在全部引用物化后仍是"只读历史"（不删是刻意策略：任何删除都可能
  让一条历史引用变悬空，而字节代价极小）。

## 用户贴图（user 消息携带媒体）

用户侧整条入口的服务端部分（前端 composer / 终端协议 / 压缩见前端侧文档）。语义由总任务书
D5/D6/D7 拍板：

- **用户贴图（字节侧入口）**：`POST /api/session/media?session_id=<sid>[&name=<n>]`，body = **原始图片字节**
  （非 JSON / 非 multipart；`name` 是展示素材，缺省 / 空串 = 不记录，建议给 basename，长度上限
  255 字符、超限 400）。**收包顺序**：会话存在（否则 404；**不隐式建会话**）→ `Content-Length`
  预检（声明值超 `images.max_bytes` 直接 400、**不读 body**）→ 收包 → 字节校验：body 非空 →
  `≤ images.max_bytes`（无声明 / 声明说谎时按**真实字节**兜底）→ magic bytes ∈
  {png, jpeg, webp, gif}（`sniff_image_mime`）→ 头部尺寸可解析（`image_dimensions`）。闸门是纯函数
  （`wing.media.upload_image_ref`：不碰存储、不依赖 config——上限由调用方传入），**落盘是最后一步**：
  任何失败都**零副作用**（不建目录、不写对象）。id = 字节的 sha256 → `store.write_media`（内容寻址、
  幂等：同字节二次上传返回同一 id 且不重写对象）。响应 `{"ok":true,"media":{id,mime,bytes,width,height,name?}}`，
  其中 `media` 与消息 `media` 数组的元素同形——可直接回投。**不水合**已逐出会话（`/api/session/tag`
  同款：上传一张图不该把 inactive 会话翻成活跃，见 `docs/dev/http-api.md`）。
- **字节读取端点（图片预览的字节源）**：`GET /api/session/media?session_id=<sid>&media_id=<id>`——
  原始字节响应（非 JSON），头 = `Content-Type`（**由字节嗅探现读**：不水合会话就拿不到 MediaRef 的
  mime；四类受支持图片的 magic bytes 就是事实，嗅探不出回落 `application/octet-stream`）+
  `ETag: "<id>"` + `Cache-Control: private, max-age=31536000, immutable`（内容寻址 ⇒ 内容永不
  变更，白拿强缓存）+ `X-Content-Type-Options: nosniff`。读路径 = `store.read_media` 原样复用
  （会话目录优先、老池回退并**物化**）；**非法 id 与不存在同价 404**（`validate_media_id` 的闸门在
  路径拼接之前）；归档会话 404 + `error="session_archived"`；memory 后端天然可读（字节在进程内桶）。
  鉴权开启时 `?api_key=` 可用（`<img>` 带不了请求头，见 `docs/dev/http-api.md` 的鉴权章节）。
- **文件上传端点（非图片，文件系统通道）**：`POST /api/session/files?session_id=<sid>&name=<n>`——
  body = 原始文件字节，落 `<root>/<sid>/files/<实际名>`（**保留原名**：agent 的 Read 看得懂），响应
  `{ok, file:{name, path, bytes}}`，`path` = 网关宿主机绝对路径（消费方写进消息正文
  `[file: 名 · 大小 · 路径]`，agent 用 Read / Bash 去读）。收包纪律 / 零副作用 / 不水合与上传端点
  同款（多一道免费的 query 名字闸门）；上限是 **`files.max_bytes`**（默认 64 MiB，与图片的
  `images.max_bytes` 互不感知）。存储 API：`SessionStore.write_file(session_id, filename, data)
  -> FileRef`（幂等 / 冲突递增在 store 层实现；`FileRef.path` 是本地绝对路径，memory 后端 null）。
  大小写 / Unicode 归一化不敏感的文件系统上（macOS APFS / Windows NTFS）请求名先解析成
  **磁盘真名**再判定：别名拼写（`report.pdf` vs `Report.pdf`）命中时返回真名、不产生第二份
  （响应里的 `name` 恒为实际落盘名）。同名**同字节幂等**、同名**异字节 `stem-2.ext` 递增且绝不覆盖**
  （链上的 `[file: …]` 引用指向既有那份；覆盖 = 静默数据损坏）。memory 后端 `path` 为 null。
  端点级细节见 `docs/dev/http-api.md` 的 session 端点表。
- **引用（引用侧入口）**：客户端把 refs 放进消息——WS `ClientRequest.media` 或 HTTP
  `POST /api/session/send` 的 `media`（两条等价路径共用同一透传链：`WingRuntime.post → SM._post →
  Session.post → Agent.post → Inbox.post → Message(media=…)`）。字段缺省 / null = 无媒体——旧客户端
  / 旧帧与引入 media 之前**逐字节一致**。
- **入口闸门**（`session.validate_media_refs`，在 `SM._post` 里、`DeliveredEvent` **之前**）：
  每个 ref 的 id 必须是 64 位小写 hex、mime 必须在读图支持列表内（`SUPPORTED_IMAGE_MIMES`——
  它原样进线格式 `data:{mime};base64` / anthropic `media_type`，写错会让整次请求被上游打死），
  且对象必须已存在（会话目录或老池；`media_exists` 是纯查询，不物化）。不合规 →
  `InvalidInputError`：HTTP 400 / WS `error` 帧；消息**不进历史、不进队列、没有送达 ack**。
  `bytes` / 尺寸仍是提示性元数据（只进驱逐启发式），不读字节复核。WS 错误帧携带客户端随帧发来的
  `request_id` 与 `session_id`——前端据此把"拒绝"关联回刚发出的那条消息。
- **合并**（drain-and-merge）：同一轮取出的多条消息拼成一条 user message——`content` 用
  `\n` 拼接、`media` 按**消息序 × 消息内序** concat。**纯图消息合法**（content 为空、media 非空：
  请求体只有一个 image part、没有空 text part），`user_message_accepted` 的发射条件相应放宽为
  content 或 media 非空（否则纯图消息的 pending 会卡到 turn end）。定向答复（`tool_call_id`）
  是纯文本通道：同时携带 media 时记 WARN 并按文本 resolve。
- **steer 阻断（D7）**：`Inbox.drain_for_steer` 只取**队首连续**的「无 media 纯文本」消息；遇到
  第一条带 media 的消息立即停止——它与其后所有消息（含后续纯文本）按原序留在队列，等下一轮
  drain-and-merge 一起进上下文（"steer 变 next turn"）。interrupt 的 `clear()` 照旧清空队列，
  但**媒体对象永不删除**。
- **无 vision 降级（D6）**：text 模型收到贴图 → 不发 image part，改发占位文本并**告知图片存在与
  保存路径**（`MediaAccess.path` → `store.media_path`，纯查询不读字节）。memory 后端 / 路径不可得
  → 降级为"仍在会话中"文案（不编路径）。模型据此可换路（换视觉模型 / 让用户换模型 / 对文件 OCR）。
- **user 图参与请求期驱逐（D5）**：字节高水位 / 计数规则对 user 图与 tool 图一视同仁（都按最旧
  优先、量子批量）；user 文本**永不被驱逐**，被驱逐的 user 图用 user 变体占位（见「请求期图片
  投影」的占位表）。

### TUI 里的用户图（前端侧的消费链路）

后端一侧到这里就完整了：消息只携带引用。**把它画出来**是前端的活，TUI 的这条链路是：

```text
ChatCell::UserMessage(UserMessageBody{ text, media })       ui/chat_view/media.rs
   │ 形状来自 MediaRef（上传闸门按 magic bytes 解析的像素尺寸）—— 不探测、不读盘、不等网络
   ▼
每图一个锚点块：caption `▢ Image #1 · 1024×768` + 覆盖行   （行数 = render/fit 的共享 fit）
   │ 图在画不了时（off / 无协议 / 丢弃态）退回一行标签：`[Image #1 1024×768 384KB]`
   ▼
FrameImage{ source: PictureSource::Media(id) }              ui/chat_view/image.rs
   │ 屏幕上要哪几张由 `App::draw` 记录下来（与新鲜度 lane 同一个观察点）
   ▼
app/images/media.rs：屏幕上有、本机没有、也没在飞 → AppIntent::LoadMedia
   ▼
后台 GET /api/session/media?session_id&media_id（响应体客户端上限 16 MiB）→ FetchPayload::MediaBytes
   ▼
字节缓存（目标 max(8, 在屏张数)、硬上限 24 MiB）→ ImageStore::request_media(id, target, bytes) → 编码 → 画出来
```

三条前端口径（机制细节见 [tui-images.md](tui-images.md)）：

- **布局是内容的纯函数**：形状来自引用，字节到没到只决定「图上不上」（画不出来时 caption 留着）。
  滚动不会因为某张图加载完成而跳动，这是刻意的取舍——代价是幽灵引用（对象被手工删掉）会在气泡里
  留下一个空盒子（caption 可见），不会退回标签行。
- **只抓屏幕上的**：打开一个 50 张图的老会话不会立刻发 50 个 GET，滚到哪抓到哪（内容寻址 ⇒ 抓到的
  永远同一份字节，丢掉再抓与一直留着等价）。
- **提交路径不抓网**：本机的压缩产物在上传之前就能算出它将要得到的 `id`（`id = sha256(bytes)`，
  见 `protocol/media.rs::predicted_ref`），所以 pending 气泡立刻有图、promote 之后命中同一份编码缓存。

## 事件与序列化面

- `ToolCallResultEvent.tool_media: list[dict]`（`MediaRef.model_dump()` 列表）——只服务
  直播 / 前端渲染 / probe 断言；字节永不进事件。旧前端忽略新字段。
- `serialize_message(msg)["media"]`：`SyncSessionEvent`（重放）与直播共用同一投影——
  **重放 == 直播**。`history.jsonl` 里 tool 记录的 `media` 数组就是同一份引用。

## 模型能力声明

`providers[].models` 的元素支持字符串（存量）或对象：

```yaml
providers:
  - name: my-provider
    models:
      - legacy-model                       # 存量：纯字符串 = id 与调用名同为 legacy-model，无能力声明
      - id: ds-flash                       # 显式 id = 全局唯一引用词（缺省 = name）
        name: dfmodel-2026                 # name = 实际调用名（发给 provider API 的值）
        display_name: DeepSeek-Flash       # 展示名（缺省前端回落 name）
        description: 深度求索正式版模型
        capabilities:
          vision: true                     # 本期唯一能力；缺省 false
```

- **未声明 = text-only**（安全默认）；不做名字启发式。
- `resolve_model_capabilities(provider_cfg, model)` 是唯一解析入口（`model` 是**调用名**——
  运行期口径：agent 持有调用名，能力声明按调用名查声明项）；`ToolContext.capabilities`
  实时解析（无缓存），会话中 `/model` 切换后门禁与投影随之变化。
- 对象形态未知键**静默忽略**（pydantic `extra="ignore"`）：`capabilites` / `visionn` 这类
  拼写错误**不报错**，按"未声明"处理 → text-only。没有启发式会替你打开能力——这是安全默认，
  不是校验遗漏（配置校验只做 id 全局唯一 + 同 provider 内调用名去重）。
- `GET /api/models` 的 `providers[].models` 是**对象数组**：
  `{id, name, display_name, description, context_window_tokens, keep_recent_tokens, capabilities: {vision}}`
  （`id` = 引用词，`name` = 调用名；窗口 / 保留区是压缩双阈值的生效值——见
  [settings.md](settings.md) 的生效域表；旧的「调用名数组 + `model_details` 平行数组」已删除
  ——能力内嵌在同一个对象里，无逐项对齐契约）。
- **展示名随会话状态下发**：会话 agent 快照（`sync_session.agent` / `GET /api/session/get`）、
  `session_state_changed` 与 `GET /api/session/info` 携带 `model`（调用名）+ `model_id`（引用词）
  + `provider_name` + `model_display_name`（未声明 / 空串 = 缺失或 null）——前端渲染展示名、
  缺省回落调用名，不必拿调用名去 `/api/models` 里自查；**身份是 `model_id`**，展示名不参与匹配。
- 存量兼容：旧 config（字符串形态）零修改可用（id = 调用名）；读图门禁的 `resolve_model_capabilities`
  口径不变（字符串形态 = 未声明能力）。

## 请求期图片投影（核心）

实现：`wing/media/policy.py::plan_request_media`（纯函数；provider 侧只做配置解析与消息对齐，
见 `provider/media.py::plan_for_request`）。

```python
def plan_request_media(messages, *, policy: MediaPolicy, vision: bool,
                       max_image_bytes: int | None = None) -> list[MediaPlan]
```

算法（确定性、无状态、**不读图片字节**，丢弃恒为最旧优先）：

```
0) vision=False                       → 全部 dropped("no_vision")
1) image_max_bytes 设置且 bytes > cap → dropped("too_large")（严格大于）
2) 计数：max_images <= 0 = 关闭（默认）；否则
   eligible 数量超 max_images        → 丢 ceil(excess / count_quantum) * count_quantum 条
3) 字节：Σ encoded_len(bytes) 超 request_budget_bytes
                                     → 目标释放量 = ceil(excess / evict_quantum_bytes)
                                       * evict_quantum_bytes，从最旧累加直到覆盖
4) 两规则在同一基线上独立计算，取更激进者（丢条数取 max）
5) kept → 线格式发射；dropped → 原文本块不动，其后追加固定占位文本块
```

- `role == "system"` 的消息上的 media 一律**忽略**（既不投影为图、也不追加占位）：两个
  协议都不允许 system 携带图片（openai 的 system content 只允许文本 part，anthropic 的
  system 段只取文本）——"占位"文案的语义是"被丢弃 / 不可用"，与协议不承载无关。
- `encoded_len(n) = 4*ceil(n/3)`（base64 后的精确长度）——所以投影不需要读字节，纯元数据运算。
- 单图兜底 `image_max_bytes` 是 provider 级配置（如 Anthropic 5 MiB）：超过的图片位在请求期
  降级为占位，避免"读图时合法、换 provider 后 400 打死整次请求"。
- 占位文案（冻结字符串，追加在**原文本块之后**；按消息角色分流——user 贴图与 tool 读图不同，
  原文案（对 tool）逐字节不变）：

| 原因 | tool / system | user（贴图） |
|------|---------------|--------------|
| `no_vision` | `(image omitted: this model does not accept image input)` | `(image omitted: this model does not accept image input; the image is saved at {path})`；路径不可得（memory 后端 / 查询失败）→ `(image omitted: this model does not accept image input; the image is kept in the session)` |
| `budget` | `(image omitted from this request: context image budget; re-read the file to attach it again)` | `(image omitted from this request: context image budget; the image is still in the session)` |
| `too_large` | `(image omitted: exceeds this provider's per-image size limit)` | 同 tool（"图过大"对两者语义相同） |
| 字节缺失（独立路径） | `(image unavailable: stored image bytes could not be read; re-read the file to attach it again)` | 同 tool（存储故障与角色无关） |

  user 侧改述的理由：tool 原文案 "re-read the file to attach it again" 只对"模型自己读过文件"
  成立，贴图没有可重读的文件；no_vision 附路径是 D6 的兑现（`MediaAccess.path` 只在 user +
  no_vision 时查询——要渲染才查，且不读字节）。`drop_placeholder(reason, role=…, path=…)` 是唯一
  分流点（`wing/media/policy.py`）。

### 三条不变量

1. **原文本 = 存储事实的纯函数**：投影只追加、不改写 → 被保留消息的字节在驱逐前后完全一致；
2. **投影不读字节**：只需 `MediaRef.bytes/width/height`；
3. **投影是消息列表的确定性函数**：无持久状态、无时间戳、无随机——任何时刻重算结果全等。

### 为什么不驱逐、为什么按量子批量驱逐（KV / 前缀 cache）

视觉 token 内联在 token 序列中，**任何让图片表示变化的动作都会从首个受影响 token 起**
让前缀 cache 失效（DeepSeek 适配器 DSH 的 README 与 PI 的结论一致；PI 干脆不驱逐，
直到 compaction 把窗口整体改写）。因此策略是：

- **常态一张不丢**（cache 友好）；只有硬阈值逼到才丢，且**按量子批量**丢最旧的：
  字节规则一次释放 `evict_quantum_bytes`（默认 18 MiB）的整数倍；计数规则（默认关闭，
  显式调回正数才生效）一次至少丢 `count_quantum`（默认 8）条。
- **计数驱逐默认关闭**（`max_images: -1`；≤0 同为关闭）：高强度读图（前端开发大量截图）
  会让图片数很快越过任何固定张数，从而**每次请求都在驱逐**、请求图片集合周期性变化——
  断 cache 的代价远超收益。字节高水位保留为兜底，触发点随单图大小变化
  （`encoded_len = 4*ceil(raw/3)`，预算 36 MiB 编码后 ≈ 27 MiB 原始字节）：
  200 KB 截图约 139 张、1 MiB 约 27 张、2 MiB 约 14 张、4 MiB 约 7 张——
  小图会话基本够不到，MiB 级大图仍会触发（它就是留给大图的上限兜底）。
- 被丢弃的出现**不修改原文本块**，只在其后追加占位文本——保留部分的消息字节不变，
  受影响前缀从该位置起才重算，而不是整条请求重放。

`TokenCounter.estimate_message` 为每个 media 出现累加 `ceil(w*h/750)`（保守口径，
覆盖全部出现），驱动 compaction 提前触发。

## 线格式（两种投递形态）

`providers[].image_delivery: "inline" | "followup"`；**缺省按协议**：
**openai → `followup`**（DeepSeek/DashScope 等兼容网关的文档口径都是 user 消息带图，
最宽兼容）；**anthropic → `inline`**（`tool_result` 内嵌 image block，官方原生路径）。
显式配置永远优先。

| 协议 | `inline` | `followup` |
|------|----------|------------|
| OpenAI 兼容 | tool 消息 `content` 数组 = `[text(信封), image_url…]`；data URL `data:{mime};base64,…` | tool 消息只留文本（字符串原样）；连续 tool 段内保留图汇总为**段后一条 user 消息**：首块 `Images read by the preceding tool results are attached below.` + image parts，顺序 == tool 顺序 × 消息内顺序 |
| Anthropic | `tool_result.content` 数组 = `[text, image block]`（`source.type="base64"`、`media_type`） | tool_result 只留文本；图片以 image block 追加进段后 user 消息（复用同角色合并路径） |

- 丢弃位 / 字节缺失位：原文本 part 不动，其后追加占位 text part（Anthropic 侧缺文本块时不发射
  空 block）。整条请求无媒体时走快路径，序列化结果与引入媒体之前**逐字节一致**。
- `explicit_cache_mode`（默认开）给最后一条消息的最后一个 part 打 `cache_control`；末位是
  图片时回退到其前面最后一个非图片 part（缓存前缀最大化）。
- **压缩剥离**：`Compactor` 的压缩请求对带媒体消息做 `model_copy(update={"media": None})`
  浅拷贝——摘要目标是文本，压缩请求体里既无 image part 也无占位；原链消息零改动。

## 配置

```yaml
images:                       # 顶层段（声明在 config/models.py::ImagesConfig，模板由 config/emit.py 生成）
  max_bytes: 4718592          # 单图原始字节上限 4.5 MiB（读时拒绝 + 降采样提示；上传端点超限 400）
  max_images: -1              # 请求期计数高水位；≤0 = 关闭计数驱逐（默认 -1）
  count_quantum: 8            # 计数驱逐量子（启用计数驱逐时每次超限至少丢这么多张）
  request_budget_bytes: 37748736   # 请求期 base64 编码后累计高水位（36 MiB）
  evict_quantum_bytes: 18874368    # 字节驱逐量子（18 MiB = 预算一半，与 DSH 同构）

files:                        # 顶层段（声明在 config/models.py::FilesConfig）——非图片文件的落盘上限
  max_bytes: 67108864         # 单份上传文件 ≤ 64 MiB（只走 POST /api/session/files；图片不受它管）

providers:
  - name: my-provider
    image_delivery: inline    # 可选：inline | followup（缺省按协议）
    image_max_bytes: 5242880  # 可选：单图请求期兜底（Anthropic 建议 5 MiB）
```

> 保留策略只有一套口径：`max_images`（计数，**≤0 = 关闭**，默认 -1）+
> `request_budget_bytes`（字节兜底）+ 两个量子（`count_quantum` /
> `evict_quantum_bytes`，要求 > 0——非正数会让量子退化为逐条驱逐，配置层拒绝以免误配）。
> `files.max_bytes` 是**另一条通道**的预算（文件系统 vs 视觉），两端点互不感知。

## 已知边界与 future work

- **请求内图片数量没有上界**（计数驱逐默认关闭）：唯一请求期约束是字节高水位。若某个
  provider 对单请求图片数有硬上限（外部文档口径），超限表现为该 provider 的请求错误；
  需要时把 `images.max_images` 调回一个远高于日常用量的正数，或调低
  `request_budget_bytes` 让字节兜底更早生效（两者触发时都会断一次前缀 cache）。
- **读时缩图（read-time downscale）未做**：超限图片是"拒绝 + `sips` 提示"，模型自己转换/
  降采样后再读。
- **媒体 GC 未做（刻意）**：对象只增不减（孤儿对象保留），没有 `delete_media` 入口，也没有按引用
  的反向索引；老共享池物化后也**不清理**（只读历史）。语义是"任何对上下文的操作都不删字节"——
  删除会让历史引用悬空，是比磁盘占用差得多的失败模式；当前量级下收益不足。后续若要清理，
  只能以「会话目录整体删除」为粒度（session 即搬运单元）。
- **Files API / file_id 引用模式未做**（DeepSeek Files API 这类"先上传再引用"的上游优化）——
  本仓的 `POST /api/session/files` 是**网关侧**落盘 + 路径告知（文件系统通道），与上游 Files API 无关。
- **文件名卫生不覆盖 Windows 的形态约束**：保留设备名（`CON` / `PRN` / `AUX` / `NUL` /
  `COM1`…）与尾随点 / 空格（Windows 会在写入时剥掉，于是 `foo.` 与 `foo` 是同一个名字）不在
  `validate_file_name` 的闸门里——那些名字在 macOS / Linux 上完全合法，按平台条件化拒绝会让
  同一个请求在不同宿主上得到不同结论（更差的失败模式）。现状：尾随点 / 空格靠**别名解析**
  落回真名（响应仍诚实），保留设备名在 Windows 上写入失败会如实报 5xx（日志里有系统错误）。
- **用户文件（`files/`）无 GC / 无列目录 / 无下载端点**（与媒体同 doctrine：只增不减，目录就是
  事实源）；上限由 `files.max_bytes` 框住。**fork 不复制 `files/`**（设计如此：文件是"用户现场
  素材"而非链上引用，没有悬空引用的失败模式——agent 只拿到过一次路径）。
- **DSH 式持久 offload 决策 + 请求失败自愈重试未做**：投影是无状态纯函数，失败不做重试调整。
- 不做 EXIF 方向修正、动画 GIF 多帧解析（只读头部尺寸）、图片自动重编码。
- **Apple CgBI 变体 PNG 不解析尺寸**：CgBI 是 Apple 的非标准 PNG（`CgBI` chunk 排在
  IHDR 之前，IHDR 位于偏移 28），标准布局解析器读不到尺寸——`ReadImage` 识别该特征并报
  "Apple CgBI variant … convert it first"（附 `sips -s format png` 示例），**不**误报
  "truncated or corrupt"。其私有字节序刻意不解析（无可验证的规范来源）。
- 远程工具（`wing-sdk` / `tool_host.rs`）返回图片、**全局**媒体池端点（`GET /api/media/{id}`——
  会话私有媒体已由 `GET /api/session/media` 提供，见「用户贴图」）不在本期。TUI 侧的用户图渲染
  已交付（见「TUI 里的用户图」一节与 [tui-images.md](tui-images.md)）；**VSCode 的 webview 渲染**
  仍未做，`tool_call_result.tool_media` / tool 消息里的 `media`（ReadImage 的结果）也还没进
  TUI 图块——两处都只差「同一套锚点接到另一张卡片上」。
- **上传 body 没有服务级大小上限**：两个上传端点（媒体 / 文件）都自己做了两道前置（会话存在性 →
  `Content-Length` 预检）——声明超限的请求在收包**之前**就 400，幽灵会话也先 404；但**没有声明 /
  声明说谎**的 body 仍会先被 `await request.body()` 整包收进内存，`≤ images.max_bytes`（/ 文件端点的
  `files.max_bytes`）的字节判定发生在收包**之后**（超限会被 400 拒绝，只是字节已经过了一遍内存）。
  熔断点该是 ASGI 服务级的 body 上限（中间件 / 反代），而不是端点自己补一条口径；与既有 JSON 端点
  同源（它们也先整包收）。
- **贴图引用的元数据不校验内容一致性**：入口闸门只认 id 格式 + mime + 对象存在（安全面与"线格式
  可用性"）；ref 里的 `bytes` / `width` / `height` 是提示性元数据，只参与请求期驱逐启发式——服务端
  不读字节复核（要读字节就意味着每次发消息都全量哈希，代价与收益不成比例）。
- `vision: auto` 名字启发式**刻意不做**（未声明 = text-only）。

## 整机证据（wing-probe）

`libs/wing-probe/scenarios/test_read_image.py`（真网关 + 假 Provider + 公开协议；断言锚定
事件时间线 / 请求留档 / `history.jsonl` / 文件系统）：

> `@pytest.mark.probe_env(models=…)` 只声明 provider 的**静态模型列表**（能力 / 展示元信息，
> 即 `/api/models` 与 `resolve_model_capabilities` 的数据源），**不改 agent 默认模型**——
> 场景必须显式 `probe.session(model=…)`（值是 **model_id**，且必须落在 `models` 声明里）。
> 否则请求会打到 agent 模板里的占位模型（`probe/default`），而假 Provider 的剧本按 model 名路由，
> 表现为 5xx 或断言对不上。

| 场景 | 断言要点 |
|------|----------|
| `test_positive_read_default_followup` | `tool_call_result.tool_media` 逐字段 == 文件事实；信封文本 == 工具结果；请求中图片在全部 tool 消息之后的 user 消息里、data URL 解码 == 文件；history 有引用、**无 base64**；`<sid>/media/<id>` 落盘且 sha256 == id |
| `test_inline_delivery_keeps_image_in_tool_message` | `provider_extra={"image_delivery": "inline"}`：图在 tool 消息 content 数组（紧跟文本），无带图 user 消息、无引导文案 |
| `test_vision_gate_refuses_without_side_effects` | 拒绝文案含模型名；全请求无 image part；会话媒体目录 `<sid>/media/` 不存在（零写入） |
| `test_model_switch_downgrades_to_placeholder`（红线） | vision 读图 → `POST /api/session/update` 切 text 模型 → 下一轮无 image part、图片位为 `no_vision` 占位；**原信封 part 逐字节未改** |
| `test_high_water_quantum_eviction`（红线） | `images={"max_images": 3, "count_quantum": 2}` 连读 4 张 → 恰最新 2 张为图、旧 2 条为 `budget` 占位；连续两次请求媒体形态签名全等（确定性） |
| `test_compact_request_strips_media`（红线） | 压缩请求（非流式）无 image part 也无占位；压缩后仍可续跑 |
| `test_fork_keeps_media_usable` | 子会话请求图片仍为 image part；对象在源/子会话目录各有一份**独立副本**（fork 复制、自包含），老共享池不参与 |
| `test_legacy_history_without_media_resumes` | 逐出 → 裁掉 `history.jsonl` 的 `media` 键 → resume 续跑成功；请求无图无占位 |
| `test_provider_image_max_bytes_degrades_too_large` | `provider_extra={"image_max_bytes": 100}`：工具仍成功入库（`tool_media` 带引用、对象文件在），请求期该位为 `too_large` 占位、整条无 image part |
| `test_images_max_bytes_refuses_read_without_writing` | `images={"max_bytes": 100}`：读时拒绝（文案含大小与上限 + `images.max_bytes`）；会话媒体目录不存在（零写入）；请求无图无占位 |
| `test_missing_media_object_degrades_to_unavailable` | 删除 `<sid>/media/<id>` 对象后再发一轮：该位为 `UNAVAILABLE` 占位、请求成功、无 error 事件 |
| `test_default_max_bytes_refuses_read_without_writing` | **不注入** `images`（走代码默认值 4.5 MiB）：5 MiB 文件读时拒绝（文案含 `5.0 MB exceeds the 4.5 MB per-image limit`）；会话媒体目录不存在（零写入）；请求无图无占位 |
| `test_default_count_eviction_off_keeps_all_images`（红线） | **不注入** `images`（走代码默认 `max_images=-1`）：一轮连读 33 张（> 旧默认 32）→ 请求体里 33 张全为 image part、零占位（计数驱逐回退即变红） |
| `test_zero_max_images_disables_count_eviction`（红线） | `images={"max_images": 0, "count_quantum": 2}`：0 与 -1 同为"关闭"——配置合法（网关能起）、连读 4 张全保留 |
| `test_byte_eviction_still_applies_when_count_disabled`（红线） | `images={"max_images": 0, "request_budget_bytes": 250, "evict_quantum_bytes": 100}`：4 张图 Σencoded 400 > 250 → 字节规则丢最旧 2 张（占位追加、最新 2 张仍为 image part）；计数关闭不参与 |

存储布局（`libs/wing-probe/scenarios/test_media_layout.py`，01 storage_layout）：

| 场景 | 断言要点 |
|------|----------|
| `test_session_dir_copies_to_fresh_home_with_images` | 读图落 `<sid>/media/<id>` → 整个会话目录拷进**全新**临时 `WING_HOME`/sessions-root（第二个 `ProbeEnv`：新网关 + 新假 Provider）→ resume → 请求体 image part 解码**逐字节 == 原文件**；新环境没有 `.media` 池（对象只能来自会话目录） |
| `test_legacy_pool_object_is_materialized_on_load` | 对象搬到老布局 `<root>/.media/<id[:2]>/<id>` → 逐出内存态 → resume：**发消息之前**对象已出现在 `<sid>/media/`（加载期物化），老对象保留；请求体 image part 逐字节 == 原文件 |
| `test_fork_copies_media_into_child` | fork 后子会话目录有独立副本（inode 不同）；删掉源会话副本（且无老池）后子会话请求仍成功——子会话自包含 |

上传端点（`libs/wing-probe/scenarios/test_session_media_upload.py`，03 upload_endpoint；响应 / 文件系统 /
假 Provider 请求 / history 四个取证面）：

| 场景 | 断言要点 |
|------|----------|
| `test_upload_returns_facts_and_persists_object` | 响应 `media` 逐字段 == 文件事实（sha256 id / mime / bytes / 尺寸 / name）；对象落 `<sid>/media/<id>` 且字节全等、目录只此一份；HTTP 留档显示走的是**原始字节体**（非 JSON body） |
| `test_reupload_of_same_bytes_is_idempotent` | 同字节二次上传 → 同一 id；对象 inode / mtime 不变（不重写）、目录仍一份 |
| `test_rejected_uploads_leave_no_trace` | 空 body / 非图 / 头部截断 → 400（文案各自可辨）；`<sid>/media/` **不存在**；随后合法上传照常成功（闸门不把会话弄坏） |
| `test_oversize_upload_is_rejected_by_config_limit` | `images={"max_bytes": 150}`：200 B 图 → 400（detail 含 `image too large` + `images.max_bytes`）、目录对象数不增；同环境小图照常上传 |
| `test_ghost_session_upload_is_404` | 幽灵会话 / 路径穿越式 id → 404 `session not found`，无目录残留 |
| `test_upload_rejects_by_headers_before_reading_the_body`（收包顺序） | 裸 socket 只写请求头、**不发 body**：幽灵会话 + 声明超限 → 404（存在性先于大小校验）；真实会话 + 声明超限 → 400（detail 含 `image too large` + `images.max_bytes`）、媒体目录不存在——"先校验后收包"的观测面（读包即超时失败） |
| `test_uploaded_bytes_reach_the_model_request`（红线） | 上传 → 消息带 ref → 请求体 user content = [原文本, image part]、解码**逐字节 == 上传字节**；history 有引用、**无 base64**；对象仍在 |
| `test_upload_without_name_is_referencable` | 不带 `name` → `name: null`；对象照样可被引用（`name` 不是寻址键） |
| `test_alias_spelling_lands_in_the_canonical_session_dir` | 别名拼写（大小写不敏感 FS）：200 + 对象落**真名目录**、目录清单只一份；敏感 FS（CI）：404 + 同样不产生第二套目录（两分支各自断言） |

字节读取端点（`libs/wing-probe/scenarios/test_session_media_read.py`，B1；HTTP 响应 / 文件系统 /
列表投影三个取证面）：

| 场景 | 断言要点 |
|------|----------|
| `test_read_returns_bytes_and_cache_headers` | 上传 → 读回：字节逐字节相等；`Content-Type: image/png` + `ETag: "<id>"` + `Cache-Control: private, max-age=31536000, immutable` + `X-Content-Type-Options: nosniff`；query 只有 `session_id` / `media_id` |
| `test_404_faces_leave_no_trace` | 不存在的 64-hex id → 404 `media not found`；非法 id（非 hex / 路径穿越）同价 404；幽灵会话 404 `session not found`；媒体目录清单不增、无越界目录 |
| `test_archived_session_read_is_frozen` | 归档后读 → 404 + `error="session_archived"`（含 restore 提示）；对象随目录进 `.archive` 且字节不变；**恢复后同一 id 立刻可读** |
| `test_evicted_session_can_read_without_being_hydrated` | 逐出后读 200 + 字节相等，且 `/api/session/list` 里仍是 `inactive`（读端点不水合） |
| `test_legacy_pool_object_is_served_and_materialized` | 只有 `.media/<id[:2]>/<id>` 有对象时照常 200；读路径把它物化进 `<sid>/media/`；老池对象保留（只读不删） |
| `test_memory_backend_serves_bytes_from_its_bucket` | memory 后端天然可读（200 + 字节）；桶里没有的 id 仍 404 |
| `test_query_api_key_serves_bytes_without_headers` | 鉴权开启：无 key 401；`?api_key=` 200 + 字节相等；坏 key 401（`<img>` 通道的存在前提） |

文件上传端点（`libs/wing-probe/scenarios/test_session_files_upload.py`，B2；响应 / 文件系统 /
假 Provider 请求 / HTTP 留档四个取证面）：

| 场景 | 断言要点 |
|------|----------|
| `test_upload_returns_path_and_persists_bytes` | 响应 `{ok, file:{name, path, bytes}}` 与磁盘一致（`path` 是绝对路径）；中文名保留原名、目录只此一份；HTTP 留档 = 原始字节体（非 JSON body） |
| `test_agent_reads_the_uploaded_file_by_returned_path`（红线） | 把返回的绝对路径写进消息正文 → 模型发起的 `Read` 工具续轮里 tool 消息含文件内容（这条通道真能用） |
| `test_same_name_never_overwrites_and_same_bytes_is_idempotent` | 同名同字节 → 同路径、inode / mtime 不变；同名异字节 → `report-2.pdf` / `report-3.pdf`，首份字节**原样保留** |
| `test_rejected_uploads_leave_no_trace` | 坏名字（`../x` / 点开头 / 分隔符 / 控制符 / 超长）/ 缺 name / 空 body → 400；`<sid>/files/` 不存在；随后合法上传照常 |
| `test_oversize_upload_is_rejected_by_config_limit` | `files={"max_bytes": 16}`：100 B → 400（`file too large` + `files.max_bytes`）、目录文件数不增 |
| `test_upload_rejects_by_headers_before_reading_the_body`（收包顺序） | 裸 socket 不发 body：幽灵会话 + 声明超限 → 404；坏名字 → 400；真实会话 + 声明超限 → 400（同一句文案）、`files/` 不存在 |
| `test_ghost_session_upload_is_404` / `test_archived_session_upload_is_404_with_the_marker` | 幽灵会话 / 穿越式 id → 404 无残留；归档 → 404 + `session_archived`、`.archive/<sid>/files` 不存在；恢复后照常可上传 |
| `test_memory_backend_has_no_path` | memory 后端 `path: null`（没有本地文件，如实降级） |
| `test_alias_spelling_resolves_to_the_on_disk_file_name` | 大小写不敏感 FS：别名拼写（`report.pdf` vs `Report.pdf`）幂等命中返回**磁盘真名**、不产生第二份；候选名的别名拼写同样解析到真名。敏感 FS（CI）：别名是另一个名字 → 各落一份（两分支各自断言） |

用户贴图链路（`libs/wing-probe/scenarios/test_user_media.py`，02 message_chain；对象由场景预置进
`<sid>/media/<id>`——上传端点落地后，同一存储布局的写入侧由上面的文件走真上传覆盖）：

| 场景 | 断言要点 |
|------|----------|
| `test_user_pasted_image_reaches_request_and_history` | 带 refs 的 user 消息 → 请求体 user content = [原文本, image part]、data URL 解码**逐字节 == 对象字节**；history 有引用、**无 base64**；对象在 `<sid>/media/` |
| `test_pure_image_message_is_accepted` | content 为空、media 非空：请求体只有一个 image part（无空 text part）、turn 收口、`user_message_accepted` 照发（`origin_request_id` 对得上） |
| `test_queued_media_messages_merge_in_order` | 工具执行期间两条带图消息 → 第一条阻断 steer；下一轮合并为**一条** user 消息，media 顺序 = 消息序 × 消息内序（[r1,r2] + [r3]） |
| `test_steer_stops_at_media_message`（红线） | `[纯文本A, 带图B, 纯文本C]` → 只有 A 进 steer note（工具结果头部）；B/C 本轮请求里**都不在**，下一轮完整保序进入；accepted 的 `origin_request_id` 顺序 = 消费顺序 |
| `test_no_vision_user_image_placeholder_carries_saved_path`（红线） | text 模型：无 image part、原文本一字节不改、占位 == `…saved at <sessions-root>/<sid>/media/<id>>`；对象仍在（永不删除） |
| `test_memory_backend_no_vision_placeholder_has_no_path` | memory 后端（对象经 ReadImage 入进程内桶）：占位降级为"kept in the session"，请求体无本地路径；tool 位仍是 tool 冻结文案（分流按角色） |
| `test_budget_eviction_includes_user_images`（红线） | `images={"max_images":0,"request_budget_bytes":200,"evict_quantum_bytes":64}`：user 图（最旧）+ tool 图合计超限 → 丢 user 图（user 变体占位"仍在会话中"）、tool 图仍为 image part；链上引用与对象不动 |
| `test_ghost_media_reference_is_rejected` | 不存在的 ref → HTTP 400（detail 含 id 前缀）；非法 id 格式 → 400；WS 路径收 `error` 帧；history 无该消息、无 `delivered`、无 `user_message_accepted` |

跑法：

```bash
uv run --no-sync pytest libs/wing-probe/scenarios/test_read_image.py \
    libs/wing-probe/scenarios/test_media_layout.py \
    libs/wing-probe/scenarios/test_user_media.py \
    libs/wing-probe/scenarios/test_session_media_upload.py \
    libs/wing-probe/scenarios/test_session_media_read.py \
    libs/wing-probe/scenarios/test_session_files_upload.py -q --timeout=120
make test-probe        # 全量（含基础设施自测）
```

> probe 侧只连 loopback：环境 / 系统 HTTP 代理豁免（`LOOPBACK_HOSTS`、子进程 `NO_PROXY`、
> 客户端 `trust_env=False`）见 `libs/wing-probe/wing_probe/env.py` 的模块注释；断言原语见
> `probe-testing.md`。这些场景不访问外网。
