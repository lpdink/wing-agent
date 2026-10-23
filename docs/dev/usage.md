# 用量统计（usage）：审计流水 + 会话目录 → 一个聚合端点

> 本页是这条特性的**口径参照点**：数字从哪来、怎么算、边界在哪、和 `wing cache` 与
> 会话账本（`SessionUsage`）差在哪。实现落点：迁移 `libs/core/wing/db/migrations.py`
> （v4 两张表）+ 采集点 `libs/core/wing/audit/recorder.py` 与
> `libs/core/wing/session/manager.py` + 领域层 `libs/core/wing/metrics/`（L3，唯一 SQL 处）
> + 端点 `libs/core/wing/gateway/routes/usage.py`（`GET /api/usage`）；整机证据
> `libs/wing-probe/scenarios/test_usage_metrics.py` 与
> `libs/core/tests/{test_metrics_store,test_gateway_usage}.py`。
> 逐字段契约以 `libs/core/wing/gateway/protocol/usage.py` 与测试断言为准。

## 1. 数据从哪来

三个来源，全是**事实流水 / 派生投影**，没有一处是写时聚合：

| 表 | 一行 = | 谁写 | 时间列 |
|---|---|---|---|
| `audit_llm_call` | 一次带 usage 的 LLM 调用 | `AuditRecorder`（`LLMCallMetricsEvent`） | `ts` / `ts_ms` |
| `audit_tool_call` | 一次**真实执行**的工具调用 | `AuditRecorder`（`ToolCallResultEvent`） | `ts` / `ts_ms` |
| `audit_user_message`（v4） | 一次**被接受**的用户输入 | `AuditRecorder`（`UserMessageAcceptedEvent`） | `ts` / `ts_ms` |
| `session_catalog`（v4） | 一个会话的创建时刻 + 血缘 / workspace / 模板 / 模型引用词 | `SessionManager` 的新建分支 + 启动回填 | `created_ts` / `created_at_ms` |

- `audit_*` 三张表**只追加、永不删**（没有 DELETE，归档不只清），与 v1 起就有的三张表同口径。
- `session_catalog` 是**派生投影**：磁盘（`metadata.json`）才是事实源，表可随时重建。
  它只在创建时写一次（`INSERT OR IGNORE`），之后不再更新——`model_id` 是**创建时刻的绑定**，
  不是当前模型；标题不存（由前端 join `/api/session/list`）。
- 归档 / 删除会话**不清理**这两组行：统计保留历史事实（与 `audit_*` 一致，没有级联）。

用户消息为什么要单独一张表：`audit_*` 只有模型与工具调用，用户消息只有一个 lifetime 计数器
（`metadata.usage.user_messages`），没有时间维度——热力图缺「人说话」这个语义轴。

## 2. 端点

```
GET /api/usage
  since       YYYY-MM-DD | "all"   缺省 = until 往前 84 天
  until       YYYY-MM-DD           缺省 = 今天（网关本地时区）
  group_by    day|model|session|tool|workspace|agent|origin   缺省 day
  session_id  会话 id（可选）       范围收敛到该会话；tree 时含全部子孙
  tree        bool = false         必须配 session_id
  limit       int 1..200 = 20      只作用于 model/session/tool/workspace/agent
```

响应：

```json
{
  "range": {"since": "2026-08-03", "until": "2026-10-23", "data_since": "2026-10-20"},
  "group_by": "day",
  "totals": {"...11 个度量..."},
  "rows": [{"key": "2026-10-23", "label": null, "parent_session_id": null, "...11 个度量..."}],
  "truncated": false
}
```

- `range.data_since` = **审计流水**（三张 `audit_*`）最早的一天；没有任何审计行 → `null`。
  它刻意**不含** `session_catalog`：回填把会话追溯到安装之前，而 token / 调用计数不行——
  这个差值正是「更早的日子只有会话计数」那句提示的依据（消费方只读这个字段，不自己算）。
- `since=all` = **有记录的第一天**（四张表的 `MIN`）；库完全为空时收敛到 `until`（仍是稠密的一行）。
`since=all` 取 `min(最早一天, until)`：时钟回拨 / 向西跨时区留下的「记录时刻晚于今天」的行
不会把这条逃生路径变成 400。显式 `since` 与缺省窗口按请求原样收敛（`day` 的行数 = 跨度天数）。
- **`day` 是稠密的**：窗口内每天都有一行，空天全 0——热力图直接画，客户端不做日历数学。
- **`origin` 恰好两行**，顺序固定 `me`（根会话）、`agents`（有父的会话）。
- **`totals` 永远由独立的全量查询算出**（不依赖被 `limit` 截断的行），是唯一的对账基准；
  行和 ≠ totals 只发生在「不可归因」（见 §3 表）、「被截断」与 `day` 的 `sessions`（去重计数不可加）
  三种情形。
- **同一响应内的两次查询不是同一个快照**：`rows` 与 `totals` 分别读库（只读路径不开事务），
  两次之间恰好有一笔真实调用落库时，TOTAL 会比明细多/少 1。
- 给了 `session_id` 时**自身那一行恒存在**（没活动就全 0，且不参与 `limit` 截断——它是
  作用域的锚点）；子孙行只出有活动的。
- **不水合会话**：整条路径只读库，不 resume、不建会话、不发事件。
- **库降级**（`wing.db` 打不开）：聚合全零、行仍稠密、端点照常 200；带 `session_id` 也**不**判
  404——「读不到」不是「不存在」。
- 状态码：语义非法（未知 `group_by` / 日期格式 / `since > until` / `tree` 没配 `session_id` /
  会话 id 词法非法）→ **400**；未知会话（目录里没有**且**审计表也没有它的任何行）→ **404**；
  `limit` 越界 → **422**（FastAPI 约束）；空数据永远 **200**。

## 3. 11 个度量与可归因性

| 度量 | 定义 | 边界 |
|---|---|---|
| `sessions` | 桶内有 ≥1 条审计事实（LLM / 工具 / 用户消息）的**不同**会话数 | 空会话不计（TUI 启动即建的空会话不是「用过」）；**跨天不可加**（同一会话两天各算 1，合计仍算 1） |
| `sessions_created` | `session_catalog.created_at` 落在桶内的会话数 | 含回填的历史会话（精度见 §5） |
| `user_messages` | `user_message_accepted` 事件数 | 含编排方投递与唤醒信封——**不是**「我亲手打的字」；比 `role=user` 消息多（steer 注入 / 一轮合并输入逐条计） |
| `llm_calls` | `audit_llm_call` 行数 | 一次「带 usage 帧的调用」；重试重放会多算（与账本同口径） |
| `tool_calls` | `audit_tool_call` 行数 | 只含**真实执行**：中断合成结果 / 参数非法 / 未知工具两条短路**不落行** |
| `tool_errors` | `success = 0` 的行数 | 只覆盖工具抛错 / 超时；`Bash` 非零退出码也算（success = 退出码 0），所以 `grep` 无匹配也算失败 |
| `prompt_tokens` | `SUM(prompt_tokens)` | 含缓存命中部分（`cached_tokens` 是它的子集）；压缩调用不在内（见 §6） |
| `completion_tokens` | `SUM(completion_tokens)` | 同上 |
| `cached_tokens` | `SUM(cached_tokens)` | 实际命中；命中率 = `cached_tokens / prompt_tokens`（消费方算，线格式不含派生值） |
| `llm_time_ms` | `SUM(audit_llm_call.duration_ms)` | 从发起到 usage 帧 |
| `tool_time_ms` | `SUM(audit_tool_call.duration_ms)` | 并行工具各算各的 → `llm + tool` 是工时**上界** |

**可归因性**（不可归因的度量填 0，不是「测得 0」）：

| group_by | 不可归因（恒 0） | 原因 |
|---|---|---|
| `model` | `tool_calls` / `tool_errors` / `tool_time_ms` / `user_messages` / `sessions_created` | `audit_tool_call` 与 `audit_user_message` 没有 model 列 |
| `tool` | `llm_calls` / `prompt_tokens` / `completion_tokens` / `cached_tokens` / `llm_time_ms` / `sessions_created` | `audit_llm_call` 没有工具名列 |
| 其余 5 个维度 | — | 全部 11 个都可归因（`day` / `session` / `origin` / `workspace` / `agent`） |

边界：**没有会话归属的审计行只进 `totals`**（`audit_llm_call.session_id` 允许 NULL）。两条发射点
都强制带会话，因此今天只可能是手改库造成的；那时 `origin` / `session` / `workspace` / `agent`
的行和会比 `totals` 少这几行。

`model` 的 key 是 **`<provider>/<model>` 复合键**（provider 名不含 `/`）：同名跨 provider 真实
存在（同一个调用名被两个 provider 声明），只按调用名合并会算错。`label` = 配置里的展示名
（`(provider, 调用名)` → 声明 → `display_name`），解析不出为 `null`（消费方回落 key 或调用名）。

## 4. 口径细节

### 4.1 「一天」= 网关本地时区的 00:00–24:00

按 `substr(ts, 1, 10)` 分桶：`ts` 是写盘机**本地墙钟的冻结值**（`datetime.now().isoformat()`）。
**不用** `date(ts_ms, 'unixepoch', 'localtime')`——它随**读端** `TZ` 漂移（实测同一份库在
UTC 与 CST 下每天差几十行），且两种写法都不可索引。代价是跨时区 / 跨机器的历史按**当时本地
时间**归属，不做归一。

窗口过滤同时给两个谓词：`ts` 文本判定归属（精确、与分桶同口径）+ `ts_ms` 的范围包络
（走 `idx_audit_*_ts`，故意放宽 ±2 天）——读写两端时区不同或遇 DST 时，窄的 `ts_ms` 边界会
切掉窗口边缘的行。

### 4.2 根会话 / 子会话（`origin`）

`session_catalog.parent_session_id IS NULL` = **根会话**（`me`），否则 = **子会话**（`agents`）。
口径诚实性：血缘是**软机制**（见 [session-lineage.md](session-lineage.md)），绕过面存在
（HTTP 直调不带 parent、`env -u WING_SESSION_ID`），因此文案写「根 / 子会话」，不写「人 / 机器」。
两条例外：**目录里没有的会话按 `me` 计**（不知道就当是自己开的）；**fork 出来的会话是根**
（fork 只写 `forked_from`，不写 parent）。

### 4.3 会话作用域与树

`session_id` + `tree=true` 用递归 CTE 取子孙闭包（`UNION` 而非 `UNION ALL`：血缘里出现环时
递归自然终止）。`tree` 不配 `session_id` 是 400——「全库 + 子孙」没有意义。

### 4.4 排序与截断

`day` 升序；`model` / `session` / `workspace` / `agent` 按 `prompt_tokens + completion_tokens`
降序；`tool` 按 `tool_calls` 降序；**并列一律按 key 升序**（全序：不随库内顺序漂移）。
`truncated` 只对排行类维度有意义（行被 `limit` 切断时为 true）。

## 5. 历史起点与回填

- **token / 调用统计从审计上线（`wing.db` v1 建库）起算**：本期**不回填**历史 token 流水
  （旧的 `metrics.json` / `history.jsonl` 导入是一条可选项，不在本期）。
- **计数类指标从第一天就是完整的**：`session_catalog` 的启动回填补齐磁盘上的存量会话。
  创建时刻按可靠性三档标注 `created_source`：

| `created_source` | 来源 | 精度 | 覆盖（本机实测） |
|---|---|---|---|
| `create` | 创建时直接写 | 毫秒（实况） | 新会话 |
| `id` | 会话 id 前缀 `YYYYMMDD-HHMMSS` | 秒 | 自带时间戳 id 的会话（688/690） |
| `log` | `history.jsonl` 首条记录的 `ts` | 毫秒 | 有历史、id 不自带时间的会话 |
| `meta` | `metadata.last_interaction` | 微秒，但语义是「最后一次互动」 | 兜底 |

三档都拿不到就**跳过**（不发明时间，下一次回填再试）。**写**是差集语义（只补「磁盘上有、
目录里没有」的会话），**读**却是全目录扫描（每个会话一次 `metadata.json`；本机 1000 会话
≈ 40ms）——因此在 `BackgroundScheduler` 上首轮启动即跑、此后每天一轮：周期跑等于一次自愈
（运行期被拷进来的会话目录也能进目录），代价是每天几十毫秒的后台线程工作。归档区也扫，
父 / fork 源从归档目录自己的 `metadata.json` 读（归档索引不存这两列）。

## 6. 与其它账本的口径差异（**不是 bug**）

| 对照 | 差异 | 判定方法 |
|---|---|---|
| `wing cache`（`GET /api/session/cache`） | 那边是**每会话最近 ≤200 次调用**的滚动窗口 + **理论命中率**（若 provider 满兑现能做到的水位）+ 分叉归因；这里是 lifetime / 全库的**实际**命中率。窗口不同 → 数字必然不同 | 见 [cache-accounting.md](cache-accounting.md)；本期**不报**理论命中率 |
| `SessionUsage`（`metadata.usage`，`info` / 状态栏 / 退出摘要） | 同源事件、**独立账本**：那边节流 10s 落盘（非优雅退出丢最后一个窗口），这里是逐事件追加；memory 后端不落盘时那边归零、这里照留 | 差额 = 节流丢失窗口 + 建账前的历史。**权威性**：全局数字以 `audit_*` 为准，`SessionUsage` 是单会话账本，两者互不校验 |
| 审计 vs `cache_call` | 压缩调用**不进** `audit_llm_call`（不发 metrics 事件），但进 `cache_call`（`purpose=compact`）→ `Σcache_call.prompt ≠ Σaudit_llm_call.prompt_tokens` | 跨源对账必然在这一处差一截 |

## 7. 增长与保留

- 行宽实测 ≈125 B（含索引）；重度用户（2500 次 LLM + 2800 次工具调用/天）≈ 1.3 MB/天、
  一年 ≈ 400 MB。**本期不裁剪**，也不做写时聚合（历史教训：`MetricsRegistry` 死在写放大上）。
- 查询只读窗口（`ts_ms` 索引 + 稠密化在 Python 侧），不随库增长而线性变慢。
- 逃生路径（v2 再上）：① 按天 / 按模型的**日汇总表 + 原始行保留期**；② 手工重建 `wing.db`
  （丢历史明细）。回退二进制前要先移走 `wing.db*`（`user_version` 只增不减，见
  [cache-accounting.md](cache-accounting.md) §6）。

## 8. 已知缺口

| 缺口 | 影响 | 修复路径 |
|---|---|---|
| **自动压缩不可观测** | 双击阈值的后台压缩**不发事件**（`audit_compaction` 只覆盖手动压缩，本机 0 行），所以压缩次数/成本不进任何度量；压缩调用的 token 也不进 `prompt_tokens` | 给自动压缩补一条审计事实（不在本期） |
| `tool_errors` 低估 | 参数非法 / 未知工具两条短路不落行 → 真实失败率比读数更高；`Bash` 的 success 是「命令成功」不是「用户满意」 | 短路也记一行（会改变既有审计语义，需单独评估） |
| `user_messages` 含投递 | 编排队列投递、唤醒信封都算「被接受的用户输入」 | 口径声明；不做人 / 机器分类（没有 actor 字段） |
| `sessions` 跨天不可加 | 行和 ≠ totals 的一个正当来源（`day` 维度） | 文档声明；`totals.sessions` 才是窗口内去重数 |

## 9. 排错

| 症状 | 先看哪里 |
|---|---|
| 今天的格子是 0，但明明跑过对话 | `SELECT * FROM audit_llm_call ORDER BY id DESC LIMIT 5` 有没有今天的行；`wing.db` 是否降级（`core/logs` 里一行 `wing.db disabled`）；`ts` 与 `ts_ms` 是否同一时区口径 |
| `sessions_created` 全是 0 | `SELECT count(*) FROM session_catalog`：为 0 说明回填没跑（网关启动后第一个 tick）或库降级；`created_source` 缺失档位说明会话目录里没有可用时间 |
| `origin` 全是 `me` | 子会话创建时没带 `parent_session_id`（软机制，CLI 从 `WING_SESSION_ID` 填）；`SELECT session_id, parent_session_id FROM session_catalog` 里有没有非空 |
| 端点 404 但会话明明存在 | 404 判据是「目录里没有**且**审计表没有」。本特性之后新建的会话在创建时**同步**写目录行，不会命中这一档；命中的只有**存量**会话里「id 不自带时间戳 + 没有 history + 没有 `last_interaction`」的那批（回填跳过了它们）。`SELECT count(*) FROM session_catalog WHERE session_id = '<id>'` 一看即知 |
| 与服务端 `info` 的 usage 对不上 | 见 §6 第三行（两条账本，口径不同） |

## 10. CLI（`wing usage` 与 `wing info` 的用量块）

实现落点：`crates/wing/src/cmd/usage.rs`（命令面 + 仪表盘 + 各维度明细）、
`crates/wing/src/render/heatmap.rs`（热力图纯渲染）、
`crates/wing/src/cmd/ps.rs::format_usage_block`（`info` 的块）、
`crates/wing-api-client/src/{models.rs,client.rs}`（DTO + `get_usage`）；
整机证据 `crates/wing/tests/usage_e2e.rs`（假网关逐字钉请求路径与输出）。

```
wing usage [--since WHEN] [--until WHEN] [--by DIM] [--metric M]
           [--session SID] [--tree] [--limit N] [--json] [-w]
```

| 参数 | 取值 | 说明 |
|---|---|---|
| `--since` | `all` \| `today` \| `yesterday` \| `7d` \| `4w` \| `3m` \| `1y` \| `YYYY-MM-DD` | 缺省 `12w`（= 84 天，与端点缺省窗口同宽） |
| `--until` | 同上（`all` 非法） | 缺省 `today` |
| `--by` | `day` \| `model` \| `session` \| `tool` \| `workspace` \| `agent` \| `origin` | 缺省 = 仪表盘 |
| `--metric` | `tokens` \| `messages` \| `calls` \| `tools` \| `sessions` | 只作用于仪表盘的热力图 |
| `--limit` | `1..=200` | 只作用于排行类维度（`day` / `origin` 不受影响） |

**时间的解释方**：时长写法相对**本地今天**往回减（`7d` = 今天往前 7 天），天数换算
`w=7` / `m=30` / `y=365`；CLI 把人类写法翻成网关本地日期后再发请求，服务端只认
`YYYY-MM-DD` 与字面量 `all`。**参数闸门在连网关之前**（非法值不拉起守护进程）。

**仪表盘** = 三次并发请求（`group_by=day` / `origin` / `model&limit=5`）拼五段：
标题（范围 + 周数）、热力图、`active days · streak · longest`、`TODAY`（仅当今天在窗口内
**且**今天有活动）、`WHO`（`me` / `agents` + `total`）与 `MODEL` 两张表。
`range.data_since` 晚于 `range.since` 时打一行
`token / call audit starts …; earlier days only have session counts`（字段缺席 = 老网关，不打印）。

**热力图**：列 = ISO 周（周一起），行 = 周一..周日；档位取**非零日的四分位**（相对量，
GitHub 同款思想），0 档在范围内画 `·`、范围外画空格。有颜色时格子统一 `█`，档位由
`surface → accent` 的 4 档明度表达；无颜色（pipe / `NO_COLOR` / `TERM=dumb`）时退化成
`·░▒▓█`。宽度不够只画最近 N 周并在标题注明 `(last N shown)`。绝对值永远从
`--by day` 或 `--json` 读——热力图的格子表达的是「相对忙」。

**`--json`** 只输出**一次**端点响应（单行，`print_json_compact`）：有 `--by` 是那一次，
没有是 `group_by=day` 那次。不发明本地包封结构。

**`wing info <sid>`** 的 `usage:` 块与 `wing usage --session <sid>` 同源
（一次 `since=all&group_by=session&session_id=<sid>&tree=true&limit=200`）：自身行是前四行，
其余行求和是 `tree:` 行（行数 = 1 时不打印）。取不到端点 → 整块静默省略；流水全 0 →
一行 `usage:  —  (no audit flow; ledger: …)` 回落到会话账本（`SessionInfoResponse.usage`），
**不写一排 0**（`0` 是一个读数）。

### 10.1 与 `wing cache` / 会话账本的读数为什么不同

同一份数据、三种窗口，见 §6。`wing usage` 与 `wing info` 的用量块读的都是**审计流水**
（lifetime、全库口径），所以这两处数字永远一致；它们与 TUI 状态栏 / 退出摘要
（会话账本 `metadata.usage`）、`wing cache`（每会话最近 ≤200 次调用的滚动窗口 + 理论命中率）
不同是设计使然。

### 10.2 排错

| 症状 | 先看哪里 |
|---|---|
| `wing usage` 说 `does not serve /api/usage` | 运行中的网关是本特性之前构建的：`wing stop && wing start` |
| 热力图几乎全是 `·`，但明明有会话 | 审计流水从 `wing.db` 建库那天起——看输出的 `token / call audit starts …` 那一行；`--since all` 会拉长窗口但补不出更早的读数 |
| `TODAY` 那一行不见了 | 今天在窗口内**且有活动**时才打；今天没干活就不打（避免一排 0） |
| `--by session` 的 TITLE / WHO 是 `—` | 那个会话不在 `/api/session/list` 里（已归档 / 已删除）：id 仍可拷贝，展示素材不猜 |
| `--session` 报 `session not found` | 404 = 目录与审计表都没有这个 id（见 §9 最后两行） |
