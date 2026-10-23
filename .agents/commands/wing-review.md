---
name: wing-review
description: 小改动的双视角独立审查（非 scheduler）。把已提交的改动交给两个并发的 headless 子 agent 做对抗式审查（正确性 + 架构）——最小上下文、只读工具（仅可写自己的报告）、B/S/N 分级报告；读完两份报告修 B/S、判断 N，再由你 push / 开 PR。当用户说「派个 AI 审查一下」「把这几个提交 review 一下」「开发完了检查一遍」且改动规模较小（1~3 个分支 / worktree，不需要多波次编排）时使用。（用法：/wing-review）
---

# Wing Review — 入口

本文件只是 `.agents/skills/wing-review/SKILL.md` 的入口镜像（斜杠命令名 → skill 路径）。
**唯一事实来源是那份 skill**：派发、纪律、B/S/N 处置与红线，全部以它为准，本文不复制。
