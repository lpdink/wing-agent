# 媒体与图片（read-image）

「图片文件 → 模型可见」这条链路的机制说明：`ReadImage` 工具、内容寻址的媒体存储、
消息里的媒体引用、**请求期图片投影**（高水位 + 量子批量驱逐）与两条协议线格式。

事实来源：代码 `libs/core/wing/{media.py,schema.py,store/,agent/,provider/,context/compaction.py}`、
`libs/core/wing/tools/read_image.py`；整机证据 `libs/wing-probe/scenarios/test_read_image.py`
（本文末列 12 条场景）。本页只讲 *why* 与不变量，逐行契约以代码为准。

## 链路总览

```
ReadImage(path)                                            工具调用（模型发起）
   │ magic bytes 判格式（png/jpeg/webp/gif）+ 纯 Python 头部解析尺寸
   │ 能力门禁：模型未声明 capabilities.vision → ToolError（不读文件、不写存储）
   ▼
SessionStore.write_media(id, bytes)                        内容寻址入库（幂等）
   │ file 后端：<sessions-root>/.media/<id[:2]>/<id>        id = sha256(bytes)
   │ memory 后端：进程内 dict（同一 store 实例共享）
   ▼
ToolOutput(content=信封, media=[MediaRef])                 ToolExecutor → tool 消息
   │ history.jsonl 只落引用（绝无 base64）；tool_call_result 事件带 tool_media
   ▼
plan_request_media(messages, policy, vision, cap)          请求期投影（确定性、不读字节）
   │ kept → 按线格式发射；dropped → 原文本块后追加固定占位文本
   ▼
openai: followup(默认) / inline      anthropic: inline(默认) / followup
   ▼
模型请求体里的 data URL / base64 image block      （字节由 provider 按需从存储读取）
```

`ReadImage` 已在 `default_config.py` 的默认 agent 工具集里；未声明 vision 的模型调用它时
按门禁安全拒绝（见下）。

## ReadImage 工具

- 注册名 `ReadImage`，唯一参数 `path`（相对路径按会话 workspace 解析，见 `tools/utils.py`）。
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
    media: list[MediaRef] | None = None   # 当前仅 tool 消息使用（user 为预留）
```

- 空列表归一为 `None`：无媒体消息的 `history.jsonl` 记录与引入 media 之前**逐字节一致**。
- `ToolOutput(content, media)` 是工具的结构化返回（`str` 仍兼容）；工具结果截断与
  `after_tool_call` hook **只作用于文本**，media 是引用元数据、原样透传。
- 存储 API（`SessionStore`）：`write_media(id, data)` / `read_media(id) -> bytes | None`。
  - id 必须是 64 位小写 hex（**防路径穿越的唯一防线**）；首写校验 `sha256(data) == id`，
    已存在即跳过（内容寻址下已存在必然同内容）。
  - file 后端布局 `<sessions-root>/.media/<id[:2]>/<id>`（两级散列防单目录膨胀），
    跨 session 共享——**fork 零拷贝**，子会话与源会话引用同一个对象。
  - memory 后端进程内 dict，与 store 实例同生命周期（重启即消失，与"不落盘"语义一致）。
  - 读取失败（对象缺失 / I/O 错误）**绝不抛**：WARN + 返回 None，序列化层降级为占位文本。
- **没有 GC**：对象只增不减，孤儿对象不清理（引用反向索引不存在）——见"已知边界"。

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
      - legacy-model                       # 存量：纯字符串 = 无能力声明
      - name: dfmodel-2026                 # 对象形态：name = 实际调用名
        display_name: DeepSeek-Flash       # 展示名（缺省前端回落 name）
        description: 深度求索正式版模型
        capabilities:
          vision: true                     # 本期唯一能力；缺省 false
```

- **未声明 = text-only**（安全默认）；不做名字启发式。
- `resolve_model_capabilities(provider_cfg, model)` 是唯一解析入口；`ToolContext.capabilities`
  实时解析（无缓存），会话中 `/model` 切换后门禁与投影随之变化。
- 对象形态未知键**静默忽略**（pydantic `extra="ignore"`）：`capabilites` / `visionn` 这类
  拼写错误**不报错**，按"未声明"处理 → text-only。没有启发式会替你打开能力——这是安全默认，
  不是校验遗漏（配置校验只做实际调用名去重）。
- `GET /api/models` 在 `models: [str]` 之外**追加** `model_details: [{name, display_name,
  description, capabilities: {vision}}]`，与 `models` 逐项同序同名（缺 detail 的补最小条目）。
- **展示名随会话状态下发**：会话 agent 快照（`sync_session.agent` / `GET /api/session/get`）与
  `session_state_changed`、`GET /api/session/info` 与 `model`（`model_name`）同刻携带
  `model_display_name`（未声明 / 空串 = 缺失或 null）——前端渲染展示名、缺省回落实际调用名，
  不必拿调用名去 `/api/models` 里自查；展示名不参与身份（匹配 / 变更仍以实际调用名 + provider 为准）。
- 存量兼容：旧 config（`models: [str]`）零修改可用；旧前端忽略 `model_details`。

## 请求期图片投影（核心）

实现：`wing/media.py::plan_request_media`（纯函数；provider 侧只做配置解析与消息对齐，
见 `provider/media.py::plan_for_request`）。

```python
def plan_request_media(messages, *, policy: MediaPolicy, vision: bool,
                       max_image_bytes: int | None = None) -> list[MediaPlan]
```

算法（确定性、无状态、**不读图片字节**，丢弃恒为最旧优先）：

```
0) vision=False                       → 全部 dropped("no_vision")
1) image_max_bytes 设置且 bytes > cap → dropped("too_large")（严格大于）
2) 计数：eligible 数量超 max_images   → 丢 ceil(excess / count_quantum) * count_quantum 条
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
- 占位文案（冻结字符串，追加在**原文本块之后**）：

| 原因 | 文案 |
|------|------|
| `no_vision` | `(image omitted: this model does not accept image input)` |
| `budget` | `(image omitted from this request: context image budget; re-read the file to attach it again)` |
| `too_large` | `(image omitted: exceeds this provider's per-image size limit)` |
| 字节缺失（独立路径） | `(image unavailable: stored image bytes could not be read; re-read the file to attach it again)` |

### 三条不变量

1. **原文本 = 存储事实的纯函数**：投影只追加、不改写 → 被保留消息的字节在驱逐前后完全一致；
2. **投影不读字节**：只需 `MediaRef.bytes/width/height`；
3. **投影是消息列表的确定性函数**：无持久状态、无时间戳、无随机——任何时刻重算结果全等。

### 为什么不驱逐、为什么按量子批量驱逐（KV / 前缀 cache）

视觉 token 内联在 token 序列中，**任何让图片表示变化的动作都会从首个受影响 token 起**
让前缀 cache 失效（DeepSeek 适配器 DSH 的 README 与 PI 的结论一致；PI 干脆不驱逐，
直到 compaction 把窗口整体改写）。因此策略是：

- **常态一张不丢**（cache 友好）；只有硬阈值逼到才丢，且**按量子批量**丢最旧的：
  计数规则一次至少丢 `count_quantum`（默认 8）条 → 稳态下每新增约 `count_quantum` 张图
  才会断一次 cache；字节规则一次释放 `evict_quantum_bytes`（默认 18 MiB）的整数倍。
- 被丢弃的出现**不修改原文本块**，只在其后追加占位文本——保留部分的消息字节不变，
  受影响前缀从该位置起才重算，而不是整条请求重放。
- 两个高水位都远高于日常用量（32 张 / 36 MiB 编码后）——正常会话基本不会触发驱逐，
  触发时也是一次批量、而非逐张抖动。

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
images:                       # 顶层段（default_config.py 模板同步维护）
  max_bytes: 4718592          # 单图原始字节上限 4.5 MiB（读时拒绝 + 降采样提示）
  max_images: 32              # 请求期计数高水位（超出触发批量驱逐）
  count_quantum: 8            # 计数驱逐量子（每次超限至少丢这么多张）
  request_budget_bytes: 37748736   # 请求期 base64 编码后累计高水位（36 MiB）
  evict_quantum_bytes: 18874368    # 字节驱逐量子（18 MiB = 预算一半，与 DSH 同构）

providers:
  - name: my-provider
    image_delivery: inline    # 可选：inline | followup（缺省按协议）
    image_max_bytes: 5242880  # 可选：单图请求期兜底（Anthropic 建议 5 MiB）
```

> 数值全部要求 > 0：`count_quantum` / `evict_quantum_bytes` 是投影算法的除数，
> 高水位为 0 会让所有图被丢——属误配而非"关闭"。保留策略只有一套口径：
> `max_images`（计数）+ `request_budget_bytes`（字节）+ 两个量子。

## 已知边界与 future work

- **读时缩图（read-time downscale）未做**：超限图片是"拒绝 + `sips` 提示"，模型自己转换/
  降采样后再读。
- **媒体 GC 未做**：对象只增不减（孤儿对象保留），没有 `delete_media` 入口，也没有按引用
  的反向索引；后续可考虑按会话 watermark 清理。
- **Files API / file_id 引用模式未做**（DeepSeek Files API 这类"先上传再引用"的上游优化）。
- **DSH 式持久 offload 决策 + 请求失败自愈重试未做**：投影是无状态纯函数，失败不做重试调整。
- 不做 EXIF 方向修正、动画 GIF 多帧解析（只读头部尺寸）、图片自动重编码。
- **Apple CgBI 变体 PNG 不解析尺寸**：CgBI 是 Apple 的非标准 PNG（`CgBI` chunk 排在
  IHDR 之前，IHDR 位于偏移 28），标准布局解析器读不到尺寸——`ReadImage` 识别该特征并报
  "Apple CgBI variant … convert it first"（附 `sips -s format png` 示例），**不**误报
  "truncated or corrupt"。其私有字节序刻意不解析（无可验证的规范来源）。
- 用户侧贴图（composer paste / stdin 图片输入）、远程工具（`wing-sdk` / `tool_host.rs`）返回
  图片、`GET /api/media/{id}` 端点、TUI/VSCode 像素渲染（kitty/iTerm2 协议、webview）均不在本期。
- `vision: auto` 名字启发式**刻意不做**（未声明 = text-only）。

## 整机证据（wing-probe）

`libs/wing-probe/scenarios/test_read_image.py`（真网关 + 假 Provider + 公开协议；断言锚定
事件时间线 / 请求留档 / `history.jsonl` / 文件系统）：

> `@pytest.mark.probe_env(models=…)` 只声明 provider 的**静态模型列表**（能力 / 展示元信息，
> 即 `/api/models` 与 `resolve_model_capabilities` 的数据源），**不改 agent 默认模型**——
> 场景必须显式 `probe.session(model=…)`。否则请求会打到 agent 模板里的占位模型
> （`probe/default`），而假 Provider 的剧本按 model 名路由，表现为 5xx 或断言对不上。

| 场景 | 断言要点 |
|------|----------|
| `test_positive_read_default_followup` | `tool_call_result.tool_media` 逐字段 == 文件事实；信封文本 == 工具结果；请求中图片在全部 tool 消息之后的 user 消息里、data URL 解码 == 文件；history 有引用、**无 base64**；`.media/<id[:2]>/<id>` 落盘且 sha256 == id |
| `test_inline_delivery_keeps_image_in_tool_message` | `provider_extra={"image_delivery": "inline"}`：图在 tool 消息 content 数组（紧跟文本），无带图 user 消息、无引导文案 |
| `test_vision_gate_refuses_without_side_effects` | 拒绝文案含模型名；全请求无 image part；`<sessions-root>/.media` 目录不存在（零写入） |
| `test_model_switch_downgrades_to_placeholder`（红线） | vision 读图 → `POST /api/session/update` 切 text 模型 → 下一轮无 image part、图片位为 `no_vision` 占位；**原信封 part 逐字节未改** |
| `test_high_water_quantum_eviction`（红线） | `images={"max_images": 3, "count_quantum": 2}` 连读 4 张 → 恰最新 2 张为图、旧 2 条为 `budget` 占位；连续两次请求媒体形态签名全等（确定性） |
| `test_compact_request_strips_media`（红线） | 压缩请求（非流式）无 image part 也无占位；压缩后仍可续跑 |
| `test_fork_keeps_media_usable` | 子会话请求图片仍为 image part；存储对象唯一（引用池共享、无拷贝） |
| `test_legacy_history_without_media_resumes` | 逐出 → 裁掉 `history.jsonl` 的 `media` 键 → resume 续跑成功；请求无图无占位 |
| `test_provider_image_max_bytes_degrades_too_large` | `provider_extra={"image_max_bytes": 100}`：工具仍成功入库（`tool_media` 带引用、对象文件在），请求期该位为 `too_large` 占位、整条无 image part |
| `test_images_max_bytes_refuses_read_without_writing` | `images={"max_bytes": 100}`：读时拒绝（文案含大小与上限 + `images.max_bytes`）；`.media` 目录不存在（零写入）；请求无图无占位 |
| `test_missing_media_object_degrades_to_unavailable` | 删除 `.media/<id[:2]>/<id>` 对象后再发一轮：该位为 `UNAVAILABLE` 占位、请求成功、无 error 事件 |
| `test_default_max_bytes_refuses_read_without_writing` | **不注入** `images`（走代码默认值 4.5 MiB）：5 MiB 文件读时拒绝（文案含 `5.0 MB exceeds the 4.5 MB per-image limit`）；`.media` 目录不存在（零写入）；请求无图无占位 |

跑法：

```bash
uv run --no-sync pytest libs/wing-probe/scenarios/test_read_image.py -q --timeout=120
make test-probe        # 全量（含基础设施自测）
```

> probe 侧只连 loopback：环境 / 系统 HTTP 代理豁免（`LOOPBACK_HOSTS`、子进程 `NO_PROXY`、
> 客户端 `trust_env=False`）见 `libs/wing-probe/wing_probe/env.py` 的模块注释；断言原语见
> `probe-testing.md`。这些场景不访问外网。
