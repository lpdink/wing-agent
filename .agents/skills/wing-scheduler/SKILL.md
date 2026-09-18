---
name: wing-scheduler
description: 超大型任务的无人值守交付编排（scheduler 模式）。把一个大任务拆成步骤与波次，在独立 worktree 上并发派发 headless 子 agent（executor）执行、派独立子 agent（reviewer）以 B/S/N 分级审查，最后由 scheduler 集成收口、push 并产出单一 PR。当用户要求「实施/交付一个大型任务」「拆开并行做掉」「无人值守把这件事做完」「多子 agent 协同开发」，或任务明显需要多波次、多 worktree、跨模块大范围改动时使用。
category: Workflow
tags: [orchestration, scheduler, multi-agent, delivery, headless]
---

# Wing Scheduler — 超大型任务的全自动交付

一句话：**你（scheduler）负责想清楚、拆、派、审、集成、收口；子 agent 负责执行。需求澄清之后全程无人值守。**

本 skill 只依赖 `wing` CLI（`wing run` / `wait` / `ps` / `info` / `tail` / `head`）与普通 shell / git 命令，不需要额外脚本或编排框架。

本文用 `$WING_HOME` 指 wing 的状态目录（默认 `~/.wing`，可用同名环境变量覆盖）：任务文档与 worktree 都放在它下面，与目标仓库解耦。

## 调度主循环（一页地图）

```
0. 需求澄清 → 写 proposal.md → 用户批准 → 初始化 scheduler_log.md          （§4）
1. 拆 DAG + lane 规划 + 建 worktree + 写步骤 proposal                      （§5）
2. while 还有未收敛的 step：
     a. 选出"依赖已就绪"的 step，按 lane 上限派发 executor（≤3 在飞）        （§6）
     b. wing wait 等它们，判读结果（idle / timeout / error 三种走法）        （§7）
     c. 交付达标的 step：写 review_request.md → 派 reviewer                  （§8）
     d. wing wait 等 reviewer → 读报告 → B/S 必修                            （§8.5）
     e. 返修循环（≤3 轮）→ 该 step 收敛，或判定阻塞并跳过下游                （§9）
     f. 该 lane 空闲 → 派这条 lane 的下一个 step（别的 lane 继续跑）
     g. 更新 scheduler_log + 向用户播报两行进度                              （§4.3 / §11）
3. 全部收敛 → 集成到单一分支 → 全量验收 → push / 开 PR                      （§10）
4. 写 summary.md → 向用户汇报结论                                           （§11）
```

三个必须同时守住的不变量：**在飞 ≤3**、**一个 worktree 一个 executor**、**每个 session 都登记**。

## 0. 适用与不适用

**适用**（满足任意两条即应考虑）：

- 跨模块 / 跨文件的大范围改动，单次对话上下文装不下；
- 能拆成 ≥3 个可独立验证的步骤，且步骤之间存在并行机会；
- 交付标准客观（测试 / 构建 / 可执行验证命令），不需要人类主观判断；
- 用户期望「我说完需求，你干完给我 PR」。

**不适用**：

- 小改动（一个文件、一次编辑）→ 直接自己做，不要开子 agent；
- 探索 / 调研 / 问答 → 自己查或派只读调查（`Explorer`）；
- 需求还在变、需要持续与人交互 → 先把需求澄清完，不要进入调度循环。

## 1. 角色

| 角色 | 是谁 | 工具集（写死） | 权限边界 |
|---|---|---|---|
| **scheduler** | 你，当前会话 | Bash / Read / Write / Edit / Glob / Grep（+ AskUserQuestion） | 唯一与用户对话；唯一能 push / 建 PR；负责任务文档、派发、审查组织、集成、收口 |
| **executor** | `wing run` 起的 headless session | `Bash,Read,Write,Edit,Grep,Glob` | 只在自己被分配的 worktree 内改代码；**必须清晰 commit**；禁止 push / 切分支 / rebase / 动别的 worktree |
| **reviewer** | `wing run` 起的 headless session | `Bash,Read,Glob,Grep,Write,Edit` | 只读被审 worktree（可跑测试）；唯一允许的写入是任务目录下的 review 报告（Write 起草、Edit 增量改） |

子 agent **一无所知**：不知道用户是谁、不知道任务背景、不知道别的步骤在做什么、不知道你的计划。它们的全部上下文 = 你写的 prompt + 你指给它的文件。**信息缺口是编排失败的第一大原因**。

> 由 `wing run` / `wing -p` 创建的 session 一律强制 `yolo=true`（危险命令不询问），这正是无人值守需要的；也意味着子 agent 的破坏力没有闸门——所以**工具集与工作区边界必须写死，不能靠子 agent 自觉**。

## 2. 铁律（违反任意一条，编排都会崩坏）

1. **并发 ≤ 3**：executor + reviewer 加起来，同一时刻最多 3 个在飞（你自己不算）。TPM 限流是硬约束，不是建议。
2. **同一个 worktree，同一时刻只能有一个 executor**：想并行 → 开多个 worktree（一条 lane 一个）。绝不让两个 executor 在同一目录里写代码。
3. **不设 `--max-turns`**：现代 agent 需要的 ReAct 轮数很高，设了会让任务中途夭折。
4. **不改模型与 effort**：用模板默认（不加 `-m` / `--provider` / `--effort`）。模型选择带来的收益远小于版本漂移与不可复现的风险。
5. **工具集写死**（见上表）：不要给子 agent 加 `TodoWrite` / `Explorer` / `AskUserQuestion`。
6. **子 agent 不问问题**：没有人类在线（`AskUserQuestion` 不在工具集里；即使问了也没人答，会白挂 100 分钟）。契约里必须明确「遇到不确定自己决策，并把假设写进 design.md 与最终消息」。
7. **push / 建 PR 只能你做**：executor 只 commit（commit message 要写清楚，reviewer 靠它取信息）。
8. **任务文档一律放 `$WING_HOME/tasks/<task>/`，用绝对路径引用**：绝不写进 worktree（会污染 diff 与 review 范围）。任务目录在 worktree 之外是刻意设计。
9. **先记录 base commit，再派发**：没有 base 就没有 review 范围（`<base>..HEAD`）。
10. **每个 session 都要登记**：派发即写进 `scheduler_log.md`（sid / 角色 / step / lane）。任务一大，不记就失忆。
11. **失败优先 `-r` 续跑**：`wing run -r <sid> -p "..."` 保留原上下文。开新 session 只在原上下文彻底跑偏时才考虑（不推荐）。
12. **`wing wait` 的 Bash timeout 必须放大**：Bash 工具默认 30s 会把 `wing wait` 进程杀掉（会话本身不受影响，但白等一轮）。`timeout = wait --timeout + 100` 起步。
13. **别相信自己的记忆，相信日志**：你自己的上下文会被自动压缩，跑到后面会忘掉前面。关键状态（sid、base sha、决策、结论）一律**当场**写进 `scheduler_log.md`，需要时读回来。

## 3. 目录与产物（标准布局）

```
$WING_HOME/tasks/<task_name>/
├── proposal.md                  # 任务书（scheduler 写，用户批准）
├── scheduler_log.md             # 调度日志：时间线 + session 台账 + 决策（scheduler 持续更新）
├── summary.md                   # 交付报告（scheduler 收尾写）
├── <NN>_<step_name>/
│   ├── proposal.md              # 步骤任务书（scheduler 写）
│   ├── design.md                # 实现设计（executor 写，动手前写）
│   ├── task.md                  # 勾选清单（executor 写，随做随勾）
│   ├── review_request.md        # 审查要求（scheduler 写）
│   └── review_r<N>.md           # 审查报告 B/S/N 分级（reviewer 写，r1/r2/r3 不覆盖）
└── ...
```

- `<task_name>` 用 **kebab-case**（会出现在命令行与 prompt 里，避免空格与中文）；`<NN>` 是两位编号（01、02…），体现拓扑顺序；依赖关系写在步骤 proposal 里。
- **产物 owner 唯一，不许互相覆盖**：scheduler 只写 proposal / review_request / scheduler_log / summary；executor 只写 design / task + 代码；reviewer 只写 review 报告。

## 4. 阶段 1 — 需求澄清与任务书（唯一需要人类介入的阶段）

### 4.1 澄清什么（在本阶段问完，可以多次调用 AskUserQuestion，别挤牙膏）

| 维度 | 要拿到的东西 |
|---|---|
| 目标 | 做什么、**明确不做什么**、边界情况 |
| 验收 | 怎么算达成？有测试 / 命令 / 清单吗？谁验收？ |
| 质量 | 兼容性要求、性能红线、代码风格、测试要求 |
| **交付终态** | 只到本地分支？push + 开 PR？直接合并？PR 的 base / target 分支是哪个？ |
| 资源 | 目标仓库与分支、是否已有 worktree / 未提交改动、能否联网拉依赖、磁盘余量（worktree + 构建产物很吃盘） |
| 风险 | 不可逆操作、需要外部审批的系统、回滚方案 |

原则：**先给出你认为最好的方案与建议，再让用户决策**；读仓库（AGENTS.md / 现有代码 / CI 配置）能用事实回答的问题不要问人。不要把可以自己判断的东西丢回给用户。

### 4.2 写 `proposal.md`（模板见附录 A）

用 openspec 骨架（英文小标题）+ 中文正文。要点：

- **Out of Scope 必须写**：这是防止 executor 自由发挥的第一道闸；
- **Verification 必须可执行**：一条条命令 / 测试，不能是「功能正常」这种话；
- **Delivery 写明终态**：你照着执行，不再猜；
- **Plan 给出拆分概览**：步骤编号、名称、依赖、并行机会。

写完 → 呈现给用户 → **得到明确批准后才进入下一阶段**。批准即授权：此后到交付前不再等待用户。

### 4.3 初始化调度日志

在任务目录建 `scheduler_log.md`（模板见附录 F），先写元信息：任务目录、仓库绝对路径、基线分支与 commit、lane 规划、并发上限、交付终态。

## 5. 阶段 2 — 拆分、lane 规划与 worktree

### 5.1 拆分准则

- **一个 step = 一次能交付的完整可验证单元**：能在一次 1800s 等待窗口内产出「代码 + 验证 + commit + 总结」。超了就再拆——宁可多一个步骤，也不要一个巨型步骤。
- **按接口切，不按文件切**：步骤之间只通过明确的接口 / 文件 / 数据结构交互，这些接口写进步骤 proposal 的 Dependencies。
- **验证自包含**：每个 step 都能独立跑通自己的验证命令（跑不起全量测试说明粒度不对）。
- **可并行、可独立 review、可独立回滚**。

### 5.2 依赖图与波次

把步骤编成 DAG 并按拓扑分层：

- 同一波的步骤之间**没有依赖** → 可以并发；
- 有依赖的步骤放到后续波次，等上游 commit 落地后再派；
- **先派关键路径**（最长依赖链上的步骤），让最长的链条先跑起来。

### 5.3 lane 与 worktree（核心决策）

**lane = 一条串行执行线 = 一个 worktree。**

| 情况 | 做法 |
|---|---|
| 步骤之间有依赖 / 会改同一批文件 / 需要独占资源 | **同一个 worktree 串行**执行（上一个交付验收完再派下一个） |
| 步骤互相独立、文件不重叠 | **各自独立 worktree**（每条 lane 一个），并行跑 |
| 没有任何并行机会 | 就一个 worktree，保持简单 |

**铁律 2 不可协商**：同一 worktree 同一时刻只有一个 executor。想并行 → 开 worktree。

```bash
REPO=<目标仓库绝对路径>
WT=${WING_HOME:-$HOME/.wing}/worktrees/<repo>-<task>-l1   # l1 / l2 / l3 对应 lane
cd "$REPO" && git fetch origin                          # 可选：确保基线最新
git worktree add -b <task>/l1 "$WT" <base_branch>
git -C "$WT" rev-parse HEAD                             # ← 记进 scheduler_log，作为 review 范围起点
```

- 分支命名：`<task>/l1`、`<task>/l2`…，最终合入 `<task>/integration`（§10）；
- **worktree 放哪**：默认集中放 `$WING_HOME/worktrees/<repo>-<task>-l<N>`（`<repo>` 取仓库目录名）——与任务文档同源，交付后一处就能清理干净，也不污染目标仓库的目录树；**如果目标仓库已有既定的 worktree 约定**（仓库文档里写了 / 旁边已有专门的存放目录），沿用它，不要另起炉灶；
- **绝对路径必须记进 scheduler_log**（子 agent 全靠它）：worktree 里的相对路径对子 agent 毫无意义；
- 存放目录要与仓库同卷、且留足空间：Rust / Node 这类构建产物动辄 GB 级（起几个 lane 就能吃掉十几 G）；
- **交付后清理**：`git worktree remove --force <路径>`（连构建产物一起带走）+ `git worktree prune`，别让 target / node_modules 在磁盘上过夜；

### 5.4 写步骤任务书（每步一份）

`<NN>_<step_name>/proposal.md`，骨架与总 proposal 一致（附录 B），但聚焦本步骤：

- 只写这一步要做什么、怎么验证、与上下游的接口；
- **不写实现细节**——那是 executor 在 `design.md` 里的事。你写死了，它就只会照抄，遇到现实立刻僵住；
- 必须写清「上游产物在哪」（commit sha / 文件 / 接口签名）：executor 一无所知。

### 5.5 更新 scheduler_log

写下本波步骤清单、依赖、lane 归属、并发计划。

## 6. 阶段 3 — 派发 executor

### 6.1 用哪条命令

| 命令 | 语义 | 用途 |
|---|---|---|
| `wing -p "<prompt>"` | stdio 模式：**阻塞**到本轮结束，输出最终结果（Claude Code 兼容 NDJSON） | 要一次性结果、同步串行执行 |
| `wing run -p "<prompt>" --json` | **非阻塞**：建会话 + 发消息后**立即返回 session id** | **调度就用这个**（拿 sid → wait） |

```bash
cd "$WT" && wing run --json \
  --tools "Bash,Read,Write,Edit,Grep,Glob" \
  -p "$(cat <<'EOF'
<执行者契约（§6.2）+ 本步骤的具体交代>
EOF
)"
# → {"session_id":"20260917-145630-6e9eb4c3", ...}
```

- **`cd <worktree>` 决定 session 的 workspace**（子 agent 所有工具的相对路径基准）——必须 `cd` 到 worktree，不能在自己的目录里派发；
- `--json` 给你可解析的 session id：`| jq -r .session_id`；
- 用 `<<'EOF'`（**带引号**）确保 prompt 里的反引号、`$VAR`、引号原样传入（已实测）；
- 想留痕可先把同一段 prompt 落盘到 `<step_dir>/prompt_executor.md`，再 `-p "$(cat ...)"`（可选，便于事后复盘）。

### 6.2 执行者契约（原样内联，每次派发都要带）

```
【角色】你是执行者（executor），一个 headless wing agent。没有人类在线：不要提问、不要等待确认；遇到不确定就按最合理的解释继续，并把假设写进 design.md 的 Assumptions 与最终总结。
【工作区】<worktree 绝对路径>（你的所有改动都必须在这里，不要碰其它目录）
【任务根目录】<$WING_HOME/tasks/<task>/ 绝对路径>
【必读（按顺序）】① 总任务书 <.../proposal.md> ② 本步骤任务书 <.../<NN>_<step>/proposal.md>
【上游产物】<依赖步骤的 commit sha / 接口 / 文件清单；没有就写「无」>

【必须遵守的流程】
1. 动手前先写 <.../<NN>_<step>/design.md>：英文骨架（## Context / ## Goals / ## Non-Goals / ## Decisions / ## Assumptions），中文正文；写清你怎么实现、为什么这么实现、放弃了哪些方案。
2. 再写 <.../<NN>_<step>/task.md>：把实现拆成可勾选任务项（`- [ ] 1.1 ...`），每项注明涉及文件与验证方式。
3. 按 task.md 实施，完成一项立刻勾一项（`- [ ]` → `- [x]`），不要攒到最后统一勾。
4. 实施中真实执行验证（测试 / 构建 / lint / 脚本）；失败就修到通过，或如实说明为什么无法验证。
5. 交付前把 task.md 全部勾完；清掉一次性脚本、临时文件、调试产物；不留半成品。
6. 提交：`git add <你改的文件>`（精确 add，禁止 `git add -A`）+ `git commit -m "<type>(<scope>): <中文描述>"`。禁止 push、禁止切换/新建分支、禁止 rebase、禁止动其它 worktree。
7. 最终消息（调度者会读，务必完整）：
   - 做了什么（对应 task.md 哪些项）
   - 关键文件清单
   - 验证命令与真实结果（贴关键输出摘要）
   - 风险 / 未完成项 / 需要下游注意的接口约定
   - commit sha 列表

【红线】
- 只改本 worktree 与你自己的 design.md / task.md，其它任务文档只读。
- 不许为了让测试通过而弱化测试、跳过失败、删断言。
- 不许留后台进程或长驻服务。
```

### 6.3 派发后立刻做三件事

1. 把 `session_id` 写进 `scheduler_log.md`（角色 / step / lane / 时间 / prompt 摘要）；
2. 核对上下文：`wing info <sid> --json | jq '{status, workdir, tools}'` —— `workdir` 必须是预期的 worktree，`tools` 必须与契约一致（这一步能兜住"忘了 cd"）；
3. 记账在飞数量（executor + reviewer，**不含你自己**）：以 `scheduler_log.md` 的台账为准；巡检时用 `wing ps --json` 交叉核对——注意你自己的会话也会显示成 `working`，别把自己算进去。

## 7. 阶段 4 — 等待、巡检与交付判定

### 7.1 等待

```bash
wing wait <sid1> <sid2> --timeout 1800 --json
```

- **Bash 工具必须显式设 timeout**（如 `timeout: 2000`）——见铁律 12；
- `--timeout` 是上限不是固定时长：全部会话 idle 就立即返回；
- 复杂任务给更长（1800s 起步，重活 3600s+，你判断）；
- wait 是**幂等**的：已 idle 的会话立即返回结果，所以可以反复 wait、分批 wait。

```bash
wing wait <sid> --timeout 1800 --json | jq '.results[] | {session_id, status, subtype, is_error, result: (.result[0:200])}'
```

### 7.2 结果判读

| 观察 | 含义 | 动作 |
|---|---|---|
| `status=idle, is_error=false` | 本轮正常结束 | 进入交付判定（7.3） |
| `status=timeout, is_error=true` | **只是没等到**，会话还在跑 | `wing info <sid>` 看 status：`working` → 继续 wait；`waiting` → 走 7.4 逃生舱 |
| `subtype=error_during_execution` | 执行中出错 | `wing tail <sid> -n 30 -t tool_result` 看现场 → §9.3 决定续跑/返修 |
| `is_error=true` 且 result 为空 | 可能是"还没开始"的极少数竞态 | 再 wait 一次 |
| 退出码 1 | 有任一 session is_error | **逐个处理，不要整体重试** |

`num_messages` 在 WS 路径下其实是 turn 数，别用它做判定。

### 7.3 交付判定（executor 说"做完了"之后，你自己核对）

```bash
WT=${WING_HOME:-$HOME/.wing}/worktrees/<repo>-<task>-l1
STEP=$WING_HOME/tasks/<task>/01_<step_name>
grep -n '^- \[ \]' "$STEP/task.md"          # 应为空：所有项都勾了
git -C "$WT" log --oneline <base_sha>..HEAD # 应有清晰的 commit
git -C "$WT" status --porcelain             # 应干净
ls "$STEP"/design.md "$STEP"/task.md        # 必须存在
```

四项缺任一 → **不进入 review**，按 §9.3 让 executor 用 `-r` 补齐。

### 7.4 巡检与逃生舱

- 全局巡检：`wing ps --json | jq '.[] | {id, status, last_interaction}'`（默认已过滤 `inactive`；状态取值：`working` / `idle` / `waiting` / `inactive`）；
- 看某人正在干什么：`wing tail <sid> -n 10 -t tool_call`（工具调用一览）、`-t content`（结论）、`-t reasoning`（思路）、`-t user`（你派发时给的 prompt）；
- **识别"假忙"**：`wing tail <sid> -n 10 -t tool_call` 看最近工具调用是否在原地打转（同一条命令 / 同一个文件反复出现、报错反复重试）→ 别干等：interrupt，然后 `-r` 直接点破（「你已经连续 3 次因为 X 失败，换成 Y 试试」）；
- **卡死逃生舱**（长时间 `working` 却无新增消息，或状态是 `waiting`）：

```bash
curl -s -X POST http://127.0.0.1:32523/api/session/interrupt \
  -H 'Content-Type: application/json' -d '{"session_id":"<sid>"}'
# → {"ok":true}；会话回到 idle，已生成的半成品会保留在上下文里
```

网关地址默认 `127.0.0.1:32523`（以 `$WING_HOME/core/config.yaml` 的 `gateway.host/port` 为准）。中断后用 `-r` 说明卡点、让它换思路继续。

## 8. 阶段 5 — 独立审查

### 8.1 什么时候审（你判断）

| 场景 | 建议 |
|---|---|
| lane 之间独立（不同 worktree、不同模块） | **逐 lane 完成即审**：某条 lane 交付后就派 reviewer，它的空闲期正好落在别的 lane 还在跑时，槽位利用率最高 |
| 一波内强耦合（共享接口 / 约定，要一起看才看得出问题） | **整波一起审**：全部 wait 完成、统一写 review 要求再派 |
| 只有一条 lane | 交付即审 |

reviewer 与别的 worktree 上的 executor 可以同时跑（不同目录互不影响），只要在飞总数 ≤3。

### 8.2 写 `review_request.md`（模板见附录 C）

必须包含：

- **审查范围**：worktree 绝对路径 + `<base_sha>..HEAD`（含未提交改动要显式说明）+ 本步骤拥有的文件清单；
- **验收标准**：从步骤 proposal 的 Verification 抄过来（reviewer 要照此复现）；
- **上下文**：总任务书 / 步骤任务书 / design.md / task.md 的绝对路径 + executor 的最终总结（贴进去）；
- **审查重点**：本步骤的风险点（边界、兼容、并发、安全、性能、测试充分性）；
- **纪律**：只读（可跑测试），不得改 worktree；唯一允许写入 = 报告路径；
- **输出格式**：B/S/N 分级 + 结论 + 已验证清单（附录 D）。

### 8.3 派发 reviewer

```bash
cd <worktree> && wing run --json --tools "Bash,Read,Glob,Grep,Write,Edit" \
  -p "$(cat <<'EOF'
<审查者契约 + 审查要求文件绝对路径>
EOF
)"
```

审查者契约（内联）：

```
【角色】你是独立审查者（reviewer）。你的立场与执行者相反：默认它做得不对，用证据说话。没有人类在线，不要提问。
【审查对象】worktree：<绝对路径>；范围：<base_sha>..HEAD；本步骤文件清单：<...>
【审查要求】先读 <.../<NN>_<step>/review_request.md>（含验收标准与上下文）
【必读】① 总任务书 <...> ② 步骤任务书 <...> ③ design.md <...> ④ task.md <...>

【你要做的】
1. 读 commit（`git log -p <base>..HEAD`、`git show`）与相关代码，判断是否正确、完整、符合验收标准。
2. 复现执行者声称的验证：跑它列出的命令，看到真实结果——不是相信它的话。
3. 独立设计并执行额外验证（边界值、异常路径、回归面、兼容性）；可疑点必须构造实验证伪或证实。
4. 按 B/S/N 分级写报告：`[B1]` 阻塞（不修不能交付：正确性 / 验收标准 / 安全 / 数据损坏）· `[S1]` 建议（明显更好或潜在风险，应该修）· `[N1]` nit（风格、措辞、可选优化）。每条给出：文件:行 / 问题 / 证据或复现步骤 / 修复建议。
5. 报告末尾给出：结论（`通过` / `需返修`——存在任何 B 或 S 一律「需返修」）、已验证清单（命令 + 结果）、未验证项与原因、置信度。

【红线】
- 不得修改 worktree 里任何文件（不许"顺手修一下"，不许留调试文件；测试产生的临时文件必须清掉）。
- 不得 git commit / checkout / stash 等任何写操作。
- 唯一允许的写入：把报告写到 <.../<NN>_<step>/review_r1.md>（任务目录内绝对路径；用 Write 起草、用 Edit 增量追加/修订）。Write 与 Edit 除了这个报告文件之外没有任何合法用途——不允许对 worktree、代码、任务目录里其它文件做任何修改。
- 跑测试可以；长驻服务、会改数据库 / 远端状态、要下载大依赖的命令不要跑；跑完确保 `git status` 干净。
```

### 8.4 审查结束后你必须核对

```bash
git -C <worktree> status --porcelain    # 必须干净：reviewer 没碰过工作区
```

不干净 → 审查视为无效：`git checkout -- <paths>` 恢复（或 `git stash`），记一笔，必要时换个 reviewer 重审。

### 8.5 读报告并分类

`Read <step_dir>/review_r<N>.md`，把 B/S/N 摘进 `scheduler_log.md`（含编号与结论），据此决定返修范围（§9）。

## 9. 阶段 6 — 返修闭环

### 9.1 红线

- **B 必须修**（不修不能交付）；**S 必须修**（除非你能给出明确理由不修，理由写进 scheduler_log 与 summary）；
- **N 由你判断**（顺手批量修可以，也可以记录到 summary 交给用户）；
- **B 修完必须复审**（第二次只看返修点 + 回归面），S 也默认复审；
- **同一 step 最多 3 轮（审查 → 返修）**。超限即判定该 step 阻塞：**不要阻塞与它无关的步骤**，把阻塞点写进 summary，最后向用户报告。

### 9.2 返修派发（resume 原 executor，保留上下文）

```bash
cd <worktree> && wing run --json -r <原 executor sid> \
  -p "$(cat <<'EOF'
【返修任务】审查报告在 <.../<NN>_<step>/review_r1.md>。请：
1. 逐条处理其中的 [B*] 与 [S*]：修复并说明每条的修复方式；确实不该改的，给出理由与证据（不许沉默跳过）。
2. [N*] 由你判断是否顺手修复。
3. 重新跑通 <验证命令>，并跑一遍受影响的回归。
4. 更新 task.md（如有新增项）与 design.md（实现有变化就追加一节变更说明）。
5. commit（message 写清：`fix(<scope>): 修复 review B1/B2 <摘要>`），不要 push。
6. 最终消息：逐条列出 B*/S* 的处理结论 + 验证结果 + 新 commit sha。
【纪律】不要顺手重构无关代码；不要为了让报告"过"而弱化测试。
EOF
)"
```

- 返修**必须用 `-r` 原 session**（它知道自己改过什么；新 session 会丢掉全部实现上下文。只在原上下文彻底跑偏时才开新 session，并在 log 里写明理由）；
- `-r` 会忽略 `--tools` / `-m`（工具集与模型沿用原 session），不要重复传；
- 复审新写 `review_r2.md`，**不要覆盖 r1**（审查链是交付证据）。

### 9.3 交付不达标（未 commit / 未勾选 / 跑不起来）的处理

同样用 `-r` 补齐，prompt 里直接点明缺什么：

```
【交付缺口】你的交付还缺：① task.md 第 3.2 / 3.3 项未勾选；② worktree 有未提交改动；③ 验证命令 `<...>` 未执行。
请补齐后重新给出最终消息。不要解释，直接补齐。
```

## 10. 阶段 7 — 集成、验收与交付

所有步骤通过审查后，**你亲自做集成**（这是 scheduler 的活，不要外包）：

### 10.1 集成分支

```bash
REPO=<目标仓库绝对路径>
cd "$REPO" && git fetch origin && git switch -c <task>/integration <base_branch>
git merge --no-ff <task>/l1 -m "merge: <task> l1 (<step 摘要>)"
git merge --no-ff <task>/l2 -m "merge: <task> l2 (<step 摘要>)"
```

- 顺序：按依赖顺序合入（上游先、下游后）；无依赖的按编号 / 时间顺序；
- 冲突：**简单冲突你直接解**（你有 Read/Write/Edit/Bash）；涉及大块语义、需要重新设计的冲突，才派一个 executor 到集成 worktree 里解（串行、一次一个），你只做收尾；
- 每次合并后跑一次受影响范围的验证，别把冲突和回归留到最后。

### 10.2 全量验收（交付前最后一道门）

按总任务书 proposal 的 Verification 逐条执行，全部通过才算交付：

```bash
cd "$REPO" && <proposal 里写的构建 / 测试 / 验证命令>
```

任何一条不过 → 回 §9 修（对应 lane 的 executor 用 `-r`），**不要带病交付**。

### 10.3 push 与 PR（按阶段 1 与用户约定的终态）

- 约定是「本地分支就绪」→ 到此为止，把分支名、commit 清单、验证方式写进 summary；
- 约定是「push + PR」→ 由你执行：

```bash
git push -u origin <task>/integration
# 再按目标仓库的托管平台开 PR：gh pr create / 平台 CLI / 网页，
# 并遵循该仓库自己的 PR 规范（模板、标题格式、目标分支）
```

- **单一 PR**：多个 worktree 的产物必须先合进同一分支，再开一个 PR。绝不给每个 lane 开一个 PR；
- PR 描述必须包含：Why / What / How to verify / 风险与回滚，并引用 `$WING_HOME/tasks/<task>/summary.md` 的要点。

## 11. 阶段 8 — 交付报告与汇报

写 `$WING_HOME/tasks/<task>/summary.md`（附录 E），然后向用户汇报一段话（**不要复制整个文档**）：

- 交付终态与位置（分支 / PR 链接 / commit 范围）；
- 变更清单（按步骤，一句话一个）；
- 验证证据（跑了什么命令、结果如何）；
- **没做的事**（Out of Scope、未修的 S/N、被判定阻塞的步骤）与风险；
- 需要用户决定的事（合并时机、上线窗口、遗留项）。

全程「先结论后细节」：用户要的是「能不能用、在哪看、有什么坑」。

### 过程中的进度播报（不阻塞）

全自动 ≠ 全程静默：每次状态跃迁（一波派发完 / 一波 wait 完 / 一轮 review 收敛 / 判定某步阻塞）向用户输出**两行以内**的进度，然后立刻继续干活——**只播报，不等待确认**。这样用户随时回来看都知道跑到哪了。

## 12. 命令速查

| 目的 | 命令 | 备注 |
|---|---|---|
| 网关状态 | `wing status` / `wing start` | `wing run` 会自动拉起网关 |
| 派发（非阻塞） | `wing run --json --tools "..." -p "$(...)"` | **必须先 `cd <worktree>`** |
| 续跑同一 session | `wing run -r <sid> -p "..."` | 忽略 `--tools` / `-m` |
| 等待（阻塞） | `wing wait <sid...> --timeout 1800 --json` | Bash 工具 timeout 要更大 |
| 会话列表 | `wing ps --json` | 默认过滤 `inactive` |
| 单会话状态 | `wing info <sid> --json` | status / workdir / tools / tokens |
| 看消息 | `wing tail <sid> -n 20 -t <type>` | type: all / user / assistant / tool_call / tool_result / reasoning / content |
| 看开头 | `wing head <sid> -n 5 -t user` | 复核你派发时给的 prompt |
| 中断卡死会话 | `curl -s -X POST http://127.0.0.1:32523/api/session/interrupt -H 'Content-Type: application/json' -d '{"session_id":"<sid>"}'` | 返回 `{"ok":true}`；会话回 idle |
| 模型 / 工具 / 模板 | `wing models` / `wing tools` / `wing agents` | 排查工具名写错 |

## 13. 陷阱与反模式

1. **Bash timeout 忘了放大** → `wing wait` 被 30s 默认超时杀掉。会话没事，重新 wait 即可，但白费一轮。
2. **忘了 `cd` 到 worktree** → 子 agent 在错误目录里改代码。用 `wing info <sid>` 的 `workdir` 兜底核对。
3. **同一 worktree 派两个 executor** → 互相覆盖、git index 冲突、review 范围不可分。并行必须多 worktree。
4. **把设计写死** → 步骤 proposal 里写"怎么做"，executor 就只会照抄、遇到现实立刻卡住。proposal 管「做什么 / 怎么算完成」，design 管「怎么做」。
5. **不给绝对路径** → 子 agent 的 workspace 是 worktree，相对路径全部会歪。所有文档引用一律绝对路径。
6. **不登记 session** → 任务一大就不知道谁是谁、哪条 lane 在跑什么。派发即写 log。
7. **reviewer 顺手改代码** → 审查失效。每次审查后用 `git status --porcelain` 兜底。
8. **直接相信 executor 的"验证通过"** → reviewer 必须复现命令，你也要抽查（尤其最后一轮全量验收）。
9. **给每个 lane 开 PR** → 用户要的是单一交付物：先合集成分支，再开一个 PR。
10. **`git add -A`** → 把无关文件带进 commit。契约里要求精确 add。
11. **让 executor push** → 明确禁止；push / 建 PR 是 scheduler 的专属权限。
12. **任务文档写进 worktree** → 污染 diff 与 review 范围。任务目录永远在 `$WING_HOME/tasks/`。
13. **设 `--max-turns`** → 任务中途夭折。不设。
14. **用 sleep 轮询代替 `wing wait`** → 又慢又费 token；wait 是幂等的，直接 wait。
15. **交付前不查 `git status`** → 临时脚本、调试文件混进交付物。

## 14. 附录（模板）

### 附录 A — `proposal.md`（任务书）

```markdown
# <task_name> — 任务书

## Why
（背景、痛点、为什么现在做；一两段，不要复述用户原话）

## What Changes
（交付内容清单，按模块 / 接口 / 文件粒度）

## Scope
### In Scope
### Out of Scope
（明确不做的事——防止 executor 自由发挥的第一道闸）

## Deliverables
（产物清单：代码 / 文档 / 配置 / 迁移脚本…，各自的验收方式）

## Verification
（如何验证达成目标：可执行命令、测试、验收清单；必须可复现）

## Quality Bar
（质量标准：测试要求、兼容性、性能、代码风格、提交规范）

## Delivery
（交付终态：本地分支 / push+PR / 合并；分支与目标分支；PR 描述要点；谁收口）

## Impact & Risks
（影响面、外部依赖、风险与回滚方案）

## Plan
（拆分概览：步骤编号、名称、依赖、并行机会、波次）
```

### 附录 B — 步骤任务书 `<NN>_<step_name>/proposal.md`

```markdown
# <NN> <step_name> — 步骤任务书

## Why
（本步骤在总任务中的位置与价值）

## What Changes
（本步骤要改什么，文件 / 模块粒度）

## Scope
### In Scope
### Out of Scope

## Verification
（验收标准 + 可执行验证命令——reviewer 会照此复现）

## Quality Bar
（本步骤特有的质量要求）

## Dependencies
（上游步骤 / 产物：commit sha、接口签名、数据约定；本步骤的输入）

## Handoff
（期望交付物：改动文件、commit、task.md 全勾、最终消息要点）
```

### 附录 C — `review_request.md`（审查要求）

```markdown
# <NN> <step_name> — 审查要求（第 <N> 轮）

## 审查对象
- worktree：<绝对路径>
- 范围：`<base_sha>..HEAD`（commit 列表：<sha 摘要>）
- 本步骤拥有的文件：<路径清单>
- 是否含未提交改动：<是 / 否>

## 必读
- 总任务书：<绝对路径>
- 步骤任务书：<绝对路径>
- design.md / task.md：<绝对路径>

## 验收标准（照此复现）
1. <命令 / 断言>
2. ...

## 执行者的最终总结（原文）
<粘贴>

## 审查重点
（本步骤的风险点：边界、兼容、并发、安全、性能、测试充分性…）

## 纪律
- 只读；可跑测试；不得修改 worktree 中任何文件
- 唯一允许的写入：<.../<NN>_<step>/review_r<N>.md>
- 跑完确保 `git status --porcelain` 干净

## 输出格式
（见附录 D：B/S/N 分级 + 结论 + 已验证清单）
```

### 附录 D — 审查报告 `review_r<N>.md`

```markdown
# <NN> <step_name> — 审查报告（第 <N> 轮）

**审查范围**：<base_sha>..<head_sha>（worktree 绝对路径）
**结论**：通过 / 需返修（存在 B 或 S 一律「需返修」）
**置信度**：高 / 中 / 低（低要说明为什么）

## 发现

### [B1] <一句话标题>
- 位置：`path/to/file:42`
- 问题：
- 证据 / 复现：<命令 + 输出，或推理链>
- 修复建议：

### [S1] <一句话标题>
（同上）

### [N1] <一句话标题>
（同上）

## 已验证
| 验证项 | 命令 | 结果 |
|---|---|---|
| 验收标准 1 | `<cmd>` | 通过 / 失败（输出摘要） |

## 未验证 / 无法验证
（项 + 原因）

## 备注
（对交付物的整体判断、与其它步骤的接口风险）
```

### 附录 E — `summary.md`（交付报告）

```markdown
# <task_name> — 交付报告

## 结论
（一句话：交付了什么、现在在哪、能不能用）

## 交付终态
- 分支：`<task>/integration`（base `<base_branch>`）
- commit 范围：`<sha>..<sha>`
- PR：<链接 或「未开（约定为本地分支就绪）」>

## 变更清单
| 步骤 | 内容 | commit | 审查结论 |
|---|---|---|---|
| 01_xxx | … | `abc1234` | 通过（r2） |

## 验证证据
| 验证项 | 命令 | 结果 |
|---|---|---|

## 未完成 / 未做的事
- Out of Scope：…
- 未修的 [S*] / [N*]：…（为什么）
- 阻塞的步骤：…（原因、影响、建议）

## 风险与回滚
（上线风险、回滚方式）

## 调度概览
（波次、lane、session 数量、总耗时；session id 见 scheduler_log.md）
```

### 附录 F — `scheduler_log.md`

```markdown
# <task_name> — 调度日志

## 元信息
- 任务目录：`$WING_HOME/tasks/<task_name>/`
- 仓库：`<repo 绝对路径>`；基线分支：`<base>`；基线 commit：`<sha>`
- lane 规划：L1 = `<worktree 绝对路径>`（分支 `<task>/l1`，base `<sha>`）；L2 = …
- 并发上限：3（executor + reviewer）
- 交付终态：<与用户约定>

## Session 台账
| sid | 角色 | step | lane | 派发时间 | 状态 | 结论 / 备注 |
|---|---|---|---|---|---|---|
| 2026… | executor | 01_xxx | l1 | 09-17 15:02 | idle | 交付 3 commit，见 review_r1 |

## 时间线
| 时间 | 事件 | 对象 | 详情 |
|---|---|---|---|
| 09-17 15:02 | dispatch | 01_xxx / l1 | sid=2026…；prompt 摘要：… |
| 09-17 15:41 | wait | 01_xxx / l1 | status=idle，commit `abc1234` |
| 09-17 15:45 | review | 01_xxx r1 | sid=2026…；结论：需返修（B1、S2） |
| 09-17 16:10 | fix | 01_xxx r1 | sid=2026…（resume）；B1/S2 已修，commit `def5678` |
| 09-17 16:20 | merge | L1 → integration | `merge: <task> l1` |

## 决策记录
- （为什么这么拆、为什么串行 / 并行、为什么不修某个 S、为什么开新 session…）
```
