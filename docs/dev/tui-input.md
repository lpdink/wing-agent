# TUI 输入通道：键盘 / 鼠标 / 滚轮

本文件是 TUI 输入语义的**单一登记处**——新增鼠标相关能力（选择、滚动条、hover）时先读这里，不要凭直觉开 DECSET。

## 鼠标上报：只开 1000 + 1002 + 1006，且生命周期对称

进入 alternate screen 时写 `?1000h ?1002h ?1006h`（按键上报 + 拖拽运动 + SGR 坐标），退出与 panic 恢复写逆序 `?1006l ?1002l ?1000l`（**先关鼠标、再离开 alternate screen**）。三条路径（`init_terminal` / `restore_terminal` / `cmd` 的 panic hook）共用 `tui::enter_sequence` / `tui::leave_sequence` 两个函数，字节串由 `crates/wing/src/tui/mod.rs` 的单测逐字节断言——改顺序 = 测试失败。

**不启用的模式与理由**：

| 模式 | 为什么不启 |
|---|---|
| `?1003`（全运动 / hover） | 事件量洪流（tmux/SSH 下延迟明显）；当前没有任何 hover 交互。**唯一引入点是 `tui-scrollbar`**（hover 加粗滚动条），届时按 Pi 的做法在多路复用器下降级为 1002。 |
| `?1015`（RXVT 坐标） | 与 `?1006`（SGR）重复且坐标语义不同；crossterm 的 `EnableMouseCapture` 会顺手打开它，所以我们手写 Command 而不用那个 helper。 |
| `?1007`（alternate scroll） | 终端会把滚轮翻译成 ↑↓ 键，与真实方向键不可区分 → 面板一吞键滚轮就失效（#28 的教训，已回退）。 |

## 滚轮是独立通道，键盘方向键归焦点

- 滚轮（`MouseEventKind::ScrollUp/ScrollDown`）**永远**滚动 chat view（3 行/事件，`WHEEL_SCROLL_LINES`），在任何界面状态下都不被面板 / popup 消费；它走 `App::handle_mouse`→`run_app` 的独立分支，不经过 `handle_key` 的按键消费链。
- 无修饰键 **↑↓ 只归焦点**：面板打开时导航面板，否则移动 composer 光标；键盘滚动只由 PageUp/PageDown、Ctrl+↑/↓、Ctrl+Home/End 承担。**不要**再引入「阅读中 ↑↓ 滚历史」这类启发式。
- 终端不支持鼠标上报时滚轮无响应，键盘滚动键是兜底（不做能力探测）。

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

**已知限制**：选区只覆盖 chat 区域（composer / 状态栏 / 弹层不参与，点击定位光标属 `tui-composer-pointer`）；选择期间滚轮仍可滚动，滚出快照范围的行不参与复制（所见即所得）；拖动期间内容重建（compaction / rewind / 会话切换）会中止选择，需重新拖选；OSC52 在部分终端仍可能「假成功」（无法探测，本地平台命令优先已缓解）；不做词/行粒度、键盘选择、Esc 清除、搜索与滚动条。

## 链接渲染与单击打开（`tui-link-open`）

markdown 链接渲染为 OSC8 超链接，单击（无拖动）打开。模块分工：

| 关注点 | 位置 |
|---|---|
| 链接区间（IR → 行内显示列）/ OSC8 纯函数 / `ComposedLines` | `crates/wing/src/render/markdown/links.rs` |
| 流式渲染同步维护链接（`compose_into` / `lines_and_links`） | `crates/wing/src/render/markdown/stream.rs` |
| 行缓存携带链接 + 行号是否精确 | `crates/wing/src/ui/cached_cell.rs` |
| 本帧链接快照 / OSC8 注入 / 命中查询 | `crates/wing/src/ui/chat_view.rs` |
| 目标解析 / argv / 进程启动 | `crates/wing/src/util/open.rs` + `app/runner.rs`（`AppIntent::OpenLink`） |
| 点击 vs 拖动分流 | `crates/wing/src/app/mod.rs`（`App::mouse_link`） |

**OSC8 注入方式**：ratatui 0.30 没有任何超链接 API（`Cell` 只有 symbol/style/diff_option），所以序列写进 **`Cell` 的 symbol**：`ESC]8;;URL ST` + 字素 + `ESC]8;; ST`，**逐字素头**注入（宽字符只注入头单元，尾单元是填充空格，写进去会覆盖半个宽字）。两个必须同时做的细节：

1. **`CellDiffOption::ForcedWidth(1)`**：`Cell::cell_width()` 会用 `symbol().cell_width()` 量宽度，而 `BufferDiff` 用它跳过宽字符的尾随单元格（`self.pos += cell_width - 1`）——不 pin 住宽度，一条 URL 长度的 symbol 会让 diff **跳过链接后面的一整行**（屏幕残留旧内容）。ratatui 为此场景预留了 `ForcedWidth`（其文档原文提到转义序列的宽度与屏幕不一致，自带 kitty 占位符测试同款用法）。
2. **逐字素头自带 open/close**（而不是「首单元开、末单元关」）：diff 只重绘部分单元格时（例如滚动后只有部分格变化），每个被写出的单元格都自带完整序列，终端不会丢掉某个字符的链接属性。代价是链接单元格多 ~`URL 长度 + 12` 字节，仅在实际重绘时产生。

每帧开始时 ratatui 会 `reset()` 下一个 buffer（"each render pass starts from an empty buffer"），所以注入不跨帧残留；同一 URL 的序列逐字节恒定，连续帧的 `Cell` 相等 → **diff 不抖动**。

**链接区间链路**：`MarkdownSegment.link_target`（解析层已产出）→ `line_link_spans()`（列 = 前序段 `unicode-width` 累加）→ 与渲染行平行的 `Vec<Vec<LinkSpan>>`（`ComposedLines`；流式路径在 `compose_into` 里加 2 列前缀偏移，`to_lines` 重放路径同理）→ `ChatViewWidget::render` 逐行映射成**屏幕绝对列**，落到两个消费者：① 本帧 Buffer 注入 OSC8；② `ChatView::frame_links` 快照（`link_at(column, row)`）。

**行号必须精确**：只有当 cell 的每一行显示宽度 ≤ 渲染宽度时（`CellLines::rows_exact`）才建立链接——此时 `Paragraph` 不可能折行，屏幕行号 == cell 内行号，列区间精确。两个取值来源：① 流式 cell 由 `stream.rs` 的硬折行保证恒真；② `to_lines`（冻结/replay/resize 之后）在**有链接时**逐行量一次（`!composed.has_links() || composed.rows_are_exact(width)`），随 width+generation 缓存。**真的会不成立**：IR 层的 UAX #14 预折行只作用于 prose，代码块与缩进代码是显式豁免的（`wrap.rs::is_prose_line`），所以「同一 cell 里有一个超宽代码行 + 后面有链接」时 `Paragraph` 会把代码行折成两屏行，之后的行号整体错位。此时该 cell **一个链接都不建立**（`rows_are_exact == false`）——宁可链接点不开，也不能把命中框和 OSC8 注入到代码文本上（保守降级，不猜列）。单测：`an_overwide_code_line_puts_the_cell_off_limits` / `an_overwide_indented_code_line_puts_the_cell_off_limits` / `a_fitting_code_line_keeps_its_link_on_the_right_row`。**链接目标不使用 `Style` 承载**（不可行：`set_stringn` 会丢掉含控制字符/零宽的字素，URL 的可见字符还会被算进折行列宽）。

**复制仍然干净**：`buffer_row_graphemes` 读 symbol 前先 `strip_osc8`，所以选区快照的宽度不被 URL 撑大、复制文本不含控制字符——**任何从 Buffer 反读文本/宽度的地方都必须先剥序列**。

**点击 vs 拖动**：`Down(Left)` 在 chat band 内先用**本帧快照**查链接（记进 `App::mouse_link`）**再**照旧开始选择（拖动要能选中链接文本）；`Up(Left)` 在「记录了链接且 `is_dragged() == false` 且释放点 == 按下点（`selection.anchor()`）」时打开并提前返回（零宽单击本来也不复制，所以不与复制冲突）。两个条件都查是刻意的：终端丢 motion 事件时不能靠「没收到 `Drag`」把一次真实的拖拽当成单击（reference 的 `isClick = !dragged && anchor == point` 同款）。`cancel_selection`（焦点丢失 / 结构失效）连 `mouse_link` 一起清——中止的手势不打开任何东西。命中来自按下那一帧，滚动/流式追加都不会让「按下 A、松开 B」。toast 画在 chat band 之上，所以 `render_toast` 返回绘制区域、`ChatView::mask_links` 把被盖住的命中框删掉（看不见的链接不可点）——**下一个会盖住 chat band 的是 `tui-scrollbar` 的滚动条列**，落地时必须把那一列也交给 `mask_links`（否则点滚动条会打开它盖住的链接）。

**目标解析与打开方式**（`util/open.rs`）：带 scheme（`http` / `https` / `mailto` / `file` …；单个字母 + `:` 视为 Windows 盘符不算 scheme）= URL → 平台默认启动器（macOS `open` / Windows `cmd /c start ""` / Linux `xdg-open`）。其余按本地路径处理：`~` 展开为 `$HOME`、`file://`（含可选 `localhost`）剥离、**相对路径锚定 `current_dir()`（wing CLI 启动目录，不是会话 workspace）**、`#L10` / `#L10C5` / `:10` / `:10:5` 后缀剥离并解析成行（列）；路径先 `canonicalize`（失败则退化为绝对路径）再 `exists()` 检查——不存在就**不启动任何进程**。有行号时先按白名单找支持行跳转的编辑器 CLI（`code -g` / `subl` / `zed`，PATH 探测不用 `which` 子进程），都没有才退回默认程序（文件仍会打开，行号降级——这是「尽量」的诚实边界；`open` / `xdg-open` 都不接受 `path:line`）。

**安全边界**：① 只对 markdown 链接区间触发（消息里出现 `/etc/passwd` 不会变成可点）；② `Command::new(program).args(argv)`，**绝不经过 shell**、不拼字符串执行（`;`、`|`、`$(...)`、空格、引号都只是 argv 里的普通字符）；③ 注入 OSC8 的目标先剥控制字符（`sanitize_osc8_target`，防 markdown 正文反向注入终端序列）——**净化只用于注入，打开时仍用原始目标**；④ 进程输出 `Stdio::piped()` 捕获（raw mode 下继承 stdio 会写坏屏幕，crate 也 deny `print_stdout/stderr`），在 `spawn_blocking` 上跑并 `tokio::time::timeout`（500ms，与剪贴板探针同款上限——run loop 是串行 await，这个上限就是最坏 UI 冻结时长）封顶。

**反馈**：成功静默（`tracing::debug!`，打开本身有可见结果）；失败 `Open failed: …` toast + `tracing::warn!`（opener 不存在 / 非零退出并带 stderr / 路径不存在 / 超时）。

**已知限制**：只覆盖 chat 内容里走 markdown 的 cell（assistant 消息与 thinking；Ask 面板 / tool 输出不参与）；**同一 cell 里出现超宽代码行/缩进代码行时该 cell 全部链接失效**（`rows_are_exact`，见上，宁可没有链接也不错行）；行号跳转只认白名单编辑器；`~user/x` 不展开、Windows 上 `.cmd` shim（PATHEXT）探测不到 → 只降级行号（`util/open.rs` 模块注释同样登记）；`file://` 只支持空 authority 与 `localhost`（`file://nas/share/x` 明确报错而不是拼到启动目录下）；tmux/zellij 的 OSC8 透传取决于用户配置；不做 hover 自绘下划线/预览。

## 已知中间态：原生拖选需要 Shift/Option

鼠标上报接管后，终端不再把拖拽交给自身的文本选择——**不按 Shift（macOS 用 Option）的拖选不再选中文本**。这是回退 #28 的已知代价：应用内自研选择（`tui-text-selection`）已恢复 chat 区域的免修饰键体验，同时保留 Shift/Option 原生拖选作为兜底（composer 区域的拖选仍只能用原生方式）。
