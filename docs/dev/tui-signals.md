# TUI 终端状态信号（OSC 0 / OSC 9 / OSC 7501）

一句话：wing 向前端终端发出三类状态信号——**人看**的窗口标题（OSC 0）、**一次性**桌面通知（OSC 9）、**机器读**的程序状态记录（OSC 7501）。三者同源于同一份 App 状态，区别只在读者与语义。

| 信号 | 读者 | 语义 | 代码 |
|------|------|------|------|
| OSC 0 标题 | 屏幕前的人 / 第三方启发式（读标题猜状态） | 单槽字符串：`☾/⠋/❓/⚠/✓ wing [dir]`，working 时每个 spinner 帧重写 | `util/title.rs`，接线在 `app/` |
| OSC 9 通知 | 桌面通知（人） | **一次性事件**：回合结束摘要（turns/耗时/token + 结果首行），仅失焦时发 | `util/osc9.rs` + `notify_unfocused` |
| OSC 7501 记录 | 终端 / agent 面板 / inbox（机器） | **当前事实**：`idle/working/blocked/done/error`，可带 `kind`/`msg` | `util/program_status.rs` + `app/` 接线 |

OSC 7501 = [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)（Mitchell Hashimoto 起草，2026-10 发布）：程序用 `ESC ] 7501 ; state=…:app=…:msg=<base64> ST` 告诉终端自己在干什么；终端/面板自行决定怎么展示（标签页角标、通知、未读清单）。wing 只发**根记录**、`app=wing`。它解决的是标题通道解决不了的问题：机器读标题需要**每个消费者**为每个程序手写正则（读标题/屏幕猜测），而协议是机器可读、一次发射全生态通用。

光标转义不在这三类之内——它由帧收尾（ratatui）写出，与会话状态无关，见文末
[「光标转义：只发变化」](#光标转义只发变化tuicursorrs)。

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
| 退出 | 不写任何报告（见设计决策） |

## 光标转义：只发变化（`tui/cursor.rs`）

光标不在上表之列，但它同样是一族终端状态，而且它的写者不是 App 而是 **ratatui 的帧收尾**：
`Terminal::draw` 每帧无条件 `show_cursor` / `hide_cursor` + `set_cursor_position`，而
`CrosstermBackend` 把三条都实现成 `execute!`（自带 flush）。于是**一帧会切成两次 write**：
先是「diff + SGR 复位 + `?25h`」（此刻物理光标停在 diff 最后一个被改写的格子上、而且可见，
是一个完整可呈现的画面），然后才是把光标搬回落点的 `CUP`。终端可以在这两次 write 之间出帧
——开屏扫光期（≈25fps、diff 全在牌子上、离 composer 光标十几行）看到的就是「光标在动画区域
乱闪」。

`tui::CursorGate`（装在 `CrosstermBackend` 外面的 backend 装饰，唯一装配点是
`tui::init_terminal`）把光标命令从「每帧声明」改成「只在变化时声明」：

| 契约 | 规则 |
|---|---|
| `?25h` / `?25l` | 可见性是显式状态，本装饰器是**参与这套状态机的**唯一写者：只在「未知→某值」与「值→另一个值」时发。`enter_sequence` / `leave_sequence` 只在段边界出现、不参与状态机（前者直写 raw stdout，后者经 `Write` 走本装饰器但不动可见性状态）。 |
| `CUP` | 只在目标 ≠ **已知**物理位置时发。 |
| 位置何时作废 | 写了任何一格 diff（后端对相邻格省略 `MoveTo`、宽字符前进量由终端裁定）、`clear_region`（`Terminal::clear` / `resize` 的必经之路）、`clear`、`append_lines`、终端尺寸变化（重排）、以及**走 `Write` 的原始写**（绕过 `Backend`：OSC 标题 / 通知 / 程序状态 / 剪贴板复制（OSC 52）/ `leave_sequence`）。作废后下一帧必发 `CUP`。 |
| 覆盖范围 | 只限经过本装饰器的字节：`tui/clipboard_protocol.rs` 的协议机直写 stdout（OSC 5522，不移动光标），不经过这里、也就不在这次去重之内。 |
| 红线 | **物理位置未知时不得省 `CUP`**：真实光标是输入法候选窗的锚，位置错了就是硬伤。所以"已知"只认装饰器自己设置过的位置，宁可多发一次。 |

稳定态因此是：**一帧 = 一次 write**（diff 与 `CUP` 同批，终端不可能看到半截状态），内容没
变化的那一帧连光标字节都没有（只剩后端无条件的 SGR 复位）。

**残留**：光标从隐藏转为可见的那一帧仍不是一次 write——面板合上时是 `\e[2J` / `\e[1;1H` /
`diff…?25h` / `CUP` 四段，`?25h` 与它前面那段 diff 同批出去，终端在两次之间看到的是「光标可见、
停在 diff 最后一个被改写的格子上」。它只在**离散事件**上出现（本段首帧、每次 `needs_full_redraw`：
设置面板开合、焦点恢复、调色板变更……），不在扫光那种 25fps 的稳态帧里，所以不构成"不停闪烁"。
resize 也整屏重画、那一帧同样不原子（`\e[2J` ×2 + `diff…CUP`），只是它不翻转可见性，没有这一条。
要连它们一起消掉得让整帧原子化（`CSI ? 2026 h/l` 同步输出，Ghostty 支持），那是另一件事
（见 `design.md` 的 Non-Goals）。

**已知取舍：可见性不再"每帧重申"，也就没有自愈。** 旧行为每帧都发 `?25h`，所以任何**在 app
背后**把光标藏掉的东西（终端自身策略、别的写者、上一段 TUI 的残留状态）都会在下一帧被自动纠回
来；现在只有在「未知→可见」和「隐藏→可见」这两个状态边上才发。契约成立的前提是"本装饰器是参与
这套状态机的唯一写者"——段边界（`enter_sequence` 的 `?25l`、`leave_sequence` 的 `?25h`）与
`tui/clipboard_protocol.rs` 都不改这个状态，所以今天不会出现"终端隐藏了光标而 gate 以为它还
可见"。**将来若有第三方在运行期改 DECTCEM，必须同时经本装饰器，或在这里给 `visible` 加一条
外部失效/周期性重申通道**；否则可见光标会一直不亮。

单测在 `crates/wing/src/tui/cursor.rs`（真 `CrosstermBackend` + 按 `flush()` 切段的记录
writer，字节级断言）：段边界（一帧一写）由 `a_repaint_reaches_the_terminal_in_one_write` 钉，
宽字符 / emoji（含 `emoji_width::pin`）之后落点原样重发、行尾落点不折行由
`a_wide_glyph_by_the_row_end_keeps_the_cursor_on_its_row` 钉。

## 设计决策

- **盲发、不做特性探测**。spec 明确：规范终端必须忽略未知 OSC，探测是可选的（pi / Claude Code 做了探测，是优化不是正确性要求）。探测要读终端应答，TUI 里得和图片探测、crossterm 事件流抢 stdin；不值。代价只是对不支持它的终端多发几个字节。
- **去重按整条已格式化序列**（`Reporter`）。记录是幂等快照，同一 `(state, msg)` 连发即 noop——spinner 每帧不会刷记录，重复的 `turn_started` 也不会。
- **`msg` 只放"公开摘要"**：与 OSC 9 同源（回合摘要 / 面板问题原文），会被终端与面板显示在网格之外，绝不放会话正文。经过：控制字符→空格、trim、UTF-8 边界截断（解码 ≤2048B，编码 ≤2732B，整串 <4096B——spec 硬上限）。
- **`done` 的存续交给终端**：`done/error` 在下一个 `working/idle/clear` 之前一直有效；终端自己决定何时停止展示（如用户按键）。因此 `Done` 事件不会把 `done` 覆写回 `idle`。
- **退出不写 `clear`**：spec 的生命周期规则已经覆盖退出——进程退出时终端 MUST 丢弃 `working`/`blocked`，`done`/`error` 留存供用户回来查看；主动 clear 只会删掉用户还没看到的 `done`/`error`。（`idle` 记录的清理由终端自决。）
- **不做焦点门控**：OSC 9 只在失焦时发（通知=打断），7501 无条件发（展示交给终端，失焦与否由终端自己知道）。
- **开关**：`WING_PROGRAM_STATUS=0|false|off` 关闭（默认开，`Reporter::from_env` 只读一次）。

## 已知边界

- **只有 TUI 形态接入**。stdio（`wing -p`）的 stdout 是 NDJSON 协议流，不能写转义；ACP 无终端。要覆盖 stdio 需走 stderr/TTY 判断，未做。
- **终端侧今天只有 Rex 显示**（macOS beta）；libghostty 已解析（[ghostty#14560](https://github.com/ghostty-org/ghostty/pull/14560)），Ghostty app 暂不消费。这层信号是"为消费方就绪"的前置投入。
- **压缩（`/compact`）期间不报 `working`**：失败路径没有干净的回位钩子（失败只落一条通用 toast），宁可不报也不留悬挂的 working。
- **`SyncSession` 重写记录**：重连/切会话时 `done` 会被快照的真实状态（多为 `idle`）覆盖——"未读结果"的存续让位于"当前事实"。
- 透传取决于中间层：tmux 等会丢弃未知 OSC（安全，但也不显示）。
