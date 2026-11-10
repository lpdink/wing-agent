# 上下文压缩（compaction）

两条路径、一套口径：

- **自动（early）压缩**：上下文涨到 `context_window_tokens - keep_recent_tokens` 时，
  后台算一份摘要（pending），到 `context_window_tokens` 时把它换入消息链；
- **手动压缩**（`/compact` / `wing compact` / `POST /api/session/compact`）：**全量**压缩
  ——把操作前的整个上下文窗口换成一条摘要，可带一段"保留什么"的侧重指令。
  它是**会话级独占操作**：与 turn 互斥、期间不处理任何输入、可被 interrupt 取消。

代码：`libs/core/wing/context/compaction.py`（阈值 / 切点 / 摘要 prompt）·
`context/manager.py`（pending 状态机 / 换入 / 链改写）· `agent/core.py`（手动路径的准入与
互斥）· `agent/inbox.py`（取出口闸门）。

## 1. token 口径：服务端 usage 优先，估算是兜底

`resolve_context_tokens(messages, server_tokens)` 是**唯一**口径：一次请求组装时，最后一条
可信 assistant 的 `usage.prompt_tokens` 就是"上下文有多大"（服务端说了算），没有才退回
`TokenCounter`（CJK 0.7 / 非 CJK 0.3 的本地估算）。阈值判定、`/api/session/info` 的
`context_stats`、自动压缩的切点全部走它——否则会出现"面板显示没超、却触发了压缩"。

**可信**的范围由链上的 `usage_boundary_uuid` 界定（`ContextManager._usage_trust_start`）：
压缩换入 / 手动压缩 / rewind 都会重置窗口，重置之前产生的 usage 度量的是**已经不存在的
窗口**（保留区整条复制时 `usage` 随行，是审计事实，不是当前大小）。读法是"所有可解析
边界里最靠后的那个起点"，边界悬空（被 rewind 剪断 / 日志损坏）则**一条都不采信**、退回
估算并留 WARNING。

已知边界：`TokenCounter.estimate_message` 只估算 message（不含 system prompt 与 tools
声明），而阈值是整个请求的窗口——两边混用会系统性低估。这是"切点必须锚在服务端 usage
上"的直接原因（见下）。

## 2. 自动压缩的切点

`select_cut(messages, trust_start, total_tokens, keep_recent_tokens)`：

- **候选位置** = 上下文大小已知、且不破坏工具配对的下标：`user` 消息（新 turn 起点）与
  `assistant` 消息（应答位置 / tool 结果之后的续跑）。`tool` / `system` 永远不是候选——
  切在 tool_result 之前会让它变成孤儿（两条协议都要求它紧跟对应的 tool_call）。
  工具调用对因此**构造性闭合**，不需要"先切再向后扩展补闭合"的修补。
- **该位置的大小** `p(c)` = c 及其之后第一条有 usage 的 assistant 的 `prompt_tokens`
  （c 在最后一条 assistant 之后时就是 `total` 本身——末尾那条还没被回答的 user 消息）。
- **选点**：取最早一个满足 `p(c) >= total - keep_recent` 的候选。`total - p(c)` 就是 c
  之后那段上下文的大小（system 与 tools 在差值里抵消），保留区因此"尽量大、但不超过
  `keep_recent`"。`user` 边界与它后面那条 assistant 的 `p` 相同，所以"整轮保留"永远优先
  于"保留答案、把问题压进摘要"——后者只在轮首消息无法作为切点（窗口第一条）时发生。
- **兜底**：整条信任区间都没有可信 usage（新会话 / provider 不回 usage / 边界悬空）时，
  退回本地估算的 `_calc_cut_idx` + `_find_safe_cut_idx`——与"触发也用估算"同口径，不混
  单位；这条路径仍带 system + tools 的低估偏差，是可以接受的降级（真实形态下它只在
  完全拿不到服务端数字时出现）。

切点算好之后就固化成 `PendingCompact`（`start_uuid` / `end_uuid` + 摘要正文），落
`history.jsonl` 的 aux kv 通道（`pending_compact`）。换入时按 uuid 在当前链上自校验：找不到
（rewind / 手动压缩改过链）就丢弃，不半提交。

**换入**（`_apply_pending_compact`）：摘要节点取代 `msgs[:end_idx+1]`，`unzip_last_uuid`
编码被压缩区间；保留区（tail）**整条复制**、只换链坐标（`uuid` / `parent_uuid`）——上下文
事实与 provider 审计字段（`usage` / `stop_reason`）、媒体引用都随行。写入顺序是
`append_detached(摘要) → append_detached(tail…) → set_tip(tail 末条)`，中间的崩溃窗口是
已知问题（`TODO(crash-safety)`，与手动路径同源）。

## 3. 手动压缩

预期行为：**全量**。取操作前的整个窗口（`get_context_window()`）走一次 LLM 摘要，结果成为
新链的根（`unzip_last_uuid` = 操作前链末消息），保留区为空。侧重指令走
`do_compact(instruction=…)`：条件渲染在摘要主体与格式约束（`<summary>` 标签 + CRITICAL
"禁调工具"）之间；没有指令时 prompt 与默认策略逐字节一致。

### 准入（`WingAgent.compact`）

| 情形 | 结果 |
|------|------|
| 会话未水合（不在内存） | 404 `session not found`（CLI 走"404 → resume → 重试一次"） |
| 在跑 turn / 等用户回答 ask（`status != idle`） | 409 `SessionBusyError`，先 `interrupt` |
| inbox 里有已投递、未被取走的输入 | 409，同上（那也是一轮即将开始的 turn） |
| 已有手动压缩在飞 | 409 `compaction already in progress` |
| 压缩被 `interrupt` / `shutdown` 取消 | 409 `compaction interrupted` |

准入判据与"取出口闸门"之间没有 await：`status` / `has_pending` 检查与 `Inbox.hold()` 都是
同步调用，单线程事件循环下原子。

### 与 turn 的互斥：inbox 取出口闸门

`Inbox.hold()` / `release()` 是压缩期间**不把输入交给 worker** 的闸门：

- 压缩期间到达的消息**留在队列里**（`has_pending` 为真——逐出 / 归档据此钉住会话），
  worker 停在下一次 `get()` 的等锁上；
- `release()` 之后 worker 立刻拿到它、正常开轮——"消息进 inbox、压缩结束才开始处理"；
- 闸门边界上已经被取到手的条目原样放回**队首**（顺序不被打乱）；
- `status` 在压缩期间仍是 `idle`（压缩不是 turn）：`SessionManager._blocked_reason` 另加
  一条 `compacting` 判据，逐出 / 归档 / release 一律钉住。

### 可中断

压缩本体跑在独立 task 里（`WingAgent._run_compact`），`interrupt()` / `shutdown()` 取消它
并有界等待。压缩只在**最后一个 await 之后**写链（`append_detached` / `set_tip` 之间没有挂
起点），所以取消不会半提交：链零变更，aux 也没有残留。HTTP 侧靠
`task.cancelled()` + `current_task().cancelling()` 区分"子任务被取消"与"调用方自己被取消"
（后者是客户端断连，不该再写响应）。

### 前端（TUI）

- `/compact [侧重]`：`turn.working` / 有 pending 消息 / 已有压缩在飞 → 拒绝并 toast 原因；
  通过 → 置 `compacting`、弹出**持久** toast「Compacting context…」；
- 压缩期间**禁止发送**：`submit_message` 把整份提交产物（正文 + 图片 chip）原样还回草稿
  并提示（后端另有一层：消息会留在 inbox 直到压缩收口）；
- 回执收口：`CompactDone` → 解除拦截 + 「Compact done: A → B tokens」；失败 / 被拒
  （`CompactRefused`，409 的文案直接给"先中断"这类可行动结论）→ 解除拦截 + 错误 toast；
- 切会话 / 新建 / 恢复 / fork 时复位 `compacting`（在飞的是旧会话，回执会被会话守卫丢弃）。

`POST /api/session/compact` 的客户端超时必须单独放宽（`wing-api-client` 里 1300s）：client
级 60s 会让长压缩"客户端先失败、后端照样把链换掉"。

## 4. 测试面

- 单测：`libs/core/tests/test_compactor.py`（`select_cut` 的判据：轮首 / 续跑 / 信任边界 /
  末尾提问 / 工具配对的构造性闭合 / 无 usage 兜底）、`test_inbox_hold.py`（闸门阻塞 / 保序 /
  取消不丢输入 + agent 的准入与取消）、`test_compact_token_baseline.py`（口径边界）、
  `test_manual_compact.py`（链形状 / pending 丢弃 / 失败不半提交）。
- probe（整机红线，`libs/wing-probe/scenarios/`）：`test_auto_compact.py`（early → pending →
  apply、跨进程重启、口径不回落）、`test_compact.py`（手动压缩的链形状、指令渲染、失败零
  变更、**忙碌 409**、**手动丢弃 pending**、**压缩期间投递的消息排在压缩之后**、**interrupt
  取消压缩**）、`test_fork.py` / `test_rewind.py`（在保留区 / 回退点上的口径）。
- TUI：`crates/wing/src/app/tests/commands.rs`（忙碌拒绝 / 重复压缩拒绝 / 压缩中发送回填 /
  回执解除拦截）。
