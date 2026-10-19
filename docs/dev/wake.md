# 会话唤醒（wake）：订阅收口 / 提问、定时心跳与内置信封

> 本页是这条特性的**产品语义参照点**：什么情况会叫醒谁、时间怎么算、信封长什么样、
> 边界在哪。实现落点：后端 `libs/core/wing/wake/`（L3）+ `gateway/routes/wake.py` +
> `wake_trigger` 表（`wing/db` migration v2）；前端 `crates/wing/src/cmd/{watch,wake}.rs`
> 与 `wing run --notify*`。

## 1. 产品语义

`wing run` 派出去的子会话做完就没人知道了：要么 `wing wait` 阻塞，要么人工轮询。唤醒是
**第三种姿势**——「登记一次，然后干别的」：某件事发生时，把一条**普通用户消息**投进我关心
的那个会话（开着 TUI 的会话会把它渲染成用户消息，因为投递带 `request_id`）。

一件事 = 一条**触发器**（`wake_trigger` 行），两种 `kind`：

| kind | 由什么驱动 | 订阅/排期字段 |
|---|---|---|
| `session` | 事件：source 会话收口（`TurnResultEvent`）/ 提问（`AskEvent`） | `source_session_id` + `on_events` |
| `timer` | 时间：`spec` 算出的 `next_fire_at_ms` | `spec` + `next_fire_at_ms` |

| 情形 | 行为 |
|---|---|
| source 收口（`TurnResultEvent`，任意 subtype） | 唤醒 target；done 配额 −1；信封带 subtype / 轮数 / 耗时 / 最后回复 |
| source 提问（`AskEvent`） | 唤醒 target；**不消耗** done 配额；信封带 `tool_call_id` + 问题与选项 |
| source 被 interrupt | **不触发**（中断不发射 `turn_result`，既有口径） |
| done 信封送达且配额归零 | 整条触发器立刻终结（`state=done`），不再留 ask 空转 |
| target 忙碌（working / waiting） | 消息进 inbox（drain-and-merge），不打断、不丢 |
| target 已逐出 | 投递自带水合（`ensure_loaded`），无需特殊处理 |
| target 已归档 / 不存在 | `state=failed` + 一行 warn；不重试、不影响 source |
| source 或 target 被归档 | 该会话相关的触发器一并 `canceled`（显式归档与自动归档都算） |
| 创建期校验 | source / target 都必须可解析（不水合）；`target == source` → 400；一次性 spec 带 `times` → 400 |
| 防环 | **不做**（用户明确决定）：A 订阅 B、B 订阅 A 会互相唤醒，这是使用者的事 |
| 保留期 | **不做**清理（先观察体积） |

**同一订阅重复登记 = 刷新，不是新增**：`kind=session` 的触发器以 `(source, target, on_events)`
为身份。已存在同三元的 `active` 订阅时，`POST /api/wake/create` **不新增行**，而是把那条刷成
「下一次完成时告诉我」（保留 `id` 与 `fire_count`，配额与 `prompt` 取本次请求的值，清掉
`last_error`）。理由：一条 active 订阅表达的是**状态**，不是队列——多插一行会让同一个提问 /
同一个收口被投递多次（`ask` 从不消耗配额，这个放大在整个提问期间都成立）；父会话若照着两条
信封各答一次，第二次答复已经找不到 waiter，会掉进子会话 inbox 变成一条普通用户消息（凭空多
起一轮）。同理，`wing run` 在**回答 ask**（带 `--tool-call-id`）时不再登记 notify：派发时那条
触发器仍在武装，本来就够，输出里会写明 `notify: off (answering an ask: ...)`。

**done 缺省配额是 1**：`wing watch <SID>` 的语义是「它下次收口时叫我」，叫完这条就完成
了。要盯多次用 `--times N`；只盯提问用 `--on ask`（配额永远不被消耗，等于常驻订阅）。

## 2. 时间语义（`kind=timer`）

| CLI | spec（存库字符串） | 次数 | 首轮 |
|---|---|---|---|
| `--in 30m` | `in:30m` | 1 | now+30m |
| `--at "2026-03-01 09:00"` | `at:<local iso>` | 1 | 该时刻（已在过去 → 下一 tick 立即触发） |
| `--every 30m [--times N]` | `every:30m` | N（缺省 ∞） | now+30m（不按墙钟对齐） |
| `--daily 07:00 [--times N]` | `daily:07:00` | N（缺省 ∞） | 下一个本地 07:00（已过则明天） |

- 时长语法：`30s` / `10m` / `2h` / `1d`，可组合 `1h30m`。CLI 与网关**各自**校验一次
  （手误在本机就报，不必等一次网关往返）。
- 时区：网关宿主本地时区；库里 `next_fire_at_ms` 存 epoch 毫秒（与时区无关），
  wire 上投影成本地 naive ISO（与 `last_turn_end` 同口径）。
- **夏令时**：`daily` 按本地墙钟的 `HH:MM` 解析（`datetime.replace` + `timestamp()`），
  于是跳变日会吃一次形变——春季 02:00→03:00 那天 `daily:02:30` 落在本地 `03:30`，
  秋季回拨日的 `daily:01:30` 只在第一次出现（下次是次日 01:30，相隔 25h）。行为可接受
  （不崩、不重复、不连发），一年两次、且只影响落在跳变窗口里的墙钟时刻。
  `every` 是**绝对间隔**（`1d` = 86400 秒），不受夏令时影响。
- **推进一律从 now 起算**：`every` 的下一次 = 触发时刻 + 间隔，`daily` 的下一次 = now
  之后的下一个本地 HH:MM。慢一拍、机器休眠、进程重启都不会「补发一串」。
- `remaining_done` 是**剩余触发预算**，含义按 `kind` 分岔：`session` = 还能送几次 done
  信封；`timer` = 还能触发几次（`NULL` = 无限）。表结构只有这一个预算列。
- `--times` 只对周期型合法：`in` / `at` 本来就只触发一次。

## 3. 重启恢复（错过补偿）

排期只存在 `wing.db`：进程内状态全丢，恢复只能读库。网关启动时（调度循环的第一棒）
扫一次 `active AND kind='timer' AND next_fire_at_ms <= now`：

- `in` / `at`（一次性）→ **立刻补发一次**，然后 `done`。不补就永久丢了——停机窗口里的
  触发没有任何其它补偿方；
- `every` / `daily`（周期型）→ **不补发**，`next_fire_at = advance(now)`（从启动时刻重新
  对齐）。错过的 tick 一律不追。
- 多条 missed 也不会连发：一次性只有一次投递，周期型一次都不投。

`session` 型触发器不需要补偿：它订阅的是**未来**的事件，进程重启后照样收。

## 4. 内置信封

投递内容（`prompt` 为空时）：

```
[wing] session <sid> finished (subtype=<subtype>, turns=<n>, <dur>s)

Last message:
<最后一条 assistant 文本，截断 2000 字符；无正文写 "(no text)">

Inspect:   wing tail <sid>
Continue:  wing run -r <sid> -p "..."
```

```
[wing] session <sid> is waiting for your answer (tool_call_id=<id>)

Question:
- [<header>] <question>  (<option1> / <option2> ...)

Answer: wing run -r <sid> --tool-call-id <id> -p "<answer>"
```

legacy 单问题 ask（Bash 危险命令确认）没有 header，那一行退化成 `- <question>  (y / n / yolo)`。

```
[wing] scheduled wake (<spec>, #<n>)

<prompt 原文>
```

用户给了 prompt 的 `session` 型触发器 → 正文**原样发送**（不做任何模板替换）。`timer` 型的
prompt 是必填，它总是出现在 `#<n>` 出处行的下面。

一个例外（属于投递通道的既有语义，不是信封）：prompt 以 `/` 开头且命中已注册的 prompt
命令时，会话的入站路径会把它当命令展开（`sm._post` 的行为，与用户手打命令完全一致）。
写 `/status` 这样的 prompt 时，目标会话收到的会是展开后的文本。

## 5. HTTP（`routes/wake.py`，3 个）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/wake/create` | body：`kind`（`session`\|`timer`）、`target_session_id`、`source_session_id?`、`on_events?`、`spec?`、`prompt?`、`times?`；响应 `{wake: WakeInfo}` |
| GET | `/api/wake/list` | `?all=true` 含已终结；响应 `{wakes: [...]}` |
| POST | `/api/wake/cancel` | body `{id}`；响应 `{ok, canceled}` |

错误映射：会话 / id 不存在 → **404**；非法参数（kind / spec / on_events / 自指 / times
组合）→ **400**；`wing.db` 不可用（唤醒整体未武装）→ **503**。

`WakeInfo` 字段：`id, kind, target_session_id, source_session_id, on_events, spec,
prompt, state, fire_count, last_fire_at, last_error, created_at, next_fire_at`（后四个时间
是本地 naive ISO，`null` = 不适用）。

`cancel` 一个已终结的触发器 = `200 {ok: true, canceled: false}`（幂等 no-op，不是错误）。

## 6. CLI

```sh
# 会话型：它收口 / 提问时叫我
wing watch <SID> [--to <SID>] [--on done,ask] [--times N] [-p TEXT] [--json]
wing watch --list [--all] [--json]
wing watch --cancel <ID> [--json]

# 时间型：到点叫我
wing wake (--in 30m | --at "2026-03-01 09:00" | --every 30m | --daily 07:00) \
          -p TEXT [--times N] [--to <SID>] [--json]
wing wake --list [--all] [--json]
wing wake --cancel <ID> [--json]

# 派活时顺手登记（默认开）
wing run -p "..." [--no-notify] [--notify-to <SID>] [--notify-on done,ask] [--notify-prompt TEXT]
```

- **目标缺省 = `WING_SESSION_ID`**（agent 的 shell 里有，见
  [session-lineage.md](session-lineage.md)）。显式命令（`watch` / `wake`）两者皆无 →
  exit 1 且消息可行动：`no target session: set WING_SESSION_ID or pass --to <sid>`；
  `wing run` 的隐式默认两者皆无 → **静默关闭**，文本输出 `notify: off (no parent session)`。
- `wing run` 的 notify 注册在 **发 prompt 之前**（子会话极快收口也漏不了），send 失败时
  best-effort 取消刚注册的触发器；注册失败（旧网关 / 网络 / `--notify-to` 指向子会话本身
  被 400 拦下）只 warn，`run` 不失败（与血缘的软机制同款）。
- 改时间 / 改 prompt = 取消旧的再建一条（没有「编辑」语义）。
- **stdio 参数过滤器**（`stdio::filter_unknown_args`）：`main.rs` 在 clap 之前按原始 argv
  判 stdio 模式（出现 `-p` 就算），所以 `wing wake --in 30m -p ping` 也会走那层过滤器。
  它现在认识**全部子命令 flag**（同名以顶层为准），于是这类子命令专属 flag 不会被当成
  外部 SDK flag 丢弃；放错位置的已知 flag 由 clap 响亮拒绝，而不是静默变成另一次调用
  （`-p` 在顶层与子命令上都有，就是这条兼容面的由来）。

## 7. 后端结构（`libs/core/wing/wake/`，L3）

```
spec.py       spec 语法 + 时间推进（纯函数，无 I/O）
model.py      WakeKind / 状态常量 / on_events 解析 / WakeTrigger（行 ↔ 对象）
store.py      WakeStore —— wake_trigger 的 SQL（唯一 SQL 处；走 wing.db）
envelope.py   信封渲染（纯函数）
scheduler.py  WakeScheduler —— 单 asyncio task + 可注入时钟 + notify()
ports.py      WakePorts Protocol（session_exists / is_archived / deliver）
service.py    WakeService —— 校验 / 生命周期 / 事件分发 / 投递 / 重启恢复
```

两条结构性选择：

- **窄 ports 而非直接依赖 runtime**：`wake` 是 L3，投递必须走 `runtime.post`（L4）。组合根
  （`WingRuntime._RuntimeWakePorts`）把三个方法接上；`wake` 因此不 import `runtime` /
  `gateway`，也没有任何会话读写——全部生命周期能用假 ports 在 pytest 里确定性覆盖。
- **`install()` 与 `start()` 分开**：`install()`（订阅 EventBus、DB 不可用就不武装）由
  `WingRuntime.__init__` 调用（那里没有事件循环）；`start()`（起调度 task，第一棒是重启
  恢复）由 gateway lifespan 调用，与 `BackgroundScheduler` / `SessionReaper` 同层。

事件路径的硬约束：EventBus 的订阅者**同步执行**，因此 handler 只判类型 + `create_task`，
绝不在 emit 调用栈里投递——source 的收口记录先落地，target 的一整轮 LLM 落在下一个 tick。

## 8. 归档联动

`WingRuntime.archive_session` / `archive_stale_sessions` 成功后调
`wake.cancel_for_session(sid)`（source 或 target 命中即 `canceled`）。联动只能发生在
`runtime.py`（L4）：`session_manager` 不反向依赖 `wake`。

## 9. 已知边界与取舍

- **TUI 不展示**待唤醒列表（`wing watch --list` 是唯一的查看口）；
- **无防环**：互相订阅会互相唤醒，日志里能看到事实，不做拦截；
- **无清理 / 保留期**：`wake_trigger` 只增不删（先观察体积）；
- **DB 不可用 = 唤醒整体不武装**：`wing.db` 打不开时 `install()` 只留一行 ERROR，创建
  回 503，列表为空，网关照常跑（与审计同款降级姿态）；
- **stdio（`wing -p`）不加 notify**：它是阻塞式前端，调用方本来就在等；
- **`wing wait` 不变**：作为「我就要阻塞等」的显式选择保留；
- **一台机器一个网关**：定时触发没有「认领」语义（`due_timers` 是纯读），同一个
  `WING_HOME` 上跑两个网关进程时，两条进程都会投递同一条到点的 timer 触发。部署假设
  与 `AGENTS.md` 的 worktree 纪律一致（本机只应存在一个网关）；
- **被 stall 的触发器在 `--list` 里仍显示 `active`**（状态机对外仍是封闭的四值，没有
  "stalled"）：看到 `NEXT` 在过去、日志里有 `wake: #N stalled` 就是它，`--cancel` 能把它清掉；
- **先投递、后记账**：投递链当前全程同步（没有挂起点），所以"读到写入"之间没有交错；
  将来若投递链里出现 await（锁 / 远程 target / await 式水合），预算就可能被超发一条
  （记账原子化只解决"永不终结"，不解决"多投一条"）；
- **记账写失败 = 本进程不再重试**：`wing.db` 的写是"失败即静默返回 `False`"的契约，
  投递成功但记账没落库时，那条触发器会被撤出调度（一行 ERROR，本进程内不再触发），
  而不是每 10ms 重投同一条消息；重启后按库里的旧排期重新判断。
