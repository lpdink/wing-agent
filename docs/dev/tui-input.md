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

- **不占动态布局宽度**：画在 chat 区域最右一列上（Buffer patch），只在内容溢出视口时存在；绘制顺序：chat 内容之上、toast 之下。内容这边靠一条**静态预留的 gutter** 让开这一列：常量 `scrollbar::SCROLLBAR_GUTTER`（当前 2 = 滚动条自己 1 列 + 1 列空白；该数值就是整个预留量，1 表示只留滚动条那一列）只被 `scrollbar::content_area(band)` 读一次，chat widget 渲染进这个矩形，其余一切宽度从渲染矩形自然推导。gutter 有无滚动条都预留，所以「第一次溢出」不会引起任何重排。
- **命中 = chat 区域最右一列 + 行落在视口内**（列必须严格相等，行也必须）；命中优先级在文本选择之前（这一列已在内容区之外，按下只是归滚动条）。
- **交互**：左键按下轨道空白处 → 跳转（手柄中心对齐指针）；按住手柄拖动 → 手柄**始终 1:1 跟着指针**（映射是比例的，见下）；拖动越界（离开轨道 / 离开 chat 区域）钳制到两端继续跟随；释放结束拖动。
- **为什么拖动是比例映射（不能改成「每行 3 行内容」）**：手柄的行号就是内容的进度，所以「手柄不落后于手指」⟺「拖完整条轨道 = 内容从头走到尾」⟺ `1 指针行 = max_scroll / travel` 行内容（`travel = 轨道行数 - 手柄行数`）。而「每行只走 3 行」只有在 `max_scroll == 3 × travel`（内容刚好比视口多 3~4 屏）时才与跟手同时成立；超长会话里两者数学上互斥。浏览器滚动条看起来连续，是因为它是**像素**级的比例映射（整条轨道上千像素），终端只有行、轨道只有几十行，所以这个步长必然偏大——**这是终端的量化限制，不是可调参数**。
- **细粒度交给谁**：指针停在滚动条上**滚轮**（3 行/档，与滚轮完全一致）、`Ctrl+↑/↓`（1 行/次）、PageUp/PageDown（翻页）；`Home`/`Ctrl+Home` 类跳转仍是一步到位。
- **与文本选择的分派**（`App::handle_mouse`）：`Down` / `Drag` / `Up` 先问滚动条（`scrollbar_at`：在滚动条列上才算），未命中才交给 chat 带的选区；`Moved` 只做滚动条 hover。拖动中的归属由 `scrollbar.dragging` 决定，因此「在 chat 里拖选时划到滚动条列上」仍然是扩选，不会突然开始拖滚动条。
- **滚动条列不是内容**：内容区 = chat 带减去 gutter，渲染矩形本身就把它排除在外，于是换行预算、用户消息背景框、diff tint、选区高亮与复制都**按构造**停在它左边——不再需要逐点钳制（原 `App::off_the_bar` 与 `ChatView::set_content_width` 因失效而删除）。gutter 的数值就是唯一的调节旋钮。
- **不消费滚轮**：滚轮分支在 `handle_mouse` 里先匹配，指针停在滚动条上时滚轮照常滚 chat。
- **跟随契约**：跳转 / 拖动经 `ChatView::scroll_to` 复用同一套判定（离开底部 = 阅读态，到最底部 = 重新武装跟随）。
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
| 坐标映射 / 高亮 patch / 文本快照 | `crates/wing/src/ui/chat_view/frame.rs` |
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

**失效规则（保守）**：按下时记录**区域专属**的指纹，每帧渲染前比对、松手时**再比对一次**（重建可能发生在同一轮循环的 draw 之后），任一变化即中止选择并清高亮。chat 选区用 `(cells 数, pending 数, 终端宽度, 重建计数)`（cell 增删 / pending 提升 / compaction / rewind / 会话切换 / 窗口缩放都会命中；最后一项 `rebuilds` 由 `ChatView::clear()` 自增，覆盖「重建后计数恰好相同」的场景；**纯内容追加（流式 delta、既有 cell 文本增长）不改变指纹，因此不失效**——这是与「total 高度变化即清」的关键区别）；composer 选区用 `(草稿文本, 终端宽度)`（任何编辑即中止，见下节）。焦点丢失（`Focus(false)`，拖拽松手事件永不到达）同样中止；`cancel_selection` 按区域恢复——只有 chat 选区会解除跟随冻结。

**与滚动条**：滚动条占用的那一列（chat 区域最右一列）在滚动条存在的那些帧里永远归滚动条——命中它的按下走滚动条，选区映射也被夹掉（`App::off_the_bar`）；否则复制出的文本会混进 `│`、高亮也会把滚动条反显。滚动条的拖动同样不产生复制意图。内容不溢出（没有滚动条）时整列照常可选，分派见上文「滚动条只吃按下 / 拖动 / hover」。

**已知限制**：选择覆盖 chat 区域与 composer（见下一节；状态栏 / 弹层不参与）；选择期间滚轮仍可滚动，滚出快照范围的行不参与复制（所见即所得）；拖动期间内容重建（compaction / rewind / 会话切换）会中止选择，需重新拖选；OSC52 在部分终端仍可能「假成功」（无法探测，本地平台命令优先已缓解）；不做词/行粒度、键盘选择、Esc 清除、搜索；内容溢出时最右一列被滚动条占据，那一列的字选不到也复制不到（取舍见上文——`set_content_width` 对整段生效，不只是拖选端点）。

## composer 指针（`tui-composer-pointer`）

输入框（composer）与 chat band 共用**同一个**选择状态机，但各自锚定在自己的坐标空间里——一次选择只属于**按下时所在的那个区域**（`SelectionRegion { Chat, Composer }`），跨区域的拖拽一律夹取回原区域的可见带边缘，不做跨区域合并。

**卡片外框**（`tui-composer-card`）：composer 渲染为一张**悬浮卡片**——左右各留 1 列边距（`chrome::card_area` 是唯一的「悬浮」发生地：布局按卡片宽度要高度、记录卡片矩形、把 widget 渲染进它），卡身**不填底色**（与终端背景浑然一体，边界靠边框 / 提示符 / 两条轨道划出，见下），上边框是**活动栏**（spinner / 耗时 / 中断提示）、下边框是**元信息栏**（workdir / 本轮用量 / 滚动位置）。几何由 `ui/input_area/chrome.rs` 的 `Chrome` **单一描述**（文字区左起 4 列、右留 2 列、上下各 1 行边框），widget、换行宽度、光标落点、指针映射全部读它——指针不可能描述出渲染没画过的一帧；**编辑器读的是渲染出来的那个 chrome**（`InputArea::chrome()`，无帧时回落到 `chrome::UNFRAMED_WIDTH`），所以终端把 composer 压扁时（`Chrome::of` 降级成无框）换行宽度也跟着降级，不会去用一条屏幕上不存在的宽度。原来独立的「working 行 + info separator 行」都折进边框里，所以一轮运行中**不占任何额外行**。

**唯一的状态信号**：边框与横线恒为 dim 静色（不做强调——「这里有东西可发」由**提示符 `❯`** 表达：非空草稿时 accent、空草稿时 dim）；**键盘被接管**（ask 面板 / `/model` 面板真的在吞键，命令候选 popup 只吞导航键、不算）时整个卡片转「幽灵」——草稿与提示符按 dim 绘制、"这里现在不收键盘"在你按下第一个键之前就看得见。判定谓词是 `App::composer_typing_blocked()`（**不是** `composer_pointer_blocked()`，后者的阈值含可见 popup）。

| 关注点 | 位置 |
|---|---|
| 区域抽象 / 选区状态机（纯逻辑） | `crates/wing/src/ui/selection.rs`（`SelectionRegion` / `SelectionPoint` / `bounds_in`） |
| 卡片几何 / 两条轨道 | `crates/wing/src/ui/input_area/chrome.rs`（`Chrome` / `ActivityRail` / `MetaRail`） |
| 屏幕 ↔ 逻辑命中、高亮 patch、选区文本 | `crates/wing/src/ui/input_area/pointer.rs` |
| 视觉行 ↔ 逻辑行列换算原语 | `crates/wing/src/ui/input_area/wrap.rs`（`display_col_to_char` / `char_display_offset`） |
| 单击定位光标 API | `crates/wing/src/ui/input_area/mod.rs`（`InputArea::set_cursor_from_visual`） |
| 事件接线 / 互斥 / 失效 | `crates/wing/src/app/mod.rs`（`mouse_press` / `mouse_drag` / `mouse_release` → `composer_*` / `chat_selection_*`） |

**坐标模型（与 chat 的关键差异）**：composer 的选区锚定在**逻辑坐标**（逻辑行下标 + 行内 char 下标），每帧用**渲染同一个** `wrap::build_visual_rows`（同样的可用宽度：`Chrome::text_width`，即 `input_rect.width - 6`，不足 1 列时按 1 列——`update_vertical_scroll` / `hit` / `paint_selection` / `cursor_screen_pos` 与 widget 全部同源）映射到视觉行列。渲染的 Rect 由 **`InputArea` 自己记录**（`rendered_area`，`ComposerWidget::render` 每帧回写，与 `ChatView::geometry()` 同一所有权模型：鼠标事件发生在帧之间，命中必须描述用户正看到的那一帧）。chat 必须用「渲染结果快照 + 内容坐标」是因为换行发生在 ratatui `Paragraph::wrap` 内部、脱离渲染就得复刻 `WordWrapper`；composer 的换行是应用自己算的，**模型即权威**——所以复制文本直接从 `lines` 取，且**软换行不插换行符**（只有真正的逻辑行边界才有 `\n`）。逻辑坐标对**纵向滚动与重排免疫**：滚动只改变可见窗口，不改变选区。

**复制内容为空**：只跨了换行、一个字符都没取到的区间（例如从行末拖到下一空行的行首）视为空选区——不复制、不 toast，不产生裸 `\n`。

**屏幕 → 逻辑命中**：`pointer::hit` 把屏幕位置换算成 `ComposerHit { vis_row, display_col, point, focus, on_char }`。**卡片外框不是文本**：落在上下两条轨道行上的点返回 `None`（按下无人认领 → 不落光标、不起选区），而**完全在卡片之外**的点（拖拽越界）夹进文字带——上越界 → 第一文字行、下越界 → 最后文字行，与 chat band 的夹取契约一致。带内的行再夹进可见窗口（`vertical_scroll` + 行内偏移）、夹进视觉行列表（窗口外 / 内容下方的行 → 最后一个视觉行）；列在 `area.x + Chrome::text_x` 处饱和（点在边框 / 提示符 `❯ ` 或更左 → 该视觉行行首），右侧不设上限（超出行文本 → 该视觉行行末）。锚点用 `point`（**指针前插入点**：压在字符的单元格上 → 该字符之前；压在粘贴 chip 的单元格上 → 整块 chip 之前，见下节），拖动 / 松手的终点用 `hit.focus`（`on_char` → `col + 1`，**把指针下的字符整格纳入**；压在 chip 上 → 整块 chip 的末端）——与 chat 的 `snap_focus_right` 同构，单击不吸附所以「按下不拖动 = 零宽不复制」在两个区域都成立。

**单击定位光标**：按下只记录锚点（此时还不知道是点击还是拖拽），松手时若**未发生位置移动**则把指针换算成光标位置（`InputArea::set_cursor_from_visual(available_width, vis_row, display_col)`）：显示列 → char 下标由 `model::column_point` 完成（宽字符按 char 粒度，任一格都取「字符之前」；chip 的任何一格 → 整块之前；显示列超出该视觉行文本 → 行末），并清 `desired_col`。单击不产生选区、不高亮、不复制。**「点击 vs 拖拽」只按位置判定**（`anchor != 终点`，不看 `is_dragged`）：触摸板在同一格内的抖动仍是单击（不会退化成「复制指针下那 1 个字符」），拖出去又拖回锚点 = 零宽；chat 侧因为映射会在字素内吸附，才额外 OR 上 `is_dragged()`。

**高亮**：与 chat 走同一条 Buffer patch 通道（`REVERSED` 合并语义、toast 之后绘制），但按区域分派、各用**本帧**自己那个 Rect 裁剪：composer 的区间由模型换算成显示列 `[x0, x1)`，`x0` 起于 `area.x + Chrome::text_x`（**永不染边框与提示符**），`x1` 夹到文字区右缘（`area.right() - 2`，不碰右边框）；宽字符按 char 宽度整组覆盖。

**互斥（按下时判定一次）**：ask 面板（所有 ask 都是面板，含 Bash 危险命令确认）/ `/model` 面板 / **可见的**命令候选 popup 在键盘接管状态时，落在 composer 内的按下**直接忽略**——不开始选择、不移动光标、不复制（后续 drag / release 自然也是 no-op）。这些状态下草稿正被面板的内联输入框与按键改写，指针交互进去只会与它们竞争；**滚轮与 chat band 的拖选不受影响**。

「键盘接管」按**是否真的占用键盘 / 屏幕**判定，而不是 `ActivePopup::is_active()`：后者只表示「不是 `None`」，而**无候选（不可见）**的 popup 是常态可达（`/zzz` 无匹配命令、`/session abc` 过滤后为空、候选尚未 fetch 回来）——它们高度为 0（不绘制、不产生布局位移），按键也已经直接放行给 composer，此时再拦指针只会让「看起来空闲」的界面点不动。因此条件是 `popup.active.height() > 0 || popup.active.is_must_select_empty()`：前者保证「有可见 popup 才有布局位移」，后者保留 must-select 命令（`/fork` `/rewind` `/agents` `/session` `/ss`）在无候选时对 Enter 的拦截——**输入了无匹配候选的 must-select 命令（如 `/session abc`）时，输入框内的点击 / 拖选仍不生效**（Enter 会被 popup 吃掉，指针交互没有意义）。

**失效与冻结**：composer 选区用「草稿文本 + 终端宽度」指纹——键盘输入 / 退格 / 删除 / 粘贴 / 提交 / 草稿恢复（`/new`、会话切换）**任一内容变化即中止并清高亮**（每帧渲染前与松手时各比对一次）；纯导航键（←→↑↓ / Home/End）不改变文本，因此**不中止**。chat 侧的结构变化（cell 增删 / 重建）**不影响** composer 选区，反之亦然。composer 选区**不触碰** `chat.unfollow()`，也**不武装**边缘自动滚动定时器（输入框最多 `max_lines` 行，可见窗口外的行不参与选择）。

**已知限制**：卡片边框（两条轨道行）不是文本，其上按下不落光标、不起选区（见上）；must-select 命令在「无匹配候选」时输入框内的点击 / 拖选不生效（见上）；输入框内不做边缘自动滚动（草稿超过 `max_lines` 时窗口外的视觉行无法拖选）；placeholder（空输入时显示的提示文案）不是内容——在它上面拖选不产生选区、不复制；粘贴 chip 按屏幕文本复制（`[Pasted text #N …]`，与所见一致，拖过整块就得到整块标签，payload 不参与）；宽字符单击取「字符之前」（char 下标没有半格精度）。

## 粘贴 chip：内联的原子单元（`tui-paste-chip`）

大粘贴（`>2` 行或 `>200` 字符，阈值在 `paste.rs`）不在草稿里铺开，而是在**光标处**留下一个 chip；**放不下的多行粘贴也一样**（草稿已到 `max_lines`、装不下全部行 → 整块变 chip，而不是像以前那样被静默截掉尾巴——chip 不占行，所以永远放得下）：`[Pasted text #N +M lines]`（`M` 是除首行外的行数；单行大粘贴不带行数，`[Pasted text #N]`）。chip 就是普通插入点上的一个单元——**前后文本与它在同一行**，光标落在 chip 之后继续输入，粘贴不再重排草稿。

**模型**（`ui/input_area/model.rs`，也是本节的权威）：一行的内容 = `Vec<Segment>`，段要么是 `Text`，要么是 `Paste(号码)`，要么是 `Image(号码)`（见下一节的图片 chip）；payload 与**标签文本**都在注册表里（号码 → 载荷 + 标签，标签在插入时算一次——它每帧、每次击键都被读，不能跟着粘贴体量线性变慢）：粘贴是 `Pastes`（号码 → 全文 + 标签），图片是 `Images`（号码 → 压缩产物 + 标签），两本注册表**平级**、各自计数，由 `Chips` 捆在一起供模型取用。**chip 的号码单调分配、绝不复用**：号码是 chip 的身份，用户已经读到的号码在邻居被删除后仍是它自己的（清空 / 恢复草稿才从头计数）。整块 chip 只有一个来源——插入它的人；**任何文本都不会被反向解析成 chip**，所以经 `set_text` 恢复的草稿（rewind / fork 带回来的内容）里就算写着 `[Pasted text #1 +2 lines]`，它也只是一串普通字符（不会被原子删除、提交时也不会展开）。

**原子性**：光标永不落在 chip 内部。←/→ 一步跨过整块；chip **尾端** Backspace、**首端** Delete 把整块删掉（payload 同时丢弃，不会留在注册表里）；在它两端打字则分别落在块前 / 块后。指针与上下移动命中 chip 的任何一格都解析到它的**前缘**（`model::column_point`，与宽字符「取字符之前」同款）；chip 比整行还宽、被 widget 裁剪时，**裁剪线之外**的列按「行末」处理——那是行文本之外的空白，与普通行一致，也不会把光标送进 chip 的隐藏半截（`column_point` 的 `visible_cols` 就是这条裁剪线）。拖选因此是不对称的——锚点总在块前缘，`focus` 给的是后缘：**从块内向右拖**会把整块带进选区，向左拖则从块前缘开始（块本身不在选区内），与宽字符的插入点语义一致。行合并（列 0 的 Backspace / 行末的 Delete）不特殊处理 chip：合并只是把两行的段接起来，chip 仍是整块。

**两种 chip 是同一个单元**：模型的原子操作只认「chip」这个身份（`ChipId`），不问它属于哪本注册表——图片 chip 因此与粘贴 chip 共享**同一段**跨块步进 / 整块删除 / 换行不切开 / 指针前缘吸附 / ghost dim 的代码，不存在两套实现漂移的余地。

**草稿被重写 / 被命令消费的路径（图片的丢点，逐条登记）**：

| 路径 | 图片的去向 | 可见性 |
|---|---|---|
| 命令补全（popup Enter 把草稿改写为 `name + args`） | 随 `set_text` 丢弃（粘贴 chip 则改以**展开后的文本**留在草稿里，参数里提交的是 payload 不是标签） | 无提示（用户主动改写草稿） |
| must-select 命令的草稿恢复（`/fork` 等回车后回填文本、重开 popup） | 同上，随 `set_text` 丢弃 | **info toast**：`Image dropped: <命令名> takes no image`（与命令路径同一句，见下） |
| **提交的文本恰好是前端命令**（`try_frontend_command` 命中：`/clear`、`/model …`、`/tips` …） | 命令在本地执行、不是消息，图片**没有落点**：`SubmitPayload.images` 随 `payload` 一起消失，既不发也不上传 | **info toast**：`Image dropped: <命令名> takes no image` |
| `/clear`（清 chat）/ Esc 清草稿 / 提交 | 走同一个 `clear`：两本注册表一起归零、号码从头计数 | 无提示（用户自己的动作） |
| rewind / fork / 重连带回的 server draft（`set_text`） | 随被替换的草稿丢弃 | 无提示 |
| **发送失败 / 被服务端拒绝（回填）** | **反向**：文本与图片 chip 都回到草稿（原件存在 pending 条目里，见下节） | **warning toast**：原因 + `draft restored` |

丢弃而不是「留载荷不留 chip」的理由：那会造出一个用户看不见、却会被发出的附件。消息路径（`submit_message` 的发送分支）永远两半都带：`text` 与 `images` 分开进 `SendMessage`。

**提交产物**（`SubmitPayload { text, images }`，`InputAction::Submit` 的载荷）：`text` 是展开后的草稿（粘贴 chip → 全文，**图片 chip 不贡献任何字符**），`images` 是图片 chip 的载荷列表、按**出现序**（行序 × 段序）取走（不是克隆）。两者分开是因为消息本身就这么承载（后端 content / media 是两个字段，且不承诺交错顺序）；`ImagePayload.label` 是**草稿内号码**的 chip 文案（纯展示，不上传），消息侧（pending / 历史 / 回填）各自按自己的口径重建标签——见「消息里的图片」一节。

**渲染与换行**：chip 按 accent 绘制（键盘被接管的 ghost 态随草稿一起变 dim），周围文本不受影响。换行**永不切开 chip**：放不下时整块换到下一视觉行（比整行还宽时溢出该行、由 widget 裁剪——终端窄到装不下标签时仍然看得见它是什么）。`text()`（草稿指纹、命令解析、选区复制）给出屏幕上的 chip 文本，提交的那一刻由 `expand_and_get_text` 换成 payload。

| 关注点 | 位置 |
|---|---|
| 段 / 注册表 / 单元步进 / 列解析 | `crates/wing/src/ui/input_area/model.rs` |
| 图片 chip（载荷 / 号码 / 标签文案 / `insert_image`） | `crates/wing/src/ui/input_area/image.rs` |
| 粘贴入口（阈值、小粘贴铺开、chip 插入） | `crates/wing/src/ui/input_area/paste.rs` |
| 原子删除 / 编辑 | `crates/wing/src/ui/input_area/editing.rs` |
| 光标移动（跨块步进、上下吸附） | `crates/wing/src/ui/input_area/movement.rs` |
| 换行（整块不切开） | `crates/wing/src/ui/input_area/wrap.rs` |
| 每行 run 级着色 | `crates/wing/src/ui/input_area/widget.rs` |

**已知限制**：ask 面板的内联编辑器是另一套单行编辑器（多行粘贴折叠成空格），不参与 chip 机制；chip 比整行还宽时标签尾部被裁剪（该行裁剪线以外的列按「行末」解析）；号码不复用，所以删掉若干 chip 后序号会有空档（这是刻意的——号码是身份，不是计数）。

## 粘贴（文本 / 图片）：OSC 5522 终端协议

除了 bracketed paste（`?2004`，只能承载文本、由终端直接写进 pty），TUI 还启用 **kitty clipboard protocol（DECSET 5522）**：用户 Cmd+V 时终端**不写数据**，只发一条 `OSC 5522` 的 unsolicited paste event（列出剪贴板可用的 MIME + 一次性密码），应用拿 OTP 发 follow-up read、收 DATA 分片、自己重组字节。文本与图片走同一条通道，**SSH 下同样有效**（协议走 pty 字节流，剪贴板动作发生在本地终端）。通道只负责「把字节读进来」：文本进 composer（与 bracketed paste 同一个入口 `App::handle_paste`），**照旧被模态挡板拦**（面板开着时丢给面板或丢弃，与 bracketed paste 语义自动一致）；图片进 `App::on_clipboard_image` 的单并发流水线（入队 → 后台压缩 → 图片 chip，见下），**不做门控**——图片的落点是草稿上一个可见可删的 chip，静默丢字节才是最坏的结果。

```text
TUI 启动:  CSI ? 5522 h          （与 ?2004 等在同一处：tui::enter_sequence，退出时 CSI ? 5522 l 收回）
Cmd+V:     终端 → 应用  ESC]5522;type=read:status=OK:pw=<OTP>ST
                      ESC]5522;…status=DATA:mime=<.>;…   （targets listing：空格分隔的 MIME）
                      ESC]5522;…status=DONE…
           应用 → 终端  ESC]5522;type=read:name=<b64 程序名>:id=…:pw=<b64 OTP>;<b64 MIME>
           终端 → 应用  OK → DATA（每片 ≤4096 原始字节、各自独立 base64）→ DONE
```

**为什么需要 stdin 预处理层**（`crates/wing/src/tui/stdin_proxy.rs`）：crossterm 0.29 会把 `ESC ] 5522 ; …` 解析成 `Alt+]` + 逐字符按键（协议数据污染成用户输入，负数基线钉在 `tests/crossterm_osc5522_misparse.rs`）。分流层在 crossterm 看到输入之前把完整的 5522 序列抽走，**其余字节逐字节透传**；用 **pty 接驳**而不是 pipe 是硬要求——crossterm 的输入 fd 由 `isatty(stdin)` 决定（false 时改开 `/dev/tty`，会绕过接驳），所以把 fd 0 换成**新 pty 的 slave**（仍是 tty），泵线程读原 tty、把透传字节写进 master。生命周期是**进程级**的：**第一段** TUI（配置可用时是 `tui::init_terminal()`，配置不可用时是向导的 `tui::init_terminal_without_paste_events()`）经私有的 `begin_tui_session()` 做「真实 tty 进 raw → 装接驳」，`tui::end_tui_session()` 在**所有** TUI 段结束后做「还 fd 0 → `disable_raw_mode`」（它是对 fd 0 的 termios 操作，顺序反过来会把 pty 的 termios 写进真实 tty）；`cmd` 的 panic hook 走同一条收尾路径，`run_tui` 另有一层 RAII guard 兜住提前 `?`。

分流的状态机细节（都是单测钉住的）：前缀候选最长持有 25 ms（`Esc` 键的形态就是一个孤立 `ESC` 字节，队列已空时立刻放行，不吃延迟）；序列体 5 s 等不到终止符就丢弃（BEL 与 ST 两种终止符都认）；**bracketed paste 保护区内一律原样透传**（粘贴内容里的字面 `ESC]5522;` 是用户数据，不能吃）。

**已知边界（粘贴内容以字面 `ESC` 结尾）**：保护区内「可能是闭界开头」的尾部字节会被扣住，内容末尾的字面 `ESC` 因此先被当作 `ESC[201~` 的前缀候选；真闭界到达时它原样冲回，字节流里出现连续的 `ESC` + `ESC[201~`。分流层**一字节不丢**（「透传 + 序列 == 输入」的不变式钉住），但这段流对任何下游都是两义的：哪个 `ESC` 属于粘贴内容、哪个属于闭界，只有终端知道（协议里除「闭界紧跟数据」没有别的信息可分）。固有歧义 + 极罕见，登记为已知边界，本层不做（也做不了）区分。

**两段 TUI（首次运行：向导 → 主 TUI）**：crossterm 的事件源是**进程级单例**，首次 `poll` 时把当时的 fd 0 注册进就绪源、永不重建，而**读取永远走当时的 fd 0**。于是注册与读端必须是同一个文件描述，这给出两条硬约束：

1. **接驳必须早于本进程任何一次 `poll`**。向导正是最早的机会（它自己也用 crossterm 的全局事件源），所以第一段就把接驳装上；反过来（先 `poll` 再装）泵线程会把真实 tty 的就绪吃掉，事件源从此再不被唤醒 —— 键盘永久哑掉（反面证据：`tests/stdin_proxy_stale_event_source.rs`）。
2. **段与段之间绝不拆接驳**（`restore_terminal` 只写离开序列，raw mode 与 fd 0 归会话）——拆了就必须重装，而复用的单例仍指着旧的文件描述。

两条都有测试，但分工要说清：`tests/stdin_proxy_two_segments.rs` / `stdin_proxy_stale_event_source.rs` 钉的是**机制**（同一单例跨段工作、注册先于接驳才有效），生产接线则由源码门禁 `tests/session_guard.rs` 钉住——它扫真实 `src/**`，断言 `stdin_proxy::install(` 恰好一处且位于 `begin_tui_session`、`stdin_proxy::restore(` 恰好一处且位于 `end_tui_session`、`restore_terminal` 不引用这两者（零豁免；把安装挪走或把拆解加回段边界都会变红）。

向导与主 TUI 的唯一差别是**粘贴事件模式**：向导走 `tui::init_terminal_without_paste_events()`（不发 `?5522h`），因为向导自己的事件循环不驱动协议机，开了模式只会把用户 Cmd+V 吞成一条没人读的协议事件、并把它留给主 TUI 的协议机；文本粘贴在向导里因此保持 bracketed paste（行为与从前逐字相同）。

**已知边界（段间隙的按键语义）**：raw mode 与接驳跨段保持（这正是上面第 1 条的代价），所以向导段与主 TUI 段之间那几十毫秒（connect / create_session / subscribe）里键入的字节不再走行规程——`Ctrl+C` 不会立刻 `SIGINT`，而是作为 `0x03` 排队、交给主 TUI 的按键语义（双击退出），其余按键照旧回放。要连 Ctrl+C 也拦下来的话，只能把向导拆成独立进程（或先关 raw，但那会精确打破第 1 条）。

**MIME 选择**（`tui/clipboard_protocol.rs::choose_mime`）：image/png → image/jpeg → image/webp → image/gif → 任意 `image/*` 兜底 → `text/plain`；都没有就不读（toast 告知）。比较前剥掉 `;charset=…` 参数并忽略大小写（`normalize_mime`：**请求侧挑表示与 read DATA 的 mime 归属共用这一份归一**——终端可以只在回显时带上参数 / 大小写不同，逐字相等会把 DATA 静默丢光）。请求里**必须带 `name`**——Ghostty 的授权判定把「没有 name 的 pw」当成没有密码（`stream_terminal.zig`），漏了就退回「要用户确认」甚至被拒。

**包归属**：event 三连**不带 `id`**（这是「终端自发事件」的唯一标志），我们发的 read 请求带**唯一 id**（`wing-paste-<n>`）且响应会回显——被放弃的读（超时 / 64 MiB 超限）之后终端仍会把剩下的包补发完，唯一 id 让这些残留落不到下一次读上。event 侧没有 id 可比，就比**一次性密码**（终端为每次粘贴新生成、三连里每包回显）：被放弃的 event 迟到的 listing / DONE 因此同样落不到下一次粘贴上。**失败都要有反馈**：收不全的 event、读超时、终端拒绝（EPERM/ENOSYS）、零数据 DONE（协议用它表示「该表示不可用」）、**event 队列满导致的丢弃**一律走 toast，不出现「粘了但什么都没发生」。

**图片：入队 → 压缩 → chip（单并发流水线）**。`App::on_clipboard_image` 把字节按 FIFO 入队（条数 8 / 总字节 64 MiB，超限丢**最旧**一张并给 warning toast `Image dropped: too many pending images`——与其它失败同一条红线），队列非空且没有在飞的压缩时取队首、**move** 进 `AppIntent::CompressClipboardImage { bytes, options }`（`options` 来自 `paste.image.*`，入队那一刻取，越界值已夹紧）。runner 在**阻塞池**上跑 `util/image_compress`（解码 / 白底 flatten / 长边缩放 / JPEG + 预算循环），结果经 `FetchPayload::ClipboardImage` 回到 `App::on_clipboard_compressed`：成功 → `input.insert_image`（chip 号码 = 插入序 = 粘贴序），失败 → warning toast（`Image dropped: <原因>`）。**一次只压一张**不是保守而是顺序契约（并发压完的到达序不确定，号码就会跳序）；**成功不 toast**（chip 出现本身就是反馈，与文本粘贴一致）。这条回执**不属于任何会话**：`handle_fetch_result` 在会话守卫**之前**分派它——丢掉它会让「在飞」永远为真，之后所有粘贴都不落地。

**已知边界（图片 chip 的落点）**：chip 落在**压缩回执时刻**的光标处而非粘贴时刻（压缩在阻塞池异步跑，大图可达数百毫秒），期间用户移动过光标的话 chip 就跟随最新光标——这是有意取舍（原位插入要假定粘贴时刻的行列在压缩期间仍有效，而那恰是草稿最可能变的地方；号码 = 粘贴序的契约不受影响）。

chip 的文案是**契约**：`[Image #N W×H SIZE]`（记法与消息侧的标签行共用一份实现，`util/format.rs::image_label`；形状断言钉在那里，chip 侧钉在 `ui/input_area/image.rs`），其中 `W×H` 与 `SIZE` 取的都是**压缩后**的产物（那才是真正发出去的字节；`SIZE` 用紧凑记法 `512B` / `384KB` / `1.0MB`，永不写 `1024KB`）。渲染与粘贴 chip 完全同款：accent 上色、行内、整块不切开、ghost 态随草稿一起 dim。

| 关注点 | 位置 |
|---|---|
| 分流状态机 + fd 接驳 / 泵线程 | `crates/wing/src/tui/stdin_proxy.rs` |
| 协议机（包解析 / 请求构造 / 分片重组 / 超时）+ 驱动任务 | `crates/wing/src/tui/clipboard_protocol.rs` |
| mode 启停（逐字节断言）与接线 | `crates/wing/src/tui/mod.rs`（`enter_sequence` / `leave_sequence` / `spawn_event_stream`） |
| App 边界（文本 / 图片流水线 / 失败） | `crates/wing/src/app/clipboard.rs` |
| 图片 chip 的接口（注册表 / 标签 / 插入） | `crates/wing/src/ui/input_area/image.rs` |
| 端到端证据（自开 pty，headless 可跑） | `crates/wing/tests/stdin_proxy_pty.rs`、`tests/crossterm_osc5522_misparse.rs` |

**支持终端与政策**：

| 环境 | 行为 |
|---|---|
| Ghostty（1.3.1 起） | 完整可用（文本 + 图片，本地 / SSH 同）；kitty 预期一致（协议同源） |
| 其它终端（忽略 `?5522h`） | DECSET 被忽略 → 图片粘贴不可用（**不做回落**：政策是让用户用支持协议的终端）；文本粘贴走 bracketed paste，行为不变 |
| tmux / zellij 内 | 多路复用器拦未知 OSC（除非用户开 passthrough）→ 登记为**已知限制**（先进终端直连） |
| Windows | 不承诺（接驳层是 `cfg(unix)`；非 Unix 上 `install()` 是空操作，也不开 `?5522`） |

**不做的事**：不查 DECRQM 探测支持性（忽略模式集的终端本就不受影响）；不做终端无确认弹窗之外的授权流程（OTP 由 event 自带，`clipboard_read=ask` 下也免弹窗）；不做「一次粘贴多 item」的强承诺（尽力：分片与多 event 队列处理；**队列满 / 图片流水线超预算时丢弃是取舍，但丢弃一定有反馈**——warning toast，不做静默丢弃）。

**待真机确认项**见文末「真机验收清单」的 D 节：macOS 侧 listing 的 MIME 名称按 `UTType.preferredMIMEType`
推断应是标准 MIME（`image/png` 等），需在 Ghostty 实机确认（截图 / 浏览器复制图片两种来源）；确认 `Cmd+V`
文本与图片各一次的行为、以及图片字节与剪贴板一致。

## 消息里的图片：正文之后的标签行

发出去（`pending`）与重放（resume / `SyncSession`）的用户消息，正文之后一行一张地渲染图片标签：
`[Image #1 1024×768 384KB]`——与 composer 的 chip **同一套记法**（`util/format.rs::image_label`），
但号码是**消息内**的 1-based 序号（按媒体在消息里的顺序），**不是**草稿里的 chip 号码：重放只有媒体列表、
没有草稿号码，重编号让 resume 前后同一条消息长得一样（删过 chip 时最明显）。草稿号码与消息号码是两套
自洽的口径，各自记在 `ui/input_area/image.rs` / `ui/chat_view/media.rs`。

| 关注点 | 位置 |
|---|---|
| 显示文本（正文 + 标签行）与两侧标签构造（活体 `CompressedImage` / 重放 `MediaRef`） | `crates/wing/src/ui/chat_view/media.rs`（唯一一份；无图片时**恒等**返回正文） |
| 历史投影里的 `media` 解析（宽容：缺省 / `null` / 坏条目 / 非数组都不炸整条记录） | `crates/wing/src/protocol/history.rs::SessionMessage.media` |
| 重放路径（`role=user` → cell） | `crates/wing/src/app/replay.rs` |
| pending 入队时的显示文本定格 | `ChatView::push_pending`（`ui/chat_view/model.rs`） |

**三条路径一段文本**：本客户端发出去的消息（pending → `promote` 原样搬进历史）、重连 / resume 的
重放、以及纯图消息（正文为空）——都走 `text_with_labels`。无 media 的消息**恒等**（cell 的文本就是
正文），这是「与从前逐字节一致」的落点；坏掉的媒体条目只花掉一行标签，不花掉整条消息。正文已经以
`\n` 收尾时不再补行——标签接在下一行行首（别的入口写进历史的尾换行不该在 resume 时露出一个空行；
TUI 自己的提交路径先 `trim()`，走不到这一支）。
样式上标签行走消息自身的颜色（pending 是 dim、历史是正常色），**不单列 accent**：cell 的文本是单一
样式的 `String`，要分色就得把「哪几行是标签」带进 `ChatCell`（而 `ChatCell::UserMessage(String)` 的
形状正是 frame / viewport / selection 四处特殊处理的前提）。形态（括号 / `Image` / `#N` / `W×H` / 紧凑
字节记法）与 chip 逐字一致——对齐的是形状。

## 发送失败 / 被拒：把内容还给草稿

草稿在提交那一刻就清了，内容只在 `pending` 队列里；所以**每一条失败路径都要把内容放回去**，否则用户
刚写的字与刚贴的图就没了。`pending` 条目因此携带**提交产物原件**（`Submission { content, images }`，
`ChatView::push_pending`），失败时 `ChatView::take_pending` 出队并交回原件，`App::restore_failed_send`
写回 composer + warning toast：

| 失败路径 | 触发点 | 行为 |
|---|---|---|
| 网关断连（transport 为 None） | `runner::dispatch_send_message` 的本地分支 | 不上传不发送 → 回填 + `Not sent — gateway disconnected; draft restored` |
| 上传失败 | 后台任务（`runner::deliver_user_message`，`upload_media` 出错） | 整条不发（**不发送半截**：少一张图的引用列表会被当成完整输入）→ 回执 → 回填 + `Image upload failed: …; draft restored` |
| WS 发送失败 | 同上 | 回执 → 回填 + `Send failed: …; draft restored` |
| **服务端拒绝** | `WingEvent::Error` 携带的 `request_id` 命中本地 pending（网关对失败的 `ClientRequest` 帧回填客户端的 `request_id`） | 取回 + 回填 + toast；条目**已出队**，随后 `done` 的兜底提升绝不把它当「已发送」 |

**后台发送（慢链路）**：逐张上传 + WS 发送跑在**后台任务**里（`runner::spawn_send_message`），
事件循环一步不等网络——慢链路（远程网关 / SSH 隧道）上 N 张图 = N 个完整往返，内联 await 会把 UI 冻住
秒级（冻结还会把 `TermEvent` / sequences 通道填满，联动丢粘贴流量）。失败经 `FetchPayload::SendFailed`
回执回到 App（与压缩回执同一条通道，且同样在会话守卫**之前**分派：条目按 `request_id` 认领）。
两条契约照旧：**相邻提交串行**（链尾把任务串起来，帧到达顺序 = 提交顺序，后端按到达序入队），
每条消息内部**先逐张上传（失败即停）再发帧**。

**回填是纯数据恢复**：只写 composer 与 toast——不 push intent（不重发、不重复上传）、不往 chat 写 cell
（内容回到草稿，不该同时在历史里显示一遍；这与**打断**路径刻意相反：打断用 `discard_pending` 留下一条
struck-through 的 cell，因为那内容没有回到草稿）。空草稿走 `set_text` 精确还原；草稿里已经有东西
（消息在飞的那几秒用户又打了字 / 粘了图）时**追加在整份草稿末尾**（`move_draft_end` + `insert_str`）——
`set_text` 的既定语义是替换整份草稿、连它的 chip 一起丢，那会吃掉用户刚写的东西。图片用
`insert_image` 重新入册：号码按 composer 的既有契约走——**空草稿从头计，草稿里已有图片 chip 时从图片号码
之后续号**（长文本 chip 不算：两套号码互不干扰；号码是草稿内的身份，不是计数）。

**关联不到 pending 的 `error` 什么都不做**：turn 级失败（LLM 报错等）的 `request_id` 是后端新生成的
uuid，旧网关的拒绝帧同样没有客户端 id——两者都匹配不到任何条目，行为与从前逐字相同。同类边界还有
**重连 / 切会话**：`SyncSession` 到达时 `chat.clear()` 连 pending 一起清（既有行为），若失败回调
恰好落在其后，`take_pending` 取不回内容——此时只提示原因（toast **不带** `draft restored`），
不假装回填。

## 真机验收清单（图片粘贴链路，最终版）

人工过一遍（headless 测不了：真终端 + 真剪贴板 + 真模型）。环境：**Ghostty 1.3.1+**（本地或 SSH 同款）。

**A. 粘贴与 chip（05）**

1. 截图到系统剪贴板 → `Cmd+V`：草稿里出现 accent 色的 `[Image #1 1024×768 384KB]`（数字 / 尺寸 / 大小以真实截图为准），
   光标停在 chip 之后，**无 toast**。
2. 再粘一张 → `[Image #2 …]` 追加在光标处；`←/→` 一步跨过整块；chip 尾 `Backspace` / 首 `Delete` 整块删除（号码不复用）。
3. 长文本粘贴仍是 `[Pasted text #N +M lines]`（与图片 chip 互不干扰）；`Esc` 清草稿后 chip 与图片一起消失。

**B. 发送与模型读图（06 + 07）**

4. 草稿里打几个字（或纯图）→ `Enter`：pending 气泡显示正文 + `[Image #1 …]` 标签行（dim）；
   模型收到图（让它描述图片内容 / 读图上的文字）。
5. 多张：一次消息带 2–3 张，模型按提交顺序提到它们；`/clear` + 图 → 见到
   `Image dropped: /clear takes no image` 的 info toast。
6. 发送后 `resume`（`/session` 切走再切回，或重启 TUI）：历史里那条消息仍带 `[Image #1 …]` 标签行，
   **号码与 pending 时一致**（消息内 1-based）。**例外**：排队期间被后端 batch 合并的多条消息在历史里
   是**一条**（`run_turn` 的 drain-and-merge），标签行按合并后的消息重排（`#1..#N` 覆盖 batch 内的全部
   图片）——live 的每个 pending 气泡各自从 #1 数，合并行不会。

**C. 失败与回填（07）**

7. 断开网关（`wing stop` 或杀掉进程）后 `Enter`：warning toast `Not sent — gateway disconnected; draft restored`，
   文本与图片 chip **都在草稿里**（图片可再删）。
8. 上传失败：把**网关**的 `images.max_bytes`（不属于 TUI 配置，默认 4.5 MiB）调到比压缩产物小
   （如 `1024`，前端 `paste.image.max_bytes` 保持默认）再粘一张截图 → 上传 400：整条不发（无半截消息），
   内容回填 + `Image upload failed: …; draft restored`。
9. 消息在飞的那几秒里往草稿里打字：失败后新写的内容**没有被覆盖**，失败的消息接在末尾。

**D. 终端协议与退出（04）**

10. 配置不可用 → 走完首次运行向导 → 主 TUI 里按键正常（B1 命中路径）；退出后 shell 输入正常（无残留读者）。
11. `Shift/Option` 原生拖选、滚轮、面板键位手感无变化。
12. tmux 内确认图片粘贴不可用（登记限制）、文本粘贴照旧；非 Ghostty 的 kitty 终端预期同源可用（未实测）。

**待实机核对**：macOS 侧 listing 的 MIME 名称（推断为标准 MIME，`image/png` 等）；截图与浏览器「复制图片」
两种来源的 listing 差异；SSH 场景同一流程（协议走 pty，预期同效，未实测）。

## 链接渲染与单击打开（`tui-link-open`）

markdown 链接渲染为 OSC8 超链接，单击（无拖动）打开。模块分工：

| 关注点 | 位置 |
|---|---|
| 链接区间（IR → 行内显示列）/ OSC8 纯函数 / `ComposedLines` | `crates/wing/src/render/markdown/links.rs` |
| 流式渲染同步维护链接（`compose_into` / `lines_and_links`） | `crates/wing/src/render/markdown/stream.rs` |
| 行缓存携带链接 + 行号是否精确 | `crates/wing/src/ui/cached_cell.rs` |
| 本帧链接快照 / OSC8 注入 / 命中查询 | `crates/wing/src/ui/chat_view/link.rs` |
| 目标解析 / argv / 进程启动 | `crates/wing/src/util/open.rs` + `app/runner.rs`（`AppIntent::OpenLink`） |
| 点击 vs 拖动分流 | `crates/wing/src/app/mod.rs`（`App::mouse_link`） |

**OSC8 注入方式**：ratatui 0.30 没有任何超链接 API（`Cell` 只有 symbol/style/diff_option），所以序列写进 **`Cell` 的 symbol**：`ESC]8;;URL ST` + 字素 + `ESC]8;; ST`，**逐字素头**注入（宽字符只注入头单元，尾单元是填充空格，写进去会覆盖半个宽字）。两个必须同时做的细节：

1. **`CellDiffOption::ForcedWidth(1)`**：`Cell::cell_width()` 会用 `symbol().cell_width()` 量宽度，而 `BufferDiff` 用它跳过宽字符的尾随单元格（`self.pos += cell_width - 1`）——不 pin 住宽度，一条 URL 长度的 symbol 会让 diff **跳过链接后面的一整行**（屏幕残留旧内容）。ratatui 为此场景预留了 `ForcedWidth`（其文档原文提到转义序列的宽度与屏幕不一致，自带 kitty 占位符测试同款用法）。
2. **逐字素头自带 open/close**（而不是「首单元开、末单元关」）：diff 只重绘部分单元格时（例如滚动后只有部分格变化），每个被写出的单元格都自带完整序列，终端不会丢掉某个字符的链接属性。代价是链接单元格多 ~`URL 长度 + 12` 字节，仅在实际重绘时产生。

每帧开始时 ratatui 会 `reset()` 下一个 buffer（"each render pass starts from an empty buffer"），所以注入不跨帧残留；同一 URL 的序列逐字节恒定，连续帧的 `Cell` 相等 → **diff 不抖动**。

**`ForcedWidth` 必须等于终端真正渲染的宽度**（窄字 1、宽字 / CJK / emoji 2），不能一律写 1：`CrosstermBackend::draw` 对「下一个 cell 正好在 `last_x + 1`」的情况**跳过 `MoveTo`**，这个捷径只在「每个打印出来的符号正好推进一列」时成立。宽字素的 cell 若自称 1 列而终端按 2 列推进，它之后的每个 cell 都会被写到右边一格（`百度` 变 `百 度`、行尾挤到下一行），而且**只在部分重绘时出现**——整帧重绘是按顺序整行写出的，反而看不出来（这就是「静止没事、一滚动就拖影、强制重绘能消失」的成因）。回归测试：`a_wide_linked_grapheme_moving_one_column_keeps_the_wire_in_step`（重放真实 diff 流 + 模拟终端光标，断言没有错位）、`scrolling_linked_cjk_keeps_the_terminal_in_step`（整帧 + 断言每个 cell 的 `cell_width()` 等于可见宽度）。

**链接区间链路**：`MarkdownSegment.link_target`（解析层已产出）→ `line_link_spans()`（列 = 前序段 `unicode-width` 累加）→ 与渲染行平行的 `Vec<Vec<LinkSpan>>`（`ComposedLines`；流式路径在 `compose_into` 里加 2 列前缀偏移，`to_lines` 重放路径同理）→ `ChatViewWidget::render` 逐行映射成**屏幕绝对列**，落到两个消费者：① 本帧 Buffer 注入 OSC8；② `ChatView::frame_links` 快照（`link_at(column, row)`）。

**行号必须精确**：只有当 cell 的每一行显示宽度 ≤ 渲染宽度时（`CellLines::rows_exact`）才建立链接——此时 `Paragraph` 不可能折行，屏幕行号 == cell 内行号，列区间精确。两个取值来源：① 流式 cell 由 `stream.rs` 的硬折行保证恒真；② `to_lines`（冻结/replay/resize 之后）在**有链接时**逐行量一次（`!composed.has_links() || composed.rows_are_exact(width)`），随 width+generation 缓存。**真的会不成立**：IR 层的 UAX #14 预折行只作用于 prose，代码块与缩进代码是显式豁免的（`wrap.rs::is_prose_line`），所以「同一 cell 里有一个超宽代码行 + 后面有链接」时 `Paragraph` 会把代码行折成两屏行，之后的行号整体错位。此时该 cell **一个链接都不建立**（`rows_are_exact == false`）——宁可链接点不开，也不能把命中框和 OSC8 注入到代码文本上（保守降级，不猜列）。单测：`an_overwide_code_line_puts_the_cell_off_limits` / `an_overwide_indented_code_line_puts_the_cell_off_limits` / `a_fitting_code_line_keeps_its_link_on_the_right_row`。**链接目标不使用 `Style` 承载**（不可行：`set_stringn` 会丢掉含控制字符/零宽的字素，URL 的可见字符还会被算进折行列宽）。

**复制仍然干净**：`buffer_row_graphemes` 读 symbol 前先 `strip_osc8`，所以选区快照的宽度不被 URL 撑大、复制文本不含控制字符——**任何从 Buffer 反读文本/宽度的地方都必须先剥序列**。

**点击 vs 拖动**：`Down(Left)` 在 chat band 内先用**本帧快照**查链接（记进 `App::mouse_link`）**再**照旧开始选择（拖动要能选中链接文本）；`Up(Left)` 在「记录了链接且 `is_dragged() == false` 且释放点 == 按下点（`selection.anchor()`）」时打开并提前返回（零宽单击本来也不复制，所以不与复制冲突）。两个条件都查是刻意的：终端丢 motion 事件时不能靠「没收到 `Drag`」把一次真实的拖拽当成单击（reference 的 `isClick = !dragged && anchor == point` 同款）。`cancel_selection`（焦点丢失 / 结构失效）连 `mouse_link` 一起清——中止的手势不打开任何东西。命中来自按下那一帧，滚动/流式追加都不会让「按下 A、松开 B」。toast 画在 chat band 之上，所以 `render_toast` 返回绘制区域、`ChatView::mask_links` 把被盖住的命中框删掉（看不见的链接不可点）。**滚动条列同样会盖住链接**，但它不需要 `mask_links`：`App::mouse_press` 在链接命中之前就把落在滚动条列上的按下交给滚动条，释放点又被 `App::off_the_bar` 夹回左侧一列——那一列进不了 chat 的点击路径，点滚动条不会打开它盖住的链接。

**目标解析与打开方式**（`util/open.rs`）：带 scheme（`http` / `https` / `mailto` / `file` …；单个字母 + `:` 视为 Windows 盘符不算 scheme）= URL → 平台默认启动器（macOS `open` / Windows `cmd /c start ""` / Linux `xdg-open`）。其余按本地路径处理：`~` 展开为 `$HOME`、`file://`（含可选 `localhost`）剥离、**相对路径锚定 `current_dir()`（wing CLI 启动目录，不是会话 workspace）**、`#L10` / `#L10C5` / `:10` / `:10:5` 后缀剥离并解析成行（列）；路径先 `canonicalize`（失败则退化为绝对路径）再 `exists()` 检查——不存在就**不启动任何进程**。有行号时先按白名单找支持行跳转的编辑器 CLI（`code -g` / `subl` / `zed`，PATH 探测不用 `which` 子进程），都没有才退回默认程序（文件仍会打开，行号降级——这是「尽量」的诚实边界；`open` / `xdg-open` 都不接受 `path:line`）。

**安全边界**：① 只对 markdown 链接区间触发（消息里出现 `/etc/passwd` 不会变成可点）；② `Command::new(program).args(argv)`，**绝不经过 shell**、不拼字符串执行（`;`、`|`、`$(...)`、空格、引号都只是 argv 里的普通字符）；③ 注入 OSC8 的目标先剥控制字符（`sanitize_osc8_target`，防 markdown 正文反向注入终端序列）——**净化只用于注入，打开时仍用原始目标**；④ 进程输出 `Stdio::piped()` 捕获（raw mode 下继承 stdio 会写坏屏幕，crate 也 deny `print_stdout/stderr`），在 `spawn_blocking` 上跑并 `tokio::time::timeout`（500ms，与剪贴板探针同款上限——run loop 是串行 await，这个上限就是最坏 UI 冻结时长）封顶。

**反馈**：成功静默（`tracing::debug!`，打开本身有可见结果）；失败 `Open failed: …` toast + `tracing::warn!`（opener 不存在 / 非零退出并带 stderr / 路径不存在 / 超时）。

**已知限制**：只覆盖 chat 内容里走 markdown 的 cell（assistant 消息与 thinking；Ask 面板 / tool 输出不参与）；**同一 cell 里出现超宽代码行/缩进代码行时该 cell 全部链接失效**（`rows_are_exact`，见上，宁可没有链接也不错行）；行号跳转只认白名单编辑器；`~user/x` 不展开、Windows 上 `.cmd` shim（PATHEXT）探测不到 → 只降级行号（`util/open.rs` 模块注释同样登记）；`file://` 只支持空 authority 与 `localhost`（`file://nas/share/x` 明确报错而不是拼到启动目录下）；tmux/zellij 的 OSC8 透传取决于用户配置；不做 hover 自绘下划线/预览。

## 换行键：Shift+Enter 靠终端协议，Ctrl+J 才是退路

输入区把 `Shift+Enter` / `Alt+Enter` 映射成换行（`ui/input_area/mod.rs`），但**这条映射在多数终端上永远收不到**：传统终端把 `Shift+Enter` 和 `Enter` 都发成 `\r`，应用层无从区分。所以进入 TUI 时 (`tui/mod.rs` 的 `enter_sequence_for`) 会请求一次 kitty 键盘协议（`CSI > 1 u`，只开 `DISAMBIGUATE_ESCAPE_CODES`），支持它的终端（kitty / WezTerm / Ghostty / foot / alacritty …）此后按 CSI-u 上报，`Shift+Enter` 才真的带 SHIFT 修饰键到达；离开时 `CSI < 1 u` 收回（收回失败会让**退出后的 shell** 也收到 CSI-u 编码的按键，所以它在 `leave_sequence` 里排第一）。

三个约束别踩：

* **只在 Unix 上发**：crossterm 的 `PushKeyboardEnhancementFlags` 在 Windows 上无条件返回 `Unsupported`（老控制台 API 没有这个协议），若跟着主序列一起 `?` 会把整个进入序列带崩 —— 单独写、失败即忽略；
* **不支持的终端什么都不做**：它们忽略这串字节，`Shift+Enter` 退化成 `Enter`。于是 `Ctrl+J` 是必须留的兜底（不依赖任何协议），`shared/tips.rs` 里那条提示就是为此；
* **tmux 需要 `set -s extended-keys on`**（3.2+）才透传，否则同样是退化成 `Enter`。

## 已知中间态：原生拖选需要 Shift/Option

鼠标上报接管后，终端不再把拖拽交给自身的文本选择——**不按 Shift（macOS 用 Option）的拖选不再选中文本**。这是回退 #28 的已知代价：应用内自研选择（`tui-text-selection` + `tui-composer-pointer`）已恢复 **chat 区域与 composer** 的免修饰键体验，同时保留 Shift/Option 原生拖选作为兜底（状态栏与弹层区域仍只能用原生方式）。

## Ctrl+O：思考的详细 / 简略切换（全局）

`Ctrl+O` 翻转思考块的展开（[`app/modal.rs`](../../crates/wing/src/app/modal.rs) 的 `is_toggle_reasoning_key` → `KeyRoute::ToggleReasoning`）：

* **应用保留键**，与 `Esc` / `Ctrl+C` 同档：在弹层（ask 面板 / `/model` picker / 命令 popup）之下也生效；唯一的硬要求是**必须在 composer 之前截住** —— 落到 composer 会被当普通字符 `o` 输入（composer 对 `Char(c)` 不做修饰键过滤，只有 `Ctrl+J/M` 是例外）；
* **全局**：整条 transcript（所有轮）一起翻转，不是"只作用于当前回"；模式是 `ChatView` 的一个会话级字段（`thinking_expanded: Option<bool>`），会话内一直有效 —— resume / compaction / sync 重建后保持，新起的块天然跟随；
* **初始默认**由配置给：`hidden` = 默认折叠（只剩标题行），`visible` = 默认展开（标题行 + 正文）。标题行（刷光 + 读秒）是所有思考块的固有部分；语义细节见 [tui-rendering.md](tui-rendering.md) 的「思考块：标题行与刷光」；
* **发现**：这次会话还没按过 `Ctrl+O` 时，**进行中**的标题行行尾带一条静态提示 —— 展开中 `· Ctrl+O 折叠`、折叠中 `· Ctrl+O 展开`（不扫光；按过一次即退场；宽度装不下时第一个让位）—— 键位本身也在开屏 tips 池里（`shared/tips.rs`）。
