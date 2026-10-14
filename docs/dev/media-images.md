# 媒体与图片（read-image）

「图片文件 → 模型可见」这条链路的机制说明：`ReadImage` 工具、内容寻址的媒体存储
（**会话私有目录** `<sessions-root>/<sid>/media/`，session 自包含）、消息里的媒体引用、
**请求期图片投影**（字节高水位 + 量子批量驱逐；计数驱逐默认关闭）与两条协议线格式。

事实来源：代码 `libs/core/wing/{media/,schema/,store/,agent/,provider/,context/compaction.py}`、
`libs/core/wing/tools/builtin/read_image.py`；整机证据 `libs/wing-probe/scenarios/test_read_image.py`
（15 条）+ `test_media_layout.py`（3 条存储布局）+ `test_user_media.py`（8 条用户贴图链路），本文末列。
本页只讲 *why* 与不变量，逐行契约以代码为准。

## 链路总览

```
用户贴图（TUI composer → WS ClientRequest.media）        ReadImage(path)（模型发起）
   │ 仅引用上行（字节此前已写入会话媒体目录）              │ magic bytes 判格式（png/jpeg/webp/gif）
   │                                                      │ 能力门禁：未声明 vision → ToolError
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
    name: str | None = None          # basename（展示用）

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

- **上行链路**：字节先入会话媒体目录（写入端点 `POST /api/session/media` 由后续步骤交付；在那之前
  唯一写口是 `ReadImage` 与测试预置），客户端把 refs 放进消息——WS `ClientRequest.media` 或 HTTP
  `POST /api/session/send` 的 `media`（两条等价路径共用同一透传链：`WingRuntime.post → SM._post →
  Session.post → Agent.post → Inbox.post → Message(media=…)`）。字段缺省 / null = 无媒体——旧客户端
  / 旧帧与引入 media 之前**逐字节一致**。
- **入口闸门**（`session.validate_media_refs`，在 `SM._post` 里、`DeliveredEvent` **之前**）：
  每个 ref 的 id 必须是 64 位小写 hex 且对象必须已存在（会话目录或老池；`media_exists` 是纯查询，
  不物化）。不合规 → `InvalidInputError`：HTTP 400 / WS `error` 帧；消息**不进历史、不进队列、
  没有送达 ack**。校验只认"id 格式 + 对象存在"——mime / bytes / 尺寸是提示性元数据（只进驱逐
  启发式），不是安全面。
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
  max_bytes: 4718592          # 单图原始字节上限 4.5 MiB（读时拒绝 + 降采样提示）
  max_images: -1              # 请求期计数高水位；≤0 = 关闭计数驱逐（默认 -1）
  count_quantum: 8            # 计数驱逐量子（启用计数驱逐时每次超限至少丢这么多张）
  request_budget_bytes: 37748736   # 请求期 base64 编码后累计高水位（36 MiB）
  evict_quantum_bytes: 18874368    # 字节驱逐量子（18 MiB = 预算一半，与 DSH 同构）

providers:
  - name: my-provider
    image_delivery: inline    # 可选：inline | followup（缺省按协议）
    image_max_bytes: 5242880  # 可选：单图请求期兜底（Anthropic 建议 5 MiB）
```

> 保留策略只有一套口径：`max_images`（计数，**≤0 = 关闭**，默认 -1）+
> `request_budget_bytes`（字节兜底）+ 两个量子（`count_quantum` /
> `evict_quantum_bytes`，要求 > 0——非正数会让量子退化为逐条驱逐，配置层拒绝以免误配）。

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
- **Files API / file_id 引用模式未做**（DeepSeek Files API 这类"先上传再引用"的上游优化）。
- **DSH 式持久 offload 决策 + 请求失败自愈重试未做**：投影是无状态纯函数，失败不做重试调整。
- 不做 EXIF 方向修正、动画 GIF 多帧解析（只读头部尺寸）、图片自动重编码。
- **Apple CgBI 变体 PNG 不解析尺寸**：CgBI 是 Apple 的非标准 PNG（`CgBI` chunk 排在
  IHDR 之前，IHDR 位于偏移 28），标准布局解析器读不到尺寸——`ReadImage` 识别该特征并报
  "Apple CgBI variant … convert it first"（附 `sips -s format png` 示例），**不**误报
  "truncated or corrupt"。其私有字节序刻意不解析（无可验证的规范来源）。
- 远程工具（`wing-sdk` / `tool_host.rs`）返回图片、`GET /api/media/{id}` 端点、TUI/VSCode 像素渲染
  （kitty/iTerm2 协议、webview）不在本期；用户侧入口的**服务端**链路见「用户贴图」一节，前端
  （composer 图片 chip / 终端协议 / 压缩）在前端侧交付。
- **贴图引用的元数据不校验内容一致性**：入口闸门只认 id 格式 + 对象存在（安全面）；ref 里的
  `bytes` / `width` / `height` 是提示性元数据，只参与请求期驱逐启发式——服务端不读字节复核
  （要读字节就意味着每次发消息都全量哈希，代价与收益不成比例）。
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

用户贴图链路（`libs/wing-probe/scenarios/test_user_media.py`，02 message_chain；对象由场景预置进
`<sid>/media/<id>`，03 的上传端点落地后同一链路改由上传写入）：

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
    libs/wing-probe/scenarios/test_user_media.py -q --timeout=120
make test-probe        # 全量（含基础设施自测）
```

> probe 侧只连 loopback：环境 / 系统 HTTP 代理豁免（`LOOPBACK_HOSTS`、子进程 `NO_PROXY`、
> 客户端 `trust_env=False`）见 `libs/wing-probe/wing_probe/env.py` 的模块注释；断言原语见
> `probe-testing.md`。这些场景不访问外网。
