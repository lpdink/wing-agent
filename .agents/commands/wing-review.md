---
name: wing-review
description: 小改动的独立审查（非 scheduler）。把已提交的改动交给 headless 子 agent 做对抗式审查——最小上下文、只读工具、B/S/N 分级报告；读完报告修 B/S、判断 N，再由你 push / 开 PR。当用户说「派个 AI 审查一下」「把这几个提交 review 一下」「开发完了检查一遍」且改动规模较小（1~3 个分支 / worktree，不需要多波次编排）时使用。（用法：/wing-review）
---

# Wing Review — 小改动的独立审查

一句话：**改动已 commit 但未 push → 派一个一无所知的审查员（headless 子 agent）→ 读报告 → 修一轮 → 你 push / 开 PR。**

本 skill 是 wing-scheduler 审查段落的精简版：没有拆波次 / 台账 / 集成，只保留「对抗式审查」这一件事。依赖 `wing run` / `wing wait` / `wing info` / `wing tail` 与普通 shell / git 命令。

## 何时用 / 何时不用

**用**：

- 改动已提交（未推送），1~3 个分支 / worktree 的规模；
- 核心链路、用户会 review、或用户明确点名「找人审一下」；
- 想要一双全新的眼睛：审查员**不给任务上下文**（不知道背景、不知道谁写的、不知道为什么这么改），只看提交与代码——对抗式找问题。

**不用**：

- 大型多波次任务 → 用 wing-scheduler；
- 改动还没 commit / 还在探索 → 先收敛；
- 纯文案 typo → 自己看，不值得开 session。

## 派发（三步）

### 1. 写最小 prompt

只给**范围 + 纪律 + 输出格式**，不给任务背景（刻意的：上下文越少，越不会顺着你的假设走）。模板：

```
【角色】你是独立审查者（reviewer）。立场：默认提交有问题，用证据说话。没有人类在线，不要提问，遇到不确定自行决策。
【任务】非常简单——你只需要在终端里用 git 命令看提交、读代码，然后敏锐地识别其中的问题。审查以下 worktree 里的提交（各自基于 <base_sha> 之上）：
1. <worktree 绝对路径>
2. …（每个分支一行）
逐个看：每个提交改了什么、改得对不对——正确性、边界情况、回归面、测试充分性，以及任何你认为不对劲、不严谨、有隐患的地方。
【纪律】只读：不要修改任何 worktree 里的文件；不要 git commit / checkout / stash / reset 等任何写操作。若运行测试，跑完必须确保对应 worktree 的 `git status --porcelain` 干净。
【输出】把结论写在最终消息里：按严重度分级列出问题（[B] 阻塞 / [S] 建议 / [N] nit），每条给出：文件:行、问题描述、证据或复现方式、修复建议。末尾附：已验证项与本轮的整体置信度。
```

要点：

- **base 必须给**（`<base_sha>` / 基线分支）：没有 base 就没有 review 范围；
- 多分支一次审：一个 reviewer 横向看全部 worktree（互相印证更容易看出问题）；分支之间无关也可以各派一个（在飞总数 ≤3）；
- **工具集只读**：`--tools "Bash,Read,Glob,Grep"`——不给 Write / Edit，报告写在最终消息里（零落盘、零污染）；
- prompt 先落盘（`/tmp/reviewer_prompt.md` 之类），便于事后复盘你发了什么。

### 2. 派发

```bash
cd <能看见全部 worktree 的父目录> && wing run --json --tools "Bash,Read,Glob,Grep" -p "$(cat <prompt 文件>)"
# → {"session_id":"...", ...}
```

- `cd` 决定 session 的 workspace——选 worktree 的**集散目录**（审查员要靠绝对路径进各 worktree）；
- 派发后核对：`wing info <sid> --json` 看 `workdir` / `tools` 是否符合预期；
- `wing run` 创建的 session 强制 yolo（无人值守），不要也不该附加额外工具。

### 3. 等待

```bash
wing wait <sid> --timeout 1800 --json > /tmp/review_wait.json
```

- **Bash 工具的 timeout 必须放大**（`wait --timeout + 200` 起步）——默认 30s 会把 wait 进程杀掉；会话不受影响，wait 幂等可重等；
- 报告在 `results[0].result`；等 `status=idle` 再读；异常（`is_error` / `subtype`）先 `wing tail <sid> -t tool_call` 看现场。

## 读报告与处置

| 分级 | 处置 |
|---|---|
| `[B]` 阻塞 | **必修**（正确性 / 验收标准 / 安全 / 数据损坏） |
| `[S]` 建议 | **必修**；不修必须给出明确理由，写进汇报与 PR |
| `[N]` nit | 自行判断：顺手修 或 记录不修 |

- 与**用户既定决策冲突**的 nit（文案、既定取舍）：保留原案，汇报里写明「评审建议 X，与既定口径冲突，保留待用户定夺」；
- 修复：在对应 worktree 改 → 跑该 worktree 的 check / test → **追加一个说明清楚的 commit**（message 写出修了哪些项；"审查→修复"两个 commit 是过程证据，不要 amend / force）；
- 修完复查纪律：`git -C <worktree> status --porcelain` 必须干净（reviewer 跑过测试可能弄脏工作区——如 `uv.lock` 被镜像源重写，`git restore` 恢复并记一笔）；
- **默认一轮，不请复审**；B 的修复面很大时才考虑二轮（只审返修点 + 回归面）。

## 收尾

- push / 开 PR 仍是你的活（仓库规范：commit message 英文、PR 正文中文）；
- PR 的 Known Limitations 写清「哪些评审意见没修、为什么」；
- 向用户汇报一张表：**分级 → 每条处置（修了 / 不修 + 理由）**——结论先行，细节在后。

## 坑（源自实战）

- **wait 超时不放大**：30s 默认超时会杀掉 wait 进程，白等一轮；
- **忘了 cd 到集散目录**：审查员的 workspace 不对，相对路径全歪（所有引用一律绝对路径）；
- **给了任务背景**：对抗性就没了——让它自己从 commit 得出结论；
- **给 Write / Edit**：报告散落、工作区被污染；最终消息承载报告即可；
- **不复查工作区**：审查员跑测试可能留下脏状态——`git status --porcelain` 是每次审查后的必查项。
