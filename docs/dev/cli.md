# wing CLI（形态 · 命令表 · 编排脚本）

> 面向使用 `wing` 的人**与 agent**：四种形态怎么选、每个子命令干什么、编排脚本长什么样、
> 以及踩过的坑。命令全集以 `wing --help` 为准（本文与它不一致时以 `--help` 为准并欢迎修文档）。
>
> 机制细节不在这里：HTTP / WS 协议见 [http-api.md](http-api.md)，唤醒语义（触发器、时间推进、
> 内置信封）见 [wake.md](wake.md)，父子血缘与树序见 [session-lineage.md](session-lineage.md)，
> 概念速查见 [glossary.md](glossary.md)。

## 1. 四种形态

同一个 `wing` 二进制，按调用方式分成四种前端：

| 形态 | 怎么起 | 行为 | 典型用法 |
|---|---|---|---|
| **TUI** | `wing`（无子命令）/ `wing tui` | 交互式，human-in-the-loop；`--host` / `--port` 指网关，`--dump-config [--show-secrets]` 导出 TUI 配置 | 人坐在终端里干活 |
| **stdio** | `wing -p "<prompt>"` | **阻塞**到本轮结束；输出格式 `text` / `json` / `stream-json`（Claude Code 兼容 NDJSON，`wing` 可 alias 成 `claude`） | 外部编排器同步调用 |
| **ACP** | `wing acp` | stdio 上的 Agent Client Protocol v1 服务端，把每个 ACP session 桥到本机 wing session | Zed / omnigent 等 ACP 客户端 |
| **编排 CLI** | `wing run` / `wait` / `ps` / … | 一条命令一个动作、不进交互；全部支持 `--json` | agent 编排、脚本、CI |

两个全局旗标对所有子命令可用：`--json`（机器可读输出）、`-w/--watch`（每 2 秒刷新的显示模式，
目前只有 `wing ps` 消费它——**和 `wing watch` 子命令没有关系**）。

**模式判据与旗标过滤是两件事。** stdio 模式只看**顶层** `-p` / `--prompt`（或 `--input-format stream-json`）：
有它就派发 stdio，**连子命令一起忽略**（`wing -p "..." ps` 不会跑 `ps`）。写在子命令上的 `-p` 属于那个子命令
（`wing run -p …` / `wing watch -p …` 照常派发），不是 stdio。

而 `-p` / `--prompt`（或 `--input-format stream-json`）出现在 argv **任意位置**都会启用 stdio 的**参数过滤器**
（`main.rs` 在 clap 之前按原始 argv 判定）：不认识的长短旗标被静默丢掉（`--permission-mode bypassPermissions`
连它的值一起没），不报错、也没生效。所以：外部 SDK 的旗标塞进来不会炸，但子命令自己的旗标必须跟在子命令之后
——放错位置时，`--tag` / `--agent` / `--tool-call-id` / `--session-id` / `--include-partial-messages` 会被显式
拒绝，`--resume-session-at` 在任何形态下都直接拒（本 CLI 不支持会话截断），其余（如 `--tools`）可能被静默忽略。

## 2. 红线：不要用 `wing` 把自己的任务外包出去

`wing run` / `wing -p` 会把活交给一个**全新的、什么都不知道的**会话。除非用户明确授权：

- **你自己的任务自己做**——不要为了省事把当前任务转手给子 agent 然后等它交作业；
- **你是执行者时**：可以（且应该）派**一轮**独立 review，reviewer 只读、只交报告；
- **你是 reviewer 时**：不允许再派发任何子 agent——审查不能外包，报告也不能外包。

命令给的功率很大（子会话一律 `yolo`，危险命令不询问）：工具集、工作区边界、能派谁，都要在
prompt 里写死，不要靠子 agent 自觉。

## 3. 命令表

### 3.1 网关生命周期

| 命令 | 说明 |
|---|---|
| `wing start [--host H] [--port P]` | 后台起网关守护进程 |
| `wing stop` | 停网关 |
| `wing restart [--host H] [--port P]` | stop + start；没在跑也不报错 |
| `wing status` | 守护进程状态 |

`wing run` / `wing run -r` 会自动确保网关在跑（`ensure_gateway`），脚本里通常不必显式 `start`。
**一台机器只应有一个网关**（同一个 `$WING_HOME`）：定时唤醒没有"认领"语义，两个进程会重复投递。

### 3.2 派发与等待

| 命令 | 说明 |
|---|---|
| `wing run -p "<prompt>" [--json]` | **非阻塞**：建会话 + 发 prompt，立即返回 session id |
| `wing wait <sid...> [--timeout S] [--json]` | **阻塞**直到这些会话 idle / inactive（`--timeout` 上限，默认 600） |
| `wing watch <sid> [--times N] [--on done,ask] [-p TEXT] [--to SID]` | 登记「它收口 / 提问 → 唤醒我」 |
| `wing watch --list [--all]` / `wing watch --cancel <id>` | 列出 / 取消**全部**触发器（`KIND` 列区分 session / timer，不按 kind 过滤） |
| `wing wake (--in 30m \| --at "2026-03-01 09:00" \| --every 30m \| --daily 07:00) -p TEXT [--times N] [--to SID]` | 定时（心跳）唤醒 |
| `wing wake --list [--all]` / `wing wake --cancel <id>` | 同 `wing watch --list` / `--cancel`：**全部**触发器，两个子命令等价 |

`wing watch` 的缺省语义是**叫一次就结束**：一条 `done` 信封送达后触发器终结（配额 1）；`ask` 唤醒
**不消耗**配额，所以只盯提问时等于常驻订阅。要盯多次收口用 `--times N`。

同一 `(source, target, on_events)` 再登记一次是**刷新**那一条订阅（不新增、不产生第二条投递路径）：
`--times` / `-p` 不给就保持原值——所以 `wing run -r` 的隐式登记既不会把 `--times 3` 剩下的配额降成
1，也不会清掉自定义正文（细节见 [wake.md](wake.md) §1）。

`wing run` 的参数（都写在子命令之后）：

| 旗标 | 作用 |
|---|---|
| `-p/--prompt <TEXT>` | 必填：要执行的任务 |
| `-r/--resume <SID>` | 在既有会话上继续（保留上下文）。`-r` **忽略全部创建期旗标**：`--tools` / `-m` / `--agent` / `--effort` / `--system-prompt` / `--append-system-prompt` / `--max-turns` |
| `--tools "Bash,Read,Write,Edit,Grep,Glob"` | 覆盖工具集（创建期） |
| `--agent <NAME>` / `-m/--model <ID>` / `--effort <low\|medium\|high\|xhigh\|max>` | 覆盖模板 / 模型 / 推理强度（创建期） |
| `--system-prompt` / `--append-system-prompt` / `--max-turns` | 提示词与轮数上限（创建期） |
| `--tag <TAG>`（可重复 / 逗号分隔） | 创建即打标；`-r` 时为追加 |
| `--tool-call-id <ID>` | 定向回答一个在挂的 Ask（见 4.3） |
| `--no-notify` / `--notify-to <SID>` / `--notify-on done,ask` / `--notify-prompt <TEXT>` | 唤醒登记：默认**开**，目标 = `WING_SESSION_ID` |
| `--json` | 输出 `{session_id, parent_session_id, workspace, tags, notify…}` |

唤醒登记是**软机制**：没有 `WING_SESSION_ID`（不在 agent 的 shell 里）就静默关闭，文本输出
`notify: off (no parent session)`；**回答 ask（带 `--tool-call-id`）时不登记**，文本输出
`notify: off (answering an ask: the dispatch-time trigger is still armed)`——派发时那条订阅仍在
武装，本来就够。注册失败只 warn，`run` 不失败；`watch` / `wake` 的显式目标缺省同样是
`WING_SESSION_ID`，两边都没有 → exit 1 并给一句可行动的提示。

### 3.3 会话查询

| 命令 | 说明 |
|---|---|
| `wing ps [--all] [--tag T] [--parent SID] [--tree] [--json]` | 会话列表（默认过滤 inactive，被 `pin` 的除外；`--tag` / `--parent` 隐含 `--all`） |
| `wing info <sid> [--json]` | 运行时信息：状态 / workdir / 工具 / tokens / 模型 / 用量 |
| `wing cache [sid] [--limit N] [--why TS\|INDEX] [--json]` | KV 前缀缓存账本：理论 vs 实际命中率 + 分叉归因（省略 sid = 最近活跃的那个） |

### 3.4 消息

| 命令 | 说明 |
|---|---|
| `wing tail <sid> [-n N] [-t FILTER] [--json]` | 最后 N 条消息（默认 10） |
| `wing head <sid> [-n N] [-t FILTER] [--json]` | 最前 N 条消息 |

`-t/--filter` 取值：`all`（默认）/ `user` / `assistant` / `tool_call` / `tool_result` /
`reasoning` / `content`，可重复 / 逗号分隔（并集）。

### 3.5 会话控制

| 命令 | 说明 |
|---|---|
| `wing new [--workspace D] [--agent A] [--tag T]` | 建一个空会话并打印 id |
| `wing resume <sid>` | 把已逐出的会话读回网关内存（**不是**「带 prompt 续跑」——那是 `wing run -r`） |
| `wing release <sid...>` | 显式逐出内存态（忙碌 / 被订阅 / 非持久后端 → 409） |
| `wing archive <sid...>` / `--restore <sid...>` / `--list` | 归档（冷存储）/ 恢复 / 列归档区 |
| `wing fork <sid> --at <uuid\|current>` | 在消息节点分叉，新会话保留前缀；节点原文作为 `draft` 返回 |
| `wing rewind <sid> --to <uuid\|current>` | 回退到某节点之前；被切掉的消息作为 `draft` 返回 |
| `wing branches <sid>` | 列出可 fork / rewind 的节点（`fork` / `rewind` 要的 uuid 从这里来） |
| `wing asks <sid> [--wait N] [--json]` | 在挂的 Ask 及其 `tool_call_id`（在挂 Ask 不在链上，`tail` 看不到） |
| `wing interrupt <sid>`（别名 `wing int <sid>`） | 中断当前轮次（TUI 的 Esc）；半成品留在上下文里 |
| `wing compact <sid> [instruction]` | 手动压缩上下文，可给侧重指令 |
| `wing update <sid> [--model --agent --title --thinking --effort --yolo --tools --workspace]` | 只改你传的字段；`--agent` 会带上新模板的 thinking / effort / yolo |
| `wing tag <sid> [tags...] [--remove T] / wing tag --list` | 读写会话标签（幂等；`--list` 是全局清点） |

### 3.6 系统与配置

| 命令 | 说明 |
|---|---|
| `wing reload` | 热重载配置（config / hooks / commands / providers / skills & rules / 日志级别），逐项返回结果 |
| `wing models` / `wing tools` / `wing agents` | 列模型 / 工具 / 模板（排查"工具名写错"的第一站） |
| `wing config doctor\|list\|get\|set\|unset\|add\|remove\|move\|path` | 设置面读写（网关降级到 setup mode 时也能用；`path` 完全离线） |
| `wing acp [--agent A] [--model M]` | ACP 服务端（stdio） |
| `wing tui [--host H] [--port P] [--dump-config [--show-secrets]]` | TUI；`tui` 是默认子命令，可省略 |

## 4. 典型编排脚本

### 4.1 派发 → 不阻塞等 → 醒来读结果

```sh
cd /path/to/worktree                       # workspace = 进程 cwd，必须先 cd
SID=$(wing run --json \
        --tools "Bash,Read,Write,Edit,Grep,Glob" \
        --tag executor --tag task=my-task \
        -p "$(cat /tmp/executor_prompt.md)" | jq -r .session_id)

# ……干别的。子会话收口（或提问）时，我的会话收到一条普通用户消息：
#   [wing] session <SID> finished (subtype=success, turns=41, 3902.1s)
#   Last message: ……（最后一条 assistant 文本）
#   Inspect:   wing tail <SID>
#   Continue:  wing run -r <SID> -p "..."

wing tail "$SID" -n 20 -t content                  # 醒来先读最后回复
wing info "$SID" --json | jq '{status, model, tools}'
```

### 4.2 我就是要阻塞等

```sh
wing wait "$SID" --timeout 3600 --json \
  | jq '.results[] | {session_id, status, subtype, is_error}'
```

`--timeout` 只是上限（全部 idle 就立刻返回），且 wait 是幂等的——可以反复 wait、分批 wait。任一
会话 `is_error` → 退出码 1（**放在管道里**时先 `set -o pipefail`，否则 `$?` 读到的是 `jq` 的）。

### 4.3 回答子会话的提问（Ask）

```sh
wing asks "$SID" --wait 600 --json        # 等到一个在挂的 Ask（窗口内没有 → 非零退出）
# → {"session_id":"…","status":"waiting",
#    "asks":[{"tool_call_id":"call_abc","question":"Run rm -rf build?","choices":["y","n"],"required":true}]}

wing run -r "$SID" --tool-call-id call_abc -p "y"
```

### 4.4 定时自唤醒（心跳）

```sh
wing wake --in 15m -p "回来看一眼 CI"                        # 一次性
wing wake --every 30m -p "轮询队列；没有新活就只回一行 idle"   # 周期，从 now 起算
wing wake --daily 07:00 --times 7 -p "morning triage"        # 每天本地 07:00，共 7 次
wing wake --list                                            # 还活着的
wing wake --cancel 7
```

`--times` 只对周期型（`--every` / `--daily`）合法；`--in` / `--at` 本来就是一次性。改时间 / 改
prompt = 取消旧的再建一条（没有"编辑"语义）。

### 4.5 用 fork 克隆自己（上下文交接）

```sh
CLONE=$(wing fork "$WING_SESSION_ID" --at current --json | jq -r .session_id)
wing run -r "$CLONE" -p "你是被 fork 出来的执行者：上下文里的背景就是全部前情，现在去做 X"
```

克隆继承**那一刻**的整条链（`--at current`；也可以 `wing branches` 挑节点），比写长篇 prompt
可靠；代价是上下文更大、token 更贵，且父会话后续的改动不会同步过去。

## 5. 常见坑

1. **Bash 工具的 30s 默认 timeout 会杀掉 `wing wait`**：会话不受影响，但你白等一轮。显式给
   `--timeout` 与工具 timeout 同一量级（如 `--timeout 3600` + 工具 3700）。更省的办法是别 wait：
   用 `wing run` 的 notify 或 `wing watch`。
2. **忘了 `cd`**：session 的 workspace 就是 `wing run` 进程的 cwd。派发前 `cd <目标目录>`，
   事后用 `wing info <sid> --json | jq .workdir` 兜底核对。
3. **`-r` 忽略创建期旗标**：`--tools` / `-m` / `--agent` / `--effort` / `--system-prompt` /
   `--append-system-prompt` / `--max-turns` 在 resume 路径上都不生效（工具集、模型、模板沿用原会话）。
   换模板要 `wing update <sid> --agent <name>`。
4. **`wing resume` ≠ `wing run -r`**：前者只是把逐出会话读回内存，不发消息；续跑用后者。
5. **`-w/--watch` 不是 `wing watch`**：前者是全局的「每 2 秒刷新」显示旗标（目前只有 `wing ps`
   用），后者是唤醒订阅。
6. **`-p` 会打开参数过滤器（未知旗标被静默丢）**：`wing run --permission-mode bypassPermissions -p …`
   照常派发 `run`，但 `--permission-mode` 连同它的值被当成外部 SDK 旗标丢掉——不报错、也不生效。
   子命令自己的旗标务必写在子命令之后。（顶层的 `-p` 是另一回事：那会走 stdio 并忽略子命令，见 §1。）
7. **notify 没有目标是正常的**：root shell 里没有 `WING_SESSION_ID` → 静默关闭，需要
   `--notify-to <sid>` 或显式 `wing watch`。
8. **source == target 会被 400 拒**：不能登记「它收口时叫醒它自己」；也**不做防环**（A 订阅 B、
   B 订阅 A 会互相唤醒，这是使用者的事）。
9. **归档会取消触发器**：source 或 target 进归档区 → 相关触发器 `canceled`；长期订阅别挂在会被
   自动归档（默认 90 天）的会话上，给它们打 `pin` 标签可豁免自动归档。
10. **stdio（`wing -p`）没有 notify**：它是阻塞式前端，调用方本来就在等。
11. **`wing cache` 省略 sid 会挑"最近活跃"**：脚本里永远显式给 sid，header 里也会打印它选中的是哪个。
