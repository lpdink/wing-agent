# 开发者深度参考（docs/dev）

本目录是 [`AGENTS.md`](../../AGENTS.md) 的渐进式披露补充。AGENTS.md 保持高信息密度的总览；当你需要机制级细节（数据如何流动、每个端点做什么、某个概念到底指什么）时，再来这里。

| 文档 | 内容 |
|------|------|
| [architecture.md](architecture.md) | 三层架构与数据流、TUI / stdio 两种前端、Goal 编排、会话生命周期、持久化与压缩 |
| [http-api.md](http-api.md) | 完整 HTTP 端点表、WebSocket 事件协议、Gateway 鉴权 |
| [glossary.md](glossary.md) | 核心概念速查：SessionStore / MessageLog / TrackedList、工具命名空间、prompt 命令、压缩等 |

> 事实来源优先级：**代码 > 本目录 > AGENTS.md 概述**。若发现不一致，以代码为准并欢迎修正文档。
>
> 许多设计决策的 *why* 记录在对应 PR 的 body 中，可用 `gh pr view <number>` 查阅。关键 PR：#1(stdio) · #9(HTTP 化) · #10(HTTP 生命周期) · #14(去 magic dispatch) · #22(Goal) · #34(工具命名空间) · #35(鉴权) · #39(SessionStore)。
