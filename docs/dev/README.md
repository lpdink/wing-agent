# 开发者深度参考（docs/dev）

本目录是 [`AGENTS.md`](../../AGENTS.md) 的渐进式披露补充。AGENTS.md 保持高信息密度的总览；当你需要机制级细节（数据如何流动、每个端点做什么、某个概念到底指什么）时，再来这里。

| 文档 | 内容 |
|------|------|
| [architecture.md](architecture.md) | 三层架构与数据流、TUI / stdio / 编排 CLI 三种前端形态、会话生命周期、持久化与压缩 |
| [backend-layout.md](backend-layout.md) | 后端分层规范（`libs/core/wing/**`）：分层图与依赖方向、每包职责一句话、迁移映射表、分层守门测试（`test_layering.py`）与白名单 |
| [http-api.md](http-api.md) | 完整 HTTP 端点表、WebSocket 事件协议、Gateway 鉴权 |
| [glossary.md](glossary.md) | 核心概念速查：SessionStore / MessageLog / TrackedList、工具命名空间、prompt 命令、压缩等 |
| [config-logging.md](config-logging.md) | WING_HOME 布局、config.yaml 顶层键、TUI 配置、日志轮转与查询、环境变量 |
| [media-images.md](media-images.md) | 媒体与图片（read-image）：ReadImage 工具、内容寻址媒体池、模型能力声明、请求期图片投影（高水位 + 量子批量驱逐）与 KV/前缀 cache |
| [tui-rendering.md](tui-rendering.md) | Markdown 渲染的调试入口（`render_probe`）、`Content`/`Thinking` 两个 profile 的差异、流式==终态的不变量与对账、已知边界清单 |
| [tui-images.md](tui-images.md) | 终端图片能力的两档阶梯、探测与配置、三态、资源上限与压力验证、新鲜度（重写同一路径 ≤1s 换图）、失效触发点、性能数字、真机验收清单 |
| [tui-input.md](tui-input.md) | TUI 输入通道：键盘/鼠标/滚轮的上报模式（DECSET）与生命周期对称、选择与滚动的指针语义 |
| [tui-signals.md](tui-signals.md) | TUI 终端状态信号（OSC 0 标题 / OSC 9 通知 / OSC 7501 程序状态）：读者分工、状态映射、盲发与去重决策、已知边界 |
| [vscode-extension.md](vscode-extension.md) | VSCode 扩展：四层分层与数据流、桥协议与归约（重放==直播）、会话时序与多 Tab、连接自愈、构建/测试/smoke、打包与安装 |
| [probe-testing.md](probe-testing.md) | wing-probe 确定性集成测试：跑法（`make test-probe`）、新增场景、断言原语、红线清单与口径、逃生舱 |
| [welcome-mascot.md](welcome-mascot.md) | 开屏海鸥：字母网格帧与品牌调色板、待机/干活两姿态、可见性门控的重绘成本契约、改画工作流 |

> 事实来源优先级：**代码 > 本目录 > AGENTS.md 概述**。若发现不一致，以代码为准并欢迎修正文档。
>
> 许多设计决策的 *why* 记录在对应 PR 的 body 中，可用 `gh pr view <number>` 查阅。关键 PR：#1(stdio) · #9(HTTP 化) · #10(HTTP 生命周期) · #14(去 magic dispatch) · #34(工具命名空间) · #35(鉴权) · #39(SessionStore) · #43(流式工具渲染) · #47(远程工具注册) · #49(远程工具 SDK) · #50(动态工具切换) · #52(中断提交) · #53(WingAgent 拆包)。
