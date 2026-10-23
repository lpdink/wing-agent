---
name: wing-scheduler
description: 超大型任务的无人值守交付编排（scheduler 模式）。把一个大任务拆成步骤与波次，在独立 worktree 上并发派发 headless 子 agent（executor）执行——执行者交付前自派一轮双视角独立审查（正确性 + 架构并发，B/S/N 分级），全部收口后 scheduler 再派一轮整体审查——最后由 scheduler 集成、push 并产出单一 PR。当用户要求「实施/交付一个大型任务」「拆开并行做掉」「无人值守把这件事做完」「多子 agent 协同开发」，或任务明显需要多波次、多 worktree、跨模块大范围改动时使用。
category: Workflow
tags: [orchestration, scheduler, multi-agent, delivery, headless]
---

# Wing Scheduler — 超大型任务的全自动交付

一句话：**你（scheduler）负责想清楚、拆、派、验收复核、集成、收口；子 agent 负责执行与自审。**

本 skill 只依赖 `wing` CLI（`wing run` / `wait` / `watch` / `wake` / `fork` / `ps` / `info` / `tag` / `tail` / `head`）与普通 shell / git 命令，不需要额外脚本或编排框架。`wing` 的形态、命令全集与常见坑见 [wing-cli](../wing-cli/SKILL.md)。

本文用 `$WING_HOME` 指 wing 的状态目录（默认 `~/.wing`，可用同名环境变量覆盖）：任务文档与 worktree 都放在它下面，与目标仓库解耦。

## 调度主循环（一页地图）

```
0. 需求澄清 → 写 proposal.md → 用户批准 → 初始化 scheduler_log.md          （§4）
1. 拆 DAG + lane 规划 + 建 worktree + 写步骤 proposal                      （§5）
2. while 还有未收敛的 step：
     a. 选出"依赖已就绪"的 step，按 lane 派发 executor（并发由你判断）        （§6）
     b. 登记唤醒（`wing run` 默认已开）后干别的，被叫醒就判读结果            （§7）
     c. 交付达标的 step：验收复核（执行者已自派一轮双视角 review）          （§8.1–8.2）
     d. 复核不合格 → `-r` 补齐（最多 3 轮）→ 收敛，或判定阻塞并跳过下游      （§9）
     e. 该 lane 空闲 → 派这条 lane 的下一个 step（别的 lane 继续跑）
     f. 更新 scheduler_log + 向用户播报两行进度                              （§4.3 / §11）
3. 全部收敛 → 集成到单一分支 → 全量验收 → 派一轮整体双视角 review         （§10）
4. 整体 review 的 B/S 修完 → push / 开 PR → 写 summary.md → 汇报结论         （§10 / §11）
```

两个必须同时守住的不变量：**一个 worktree 一个 executor**、**每个 session 都登记**（你派发的当场记；执行者自派的两个 reviewer 靠它的回执补记，见铁律 10）。

## 0. 适用与不适用

**适用**（满足任意两条即应考虑）：

- 跨模块 / 跨文件的大范围改动，单次对话上下文装不下；
- 能拆成 ≥3 个可独立验证的步骤，且步骤之间存在并行机会；
- 交付标准客观（测试 / 构建 / 可执行验证命令），不需要人类主观判断；
- 用户期望「我说完需求，你干完给我 PR」。

**不适用**：

- 小改动（一个文件、一次编辑）→ 直接自己做，不要开子 agent；
- 探索 / 调研 / 问答 → 自己查或派只读子 agent（`Bash` + `Read` + `Glob` + `Grep`）；
- 需求还在变、需要持续与人交互 → 先把需求澄清完，不要进入调度循环。

## 1. 角色

| 角色 | 是谁 | 工具集（写死） | 权限边界 |
|---|---|---|---|
| **scheduler** | 你，当前会话 | Bash / Read / Write / Edit / Glob / Grep（+ AskUserQuestion） | 唯一与用户对话；唯一能 push / 建 PR；负责任务文档、派发、**验收复核**、**整体 review**、集成、收口 |
| **executor** | `wing run` 起的 headless session | `Bash,Read,Write,Edit,Grep,Glob` | 只在自己被分配的 worktree 内改代码；**必须清晰 commit**；**交付前同一轮并发派两个 review（正确性 / 架构）**；禁止 push / 切分支 / rebase / 动别的 worktree |
| **reviewer ×2** | `wing run` 起的两个 headless session | `Bash,Read,Glob,Grep,Write,Edit` | 只读被审 worktree（可跑测试）；一个审正确性、一个审架构，各自只写自己那份任务目录下的报告（Write 起草、Edit 增量改）；**不得再派发任何子 agent**、不得改代码 |

子 agent **一无所知**：不知道用户是谁、不知道任务背景、不知道别的步骤在做什么、不知道你的计划。它们的全部上下文 = 你写的 prompt + 你指给它的文件。**信息缺口是编排失败的第一大原因**。

> 由 `wing run` / `wing -p` 创建的 session 一律强制 `yolo=true`（危险命令不询问），这正是无人值守需要的；也意味着子 agent 的破坏力没有闸门——所以**工具集与工作区边界必须写死，不能靠子 agent 自觉**。

### 1.1 会话标记（tags：scheduler / executor / reviewer / task）

开工第一件事给自己打标，派发时给每个子 agent 打标——巡检、收口、事后找回都靠它：

- **自己**：`wing tag $WING_SESSION_ID scheduler task=<task_name>`——`$WING_SESSION_ID` 是 Bash 工具注入的环境变量 = 你所在会话的 id（开工时先 `echo $WING_SESSION_ID` 确认非空；为空说明运行环境尚未支持该注入，打标需人工确认 sid）；
- **executor / reviewer**：派发的 `wing run` 直接带 `--tag executor --tag task=<task_name>`（reviewer 用 `--tag reviewer`，再按视角加 `--tag reviewer-correctness` / `--tag reviewer-arch`）——创建即打标、一条命令原子完成，没有"派发了但没标上"的窗口；`-r` 续跑同样接受 `--tag`（幂等追加）；
- **巡检 / 找回**：`wing ps --tag task=<task_name>` 看任务全家（多个 `--tag` 为 AND；`--tag` 隐含包含 inactive）；**归档区的会话不在 `wing ps` 里**（冷存储，用 `wing archive --list` 查）；
- 约定：标签是不透明字符串（建议小写、`k=v` 作命名空间）；角色用裸词，任务统一 `task=<task_name>`。不确定标过没：`wing tag <sid>` 读、`wing tag --list` 清点全局。

## 2. 铁律（违反任意一条，编排都会崩坏）

1. **并发由你判断，不设固定上限**：同一时刻能同时在飞多少 executor / reviewer，取决于任务粒度、机器资源与你在等什么——自己拿主意，并在 `scheduler_log.md` 里写清当前在飞哪些。真正不能破的是下一条（worktree 独占）。
2. **同一个 worktree，同一时刻只能有一个 executor**：想并行 → 开多个 worktree（一条 lane 一个）。绝不让两个 executor 在同一目录里写代码。
3. **不设 `--max-turns`**：现代 agent 需要的 ReAct 轮数很高，设了会让任务中途夭折。
4. **不改模型与 effort**：用模板默认（不加 `-m` / `--effort`）。模型选择带来的收益远小于版本漂移与不可复现的风险。
5. **工具集写死**（见上表）：不要给子 agent 加 `TodoWrite` / `AskUserQuestion`。
6. **子 agent 不问问题**：没有人类在线（`AskUserQuestion` 不在工具集里；即使问了也没人答，会白挂 100 分钟）。契约里必须明确「遇到不确定自己决策，并把假设写进 design.md 与最终消息」。
7. **push / 建 PR 只能你做**：executor 只 commit（commit message 要写清楚，reviewer 靠它取信息）。
8. **任务文档一律放 `$WING_HOME/tasks/<task>/`，用绝对路径引用**：绝不写进 worktree（会污染 diff 与 review 范围）。任务目录在 worktree 之外是刻意设计。
9. **先记录 base commit，再派发**：没有 base 就没有 review 范围（`<base>..HEAD`）。
10. **你派发的每个 session 都要登记并打标**：派发即写进 `scheduler_log.md`（sid / 角色 / step / lane）并落标签（`--tag`，见 §1.1）；自己也随手可被 `wing ps --tag scheduler` 找回。执行者**自派**的两个 reviewer 不经过你——契约要求在最终消息里各报一行 sid / 视角 / 结论（§6.2 第 8 条），你在验收复核时把这几行**补进台账**（reviewer 都带 `--tag task=<task_name>`，可用 `wing ps --tag task=<task_name>` 交叉核对）。任务一大，不记就失忆。
11. **失败优先 `-r` 续跑**：`wing run -r <sid> -p "..."` 保留原上下文。开新 session 只在原上下文彻底跑偏时才考虑（不推荐）。
12. **等待窗口给足**：执行者自派一轮双视角 review 之后，单步实测 30–40 分钟。`wing wait --timeout` 与 Bash 工具的 timeout 都要同步放大（`--timeout 3600`、Bash timeout 3700 起步）；Bash 默认 30s 会把 `wing wait` 进程杀掉（会话本身不受影响，只是白等一轮）。更省的做法是根本不 wait：`wing run` 默认登记「子会话收口 / 提问 → 唤醒我」（§7.1）。
13. **别相信自己的记忆，相信日志**：你自己的上下文会被自动压缩，跑到后面会忘掉前面。关键状态（sid、base sha、决策、结论）一律**当场**写进 `scheduler_log.md`，需要时读回来。
14. **每步的 review 归执行者**：你不写 `review_request.md`、不派每步的 reviewer——交付前并发自派两个视角（正确性 / 架构）的独立 review 是执行者契约的一部分（§6.2）。你的审查活只有两件：**验收复核**（两份报告都在、结论通过、工作区干净）与**收口后的整体 review**（§8）。

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
│   ├── review_request.md        # 审查要求（executor 写，交付前派 review 时用）
│   ├── review_r<N>.md           # 正确性审查报告 B/S/N 分级（reviewer 写，r1/r2/r3 不覆盖）
│   └── review_arch_r<N>.md      # 架构审查报告 B/S/N 分级（reviewer 写，同一轮、不覆盖）
└── ...
```

- `<task_name>` 用 **kebab-case**（会出现在命令行与 prompt 里，避免空格与中文）；`<NN>` 是两位编号（01、02…），体现拓扑顺序；依赖关系写在步骤 proposal 里。
- **产物 owner 唯一，不许互相覆盖**：scheduler 只写 proposal / scheduler_log / summary（+ 整体 review 的 review_request）；executor 只写 design / task / review_request + 代码；reviewer 各写自己那份 review 报告（正确性 `review_r<N>.md` / 架构 `review_arch_r<N>.md`）。

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

用附录 A 的骨架（英文小标题）+ 中文正文。要点：

- **Out of Scope 必须写**：这是防止 executor 自由发挥的第一道闸；
- **Verification 必须可执行**：一条条命令 / 测试，不能是「功能正常」这种话；
- **Delivery 写明终态**：你照着执行，不再猜；
- **Plan 给出拆分概览**：步骤编号、名称、依赖、并行机会。

写完 → 呈现给用户 → **得到明确批准后才进入下一阶段**。批准即授权：此后到交付前不再等待用户。

### 4.3 初始化调度日志

在任务目录建 `scheduler_log.md`（模板见附录 F），先写元信息：任务目录、仓库绝对路径、基线分支与 commit、lane 规划、并发策略、交付终态。

## 5. 阶段 2 — 拆分、lane 规划与 worktree

### 5.1 拆分准则

- **一个 step = 一次能交付的完整可验证单元**：能在一次等待窗口内产出「代码 + 验证 + commit + 总结」，窗口给到 1 小时（执行者还要自己并发派两个 review，实测 30–40 分钟/步）；只是判断依据，不是硬闸——超了就再拆，宁可多一个步骤，也不要一个巨型步骤。
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

写下本波步骤清单、依赖、lane 归属、并发计划（当前派了谁、在等谁）。

## 6. 阶段 3 — 派发 executor

### 6.1 用哪条命令

| 命令 | 语义 | 用途 |
|---|---|---|
| `wing -p "<prompt>"` | stdio 模式：**阻塞**到本轮结束，输出最终结果（Claude Code 兼容 NDJSON） | 要一次性结果、同步串行执行 |
| `wing run -p "<prompt>" --json` | **非阻塞**：建会话 + 发消息后**立即返回 session id** | **调度就用这个**（拿 sid → 干别的 / wait） |

`wing run` **默认登记「子会话收口或提问 → 唤醒我」**（目标 = `WING_SESSION_ID`，即你自己的会话）：派完就可以去干别的，被叫醒再判读，不必显式 `wing watch`、更不必干等。`--no-notify` 关掉，`--notify-to <sid>` 换个目标，`--notify-on done,ask` / `--notify-prompt TEXT` 调载荷；不在 agent 的 shell 里（没有 `WING_SESSION_ID`）时它会静默关闭，文本输出里是 `notify: off (no parent session)`。**回答 ask（带 `--tool-call-id`）时不登记**——派发时那条订阅仍在武装，本来就够；文本输出 `notify: off (answering an ask: the dispatch-time trigger is still armed)`。

```bash
cd "$WT" && wing run --json \
  --tools "Bash,Read,Write,Edit,Grep,Glob" \
  --tag executor --tag task=<task_name> \
  -p "$(cat <<'EOF'
<执行者契约（§6.2）+ 本步骤的具体交代>
EOF
)"
# → {"session_id":"20260917-145630-6e9eb4c3", ...}
```

- **`cd <worktree>` 决定 session 的 workspace**（子 agent 所有工具的相对路径基准）——必须 `cd` 到 worktree，不能在自己的目录里派发；
- **`--tag executor --tag task=<task_name>` 派发即打标**（§1.1，reviewer 用 `--tag reviewer` + 视角标签）——创建时原子写进会话，别事后再补；
- `--json` 给你可解析的 session id：`| jq -r .session_id`；
- 用 `<<'EOF'`（**带引号**）确保 prompt 里的反引号、`$VAR`、引号原样传入（已实测）；
- 想留痕可先把同一段 prompt 落盘到 `<step_dir>/prompt_executor.md`，再 `-p "$(cat ...)"`（可选，便于事后复盘）。

**上下文难以用 prompt 交代时：fork 一个自己。**

```bash
CLONE=$(wing fork "$WING_SESSION_ID" --at current --json | jq -r .session_id)
wing run -r "$CLONE" -p "你是被 fork 出来的执行者：我（父会话）的任务背景你已经在上下文里。现在去做 X……"
```

`wing fork <SID> --at current` 克隆整条链（也可以先 `wing branches <SID>` 挑一个节点，`--at <uuid>`），得到的新会话继承完整上下文——做过什么、改过哪些文件、踩过哪些坑——比写一篇长 prompt 可靠得多。

- 适用：任务要求「接着我现在的位置继续做」，而你一屏 prompt 写不全前因后果（典型：调试到一半、已验证的探索路径要复用）；
- 代价：克隆是**独立会话**，整条链都会进它的上下文——上下文更大、每轮 token 更贵；它继承的是**那一刻**的链，父会话后续的改动不会同步过去；
- 克隆仍然一无所知的是 **fork 之后才发生的事**，要靠 prompt 补：本步任务书、lane / worktree、验收命令、纪律，以及哪些决定是你 fork 之后才做的。

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
7. **交付前自己派发一轮双视角独立 review**（契约的一部分，不是可选）：写 <.../<NN>_<step>/review_request.md>（审查对象 / 范围 `<base_sha>..HEAD` / 验收标准 / 你的最终总结），用调度者给你的**两份**「审查者契约」文本（正确性 / 架构）**同一轮并发派两个 reviewer**：
   `cd <worktree> && wing run --json --no-notify --tools "Bash,Read,Glob,Grep,Write,Edit" --tag reviewer --tag reviewer-correctness --tag task=<task_name> -p "<正确性契约 + review_request.md 绝对路径>"`
   `cd <worktree> && wing run --json --no-notify --tools "Bash,Read,Glob,Grep,Write,Edit" --tag reviewer --tag reviewer-arch --tag task=<task_name> -p "<架构契约 + review_request.md 绝对路径>"`
   （`--no-notify` 是必须的：你反正要阻塞等它们，别让它们的收口再往你这里投唤醒消息。）
   然后一起等：`wing wait <正确性_sid> <架构_sid> --timeout 3600 --json`（Bash 工具 timeout 给 3700——铁律 12 的同一套数字；wait 幂等，超时了重等即可）。读**两份**报告：**[B] 与 [S] 必修、[N] 你判断**；两份冲突时以「用户既定决策 > 正确性 > 架构」为序取舍，写进最终消息。同一 step 最多 3 轮，复审写 review_r2.md / review_arch_r2.md（不覆盖上一轮），并用 `git -C <worktree> status --porcelain` 确认 reviewer 没弄脏工作区。
8. 最终消息（调度者会读，务必完整）：
   - 做了什么（对应 task.md 哪些项）
   - 关键文件清单
   - 验证命令与真实结果（贴关键输出摘要）
   - 双视角 review 的**回执**：两个 reviewer 各一行——视角 / sid / 轮次 / B-S-N 各几条 / 逐条怎么处理 / 报告路径（调度者要靠这几行把它们补进台账）
   - 风险 / 未完成项 / 需要下游注意的接口约定
   - commit sha 列表

【红线】
- 只改本 worktree 与你自己的 design / task / review_request，其它任务文档只读。
- 不许为了让测试通过而弱化测试、跳过失败、删断言。
- 不许留后台进程或长驻服务。
- **不许把自己的任务外包**：除上面那一轮双视角 review，不得再用 `wing` 派发任何子 agent（不许「派个 agent 把活干了我抄结果」）。活是你自己的。
```

### 6.3 派发后立刻做三件事

1. 把 `session_id` 写进 `scheduler_log.md`（角色 / step / lane / 时间 / prompt 摘要）；
2. 核对上下文：`wing info <sid> --json | jq '{status, workdir, tools}'` —— `workdir` 必须是预期的 worktree，`tools` 必须与契约一致（这一步能兜住"忘了 cd"）；
3. 台账登记：以 `scheduler_log.md` 为准记下这个 sid 属于哪个 step / lane、在等什么；巡检时用 `wing ps --tag task=<task_name>` 交叉核对（你自己的会话也会显示成 `working`，别把自己算进去）。

## 7. 阶段 4 — 等待、巡检与交付判定

### 7.1 等待

**默认姿势：不 wait。** `wing run` 已经替你登记了「子会话收口 / 提问 → 唤醒我」（§6.1）：派完就去派下一个、写文档、做集成，被叫醒时你的会话会收到一条普通用户消息（内置信封：subtype / 轮数 / 耗时 / 最后回复，或提问内容 + 回答用的 `tool_call_id`）。醒来的第一件事是判读（§7.2 — 7.3），不是接着睡。

想显式补登一条订阅（比如 `--no-notify` 派发的、或要盯多次收口）：`wing watch <sid> [--times 3] [--on done,ask]`。同一 `(source, target, on_events)` 再登记一次是**刷新**那一条订阅（不是新增）：保留 id 与次数计数，`--times` / `-p` 不给就保持原值——所以重复 `wing watch` 不会让同一个收口投两次，`wing run -r` 的隐式登记也不会把 `--times 3` 的剩余配额降成 1。

**要阻塞时**（你就是要等到结果再往下走）：

```bash
wing wait <sid1> <sid2> --timeout 3600 --json
```

- **Bash 工具必须显式设更大的 timeout**（如 `timeout: 3700`）——见铁律 12；默认 30s 会把 `wing wait` 杀掉；
- `--timeout` 是上限不是固定时长：全部会话 idle 就立即返回；自派 review 之后单步 30–40 分钟是常态，`--timeout 3600` 起步（重活 5400+）；
- wait 是**幂等**的：已 idle 的会话立即返回结果，所以可以反复 wait、分批 wait。

```bash
wing wait <sid> --timeout 3600 --json | jq '.results[] | {session_id, status, subtype, is_error, result: (.result[0:200])}'
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

执行者交回来的东西应当已经包含一轮自派双视角 review（§6.2 第 7 条）。核对：

```bash
WT=${WING_HOME:-$HOME/.wing}/worktrees/<repo>-<task>-l1
STEP=$WING_HOME/tasks/<task>/01_<step_name>
grep -n '^- \[ \]' "$STEP/task.md"          # 应为空：所有项都勾了
git -C "$WT" log --oneline <base_sha>..HEAD # 应有清晰的 commit
git -C "$WT" status --porcelain             # 应干净
ls "$STEP"/design.md "$STEP"/task.md        # 必须存在
ls "$STEP"/review_r*.md "$STEP"/review_arch_r*.md   # 必须存在：执行者自派的两个视角报告
```

再打开**两份最后一轮**报告的末尾，确认结论都是「通过」（任何一份存在 B 或 S 都应是「需返修」，那就还得再来一轮）。缺任一 → 按 §9.3 让 executor 用 `-r` 补齐。

### 7.4 巡检与逃生舱

- 全局巡检：`wing ps --json | jq '.[] | {id, status, last_interaction}'`（默认已过滤 `inactive`，**被 pin 的会话除外**；状态取值：`working` / `idle` / `waiting` / `inactive`）；本任务范围用 `wing ps --tag task=<task_name>`（§1.1）；
- 看某人正在干什么：`wing tail <sid> -n 10 -t tool_call`（工具调用一览）、`-t content`（助手正文）、`-t reasoning`（思路）、`-t user`（你派发时给的 prompt）；
- **识别"假忙"**：`wing tail <sid> -n 10 -t tool_call` 看最近工具调用是否在原地打转（同一条命令 / 同一个文件反复出现、报错反复重试）→ 别干等：interrupt，然后 `-r` 直接点破（「你已经连续 3 次因为 X 失败，换成 Y 试试」）；
- **卡死逃生舱**（长时间 `working` 却无新增消息，或状态是 `waiting`）：

```bash
wing interrupt <sid>
# 文本输出 `interrupted session <sid>`；会话回到 idle，已生成的半成品会保留在上下文里
```

中断后用 `-r` 说明卡点、让它换思路继续。没有 `wing` CLI 时同一个动作走 HTTP：
`POST /api/session/interrupt`（body `{"session_id":"<sid>"}`，默认地址 `127.0.0.1:32523`，以
`$WING_HOME/core/config.yaml` 的 `gateway.host/port` 为准）。

## 8. 阶段 5 — 独立审查（谁审什么）

审查分两层，**每步的活不是你的**：

| 审什么 | 谁派 | 什么时候 | 你做什么 |
|---|---|---|---|
| **每步的独立 review** | 执行者自己（契约第 7 条），正确性 + 架构并发两个 | 交付前，按它自己的节奏 | **验收复核**（§8.2） |
| **整体 review** | 你，同样两个视角并发 | 全部步骤收敛、集成 + 全量验收通过之后（§10.3） | 读两份报告 → 派执行者修 B/S |

### 8.1 每步的 review 由执行者自派

派发 prompt 里已经写死（§6.2 第 7 条）：执行者写 `review_request.md`、按 §8.3 的两份「审查者契约」**同一轮并发派两个**只读 reviewer（正确性 / 架构）、读两份报告修 B/S、最多 3 轮。它在自己 lane 的 worktree 里做这件事，与其它 lane 上的 executor 并行互不影响。

你只要做两件事：

1. **把两份审查者契约连进执行者 prompt**（§8.3 的文本原样贴过去——子 agent 读不到这份 skill）；
2. **验收复核**（§8.2）。

### 8.2 验收复核（不合格就退回，不要自己上手改）

```bash
WT=<worktree 绝对路径>; STEP=$WING_HOME/tasks/<task>/<NN>_<step>
git -C "$WT" status --porcelain      # 干净
ls "$STEP"/review_r*.md "$STEP"/review_arch_r*.md   # 两份都在（至少各 r1）
grep -n '^- \[ \]' "$STEP/task.md"   # 空：全部勾完
```

再确认**两份**最后一轮报告的结论都是「通过」，并从两份里各**抽查 1–2 条 B/S 的处理**（报告说修了 ≠ 修了：看 `git log -p` 或对应测试）。任一项不过 → §9.3 用 `-r` 退回；"报告结论是需返修、执行者却没再走一轮"也是退回。

复核通过时顺手把**执行者自派的两个 reviewer** 补进 `scheduler_log.md`（两个 sid / 视角 / step / 结论，取自它的最终消息；`wing ps --tag task=<task_name>` 可交叉核对）——铁律 10 对自派 session 的登记就落在这里。

### 8.3 审查者契约（执行者派每步 review 用它；整体 review 你派的时候也用这两份）

派发（workspace 用被审 worktree；两个视角同一轮并发派，各拿一个 sid）：

```bash
cd <worktree> && wing run --json --no-notify --tools "Bash,Read,Glob,Grep,Write,Edit" \
  --tag reviewer --tag reviewer-correctness --tag task=<task_name> \
  -p "$(cat <<'EOF'
<正确性契约 + 审查要求文件绝对路径>
EOF
)"
cd <worktree> && wing run --json --no-notify --tools "Bash,Read,Glob,Grep,Write,Edit" \
  --tag reviewer --tag reviewer-arch --tag task=<task_name> \
  -p "$(cat <<'EOF'
<架构契约 + 审查要求文件绝对路径>
EOF
)"
```

（`--no-notify`：reviewer 的收口不需要叫醒任何人——派发方自己会 `wing wait` 它们。）

契约文本（内联）——两份共用骨架，只有「角色」与「你要做的」按视角不同：

```
【角色】你是独立审查者（reviewer），本次只审<正确性|架构>。你的立场与执行者相反：默认它做得不对，用证据说话。没有人类在线，不要提问。
【审查对象】worktree：<绝对路径>；范围：<base_sha>..HEAD；文件清单：<...>
【审查要求】先读 <review_request.md 绝对路径>（含验收标准与上下文）
【必读】<按 review 类型填一处，删掉另一处>
  · 每步 review：① 总任务书 <...> ② 步骤任务书 <...> ③ design.md <...> ④ task.md <...>
  · 整体 review：① 总任务书 <...> ② 各 step 的 proposal 清单 <...> ③ 各 step 的 review 报告清单 <...>

【你要做的】<按视角二选一，见下>

【输出】按 B/S/N 分级写报告：`[B1]` 阻塞（不修不能交付：正确性 / 验收标准 / 安全 / 数据损坏）· `[S1]` 建议（明显更好或潜在风险，应该修）· `[N1]` nit（风格、措辞、可选优化）。每条给出：文件:行 / 问题 / 证据或复现步骤 / 修复建议。报告末尾给出：结论（`通过` / `需返修`——存在任何 B 或 S 一律「需返修」）、已验证清单（命令 + 结果）、未验证项与原因、置信度。

【红线】
- 不得修改 worktree 里任何文件（不许"顺手修一下"，不许留调试文件；测试产生的临时文件必须清掉）。
- 不得 git commit / checkout / stash 等任何写操作。
- 唯一允许的写入：把报告写到 <报告绝对路径>（每步 review 的正确性 = `<step_dir>/review_r<N>.md`、架构 = `<step_dir>/review_arch_r<N>.md`；整体 review = `<task_dir>/review_overall_r<N>.md` / `review_overall_arch_r<N>.md`；用 Write 起草、用 Edit 增量追加/修订）。Write 与 Edit 除了这个报告文件之外没有任何合法用途——不允许对 worktree、代码、任务目录里其它文件做任何修改。
- **不得再派发任何子 agent**：审查不能外包（不许用 `wing run` / `wing -p` 找别人来看）。审查是你一个人的活。
- 跑测试可以；长驻服务、会改数据库 / 远端状态、要下载大依赖的命令不要跑；跑完确保 `git status` 干净。
```

「你要做的」两选一：

- **正确性**：

  ```
  1. 读 commit（`git log -p <base>..HEAD`、`git show`）与相关代码，判断是否正确、完整、符合验收标准。
  2. 复现执行者声称的验证：跑它列出的命令，看到真实结果——不是相信它的话。
  3. 独立设计并执行额外验证（边界值、异常路径、回归面、兼容性）；可疑点必须构造实验证伪或证实。
  ```

- **架构**（不重复正确性——功能对不对是另一个人的活，只回答结构 / 取舍 / 整洁度）：

  ```
  1. 分层与归属：改动落在正确的包 / 层吗？有没有跨层依赖、绕过门面（路由里写 SQL、UI 层做领域决策）？该登记的地方（test_layering.py 的 FAMILIES / FAMILY_RULES、docs/dev/backend-layout.md、layer_guard.rs、AGENTS.md 结构树）登记了吗？
  2. 最小 diff：逐文件指出「这处改动对目标不是必需的」——顺手重构、重命名、格式化噪声、无关文件、顺手改的注释；能否用更小的改动达成同样目标？
  3. 重复与模板代码：抄了仓库里已有的实现（该复用 / 抽取），还是写出了第 N 份「看起来一样」的逻辑？不能复用的理由成立吗？
  4. 抽象是否恰当：过度抽象（为一次性用途造框架 / 配置 / 参数）与抽象不足（同一逻辑散落多处）都要指出；命名是否表达意图。
  5. 契约与不变量：破坏 AGENTS.md 的不变量或 docs/dev/* 声明的契约（协议形状、事件语义、持久化格式、排序 / 去重口径）了吗？该同步的协议 / 文档同步了吗？
  6. 仓库整洁度（重点）：注释与文档里有没有不该进开源仓库的东西——计划 / 任务书编号、评审分级代号（`[B1]`）、编排代号（executor / scheduler / reviewer）、辩护式注释（「按评审意见改的」「原来 X 会出问题」）、与代码不符的过期注释、只复述代码的无信息注释；commit message 是否夹带过程信息。
  7. 一致性与测试面：新代码与周边风格一致吗（错误处理、日志、命名、目录、测试组织）？新增的公共面都有测试吗？文档只更新了该更新的地方吗？
  ```

`review_request.md`（执行者写，模板见附录 C）必须包含：**审查范围**（worktree 绝对路径 + `<base_sha>..HEAD` + 本步骤拥有的文件清单 + 是否含未提交改动）、**验收标准**（从步骤 proposal 的 Verification 抄，正确性 reviewer 照此复现）、**上下文**（任务书 / design / task 的绝对路径 + 执行者的最终总结）、**审查重点**（正确性：边界、兼容、并发、安全、性能、测试充分性；架构：分层与归属、最小 diff、重复实现、抽象、契约、仓库整洁度、一致性）、**纪律 + 输出格式**（附录 D 的 B/S/N + 结论 + 已验证清单）。

### 8.4 审查结束后核对

```bash
git -C <worktree> status --porcelain    # 必须干净：reviewer 没碰过工作区
```

不干净 → 该轮审查视为无效：`git checkout -- <paths>` 恢复（或 `git stash`），记一笔，必要时换个 reviewer 重审。

### 8.5 读报告并分类

读两份报告（`<step_dir>/review_r<N>.md` 与 `review_arch_r<N>.md`），把两套 B/S/N 摘进 `scheduler_log.md`（视角 + 编号 + 结论 + 是否已处理）。每步的报告在验收复核时读；整体 review 的报告按 §9 走返修。

## 9. 阶段 6 — 返修闭环

**每步的 review 返修在执行者手里**（它自己读报告、自己修、最多 3 轮）；你的返修派发发生在两处：**验收复核不通过**（§8.2）与**整体 review 有 B/S**（§10.3）。

### 9.1 红线

- **B 必须修**（不修不能交付）；**S 必须修**（除非你能给出明确理由不修，理由写进 scheduler_log 与 summary）；两份报告合并处理，任何视角的 B/S 都照此办理；
- 两份报告冲突时，优先级：**用户既定决策 > 正确性 > 架构**；取舍写进 scheduler_log 与 summary；
- **N 由你判断**（顺手批量修可以，也可以记录到 summary 交给用户）；
- **B 修完必须复审**（两个视角各看自己那份的返修点 + 回归面），S 也默认复审；
- **同一 step 最多 3 轮（审查 → 返修）**。超限即判定该 step 阻塞：**不要阻塞与它无关的步骤**，把阻塞点写进 summary，最后向用户报告。

### 9.2 返修派发（resume 原 executor，保留上下文）

```bash
cd <worktree> && wing run --json -r <原 executor sid> \
  -p "$(cat <<'EOF'
【返修任务】审查报告在 <正确性报告绝对路径> 与 <架构报告绝对路径>。请：
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
- `-r` 会忽略 `--tools` / `-m` / `--agent`（工具集、模型与模板沿用原 session；要换模板用 `wing update <sid> --agent <name>`），不要重复传；
- 复审各自新写 `review_r2.md` / `review_arch_r2.md`，**不要覆盖 r1**（审查链是交付证据）；
- 整体 review 的 B/S 若跨多个 step：挑**一个**最相关的 executor 用 `-r` 修（它带着实现上下文），不要让两个 executor 同时改同一批文件（铁律 2）。

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

### 10.3 整体 review（收口前最后一道审查）

每步的 review 只看自己那一步——它会漏掉**跨 step** 的问题。所以全部合进集成分支、全量验收通过之后，**你同一轮并发派两个 reviewer**（正确性 + 架构）看整体：

- **审查对象**：集成分支上的整体 diff（`<base_sha>..HEAD`，跨全部 lane）+ 集成后的全量验证；
- **看什么**（写进 review_request.md）：
  - 正确性：跨 step 的回归（全量测试 / 构建）、接口接缝处（上游产物被下游误用）、任务书 Out of Scope 是否被越界；
  - 架构：跨 step 的一致性（同一件事是否两套实现 / 两个名字）、重复实现、分层与归属、文档与代码是否自相矛盾、注释里有没有过程痕迹 / 不该进开源仓库的东西；
- **派发**：用 §8.3 的两份审查者契约（`--tools "Bash,Read,Glob,Grep,Write,Edit"`、`--tag reviewer` + 视角标签、`--tag task=<task_name>`），报告写 `$WING_HOME/tasks/<task>/review_overall_r<N>.md` / `review_overall_arch_r<N>.md`；`wing wait <sid1> <sid2>` 照 §7.1 放大 timeout；
- **处理**：两份报告的 B/S 一律派执行者用 `-r` 修（§9.2，挑最相关的那个），修完复审写 `review_overall_r2.md` / `review_overall_arch_r2.md`；N 自行取舍；
- 这一轮同样是**最多 3 轮**，超限就把阻塞点写进 summary 报给用户。

### 10.4 push 与 PR（按阶段 1 与用户约定的终态）

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

全自动 ≠ 全程静默：每次状态跃迁（一波派发完 / 一波 wait 完 / 一轮双视角 review 收敛 / 判定某步阻塞）向用户输出**两行以内**的进度，然后立刻继续干活——**只播报，不等待确认**。这样用户随时回来看都知道跑到哪了。

## 12. 命令速查

命令全集、分组表与编排脚本见 [wing-cli](../wing-cli/SKILL.md)；下表只列调度循环里常用的那些。

| 目的 | 命令 | 备注 |
|---|---|---|
| 网关状态 | `wing status` / `wing start` | `wing run` 会自动拉起网关 |
| 派发（非阻塞） | `wing run --json --tools "..." --tag executor --tag task=<task> -p "$(...)"` | **必须先 `cd <worktree>`**；默认登记「收口/提问 → 唤醒我」 |
| 派发时关掉 / 改投唤醒 | `wing run --no-notify` / `--notify-to <sid>` / `--notify-on done,ask` | 目标是 `WING_SESSION_ID`；不在 agent shell 里则静默 off |
| 续跑同一 session | `wing run -r <sid> -p "..."` | 忽略 `--tools` / `-m` / `--agent` |
| 等待（阻塞，显式选择） | `wing wait <sid...> --timeout 3600 --json` | Bash 工具 timeout 要更大（≥3700） |
| 订阅收口 / 提问（显式补登） | `wing watch <sid> [--times N] [--on done,ask]` | 默认配额 1（叫一次就结束）；`ask` 不消耗配额；重复登记 = 刷新同一条订阅（不新增） |
| 看待唤醒 / 取消 | `wing watch --list [--all]` / `wing watch --cancel <id>` | 列出 / 取消**全部**触发器（`KIND` 列区分 session / timer；`wing wake --list/--cancel` 完全等价） |
| 定时自唤醒（心跳） | `wing wake --in 30m -p "..."` / `--every 30m` / `--daily 07:00 --times 7` | 改时间/改 prompt = 取消旧的再建一条 |
| 克隆自己（继承上下文） | `wing fork $WING_SESSION_ID --at current --json` → `wing run -r <克隆 sid> -p "..."` | `wing branches <sid>` 挑节点；克隆是独立会话、更贵 |
| 回答在挂的 Ask | `wing asks <sid> --wait 600 --json` → `wing run -r <sid> --tool-call-id <id> -p "<answer>"` | 在挂 Ask 的 tool_call 不在链上，`tail` 看不到 |
| 会话列表 | `wing ps --json` | 默认过滤 `inactive`（被 pin 的除外）；`--tree` 看血缘树 |
| 打标 / 读标 / 清标 | `wing tag <sid> scheduler task=<task>` / `wing tag <sid> --remove <tag>` / `wing tag <sid>` | 幂等；不唤醒逐出会话；`--json` 可解析 |
| 标签清点 | `wing tag --list` | 全局标签 + 计数（含 inactive） |
| 按标签找会话 | `wing ps --tag <tag> [--tag <tag2>]` | 多个 `--tag` 为 AND；隐含包含 `inactive` |
| 按血缘找会话 | `wing ps --parent <sid>` | 直接子；隐含 `--all` |
| 自己的 session id | `echo $WING_SESSION_ID` | Bash 工具注入的环境变量 = 当前会话 id |
| 单会话状态 | `wing info <sid> --json` | status / workdir / tools / tokens |
| 看消息 | `wing tail <sid> -n 20 -t <type>` | type: all / user / assistant / tool_call / tool_result / reasoning / content |
| 看开头 | `wing head <sid> -n 5 -t user` | 复核你派发时给的 prompt |
| 中断卡死会话 | `wing interrupt <sid>`（别名 `wing int`） | 会话回 idle；半成品保留在上下文里 |
| 缓存账本（查为什么没命中） | `wing cache <sid>` | 分叉归因 / 可避免性 |
| 回收内存 / 归档 | `wing release <sid...>` / `wing archive <sid...>` | 忙碌会话 409；归档是纯移动 |
| 模型 / 工具 / 模板 | `wing models` / `wing tools` / `wing agents` | 排查工具名写错 |
| 配置 | `wing config get/set/list/doctor` | 网关降级（setup mode）时也能用 |

## 13. 陷阱与反模式

1. **Bash timeout 忘了放大** → `wing wait` 被 30s 默认超时杀掉。会话没事，重新 wait 即可，但白费一轮。自派 review 之后单步 30–40 分钟是常态，`--timeout 3600` + Bash timeout 3700 起步。
2. **忘了 `cd` 到 worktree** → 子 agent 在错误目录里改代码。用 `wing info <sid>` 的 `workdir` 兜底核对。
3. **同一 worktree 派两个 executor** → 互相覆盖、git index 冲突、review 范围不可分。并行必须多 worktree。
4. **把设计写死** → 步骤 proposal 里写"怎么做"，executor 就只会照抄、遇到现实立刻卡住。proposal 管「做什么 / 怎么算完成」，design 管「怎么做」。
5. **不给绝对路径** → 子 agent 的 workspace 是 worktree，相对路径全部会歪。所有文档引用一律绝对路径。
6. **不登记 session** → 任务一大就不知道谁是谁、哪条 lane 在跑什么。派发即写 log。
7. **reviewer 顺手改代码 / 只派一个** → 审查失效或漏掉整个架构视角。每次审查后用 `git status --porcelain` 兜底；同一轮并发派两个、报告分开命名。
8. **直接相信 executor 的"验证通过"** → reviewer 必须复现命令，你也要抽查（尤其最后一轮全量验收）。
9. **给每个 lane 开 PR** → 用户要的是单一交付物：先合集成分支，再开一个 PR。
10. **`git add -A`** → 把无关文件带进 commit。契约里要求精确 add。
11. **让 executor push** → 明确禁止；push / 建 PR 是 scheduler 的专属权限。
12. **任务文档写进 worktree** → 污染 diff 与 review 范围。任务目录永远在 `$WING_HOME/tasks/`。
13. **设 `--max-turns`** → 任务中途夭折。不设。
14. **用 sleep 轮询代替 `wing wait` / 唤醒** → 又慢又费 token；要等就 `wing wait`（幂等），不想等就用 `wing watch` / `wing run` 的 notify，让会话自己被叫醒。
15. **交付前不查 `git status`** → 临时脚本、调试文件混进交付物。
16. **让子 agent 再派子 agent（外包）** → 执行者只剩一轮 review 的派发权，reviewer 一次都没有：审查不能外包，活也不能外包。契约里写死，验收时看 `wing ps --parent <sid>` 有没有多出来的孙会话。

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

### 附录 C — `review_request.md`（审查要求，执行者写）

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
（本步骤的风险点。正确性：边界、兼容、并发、安全、性能、测试充分性…；架构：分层与归属、最小 diff、重复实现、抽象、契约、仓库整洁度、一致性…）

## 纪律
- 只读；可跑测试；不得修改 worktree 中任何文件
- 唯一允许的写入：各自那份报告——正确性 `<.../<NN>_<step>/review_r<N>.md>`、架构 `<.../<NN>_<step>/review_arch_r<N>.md>`
- 跑完确保 `git status --porcelain` 干净

## 输出格式
（见附录 D：B/S/N 分级 + 结论 + 已验证清单）
```

### 附录 D — 审查报告（`review_r<N>.md` / `review_arch_r<N>.md`）

两个视角用同一模板，只有文件名与结论口径不同：正确性写 `review_r<N>.md`、结论针对功能与验收；架构写 `review_arch_r<N>.md`、结论只针对结构维度（写「架构结论」）。

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
- 并发策略：按任务与资源自定（不设固定上限；当前在飞：…）
- 交付终态：<与用户约定>

## Session 台账
| sid | 角色 | step | lane | 派发时间 | 状态 | 结论 / 备注 |
|---|---|---|---|---|---|---|
| 2026… | executor | 01_xxx | l1 | 09-17 15:02 | idle | 交付 3 commit + 自派双视角 review（review_r1 / review_arch_r1） |
| 2026… | reviewer-correctness（executor 自派） | 01_xxx | l1 | 09-17 16:30 | idle | 执行者回执 → 我补记：r2 通过，B2/S1 已修，见 review_r2.md |
| 2026… | reviewer-arch（executor 自派） | 01_xxx | l1 | 09-17 16:30 | idle | 执行者回执 → 我补记：r2 通过，S3 已修，见 review_arch_r2.md |

## 时间线
| 时间 | 事件 | 对象 | 详情 |
|---|---|---|---|
| 09-17 15:02 | dispatch | 01_xxx / l1 | sid=2026…；prompt 摘要：… |
| 09-17 16:12 | wake | 01_xxx / l1 | 收到 [wing] finished（subtype=success，41 轮，3900s） |
| 09-17 16:15 | accept | 01_xxx / l1 | 交付复核通过；两份自派 review 结论=通过（r2，B2/S1 已修） |
| 09-17 16:20 | merge | L1 → integration | `merge: <task> l1` |
| 09-17 17:02 | review | overall r1（正确性 + 架构） | sid=2026… / 2026…；结论：需返修（S2） |

## 决策记录
- （为什么这么拆、为什么串行 / 并行、为什么不修某个 S、为什么开新 session…）
```
