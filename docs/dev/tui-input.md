# TUI 输入通道：键盘 / 鼠标 / 滚轮

本文件是 TUI 输入语义的**单一登记处**——新增鼠标相关能力（选择、滚动条、hover）时先读这里，不要凭直觉开 DECSET。

## 鼠标上报：1000 + 1002 + 1006（+ 1003 视环境），且生命周期对称

进入 alternate screen 时写 `?1000h ?1002h [?1003h] ?1006h`（按键上报 + 拖拽运动 + SGR 坐标，`?1003` 见下），退出与 panic 恢复写高位优先的逆序 `?1006l ?1003l ?1002l ?1000l`（**先关鼠标、再离开 alternate screen**）。三条路径（`init_terminal` / `restore_terminal` / `cmd` 的 panic hook）共用 `tui::enter_sequence` / `tui::leave_sequence` 两个函数，字节串由 `crates/wing/src/tui/mod.rs` 的单测逐字节断言——改顺序 = 测试失败。关闭序列不做环境分支：关一个没开过的模式是 no-op，这样 panic hook 只有一条路径。

**`?1003`（全运动 / hover）的唯一引入点就是 `tui/mod.rs`**，由滚动条（`tui-scrollbar`）引入：滚动条需要 hover 才高亮。开启与否由环境决定——

| 环境 | 开启序列 | 后果 |
|---|---|---|
| 普通终端 | `?1000h ?1002h ?1003h ?1006h` | 滚动条有 hover 高亮 |
| `TMUX` / `ZELLIJ` / `STY` 存在，或 `TERM` 以 `tmux` / `screen` 开头 | `?1000h ?1002h ?1006h` | 无 hover（收不到 `Moved`）；点击 / 拖动 / 滚轮照常 |

降级理由：多路复用器会转发每一次指针移动，hover 事件洪流直接表现为交互延迟；按键运动（1002）已经覆盖点击 / 拖动 / 滚轮，滚动条与文本选择所需的事件类型一个不少（Pi 的既有取舍，`tui-alt-screen.ts:350-363`）。

**不启用的模式与理由**：

| 模式 | 为什么不启 |
|---|---|
| `?1015`（RXVT 坐标） | 与 `?1006`（SGR）重复且坐标语义不同；crossterm 的 `EnableMouseCapture` 会顺手打开它，所以我们手写 Command 而不用那个 helper。 |
| `?1007`（alternate scroll） | 终端会把滚轮翻译成 ↑↓ 键，与真实方向键不可区分 → 面板一吞键滚轮就失效（#28 的教训，已回退）。 |

## 滚轮是独立通道，键盘方向键归焦点

- 滚轮（`MouseEventKind::ScrollUp/ScrollDown`）**永远**滚动 chat view（3 行/事件，`WHEEL_SCROLL_LINES`），在任何界面状态下都不被面板 / popup / 滚动条消费；它走 `App::handle_mouse` 的独立分支（匹配顺序在滚动条之前），不经过 `handle_key` 的按键消费链。
- 无修饰键 **↑↓ 只归焦点**：面板打开时导航面板，否则移动 composer 光标；键盘滚动只由 PageUp/PageDown、Ctrl+↑/↓、Ctrl+Home/End 承担。**不要**再引入「阅读中 ↑↓ 滚历史」这类启发式。
- 终端不支持鼠标上报时滚轮无响应，键盘滚动键是兜底（不做能力探测）。

## 滚动条只吃按下 / 拖动 / hover

右侧边缘 overlay 滚动条（`ui/scrollbar.rs` + `App` 的一处绘制与一处分派）：

- **不占布局宽度**：画在 chat 区域最右一列上（Buffer patch），不缩小换行宽度、不重排内容；只在内容溢出视口时存在。绘制顺序：chat 内容之上、toast 之下。代价是**那一列的内容被压在 `│` 下面**（内容换行时并不知道滚动条存在）。
- **命中 = chat 区域最右一列 + 行落在视口内**（列必须严格相等，行也必须）；命中优先级在文本选择之前——同一列既是滚动条又是「最后一列内容」，**有滚动条时按下就归滚动条**。
- **交互**：左键按下轨道空白处 → 跳转（手柄中心对齐指针）；按住手柄拖动 → 保留抓取偏移实时定位；拖动越界（离开轨道 / 离开 chat 区域）钳制到两端继续跟随；释放结束拖动。
- **与文本选择的分派**（`App::handle_mouse`）：`Down` / `Drag` / `Up` 先问滚动条（`scrollbar_at`：在滚动条列上才算），未命中才交给 chat 带的选区；`Moved` 只做滚动条 hover。拖动中的归属由 `scrollbar.dragging` 决定，因此「在 chat 里拖选时划到滚动条列上」仍然是扩选，不会突然开始拖滚动条。
- **滚动条列不是内容**：只要这一帧画了滚动条，选区映射（`App::off_the_bar`）就把落点夹到它左边一列——否则复制出的文本会混进 `│`、高亮也会把滚动条反显。内容不溢出（无滚动条）时整列照常可选。
- **不消费滚轮**：滚轮分支在 `handle_mouse` 里先匹配，指针停在滚动条上时滚轮照常滚 chat。
- **跟随契约**：跳转 / 拖动经 `ChatView::scroll_to` 复用同一套判定（离开底部 = 阅读态，拖到底部 = 重新武装跟随）。
- **状态清理**：几何为 `None` 的那一帧（内容不再溢出）顺手清理 hover/drag，按下未命中滚动条、或窗口失焦时同样清理——条不在屏幕上就不保留激活态。
- **重绘节流**：指针移动与拖动事件走 `chat_dirty`（受 16ms 帧门限），滚轮 / 按下 / 释放这类离散手势走 `input_dirty`（立即重绘）；无状态变化的事件不设任何 dirty 标志。
- **整帧断言**：`App::draw` 对 backend 泛型化，测试用 `TestBackend` 走真实 `Terminal`，把「只写 chat 最右列 / toast 与滚动条同帧共存」这类帧级性质钉在单测里。

## 自动跟随契约

「底部跟随新内容 / 滚动离开底部后不被拉回 / 回到最底部重新武装跟随」由 `ChatView` 的滚动方法统一承载，所有滚动入口（滚轮、键盘、`jump_*`）共用同一状态，细节与 Scenario 见 openspec change `revert-alternate-scroll` 的 `tui-scroll-follow` spec。

## 应用内文本选择（`tui-text-selection`）

免修饰键拖选复制由应用侧实现，模块分工：

| 关注点 | 位置 |
|---|---|
| 选区状态机（纯逻辑，可单测） | `crates/wing/src/ui/selection.rs` |
| 坐标映射 / 高亮 patch / 文本快照 | `crates/wing/src/ui/chat_view.rs` |
| 事件接线 / 冻结跟随 / 失效规则 / 自动滚动 | `crates/wing/src/app/mod.rs` |
| 剪贴板链路 | `crates/wing/src/util/clipboard.rs` + `app/runner.rs` |

**坐标模型**：选区锚定在**内容坐标** `(vrow, col)`（`vrow` = chat 内容虚拟行 = header + cells + pending 的连续空间）。每帧通过 `ChatView::geometry()`（**上一帧真实使用的** chat 区域 Rect + `scroll_offset`）双向映射到屏幕行。指针越界时先**夹到可见带边缘**、再**夹到内容高度**（`last_total - 1`），而不是拒绝——这正是边缘拖拽能继续扩展选区、并驱动边缘自动滚动的前提。内容纯追加时锚点不漂移（追加不改变已有行号）。

**事件节奏**：按下 / 松开 / 滚轮是**一次性动作**，立即重绘（`MouseOutcome::Immediate`，与按键同待遇）；**拖动走 16ms 帧门**（`MouseOutcome::Coalesced` → `chat_dirty`）——触摸板每秒 60~125 个 motion 事件，每帧还要重拍一次可见行快照，不能每个事件都强制全量绘制。

**高亮**：在 `App::draw` 的 `terminal.draw` 闭包内、**所有 widget（含 toast）渲染完之后**，用 `frame.buffer_mut()` 给落入选区的单元格 `set_style(Style::default().add_modifier(Modifier::REVERSED))`。要点：① `set_style` 是**合并**语义（保留 fg/bg，只叠加 modifier），不要用 `Style::default().bg(...)` 覆盖；② 宽字符按字素整组染色（头单元 + 尾单元），不出现半个字反显；③ 一律用**本帧** chat 区域裁剪，禁止跨帧缓存矩形；④ 不需要清理逻辑——Buffer 每帧由 widget 重新填充，松手后不再 paint 就自然消失。

**复制链路**：远端会话（`SSH_CONNECTION` / `SSH_CLIENT` / `MOSH_CONNECTION`）→ 直接 OSC52（本地命令会写到**远端机器**的剪贴板）；否则平台命令优先（macOS `pbcopy` / Windows `clip` / Linux `wl-copy` → `xclip -selection clipboard` → `xsel --clipboard --input`），全部失败退回 OSC52。命令用 `Command::output()` 捕获输出（raw mode 下不能继承终端的 stdio；crate 也 deny `print_stdout/stderr`），探针在 `tokio::task::spawn_blocking` 上执行（不占用执行器线程）。**注意**：`AppIntent::CopyToClipboard` 仍由 run loop 串行 await，所以复制完成前这一轮事件循环是暂停的——`runner.rs` 用 `tokio::time::timeout`（500ms）封顶，helper 卡死时退化为 `Copy failed: …` 而不是冻结 UI。OSC52 载荷超过 100000 字节（编码后）直接报错，不写巨型序列。成功 `Copied!`，失败 `Copy failed: …`。

**文本提取**：所见即所得——拖动期间每帧把 chat 可见带抽成字素快照（`capture_visible_rows`），松手时按内容坐标区间从快照里取字符。按 `unicode-width` 推进列、跳过宽字符尾单元（CJK / emoji 不产生多余空格）；每行 `trim_end`、行间 `\n`、去掉首尾空行；全空 → 不复制不提示。快照同时记录每行的**内边距列数**（`RenderedRow::inset`，user message cell 是 2），提取时跳过——从 chat 左边缘起拖不会粘贴出行首两个空格，而**属于内容本身的缩进保留**。**不要**尝试脱离渲染自己算换行（要复刻 ratatui 的 `WordWrapper`，必然漂移）。

**终点格子**：指针停在哪个字素上，就把那个字素整格纳入选区（`ChatView::snap_focus_right`，宽字素天然整组）；单击（未拖动）不做吸附，保持零宽不复制。

**边缘自动滚动**：指针停在可见带顶/底行（含等号）时 `run_app` 的 `select!` 多出一条**绝对 deadline** 的 sleep 臂（`App::selection_autoscroll_at`，50ms 一拍）。**必须是绝对 deadline**：`select!` 每轮迭代都会重建该 future，相对 sleep 在流式期间（事件间隔远小于 50ms）永远完不成。每 tick 滚 1 行、焦点随滚过的行平移、重绘；**位置没动即停止**（到边界）并清 deadline，无 deadline 时该 future 挂 `pending()`——不 busy loop、不泄漏任务。步进期间跟随状态保持冻结。

**冻结跟随**：按下时 `ChatView::unfollow()` 置 `follow_frozen`（除了清 `auto_scroll`，还**必须**让渲染层不再「偏移在底边就重新武装」——否则按下后必然会画的那一帧就把冻结撤销了，这正是「在最新输出上拖选」的常见场景）。只有滚动入口（`scroll_up` / `scroll_down` / `jump_*`）能解除冻结；松手时 `scroll_down(0, visible_height)` 按「是否仍在底边」恢复，`tick_selection_autoscroll` 每步之后重新 `unfollow()`——仍然完全复用 `tui-scroll-follow` 契约。

**失效规则（保守）**：按下时记录结构指纹 `(cells 数, pending 数, 终端宽度, 重建计数)`，每帧渲染前比对、松手时**再比对一次**（重建可能发生在同一轮循环的 draw 之后），任一变化即中止选择并清高亮（cell 增删 / pending 提升 / compaction / rewind / 会话切换 / 窗口缩放都会命中；最后一项 `rebuilds` 由 `ChatView::clear()` 自增，覆盖「重建后计数恰好相同」的场景）。**纯内容追加（流式 delta、既有 cell 文本增长）不改变指纹，因此不失效**——这是与「total 高度变化即清」的关键区别。焦点丢失（`Focus(false)`，拖拽松手事件永不到达）同样中止。

**与滚动条**：滚动条列的位置（chat 区域最右一列）永远归滚动条，选区映射把它夹掉（见上一节）；只有内容不溢出、没有滚动条时才可选。反过来说，命中滚动条的按下不会启动选区，滚动条的拖动也不会产生复制意图。

**已知限制**：选区只覆盖 chat 区域（composer / 状态栏 / 弹层不参与，点击定位光标属 `tui-composer-pointer`）；选择期间滚轮仍可滚动，滚出快照范围的行不参与复制（所见即所得）；拖动期间内容重建（compaction / rewind / 会话切换）会中止选择，需重新拖选；OSC52 在部分终端仍可能「假成功」（无法探测，本地平台命令优先已缓解）；不做词/行粒度、键盘选择、Esc 清除、搜索；内容溢出时最右一列被滚动条占据，那一列的字选不到也复制不到（取舍见上文）。

## 已知中间态：原生拖选需要 Shift/Option

鼠标上报接管后，终端不再把拖拽交给自身的文本选择——**不按 Shift（macOS 用 Option）的拖选不再选中文本**。这是回退 #28 的已知代价：应用内自研选择（`tui-text-selection`）已恢复 chat 区域的免修饰键体验，同时保留 Shift/Option 原生拖选作为兜底（composer 区域的拖选仍只能用原生方式）。
