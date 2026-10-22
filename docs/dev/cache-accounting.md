# 缓存账本（KV 前缀缓存）

「这次请求的前缀指纹」与「provider 报的实际命中」放在同一张表上：**理论命中率**（若
provider 满兑现能做到的水位）+ **分叉归因**（第几段、哪个文件、可不可避免）。

这个能力**只观察**：不改写请求体、不改 `history.jsonl`、不加配置面、不做事前提醒。
所以它只回答一个问题——「这一轮的缓存为什么这样」——并让每个结论落到三个动作之一：
**我能避免 / 我该报 bug / 我什么都不用做**。

事实来源：机制 `libs/core/wing/schema/cache.py`（段 label 与 hash 的唯一声明处）+
`libs/core/wing/context/cache_ledger.py`（账本与归因）+ `libs/core/wing/context/manager.py`
（system 侧分段）+ `libs/core/wing/provider/`（wire 侧分段）；端口径
`libs/core/wing/gateway/routes/session.py`（`GET /api/session/cache`）；人类视图
`crates/wing/src/cmd/cache.rs`；整机证据 `libs/wing-probe/scenarios/test_cache_ledger.py`
（A–F 六条红线）与 `libs/core/tests/test_cache_ledger.py`（单元口径）。
本文只讲 *why* 与判据，逐字段契约以代码（`context/cache_ledger.py` / `schema/cache.py`）为准。

## 1. 口径

### 分段（segment）——归因的最小单位

一次请求的前缀被切成**有序具名段** `{label, sha, chars, src?}`。段落序列 == provider
真正 tokenize 的顺序，因此「第 index 段起分叉」才有意义。

| label | 谁产 | 含义 |
|---|---|---|
| `system:base` | ContextManager | 模板自带的系统提示词主体 |
| `system:append` | ContextManager | 会话级附加系统提示（`append_system_prompt`） |
| `rules:<abs-path>` | ContextManager | 每个 rules 文件一段，按加载顺序（pattern 序 × glob 序） |
| `skills:<name>` | ContextManager | 每个 skill 一段，按加载顺序 |
| `system:wire` | ModelProvider | **封口段**：实际下发的 system 载荷（协议序列化后） |
| `tools:<name>` | ModelProvider | 每个**实际下发**的工具声明一段，按声明顺序 |
| `history:<uuid>:<role>` | ModelProvider | 模型视图里每条非 system 消息一段（role ∈ user/assistant/tool） |

- 顺序固定为 `system:base → system:append → rules:* → skills:* → system:wire → tools:* →
  history:*`。`system:wire` 之所以紧跟 system 段而不是放末尾：协议会拆块 / 打
  `cache_control` 断点，封口段要覆盖「实际下发的 system 字节」；放末尾则 history 一增长就
  恒判前缀破碎。
- `sha` = 该段**确定性规范形**（`json.dumps(sort_keys=True, ensure_ascii=False,
  separators=(",", ":"))`）的 sha256 前 16 位。文本段传字符串，协议段传序列化后的 dict
  ——同一套规则，不会出现「文本 / dict 两套口径」。
- `chars` = 该段的长度，**只用于估算代价**（`tokens_lost`），不参与判定：文本段是文本的
  字符数，协议段（tools / history / `system:wire`）是协议序列化后**规范形的长度**。
- **`cache_control` 断点与 hash 的关系是不对称的**：断点恒打在**本次请求的最后一帧消息**
  上（每轮都在移动），Anthropic 另在 `body["system"]` 块上打一个（位置稳定；openai 兼容协议
  没有独立 system 块，system 提示词本身就在 messages 里）。因此 `history:*` 的 hash 刻意取
  **打标记之前**的形态（计入移动断点会让内容完全没变的相邻两轮恒判 `prefix_broken` —— 假
  阳性，`test_cache_control_does_not_leak_into_history_hashes` 钉住）；Anthropic 的
  `system:wire` 含那个 system 断点（属于下发的真实字节）；`tools:*` 的 hash 就是实际下发的
  工具声明——两条协议的断点都不落在工具声明上。
- **`history:*` 覆盖的是链上的消息**（模型视图里每条非 system 消息一段），因此**段数 ≠ wire
  消息数**：provider 在请求期合成的媒体 followup user 消息（连续 tool 段之后那条带图消息）
  没有段。影响可忽略——那条消息的字节是「哪些媒体位被保留 + 顺序」的确定性投影，而同一个
  投影决策在驱逐某位时会把占位文本追加回**所属 tool 消息**（该段 hash 随之变化），媒体本身
  又是内容寻址的（字节变 ⇒ media id 变）。
- `src = {path, mtime, size}` 只在 `rules:*` / `skills:*` 上出现，**在加载时**采集一次
  （`load_rule_files` / `collect_skill_sources`）——每次 LLM 调用的热路径不扫目录、不 stat。
- 两侧分工：CM 知道文本与来源（system 侧），provider 知道真实字节（wire 侧）；分组经
  `RequestOptions.cache_parts` 传递，探针累积器（`CacheProbe`）由 caller 每次调用新建。
- **探针不可关**：`WingAgent.request_options()` 无条件挂一个 `CacheProbe()`。传 `None` 的
  调用点（不观察的路径）让 provider 一个段都不算——请求体因此逐字节不变。

指纹 `fp = sha256("|".join(f"{label}:{sha}"))[:16]`，只用于去重存储，不参与判定。

### 账本

`<session-dir>/cache_ledger.json`（会话 aux kv，key = `cache_ledger`；`file` 后端 tmp +
fsync + rename 原子写，`memory` 后端留在进程内字典）。形状见 `runtime.cache_report`。

- **有界**：三档，全部由代码里的常量决定，没有配置面。
  - `calls` 只留最近 200 条（`MAX_CALLS`）；
  - `fingerprints` 只留被存活 calls 引用到的条目（写盘时 GC）；
  - **落盘字节预算 512 KiB**（`MAX_LEDGER_BYTES`）：超出即按「最旧调用先淘汰」裁剪，
    连带 GC 失去引用的指纹，**永远保留最近一条调用**。量级两档：常规 40 轮会话
    **≈160 KB**，200 条 × 上百段的极端形态在裁剪前能到**数 MB**——所以预算是账本的真上界。
    两处例外：预算只在**记账时**生效（旧版本写下的超大账本要等下一次调用才被裁）；单条调用
    自己的段落表就超预算时保留它（丢了它就没有「最近一条」可利用了）。
    （「~60 KB/会话」按「多条调用共享同一份段落表」估算；真实会话里消息
    每轮增长、每条调用都有独立段落表。契约的字段与公式没变，只有量级口径以这里为准。）
- **永不抛**：读、算、写全在 try/except 内，失败只 WARNING——记账坏了绝不影响 LLM 调用。
- **前向容忍**：读不到 / 版本不认识 / 解析失败 / 单条记录类型错 → 当空账本（坏记录丢弃，
  不升级成整份失败，端点因此不会 500）。
- **不碰 `history.jsonl`**：`Message` / `usage` 一个字段都不加；账本是边车文件。
- 记账是「整份读 → 改 → 整份原子写」，跑在线程池里（同步写会挡住整个网关的流式帧），
  进程级一把锁覆盖 read-modify-write（主调用与后台增量压缩可能真并行）。
- 端点的**读**路径同样跑在线程池里（`asyncio.to_thread`）：读盘 + 全量 JSON 规整 + 投影在
  最坏形态是 MB 级、数十毫秒，留在事件循环上会让所有会话的流式帧一起排队。
- 每次记录都重新读盘、不缓存内存态：`switch_template` 会重建 CM，内存态会留两个残影。

### 三档 rate（`GET /api/session/cache` 的 `summary`）

```
theory_rate        = Σ expected（仅非 null）                       / Σ prompt
actual_rate        = Σ cached                                     / Σ prompt
provider_debt_rate = Σ max(0, expected - cached)
                     （仅 verdict ∈ {provider_underdelivered, block_granularity}） / Σ prompt
```

- 分母是**整本账本**的 `Σprompt`（不是最近 N 条）；端点的 `calls` 默认只回 20 条
  （`limit=200` 才是全量），`wing cache --limit 200` 同理——想手工对账就取全量。
- 分母为 0（空账本）→ 三个 rate 均为 `null`。首调用的 `prompt > 0` 而 `Σexpected = 0`，
  所以 `theory_rate` 是 **`0.0` 而不是 `null`**：有分母、没有理论水位，就是 0%。
- `expected` = 参照调用的 `prompt`（仅当前缀延续且未过期）；无参照（cold / 前缀破碎 /
  过期）一律 `null`——不发明数字。
- `provider_debt_rate` 是「能归咎于 provider 侧的损耗」：欠兑现 + 块粒度缺口。前缀破碎
  （我们这侧碎）**不算** provider 债务，它进 `forks`。

### 参照调用怎么找

倒序扫描账本，取最近一条满足「段列表是本次的**严格前缀**、且 `prompt > 0`」的调用；段列表
与本次完全相同（重试 / 重放）也认作参照。找不到 → 无参照（`cold` / `prefix_broken`）。
参照与本调用时间差 > 72 小时（`CACHE_TTL_HOURS`，模块常量）→ `cold_expired`。

## 2. 判据

### verdict —— 这一轮的缓存算哪种情况

| verdict | 条件 | 我该做什么 |
|---|---|---|
| `cold` | 会话第一次调用，没有参照 | 什么都不用做 |
| `hit` | 参照存在且 `cached >= ref.prompt` | 什么都不用做 |
| `block_granularity` | 参照存在且 `0 < ref.prompt - cached <= tol` | 什么都不用做（缓存按块兑现，见下） |
| `provider_underdelivered` | 参照存在且 `cached < ref.prompt - tol` | **报 bug**：前缀没变而 provider 没兑现，本地无所作为 |
| `prefix_broken` | 前缀没能延续（存在 fork）——优先级高于其余 | 看 `forks` 那行的 `blame` / `avoidable` |
| `cold_expired` | 参照调用超过 72 小时 | 什么都不用做（常量，不可调） |

`tol = max(256, ceil(0.02 × ref.prompt))`。优先级：`cold_expired` > `prefix_broken` > 其余；
有 fork 一律 `prefix_broken` 且 `expected = null`。

块粒度为什么不算 provider 的错：缓存按块兑现，差一个块的 token 数和块大小有关，provider
无权「部分兑现」。连续多轮都贴 `tol` 上限说明块很大——想知道确切块大小只能去 provider 侧核对。

### blame / avoidable —— 该怪谁，能不能避开

| 分叉处 label | blame | avoidable | 动作 |
|---|---|---|---|
| `rules:` / `skills:` | `external` | `true` | **我能避免**：改了规则文件 / skill 内容就让整段前缀失效；代价 ≈ `tokens_lost` |
| `tools:` | `tools` | `true` | **我能避免**：运行期改工具集（`POST /api/session/update`）会冻结声明集并注入 System Reminder，声明一变前缀就碎 |
| `system:base` / `system:append` / `system:wire` | `composition` | `false` | 什么都做不了（模板 / 协议层，不是用户输入） |
| `history:` 且参照与本调用之间有 `compact` | `compact` | `false` | 什么都不用做（压缩的必然代价） |
| 其它 `history:` | `composition` | `false` | 什么都做不了（链拓扑变化：rewind / fork / steer） |
| verdict `provider_underdelivered` | `provider` | `null` | 报 bug（当前口径下不可达，见下注） |
| verdict `cold` / `cold_expired` | `cold` | `null` | 什么都不用做（当前口径下不可达，见下注） |

`blame` 是读取侧现算的派生值（不落盘），唯一依据是 `fork.label` 与 `verdict`。

> **后两行为什么不可达**：`forks` 只收 `fork != null` 的调用，而 `fork` 只在「前缀没能
> 延续」时产生、`prefix_broken` / `cold_expired` 又优先于其余 verdict——所以
> `provider_underdelivered` 的调用永远带不上 fork，`cold` 更是首调用的判定。这两行留在
> 表里是因为契约给了它们位置（读取侧的 blame 派生规则）；实际输出里你只会看到前五行。
> 换句话说：欠账只进 `summary.provider_debt_rate`，分叉行只谈「前缀碎在哪一段」。

> **人类视图（`wing cache`）的过滤**：按 **verdict** 兜底——`hit` / `cold` / `cold_expired` /
> `block_granularity` / 未计算一律不出现。`composition` 行**会**出现在表里（它确实是分叉），
> 只是 `AVOIDABLE=no` 表示你无法行动——看到它不必当 CLI 的 bug。

### 分叉的形状

- `kind`：`S[k].label` 不在旧侧尾部 → `insert`；旧侧 `C[k].label` 不在新侧尾部 →
  `remove`；两侧 label 相同而 sha 不同 → `modify`。
- `index` = 公共前缀长度 k（第 index 段起分叉）；`label` = 分叉处的 label。
- `segments_affected` = **最近一条调用（分叉的对照侧）** fork 点之后的段数
  （`len(C) - k`，「碎了多少段」；不是参照侧的）。
- `tokens_lost = round(ref.prompt × (1 - chars_before / ref_total_chars))`，**估算值**，
  展示必须带 `≈`；无参照时 `null`。
- `detail`（`forks[].detail`）：
  - `path` / `mtime` / `size`：`src`，能点名到文件（优先取**新侧**——被改的那个文件的当下事实）。
  - `sha_before` / `sha_after`：**对照侧** → 本次。对照侧 = 记录时分叉真正相对的那条调用
    （`recent[position+1]`）；参照（前缀延续点）在自身长度范围内与本次逐段同值，因此它
    解释不了「这里变了」——「改了又改回」时正是它会把 `sha_before` 抄成 `sha_after`。参照只在
    邻座已经滚出账本、或邻座在该位点没有段时兜底。
  - `ref_ts`：**与 `sha_before` 同源**——对照侧是邻座就是邻座的时刻，是参照就是参照的时刻。
    两者都定位不到（对照侧已经滚出环形账本）→ `null`，`--why` 那一行整行不显示：不拿邻居的
    ts 冒充一个已经不可见的时刻。

## 3. 降级

| 情形 | 表现 | 消费方该怎么做 |
|---|---|---|
| 旧网关 / 未计算 | `llm_call_metrics` 没有 `cache_*` 字段；`cache_verdict` 为空串 | 退回今天的单一命中率（TUI 显示 `67% cache`，而不是 `67% / 理论 88%`） |
| 无理论水位 | `cache_expected_tokens` 为 `null` 时**整个键不下发**（wire 出口剥 null） | 读到缺省即「无参照」，不显示理论值（不要当 0%） |
| 会话还没调用过 / 无账本 / 会话不存在 | 端点 200 + 空 `forks` / `calls` + `summary` 计数 0、三 rate `null` | 显示「还没数据」（`—`），**不要**显示 0%，也不要当错误处理 |
| 账本损坏 / 读失败 | 当空账本 + WARNING（`grep 'cache ledger' <wing_*.log>`） | 不用管；下一次调用会重建 |
| 记账写盘失败 | 只 WARNING，本轮 LLM 调用照常返回（无 `cache_*` 字段） | 不用管；磁盘 / 权限问题在日志里 |
| `memory` 后端（建会话时 `backend: memory`，不是配置字段） | 账本仅在进程内，重启即丢 | 跨重启比对时不要期待它 |
| 端点 404 | 运行中的网关**没有这个路由**（旧二进制 / 另一个 gateway 可执行文件） | 换当前 checkout 的网关；`wing cache` 的 404 只有这一个含义 |

## 4. 排错手册

| 症状 | 先看哪里 |
|---|---|
| 命中率突然掉 0%，`forks` 有一行 `rules:<path>` | 该文件是否刚被改 / 被别的工具重写：`wing cache <sid> --why <ts>` 看 `mtime` / `size` / `sha_before → sha_after`。`avoidable=YES` → 我能避免（改内容前先想清楚会碎前缀） |
| 0% 但 `forks` 为空 | 看 `calls` 末尾的 `verdict`：`provider_underdelivered` → 报 bug；`block_granularity` → 只是块粒度，什么都不用做；`cold` → 首调用 |
| 每轮都出现 `tools:…` 分叉 | 有东西在改工具集（编排器调 `/api/session/update`、hook、前端面板）。每次热切换都会冻结声明集 + 注入 reminder，前缀必然碎 |
| `history:…` 分叉后跟 `compact` 归因 | 压缩的必然代价，不用追 |
| `history:…` 分叉但没压缩 | 链拓扑变了（rewind / fork / steer）——回看那一轮的会话操作 |
| 理论值远高于实际，且 `provider_debt_rate` 大 | 报 bug：provider 没兑现我们这侧完整的前缀。附 `--json` 原始响应 |
| 端点 200 但 `calls` 空 | 会话真的没跑过模型调用；或 `WING_SESSIONS_PATH` 指到了别的目录（读的是另一个会话树） |
| 账本里只剩最近几十条调用，更早的不见了 | 字节预算（512 KiB）按「最旧先淘汰」裁过——不是丢数据；被裁掉的旧行会失去 `ref_ts`，跨裁剪的 `compact` 归因也会退化成 `composition`，最新一行不受影响 |
| 账本文件不存在而调用在跑 | `memory` 后端；或记账 WARNING（`grep 'cache ledger'`） |
| 前端数字与 `wing cache` 不一致 | 两边都可能只取了部分行：`summary` 覆盖整本账本，`calls` 默认 20 条——端点 `limit=200` / `wing cache --limit 200` 取全量再对账 |
| 想知道某条结论怎么复现 | `make test-probe` 的 `scenarios/test_cache_ledger.py`（A–F 六条红线，假 Provider 剧本驱动） |

常用命令：

```bash
wing cache                            # 最近活跃会话（表头打印解析出的 id）
wing cache <sid> --limit 50           # 更多调用行
wing cache <sid> --why 1              # 展开第一条分叉：index/总段数、label、sha 对照、src、≈代价
wing cache <sid> --json               # 端点原始响应 + resolved_session_id
curl -s "http://127.0.0.1:<port>/api/session/cache?session_id=<sid>&limit=200" | jq .summary
# 账本文件是原始账本（version / session_id / fingerprints / calls）——汇总不在文件里，
# 它是端点现算的投影；文件里值得看的只有指纹表与最近几行调用。
jq '.calls[-3:]' ~/.wing/core/sessions/<sid>/cache_ledger.json
```

## 5. 已知边界

- **provider 侧不可见**：真实命中还取决于 provider 内部（cache TTL、块大小、请求被路由到
  哪台机器、按什么 key 分片）。账本只能把「我们这侧的前缀指纹」与「provider 报的缓存量」
  并排放，**无法解释** provider 内部为什么没兑现——这正是 `provider_underdelivered` 需要
  报 bug 而不是自行修复的原因。
- **估算值**：`tokens_lost` 按字符比例估算（展示带 `≈`）；`chars` 是字符数不是 token 数。
- **常量而非配置**：环形上界 200（`MAX_CALLS`）、字节预算 512 KiB（`MAX_LEDGER_BYTES`）、
  `cache_ttl_hours=72`、`tol` 的 256 / 2% 都是 `context/cache_ledger.py` 的模块常量。本功能
  只观察，不新增配置面。
- **只记事实不存副本**：`src` 只有 path / mtime / size，没有文件内容——能点名「哪个文件变了」，
  不能告诉你「变了什么」。
- **粒度**：`history:*` 每条消息一段 → 任何消息内容变化只碎它之后的前缀；压缩 / rewind /
  fork 会让 history 段整体变化（归因落在 `compact` / `composition`）。
- **协议无关**：openai 兼容与 anthropic 两条路径产出同一套 label。`cache_control` 与 hash 的
  关系是**不对称**的（见 §1）：断点落在最后一帧消息（两条协议）与 Anthropic 的 system 块上；
  `history:*` 刻意取打标记之前的形态，`tools:*` 与断点无关。
- **会话内单位**：账本是会话级，没有跨会话汇总（`--all` 不存在）。
- **理论 ≠ 应然**：`theory_rate` 是「若 provider 满兑现能做到的水位」，不是「本该命中却没
  命中」的断言；只有 `provider_underdelivered` / `block_granularity` 才进 provider 债务。

## 6. 回滚

整笔改动只观察、不改写请求：回滚 = revert 相关提交。账本是边车文件，旧版本代码完全忽略它
（`cache_ledger.json` 可安全删除，下一次调用重建；`history.jsonl` 一个字节都没动）。
