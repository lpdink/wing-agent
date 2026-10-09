# TUI 终端状态信号（OSC 0 / OSC 9 / OSC 7501）

一句话：wing 向前端终端发出三类状态信号——**人看**的窗口标题（OSC 0）、**一次性**桌面通知（OSC 9）、**机器读**的程序状态记录（OSC 7501）。三者同源于同一份 App 状态，区别只在读者与语义。

| 信号 | 读者 | 语义 | 代码 |
|------|------|------|------|
| OSC 0 标题 | 屏幕前的人 / 第三方启发式（读标题猜状态） | 单槽字符串：`☾/⠋/❓/⚠/✓ wing [dir]`，working 时每个 spinner 帧重写 | `util/title.rs`，接线在 `app/` |
| OSC 9 通知 | 桌面通知（人） | **一次性事件**：回合结束摘要（turns/耗时/token + 结果首行），仅失焦时发 | `util/osc9.rs` + `notify_unfocused` |
| OSC 7501 记录 | 终端 / agent 面板 / inbox（机器） | **当前事实**：`idle/working/blocked/done/error`，可带 `kind`/`msg` | `util/program_status.rs` + `app/` 接线 |

OSC 7501 = [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)（Mitchell Hashimoto 起草，2026-10 发布）：程序用 `ESC ] 7501 ; state=…:app=…:msg=<base64> ST` 告诉终端自己在干什么；终端/面板自行决定怎么展示（标签页角标、通知、未读清单）。wing 只发**根记录**、`app=wing`。它解决的是标题通道解决不了的问题：机器读标题需要**每个消费者**为每个程序手写正则（读标题/屏幕猜测），而协议是机器可读、一次发射全生态通用。

## 状态映射（`app/projection.rs` 等）

| wing 事件 / 状态 | 报告 |
|------|------|
| 启动 | `idle` |
| `TurnStarted` | `working` |
| 交互式 Ask 面板（`AskUserQuestion`） | `blocked:kind=question:msg=<问题文本>` |
| 交互式 Ask 面板（`RequiredChoice`，即旧 bash 危险命令确认） | `blocked:kind=permission:msg=<问题文本>` |
| 面板被回答且队列空 | `working`（回合继续；队列非空则仍 `blocked` 在下一个） |
| `TurnResult`（成功/失败） | `done` / `error`，`msg` = OSC 9 的同一条摘要 |
| `WingEvent::Error` | `error`，`msg` = 错误文本（比 TurnResult 更具体） |
| `Interrupted` | `idle`（spec：被打断报 idle，不报 done） |
| `Done` 时记录仍停在 `working/blocked` | `idle`（`turn_result` 丢失时的兜底；`done/error` 不被覆盖） |
| `SyncSession`（订阅 / 会话切换 / 重连） | 以快照重投影：重放的面板队列 → `blocked`，回合在飞 → `working`，否则 `idle` |
| 退出 | `clear`（直接写终端，不走 intent） |

## 设计决策

- **盲发、不做特性探测**。spec 明确：规范终端必须忽略未知 OSC，探测是可选的（pi / Claude Code 做了探测，是优化不是正确性要求）。探测要读终端应答，TUI 里得和图片探测、crossterm 事件流抢 stdin；不值。代价只是对不支持它的终端多发几个字节。
- **去重按整条已格式化序列**（`Reporter`）。记录是幂等快照，同一 `(state, msg)` 连发即 noop——spinner 每帧不会刷记录，重复的 `turn_started` 也不会。
- **`msg` 只放"公开摘要"**：与 OSC 9 同源（回合摘要 / 面板问题原文），会被终端与面板显示在网格之外，绝不放会话正文。经过：控制字符→空格、trim、UTF-8 边界截断（解码 ≤2048B，编码 ≤2732B，整串 <4096B——spec 硬上限）。
- **`done` 的存续交给终端**：`done/error` 在下一个 `working/idle/clear` 之前一直有效；终端自己决定何时停止展示（如用户按键）。因此 `Done` 事件不会把 `done` 覆写回 `idle`。
- **不做焦点门控**：OSC 9 只在失焦时发（通知=打断），7501 无条件发（展示交给终端，失焦与否由终端自己知道）。
- **开关**：`WING_PROGRAM_STATUS=0|false|off` 关闭（默认开，`Reporter::from_env` 只读一次）。

## 已知边界

- **只有 TUI 形态接入**。stdio（`wing -p`）的 stdout 是 NDJSON 协议流，不能写转义；ACP 无终端。要覆盖 stdio 需走 stderr/TTY 判断，未做。
- **终端侧今天只有 Rex 显示**（macOS beta）；libghostty 已解析（[ghostty#14560](https://github.com/ghostty-org/ghostty/pull/14560)），Ghostty app 暂不消费。这层信号是"为消费方就绪"的前置投入。
- **压缩（`/compact`）期间不报 `working`**：失败路径没有干净的回位钩子（失败只落一条通用 toast），宁可不报也不留悬挂的 working。
- **`SyncSession` 重写记录**：重连/切会话时 `done` 会被快照的真实状态（多为 `idle`）覆盖——"未读结果"的存续让位于"当前事实"。
- 透传取决于中间层：tmux 等会丢弃未知 OSC（安全，但也不显示）。
