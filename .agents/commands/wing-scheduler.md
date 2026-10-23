---
name: wing-scheduler
description: 超大型任务的无人值守交付编排（scheduler 模式）。把一个大任务拆成步骤与波次，在独立 worktree 上并发派发 headless 子 agent（executor）执行——执行者交付前自派一轮双视角独立审查（正确性 + 架构并发，B/S/N 分级），全部收口后 scheduler 再派一轮整体审查——最后由 scheduler 集成、push 并产出单一 PR。当用户要求「实施/交付一个大型任务」「拆开并行做掉」「无人值守把这件事做完」「多子 agent 协同开发」，或任务明显需要多波次、多 worktree、跨模块大范围改动时使用。（用法：/wing-scheduler <任务描述>）
---

# Wing Scheduler — 入口

本文件只是 `.agents/skills/wing-scheduler/SKILL.md` 的入口镜像（斜杠命令名 → skill 路径）。
**唯一事实来源是那份 skill**：并发、review 归属、等待窗口、命令速查、红线，全部以它为准，本文不复制。
