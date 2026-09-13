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

- **不占布局宽度**：画在 chat 区域最右一列上（Buffer patch），不缩小换行宽度、不重排内容；只在内容溢出视口时存在。绘制顺序：chat 内容之上、toast 之下。
- **命中要求列严格相等**且行落在 chat 视口内——最后一列的普通点击不会误触发滚动条。
- **交互**：左键按下轨道空白处 → 跳转（手柄中心对齐指针）；按住手柄拖动 → 保留抓取偏移实时定位；拖动越界（离开轨道 / 离开 chat 区域）钳制到两端继续跟随；释放结束拖动。
- **不消费滚轮**：滚轮分支在 `handle_mouse` 里先匹配，指针停在滚动条上时滚轮照常滚 chat。
- **跟随契约**：跳转 / 拖动经 `ChatView::scroll_to` 复用同一套判定（离开底部 = 阅读态，拖到底部 = 重新武装跟随）。
- **状态清理**：几何为 `None` 的那一帧（内容不再溢出）顺手清理 hover/drag，按下未命中滚动条、或窗口失焦时同样清理——条不在屏幕上就不保留激活态。
- **重绘节流**：指针移动与拖动事件走 `chat_dirty`（受 16ms 帧门限），滚轮 / 按下 / 释放这类离散手势走 `input_dirty`（立即重绘）；无状态变化的事件不设任何 dirty 标志。
- **整帧断言**：`App::draw` 对 backend 泛型化，测试用 `TestBackend` 走真实 `Terminal`，把「只写 chat 最右列 / toast 与滚动条同帧共存」这类帧级性质钉在单测里。

## 自动跟随契约

「底部跟随新内容 / 滚动离开底部后不被拉回 / 回到最底部重新武装跟随」由 `ChatView` 的滚动方法统一承载，所有滚动入口（滚轮、键盘、`jump_*`）共用同一状态，细节与 Scenario 见 openspec change `revert-alternate-scroll` 的 `tui-scroll-follow` spec。

## 已知中间态：原生拖选需要 Shift/Option

鼠标上报接管后，终端不再把拖拽交给自身的文本选择——**不按 Shift（macOS 用 Option）的拖选不再选中文本**。这是回退 #28 的已知代价：应用内自研选择（`tui-text-selection`）会恢复免修饰键体验，同时保留 Shift/Option 原生拖选作为兜底。
