---
name: wing-review
description: 小改动的双视角独立审查（非 scheduler）。把已提交的改动交给两个并发的 headless 子 agent 做对抗式审查（正确性 + 架构）——最小上下文、只读工具（仅可写自己的报告）、B/S/N 分级报告；读完两份报告修 B/S、判断 N，再由你 push / 开 PR。当用户说「派个 AI 审查一下」「把这几个提交 review 一下」「开发完了检查一遍」且改动规模较小（1~3 个分支 / worktree，不需要多波次编排）时使用。
category: Workflow
tags: [review, headless, quality-gate]
---

# Wing Review — 小改动的双视角独立审查

一句话：**改动已 commit 但未 push → 同一轮并发派两个一无所知的审查员（正确性 / 架构）→ 读两份报告 → 修一轮 → 你 push / 开 PR。**

两个视角：**正确性**审「这个改动对不对」（功能、边界、回归面、测试）；**架构**审「这个改动该不该长成这样」（分层与归属、最小 diff、重复与模板代码、抽象、契约与不变量、仓库整洁度、一致性）。两者并发、各写各的报告。

本 skill 是 wing-scheduler 审查段落的精简版：没有拆波次 / 台账 / 集成，只保留「对抗式审查」这一件事。依赖 `wing run` / `wing wait` / `wing info` / `wing tail` 与普通 shell / git 命令；审查者契约的完整骨架见 [wing-scheduler §8.3](../wing-scheduler/SKILL.md)；两边内联的七条清单逐字一致，改一处要改两处。

## 何时用 / 何时不用

**用**：

- 改动已提交（未推送），1~3 个分支 / worktree 的规模；
- 核心链路、用户会 review、或用户明确点名「找人审一下」；
- 想要两双全新的眼睛：审查员**不给任务上下文**（不知道背景、不知道谁写的、不知道为什么这么改），只看提交与代码——对抗式找问题。

**不用**：

- 大型多波次任务 → 用 wing-scheduler；
- 改动还没 commit / 还在探索 → 先收敛；
- 纯文案 typo → 自己看，不值得开 session。

## 派发（三步，两个视角并发）

### 1. 写两个最小 prompt

共用骨架（审查对象 + 纪律 + 输出格式，两个视角逐字共用），只有「角色」与「你要做的」按视角不同——**不给任务背景**（刻意的：上下文越少，越不会顺着你的假设走）。正确性模板：

```
【角色】你是独立审查者（reviewer），本次只审正确性。立场：默认提交有问题，用证据说话。没有人类在线，不要提问，遇到不确定自行决策。
【审查对象】worktree（各自基于 <base_sha> 之上，逐个列出）：
1. <worktree 绝对路径>
2. …（每个分支一行）
【你要做的】用 git 命令逐个看每个提交改了什么、改得对不对——正确性、边界情况、回归面、测试充分性，以及任何你认为不对劲、不严谨、有隐患的地方。
【纪律】只读：不要修改任何 worktree 里的文件；不要 git commit / checkout / stash / reset 等任何写操作。若运行测试，跑完必须确保对应 worktree 的 `git status --porcelain` 干净。不得再派发任何子 agent（审查不能外包）。
【输出】把结论写进 <报告绝对路径>（Write 起草、Edit 增量改，这是 Write / Edit 唯一的合法用途）：按严重度分级列出问题（[B] 阻塞 / [S] 建议 / [N] nit），每条给出：文件:行、问题描述、证据或复现方式、修复建议。末尾附：结论（通过 / 需返修）、已验证项与本轮的整体置信度。
```

架构模板同骨架（【审查对象】【纪律】【输出】原样），角色行换成「本次只审架构（结构 / 取舍 / 整洁度）；功能对不对不是你的判断对象——另一个 reviewer 正在并行审正确性」，【你要做的】换成这七条（与 wing-scheduler §8.3 的清单逐字相同，改一处要改两处）：

```
1. 分层与归属：改动落在正确的包 / 层吗？有没有跨层依赖、绕过门面（路由里写 SQL、UI 层做领域决策）？该登记的地方（test_layering.py 的 FAMILIES / FAMILY_RULES、docs/dev/backend-layout.md、layer_guard.rs、AGENTS.md 结构树）登记了吗？
2. 最小 diff：逐文件指出「这处改动对目标不是必需的」——顺手重构、重命名、格式化噪声、无关文件、顺手改的注释；能否用更小的改动达成同样目标？
3. 重复与模板代码：抄了仓库里已有的实现（该复用 / 抽取），还是写出了第 N 份「看起来一样」的逻辑？不能复用的理由成立吗？
4. 抽象是否恰当：过度抽象（为一次性用途造框架 / 配置 / 参数）与抽象不足（同一逻辑散落多处）都要指出；命名是否表达意图。
5. 契约与不变量：破坏 AGENTS.md 的不变量或 docs/dev/* 声明的契约（协议形状、事件语义、持久化格式、排序 / 去重口径）了吗？该同步的协议 / 文档同步了吗？
6. 仓库整洁度（重点）：注释与文档里有没有不该进开源仓库的东西——计划 / 任务书编号、评审分级代号（`[B1]`）、编排代号（executor / scheduler / reviewer）、辩护式注释（「按评审意见改的」「原来 X 会出问题」）、与代码不符的过期注释、只复述代码的无信息注释；commit message 是否夹带过程信息。
7. 一致性与测试面：新代码与周边风格一致吗（错误处理、日志、命名、目录、测试组织）？新增的公共面都有测试吗？文档只更新了该更新的地方吗？
```

输出格式与正确性相同（B/S/N + 结论 + 已验证 + 置信度），结论只针对架构维度。

要点：

- **base 必须给**（`<base_sha>` / 基线分支）：没有 base 就没有 review 范围；
- 多分支一次审：一对 reviewer（两个视角）横向看全部 worktree（互相印证更容易看出问题）；分支之间无关也可以各派一对并行审，别让它们互相等（并发由你判断，不设固定上限）；
- **工具集写死**：`--tools "Bash,Read,Glob,Grep,Write,Edit"`——只读 + 唯一允许的写入是自己那份报告；
- 两份报告**分开落盘、互不覆盖**（正确性 `review_r<N>.md`、架构 `review_arch_r<N>.md`，同一父目录），且必须在**被审 worktree 之外**——报告进 worktree 就是把工作区弄脏；
- prompt 先落盘（`/tmp/review-<name>/correctness.md` / `arch.md` 之类），便于事后复盘你发了什么。

### 2. 派发（两个命令，同一轮）

```bash
cd <能看见全部 worktree 的父目录>
SID1=$(wing run --json --no-notify \
  --tools "Bash,Read,Glob,Grep,Write,Edit" \
  --tag reviewer --tag reviewer-correctness --tag task=<改动名> \
  -p "$(cat <正确性 prompt 文件>)" | jq -r .session_id)
SID2=$(wing run --json --no-notify \
  --tools "Bash,Read,Glob,Grep,Write,Edit" \
  --tag reviewer --tag reviewer-arch --tag task=<改动名> \
  -p "$(cat <架构 prompt 文件>)" | jq -r .session_id)
# → 两个 sid；下一步一起 wing wait "$SID1" "$SID2"
```

- `cd` 决定 session 的 workspace——选 worktree 的**集散目录**（审查员要靠绝对路径进各 worktree）；
- 两条命令紧挨着发，第二条不会等第一条——这就是「同一轮并发」；`--tag reviewer-correctness` / `--tag reviewer-arch` 用于事后按视角找回；
- **`--no-notify`**：你反正要阻塞等它们，别让它们的收口再往你这里投一条唤醒消息（notify 语义见 [wing-cli](../wing-cli/SKILL.md) 的 `wing run` 一节）；
- 派发后核对：`wing info <sid> --json` 看 `workdir` / `tools` 是否符合预期（两个都核）；
- `wing run` 创建的 session 强制 yolo（无人值守），不要也不该附加额外工具。

### 3. 等待（两个一起等）

```bash
wing wait <sid1> <sid2> --timeout 3600 --json > /tmp/review_wait.json
```

- `wing wait` 接受多个 sid，全部 idle 才返回；**Bash 工具的 timeout 必须更大**（如 `--timeout 3600` + 工具 3700）——默认 30s 会把 wait 进程杀掉；会话不受影响，wait 幂等可重等；
- 结果按 `session_id` 对上是哪个视角；报告在各目的文件里（`results[].result` 是收口消息，应指向报告路径）；异常（`is_error` / `subtype`）先 `wing tail <sid> -t tool_call` 看现场。

## 读报告与处置

| 分级 | 处置 |
|---|---|
| `[B]` 阻塞 | **必修**（正确性 / 验收标准 / 安全 / 数据损坏） |
| `[S]` 建议 | **必修**；不修必须给出明确理由，写进汇报与 PR |
| `[N]` nit | 自行判断：顺手修 或 记录不修 |

- **两份都要读、都要处置**：只看正确性那份 = 白派了一个 reviewer；只看架构那份 = 没人验功能；
- 两份报告冲突时，优先级：**用户既定决策 > 正确性 > 架构**；取舍写进汇报；
- 与**用户既定决策冲突**的 nit（文案、既定取舍）：保留原案，汇报里写明「评审建议 X，与既定口径冲突，保留待用户定夺」；
- 修复：在对应 worktree 改 → 跑该 worktree 的 check / test → **追加一个说明清楚的 commit**（message 写明两份报告各自修了哪些项；"审查→修复"两个 commit 是过程证据，不要 amend / force）；
- 修完复查纪律：`git -C <worktree> status --porcelain` 必须干净（reviewer 跑过测试可能弄脏工作区——如 `uv.lock` 被镜像源重写，`git restore` 恢复并记一笔）；
- **默认一轮，不请复审**；B 的修复面很大时才考虑二轮（两个视角都只看返修点 + 回归面，报告写 `review_r2.md` / `review_arch_r2.md`，不覆盖 r1）。

## 收尾

- push / 开 PR 仍是你的活（仓库规范：commit message 英文、PR 正文中文）；
- PR 的 Known Limitations 写清「哪些评审意见没修、为什么」；
- 向用户汇报一张带「视角」列的表：**视角（sid）→ 分级 → 每条处置（修了 / 不修 + 理由）**——结论先行，细节在后；两个 sid、两份报告、两套 B/S/N 都写进给用户的汇报。

## 坑（源自实战）

- **wait 超时不放大**：30s 默认超时会杀掉 wait 进程，白等一轮；两个 sid 一起等，别一个一个等；
- **只派一个 / 两个串行**：漏掉整个架构视角，或者白等一倍时间——同一轮并发发两条 `wing run`；
- **报告重名 / 写进 worktree**：两份报告互相覆盖，或者把被审 worktree 弄脏——命名分开、路径必须在 worktree 之外；
- **忘了 cd 到集散目录**：审查员的 workspace 不对，相对路径全歪（所有引用一律绝对路径）；
- **给了任务背景**：对抗性就没了——让它自己从 commit 得出结论；
- **给 Write / Edit 不设边界**：报告之外的写入一律不允许；跑完 `git status --porcelain` 是每次审查后的必查项。
