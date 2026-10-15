//! Terminal lifecycle management — init, restore, and crossterm event stream.
//!
//! Borrowed concepts from codex-rs/tui/src/tui.rs (Apache 2.0).

use std::fmt;
use std::io;
use std::io::Stdout;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use crossterm::Command;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
#[cfg(unix)]
use crossterm::event::KeyboardEnhancementFlags;
#[cfg(unix)]
use crossterm::event::PopKeyboardEnhancementFlags;
#[cfg(unix)]
use crossterm::event::PushKeyboardEnhancementFlags;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub mod clipboard_protocol;
pub mod stdin_proxy;

pub use clipboard_protocol::ClipboardFailure;
pub use clipboard_protocol::ClipboardPayload;

pub type WingTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Events from the terminal (keyboard, mouse, resize, paste, focus).
#[derive(Debug)]
pub enum TermEvent {
    Key(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Paste(String),
    /// OSC 5522 通道读回的剪贴板内容（或失败原因）——见 [`clipboard_protocol`]。
    /// 文本与 bracketed paste 走同一个入口（`App::handle_paste`）。
    Clipboard(Result<ClipboardPayload, ClipboardFailure>),
    Resize(u16, u16),
    Focus(bool),
    Tick,
}

// ---------------------------------------------------------------------------
// Mouse reporting (DECSET 1000 + 1002 + 1006, plus 1003 when hover pays off)
//
// Written by hand instead of using `crossterm::event::EnableMouseCapture`:
// the crossterm helper also enables RXVT coordinates (`?1015`). We only want
// key press/release plus button-motion (drag) events in SGR encoding, and the
// full `MouseEvent` (kind / modifiers / 0-based column & row) reaching the
// app, which is what the text selection and scrollbar changes build on.
//
// `?1003` (any-motion / hover) is what lets the scrollbar highlight while the
// pointer rests on it. It is the *only* place this mode is turned on, and the
// only mode gated by the environment: a multiplexer forwards every pointer
// movement with noticeable lag, so under tmux / zellij / screen the bar falls
// back to button motion — clicks, drags and the wheel are still reported,
// only the hover enhancement is lost (same trade-off as Pi,
// `tui-alt-screen.ts:350-363`).
//
// Alternate scroll (DECSET 1007) is deliberately NOT used: it makes the
// terminal translate the wheel into plain Up/Down keys, which are
// indistinguishable from real arrow keys and therefore get swallowed by any
// panel that navigates with Up/Down.
// ---------------------------------------------------------------------------

/// Environment variables meaning "pointer events pass through a multiplexer".
const MULTIPLEXER_ENV: [&str; 3] = ["TMUX", "ZELLIJ", "STY"];

/// Whether any-motion reporting (`?1003`) is worth enabling in this
/// environment. Pure over its inputs so both branches are unit-testable
/// without mutating the process env.
fn any_motion_enabled(env: impl Fn(&str) -> Option<String>, term: Option<&str>) -> bool {
    let multiplexed = MULTIPLEXER_ENV.iter().any(|key| env(key).is_some())
        || term.is_some_and(|t| t.starts_with("tmux") || t.starts_with("screen"));
    !multiplexed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableMouseReporting {
    /// Also report every pointer movement (`?1003`) — the scrollbar's hover
    /// state depends on it; clicks / drags / wheel do not.
    any_motion: bool,
}

impl Command for EnableMouseReporting {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1000h\x1b[?1002h")?;
        if self.any_motion {
            write!(f, "\x1b[?1003h")?;
        }
        write!(f, "\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "EnableMouseReporting: WinAPI not supported, use ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Disable mouse reporting. The modes are turned off high-bit first, so the
/// terminal never ends up in a state where motion events are still enabled
/// while SGR encoding is already off.
///
/// `?1003` is disabled unconditionally: turning off a mode that was never
/// enabled is a no-op in every terminal, and it keeps the teardown a single
/// constant — the panic hook must not have to re-check the environment.
///
/// `pub` so the panic hook (`cmd`) can share the one teardown sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableMouseReporting;

impl Command for DisableMouseReporting {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "DisableMouseReporting: WinAPI not supported, use ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Write the sequence that enters TUI mode: alternate screen, mouse
/// reporting, bracketed paste, focus reporting, keyboard enhancement, hide
/// cursor.
///
/// This is the *only* place that decides the setup order — `init_terminal`
/// must not write these commands ad hoc. The exact bytes are asserted by
/// `test_enter_sequence_is_exact`, so reordering the arguments here is a
/// test failure, not a silent behavior change.
///
/// `?5522`（kitty clipboard protocol paste events：文本与图片的粘贴通道，见
/// [`stdin_proxy`] 与 [`clipboard_protocol`]）只在 stdin 接驳装上了才发：没装上还开，
/// 终端发来的协议数据会直接进 crossterm（被解析成按键），正是接驳层要拦的东西。
pub fn enter_sequence(w: &mut impl io::Write) -> io::Result<()> {
    enter_sequence_for(
        w,
        |key| std::env::var(key).ok(),
        std::env::var("TERM").ok().as_deref(),
        stdin_proxy::is_installed(),
    )
}

/// [`enter_sequence`] 的「**不发 `?5522h`**」形态：setup 向导用它。
///
/// 向导自己的事件循环（`cmd/setup.rs` 的 `TerminalEvents`）只吃按键与 resize，**不驱动
/// 5522 协议机**：开了模式只会把用户 Cmd+V 的文本吞成一条没人读的协议事件，还会把这条
/// 陈旧事件留给主 TUI 的协议机。向导因此保持 bracketed paste 路径（与从前逐字相同）。
pub fn enter_sequence_without_paste_events(w: &mut impl io::Write) -> io::Result<()> {
    enter_sequence_for(
        w,
        |key| std::env::var(key).ok(),
        std::env::var("TERM").ok().as_deref(),
        false,
    )
}

/// [`enter_sequence`] with an injectable environment, so the multiplexer
/// downgrade is covered by tests that do not depend on how the suite is run.
fn enter_sequence_for(
    w: &mut impl io::Write,
    env: impl Fn(&str) -> Option<String>,
    term: Option<&str>,
    paste_events: bool,
) -> io::Result<()> {
    crossterm::execute!(
        w,
        EnterAlternateScreen,
        EnableMouseReporting {
            any_motion: any_motion_enabled(env, term),
        },
        EnableBracketedPaste,
        EnableFocusChange,
        crossterm::cursor::Hide
    )?;
    if paste_events {
        // 与下面几条一样是裸 DECSET：没有 crossterm 的 Command 包装，字节在单测里钉死。
        write!(w, "\x1b[?5522h")?;
    }
    push_keyboard_enhancement(w);
    Ok(())
}

/// 请求终端把有歧义的按键按 kitty 键盘协议（CSI-u）报上来 —— **Shift+Enter
/// 能不能与 Enter 区分开，全靠这一条**：不支持协议的终端把两者都发成 `\r`，
/// 输入框只能当成"发送"。
///
/// 单独写、失败即忽略，而且只在 Unix 上发：
/// crossterm 的 `PushKeyboardEnhancementFlags` 在 Windows 上无条件返回
/// `Unsupported`（老控制台 API 没有这个协议），跟着主序列一起 `?` 会把整个
/// 进入序列带崩。不支持的终端只是忽略这串字节，退化成"Shift+Enter 等于 Enter"
/// —— Ctrl+J 仍是任何终端都能用的换行键（见 `shared::tips`）。
#[cfg(unix)]
fn push_keyboard_enhancement(w: &mut impl io::Write) {
    let push = PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES);
    let _ = crossterm::execute!(w, push);
}

/// 非 Unix：不发（见 [`push_keyboard_enhancement`]）。
#[cfg(not(unix))]
fn push_keyboard_enhancement(_w: &mut impl io::Write) {}

/// [`push_keyboard_enhancement`] 的逆操作。
#[cfg(unix)]
fn pop_keyboard_enhancement(w: &mut impl io::Write) {
    let _ = crossterm::execute!(w, PopKeyboardEnhancementFlags);
}

/// 非 Unix：没发过就不必收回。
#[cfg(not(unix))]
fn pop_keyboard_enhancement(_w: &mut impl io::Write) {}

/// Write the sequence that leaves TUI mode: mouse reporting off *first*, then
/// the alternate screen, bracketed paste, focus reporting, show cursor.
///
/// Ordering is load-bearing: an interrupted teardown must never leave the
/// terminal forwarding mouse reports to the shell. Shared by the clean-exit
/// path and the panic hook (both are byte-asserted by
/// `test_leave_sequence_is_exact`).
///
/// `?5522l` 无条件写（关一个没开过的模式是 no-op），理由与 `?1003l` 相同：
/// panic hook 只有一条路径，不必重新判断环境。
pub fn leave_sequence(w: &mut impl io::Write) -> io::Result<()> {
    // 键盘增强先收回：留着的话退出后连 shell 都会收到 CSI-u 编码的按键。
    // 丢失败也无所谓（没发过的平台上是空操作），绝不能连累下面这串恢复。
    pop_keyboard_enhancement(w);
    crossterm::execute!(
        w,
        DisableMouseReporting,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableFocusChange,
        crossterm::cursor::Show
    )?;
    // 粘贴通道只在这里收回（`?2004` 之后的固定位置，单测逐字节断言）。
    write!(w, "\x1b[?5522l")?;
    Ok(())
}

/// Initialize the terminal for TUI rendering (main TUI segment).
///
/// 会话（raw mode + stdin 接驳）由**第一段** TUI 开启、各段共用；进入 / 离开序列是每段
/// 各写一次。收尾必须走 [`end_tui_session`] —— 会话不是段级资源。
pub fn init_terminal() -> Result<WingTerminal> {
    init_terminal_with(enter_sequence)
}

/// [`init_terminal`] 的「不发 `?5522h`」形态：setup 向导（同进程的**第一段** TUI）用它。
///
/// 会话（含接驳）与主 TUI 完全一致 —— [`stdin_proxy`] 的安装**必须早于本进程任何一次
/// crossterm 事件源创建**，向导正是最早的机会；差别只在粘贴事件模式（见
/// [`enter_sequence_without_paste_events`]）。
pub fn init_terminal_without_paste_events() -> Result<WingTerminal> {
    init_terminal_with(enter_sequence_without_paste_events)
}

fn init_terminal_with(enter: fn(&mut Stdout) -> io::Result<()>) -> Result<WingTerminal> {
    // 会话幂等：第一段真开（真实 tty 进 raw → 装接驳，顺序见 [`begin_tui_session`]），
    // 后续段原样复用。任何失败都把会话收回去，绝不把终端留在 raw。
    begin_tui_session()?;
    let mut stdout = io::stdout();
    let entered = enter(&mut stdout);
    let built = entered.and_then(|()| Terminal::new(CrosstermBackend::new(stdout)));
    match built {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            let _ = end_tui_session();
            Err(e.into())
        }
    }
}

/// Restore the terminal to its pre-TUI *screen* form: the leave sequence only.
///
/// **不碰 raw mode 与 stdin 接驳** —— 两者属于进程级会话（[`end_tui_session`]）：段与段
/// 之间拆接驳会让 crossterm 事件源（进程级单例，就绪源绑定注册时刻的文件描述）与读端
/// 错配，第二段的输入就永远不来了。
pub fn restore_terminal(terminal: &mut WingTerminal) -> Result<()> {
    leave_sequence(terminal.backend_mut())?;
    Ok(())
}

/// 开启进程级 TUI 会话（幂等）：
///
/// 1. 真实 tty 进 raw —— 必须发生在接驳**之前**：crossterm 的 raw 打在 `tty_fd()`（此刻
///    是 fd 0 = 真实 tty）上，它把「原始 termios」存成静态、`disable_raw_mode` 时再写回
///    fd 0；顺序反过来保存的就是 pty slave 的 termios（收尾会污染真实 tty）。
/// 2. 装 stdin 接驳（[`stdin_proxy::install`]）：换掉 fd 0 之后本进程**所有** crossterm
///    输入都走分流层，事件源也只能在此之后创建（注册与读端必须是同一个文件描述，否则
///    主 TUI 的键盘会被掐死 —— 反面证据见 `tests/stdin_proxy_stale_event_source.rs`）。
///
/// 失败不致命：接驳装不上只是没有 5522 通道（[`enter_sequence`] 据此不发 `?5522h`）。
fn begin_tui_session() -> io::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    if let Err(e) = stdin_proxy::install() {
        tracing::warn!("stdin proxy unavailable, clipboard paste events disabled: {e}");
    }
    Ok(())
}

/// 收尾进程级 TUI 会话：停事件流 → 还原 fd 0（真实 tty）→ 把保存的 termios 写回。幂等。
///
/// 顺序是硬约束：
/// 1. [`stop_event_stream`] 最先——事件任务若正卡在对 fd 0 的阻塞读上（或将被卡上），
///    `restore` 的 `dup2` 会让它「读的是新 tty、就绪源却还指着旧 slave」，永远退不出来，
///    进程收尾挂在 `Runtime::drop` 等 worker 上（用户可见：双击 Ctrl+C 不退，按 Enter 才退）。
/// 2. `disable_raw_mode` 是把 termios 写回 **fd 0** 的，而 TUI 段里 fd 0 是内层 pty 的
///    slave，所以 `restore` 必须在前。
///
/// **离开 `run_tui` 的任何路径都要走到这里**（正常收尾、提前 `?`、panic hook）。
pub fn end_tui_session() -> io::Result<()> {
    stop_event_stream();
    stdin_proxy::restore();
    crossterm::terminal::disable_raw_mode()
}

/// Draw one frame and let the terminal have it.
///
/// The **only** way a frame reaches the screen: it runs the widget closure and
/// then the wire pass over the buffer it wrote ([`crate::ui::emoji_width::pin`]),
/// before ratatui diffs that buffer against the previous frame. A draw path
/// that calls [`Terminal::draw`] itself skips the pass and can shift a whole row
/// by one column on a terminal repaint (an emoji that measures two columns while
/// the diff re-sends the cell it covers — see the pass's module docs for the
/// regression).
///
/// The pass lives here rather than at each call site so that "every frame this
/// frontend sends is normalized" is a property of the one function that sends
/// it, not a rule the next draw path has to remember. That convention is
/// machine-checked from both ends: `tests/layer_guard.rs` scans the production
/// code for `Terminal::draw` / `try_draw` calls and allows them only in this
/// file (so a new draw path cannot slip past the pass unnoticed), and
/// `tests/vs16_row_drift.rs` paints through this function and checks the cell
/// the backend receives.
pub fn draw_frame<B, F>(terminal: &mut Terminal<B>, render: F) -> Result<(), B::Error>
where
    B: Backend,
    F: FnOnce(&mut ratatui::Frame<'_>),
{
    terminal.draw(|frame| {
        render(frame);
        crate::ui::emoji_width::pin(frame.buffer_mut());
    })?;
    Ok(())
}

/// 事件流的收尾信号（进程内一次性；`end_tui_session` 置位）。
///
/// 必须用独立 flag 而不是「通道关闭」：panic hook 在 unwind **之前**跑，彼时接收端还
/// 活着——唤醒字节会被当成一条真实事件成功送进通道，任务继续循环、再卡回 `read(fd 0)`。
static EVENT_STOP: AtomicBool = AtomicBool::new(false);

/// 事件流任务的句柄（`spawn_event_stream` 全进程只调一次；收尾时取走）。
static EVENT_TASK: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

/// 收尾等待事件任务退出的上限：正常是毫秒级（唤醒字节 → 任务在循环顶 / send 前看到
/// flag）。上限只用于「不让收尾无限期停在一个坏掉的读上」；到点记 warn 后照常收尾，
/// 终端绝不能因为等不到一个任务而留在 raw。
const EVENT_STOP_WAIT: Duration = Duration::from_millis(200);

/// 有界等待期间的唤醒字节重投间隔（见 [`stop_event_stream`] 的「半截序列」一段）。
const EVENT_WAKE_REPOST: Duration = Duration::from_millis(20);

fn event_task_slot() -> std::sync::MutexGuard<'static, Option<tokio::task::JoinHandle<()>>> {
    EVENT_TASK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 转发一个事件：停止信号已置 / 通道已断都返回 `false`（调用方退出循环）。
async fn forward(tx: &mpsc::Sender<TermEvent>, event: TermEvent) -> bool {
    if EVENT_STOP.load(Ordering::SeqCst) {
        return false;
    }
    tx.send(event).await.is_ok()
}

/// 停止事件流并**有界**等待它退出（[`end_tui_session`] 的第一步；幂等）。
///
/// 顺序：置 flag → [`stdin_proxy::wake_reader`]（向 fd 0 投 1 字节，打断 crossterm 的
/// 阻塞读）→ 轮询 `is_finished`，其间每 [`EVENT_WAKE_REPOST`] 重投一次唤醒字节。
///
/// 重投要救的形态：crossterm 的解析器扣着**半截序列**（半截 CSI / `ESC O` / 残缺 UTF-8；
/// OSC 不在其列——`ESC]` 会被立即解析成 `Alt+]` 事件）时，首个唤醒字节会被当成序列的
/// final 清掉（不产事件），任务再次阻塞在 `read(fd 0)` 上；下一次重投落在空解析器上，
/// 必然产出事件并把任务带回 flag 检查。CSI 分支由
/// `tests/stdin_proxy_teardown_partial_seq.rs` 钉住。
///
/// 重投够不到的可达形态有二：① panic 展开路径上任务停在 `tx.send(...).await`（通道满、
/// rx 还活着，唤醒字节无处着力）；② 等待窗口内新字节持续到达，解析器被一直喂成半截。
/// 两种都在 200ms 上限到点后照常收尾；②留下的读者会继续在 crossterm 的读循环里，
/// `dup2` 之后它阻塞读的是**真实 tty**——用户回 shell 后的第一下按键会被它吃掉
/// （与旧症状同源，只是不再挂住进程）。
///
/// 不能 `.join()` / `.await()`：本函数是同步函数、也会从 panic hook 走到，在 runtime
/// worker 上同步 join 有自锁风险。
fn stop_event_stream() {
    let handle = event_task_slot().take();
    let Some(handle) = handle else {
        return; // 没起过（向导 / 初始化失败路径）或已停过：no-op
    };
    EVENT_STOP.store(true, Ordering::SeqCst);
    stdin_proxy::wake_reader();
    let deadline = Instant::now() + EVENT_STOP_WAIT;
    let mut next_wake = Instant::now() + EVENT_WAKE_REPOST;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
        let now = Instant::now();
        if now >= next_wake {
            stdin_proxy::wake_reader();
            next_wake = now + EVENT_WAKE_REPOST;
        }
    }
    if !handle.is_finished() {
        tracing::warn!(
            "crossterm event task did not stop within {EVENT_STOP_WAIT:?}; continuing teardown"
        );
    }
}

/// Spawn a task that reads crossterm events and sends them over a channel.
///
/// Returns the receiver end. The task runs until the sender is dropped, a
/// fatal error occurs, or the session is torn down ([`stop_event_stream`]).
///
/// 若 stdin 接驳装上了（[`stdin_proxy::is_installed`]），同时把 OSC 5522 协议机
/// （[`clipboard_protocol::spawn`]）接上：它消费分流层提取的序列，把读回的文本 / 图片
/// 投到**同一个**事件通道。必须在 crossterm 事件源首次创建之前调用（fd 0 在此时已经
/// 是 pty slave，事件源注册的就是它）。
pub fn spawn_event_stream() -> mpsc::Receiver<TermEvent> {
    let mut slot = event_task_slot();
    // 全进程只调一次（真实调用点只有 `app::run_app`）：第二次 spawn 要么覆盖旧 handle
    // （旧任务永远停不下来），要么（收尾过之后再 spawn）踩上不复位的 `EVENT_STOP`——新流
    // 在循环顶立即退出，键盘永久哑掉且零日志。常开断言（而不是 debug_assert!）：这个
    // 不变量被打破时的静默失败比启动路径上响一声坏得多；断言先于任何 spawn，所以失败
    // 调用什么都不留（`tests/tui_event_stream_once.rs` 钉住拒绝路径）。
    assert!(
        slot.is_none() && !EVENT_STOP.load(Ordering::SeqCst),
        "spawn_event_stream must run once per process"
    );
    let (tx, rx) = mpsc::channel(128);
    if let Some(sequences) = stdin_proxy::take_sequences() {
        clipboard_protocol::spawn(sequences, tx.clone());
    }
    let handle = tokio::spawn(async move {
        loop {
            // 收尾信号先于一切：置位后连一次 poll 都不再进（见 `stop_event_stream`）。
            if EVENT_STOP.load(Ordering::SeqCst) {
                break;
            }
            // Poll crossterm with a timeout so we can emit ticks.
            let timeout = std::time::Duration::from_millis(100);
            match crossterm::event::poll(timeout) {
                Ok(true) => match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(key)) => {
                        if key.kind == KeyEventKind::Press
                            && !forward(&tx, TermEvent::Key(key)).await
                        {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::Mouse(mouse)) => {
                        if !forward(&tx, TermEvent::Mouse(mouse)).await {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::Resize(w, h)) => {
                        if !forward(&tx, TermEvent::Resize(w, h)).await {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::Paste(text)) => {
                        if !forward(&tx, TermEvent::Paste(text)).await {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::FocusGained) => {
                        let _ = forward(&tx, TermEvent::Focus(true)).await;
                    }
                    Ok(crossterm::event::Event::FocusLost) => {
                        let _ = forward(&tx, TermEvent::Focus(false)).await;
                    }
                    Err(e) => {
                        tracing::error!("crossterm read error: {e}");
                        break;
                    }
                },
                Ok(false) => {
                    if !forward(&tx, TermEvent::Tick).await {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("crossterm poll error: {e}");
                    break;
                }
            }
        }
    });
    *slot = Some(handle);
    rx
}

/// Check if a key event is the quit shortcut (Ctrl+C).
pub fn is_quit_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact bytes of each path, spelled out once so that reordering or
    /// extending a sequence is a test failure rather than a silent change.
    ///
    /// `ENTER_BYTES` is the non-multiplexed variant (any-motion on);
    /// `ENTER_BYTES_BUTTON_MOTION_ONLY` is what tmux / zellij / screen get.
    /// Both carry `?5522h`（粘贴通道装上了才有这一条，见 `ENTER_BYTES_NO_PASTE_EVENTS`）。
    const ENTER_BYTES: &str = concat!(
        "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?25l",
        "\x1b[?5522h",
        "\x1b[>1u",
    );
    const ENTER_BYTES_BUTTON_MOTION_ONLY: &str = concat!(
        "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?25l",
        "\x1b[?5522h",
        "\x1b[>1u",
    );
    /// stdin 接驳没装上时的进入序列：一个 5522 字节都不发（否则协议数据会喂给 crossterm）。
    const ENTER_BYTES_NO_PASTE_EVENTS: &str = concat!(
        "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?25l",
        "\x1b[>1u",
    );
    const LEAVE_BYTES: &str = concat!(
        "\x1b[<1u",
        "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[?2004l\x1b[?1004l\x1b[?25h",
        "\x1b[?5522l",
    );

    /// Stub environment lookup (`std::env` is not readable in a deterministic
    /// test), so both multiplexer branches are covered regardless of how the
    /// suite happens to be run.
    type EnvLookup = fn(&str) -> Option<String>;

    /// No multiplexer, a plain terminal: the hover-capable variant.
    fn plain_env(_: &str) -> Option<String> {
        None
    }

    /// tmux-like environment: every pointer move would be forwarded.
    fn tmux_env(key: &str) -> Option<String> {
        (key == "TMUX").then(|| "/tmp/tmux-501/default,1234,0".to_string())
    }

    fn ansi_of(cmd: impl Command) -> String {
        let mut out = String::new();
        cmd.write_ansi(&mut out).expect("write_ansi into a String");
        out
    }

    /// Parse `\x1b[?<n><h|l>` sequences into `(mode, enabled)` pairs, skipping
    /// the kitty keyboard-protocol push / pop (`\x1b[>1u` / `\x1b[<1u`) that
    /// shares the `ESC [` prefix. Panics on anything else, so a truncated or
    /// unexpected sequence cannot pass by being silently dropped.
    fn parse_modes(seq: &str) -> Vec<(u32, bool)> {
        let mut modes = Vec::new();
        let mut rest = seq;
        while let Some(start) = rest.find("\x1b[") {
            let tail = &rest[start + 2..];
            match tail.chars().next() {
                Some('?') => {
                    let end = tail
                        .find(['h', 'l'])
                        .unwrap_or_else(|| panic!("mode flag missing in {tail:?}"));
                    let mode = tail[1..end].parse().expect("mode number");
                    modes.push((mode, tail.as_bytes()[end] == b'h'));
                    rest = &tail[end + 1..];
                }
                // kitty 键盘协议：不是模式开关，越过它继续。
                Some('>' | '<') => {
                    let end = tail
                        .find('u')
                        .unwrap_or_else(|| panic!("kitty sequence unterminated in {tail:?}"));
                    rest = &tail[end + 1..];
                }
                other => panic!("unexpected sequence {other:?} in {rest:?}"),
            }
        }
        modes
    }

    /// 键盘增强那对字节就是协议本身（CSI `>1u` / `<1u`）—— 字面量在这儿再写
    /// 一遍，改协议必须连着改测试，不能悄悄漂。
    #[cfg(unix)]
    #[test]
    fn test_keyboard_enhancement_uses_the_kitty_protocol_bytes() {
        assert!(
            ENTER_BYTES.ends_with("\x1b[>1u"),
            "进入序列末尾请求键盘增强：{ENTER_BYTES:?}"
        );
        assert!(
            LEAVE_BYTES.starts_with("\x1b[<1u"),
            "离开序列先收回键盘增强：{LEAVE_BYTES:?}"
        );
    }

    #[test]
    fn test_enter_sequence_is_exact() {
        // `init_terminal` consumes `enter_sequence`, which only supplies the
        // real environment to this function; the env is injected here so the
        // assertion does not depend on whether the suite runs inside tmux.
        let mut out = Vec::new();
        enter_sequence_for(&mut out, plain_env, Some("xterm-256color"), true)
            .expect("write enter sequence");
        assert_eq!(String::from_utf8(out).unwrap(), ENTER_BYTES);
    }

    /// 没装上 stdin 接驳就不开 5522：开了等于把协议数据喂给 crossterm（本层要拦的东西）。
    /// 向导形态（[`enter_sequence_without_paste_events`]）则永远不发 —— 与环境无关的
    /// 断言在这里查一遍字节（进程内是否装上接驳由集成测试查，见 `stdin_proxy_pty.rs`）。
    #[test]
    fn test_enter_sequence_skips_paste_events_without_the_stdin_proxy() {
        let mut out = Vec::new();
        enter_sequence_for(&mut out, plain_env, Some("xterm-256color"), false)
            .expect("write enter sequence");
        assert_eq!(String::from_utf8(out).unwrap(), ENTER_BYTES_NO_PASTE_EVENTS);
        let mut wizard = Vec::new();
        enter_sequence_without_paste_events(&mut wizard).expect("write enter sequence");
        assert!(
            !String::from_utf8_lossy(&wizard).contains("?5522"),
            "向导形态永不发 ?5522h（真实 env 下也成立）"
        );
        assert!(!ENTER_BYTES_NO_PASTE_EVENTS.contains("?5522"));
        // 两个变体只差这一条字节。
        assert_eq!(
            ENTER_BYTES.replace("\x1b[?5522h", ""),
            ENTER_BYTES_NO_PASTE_EVENTS
        );
        // 收回是无条件的（panic hook 只有一条路径）。
        assert!(LEAVE_BYTES.ends_with("\x1b[?5522l"));
    }

    #[test]
    fn test_enter_sequence_drops_any_motion_under_a_multiplexer() {
        // tmux / zellij / screen forward every pointer movement, which is
        // exactly the event flood `?1003` would create → button motion only.
        // Hover degrades; clicks, drags and the wheel still work.
        let cases: [(EnvLookup, &str); 3] = [
            (tmux_env, "xterm-256color"),
            (plain_env, "tmux-256color"),
            (plain_env, "screen.xterm"),
        ];
        for (env, term) in cases {
            let mut out = Vec::new();
            enter_sequence_for(&mut out, env, Some(term), true).expect("write enter sequence");
            let seq = String::from_utf8(out).unwrap();
            assert_eq!(seq, ENTER_BYTES_BUTTON_MOTION_ONLY);
            assert_eq!(
                seq.find("?1003h"),
                None,
                "no any-motion mode under a multiplexer"
            );
        }

        // The two environments really do produce different setup bytes.
        assert_ne!(ENTER_BYTES, ENTER_BYTES_BUTTON_MOTION_ONLY);
        assert_eq!(
            ENTER_BYTES.replace("\x1b[?1003h", ""),
            ENTER_BYTES_BUTTON_MOTION_ONLY,
            "the only difference is ?1003"
        );
        assert_eq!(
            ansi_of(EnableMouseReporting { any_motion: true }),
            "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h"
        );
        assert_eq!(
            ansi_of(EnableMouseReporting { any_motion: false }),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );
    }

    #[test]
    fn test_any_motion_policy() {
        assert!(any_motion_enabled(plain_env, Some("xterm-256color")));
        assert!(any_motion_enabled(plain_env, None));
        for key in MULTIPLEXER_ENV {
            let env = move |asked: &str| (asked == key).then(String::new);
            assert!(
                !any_motion_enabled(env, Some("xterm-256color")),
                "{key} present → downgrade (even when empty)"
            );
        }
        assert!(!any_motion_enabled(plain_env, Some("tmux-256color")));
        assert!(!any_motion_enabled(plain_env, Some("screen")));
        // Only the prefixes count: a TERM that merely mentions them does not.
        assert!(any_motion_enabled(plain_env, Some("xterm-screen-256color")));
        assert!(any_motion_enabled(plain_env, Some("zellij")));
    }

    #[test]
    fn test_leave_sequence_is_exact() {
        // Both `restore_terminal` and the panic hook consume this function.
        // Mouse reporting must be off *before* `?1049l` leaves the alternate
        // screen — that ordering lives here and nowhere else.
        let mut out = Vec::new();
        leave_sequence(&mut out).expect("write leave sequence");
        assert_eq!(String::from_utf8_lossy(&out), LEAVE_BYTES);

        // Restoring twice just writes the same constant sequence again
        // (idempotent teardown; `disable_raw_mode` is a no-op when off).
        leave_sequence(&mut out).expect("write leave sequence again");
        assert_eq!(
            String::from_utf8_lossy(&out),
            format!("{LEAVE_BYTES}{LEAVE_BYTES}")
        );
    }

    #[test]
    fn test_sequences_leave_out_forbidden_modes() {
        // ?1015 is RXVT coordinates (we want SGR / 1006 only) and ?1007
        // (alternate scroll) turns the wheel into arrow keys. ?1003 is
        // allowed in the any-motion variant only — the multiplexer variant
        // and the teardown must never enable it.
        for seq in [ENTER_BYTES, ENTER_BYTES_BUTTON_MOTION_ONLY, LEAVE_BYTES] {
            for forbidden in ["?1015", "?1007"] {
                assert!(
                    !seq.contains(forbidden),
                    "{seq:?} must not touch {forbidden}"
                );
            }
        }
        assert!(!ENTER_BYTES_BUTTON_MOTION_ONLY.contains("?1003"));
        for seq in [ENTER_BYTES_BUTTON_MOTION_ONLY, LEAVE_BYTES] {
            assert!(
                !seq.contains("?1003h"),
                "{seq:?} must never enable any-motion outside the plain path"
            );
        }
        assert!(
            ENTER_BYTES.contains("?1003h"),
            "the plain path is the one that turns hover on"
        );
        assert_eq!(
            parse_modes(ENTER_BYTES),
            vec![
                (1049, true),
                (1000, true),
                (1002, true),
                (1003, true),
                (1006, true),
                (2004, true),
                (1004, true),
                (25, false),
                (5522, true),
            ]
        );
        assert_eq!(
            parse_modes(ENTER_BYTES_BUTTON_MOTION_ONLY),
            vec![
                (1049, true),
                (1000, true),
                (1002, true),
                (1006, true),
                (2004, true),
                (1004, true),
                (25, false),
                (5522, true),
            ]
        );
        assert_eq!(
            parse_modes(ENTER_BYTES_NO_PASTE_EVENTS),
            parse_modes(ENTER_BYTES)
                .into_iter()
                .filter(|(mode, _)| *mode != 5522)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parse_modes(LEAVE_BYTES),
            vec![
                (1006, false),
                (1003, false),
                (1002, false),
                (1000, false),
                (1049, false),
                (2004, false),
                (1004, false),
                (25, true),
                (5522, false),
            ]
        );
    }

    #[test]
    fn test_mouse_reporting_sequences_are_exact() {
        assert_eq!(
            ansi_of(EnableMouseReporting { any_motion: true }),
            "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h"
        );
        assert_eq!(
            ansi_of(EnableMouseReporting { any_motion: false }),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );
        assert_eq!(
            ansi_of(DisableMouseReporting),
            "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
        );
    }

    #[test]
    fn test_disable_order_reverses_enable_order() {
        // High-bit-first teardown: the terminal must never sit in a state
        // where motion tracking is still on while SGR encoding is off. The
        // teardown is shared by both enable variants, so it disables the
        // union (turning off a mode that was never on is a no-op).
        let mut expected: Vec<(u32, bool)> =
            parse_modes(&ansi_of(EnableMouseReporting { any_motion: true }))
                .into_iter()
                .map(|(mode, _)| (mode, false))
                .collect();
        expected.reverse();
        assert_eq!(parse_modes(&ansi_of(DisableMouseReporting)), expected);

        let downgraded = parse_modes(&ansi_of(EnableMouseReporting { any_motion: false }));
        assert_eq!(
            parse_modes(&ansi_of(DisableMouseReporting)),
            expected,
            "the multiplexer path reuses the same teardown bytes"
        );
        assert_eq!(downgraded.len() + 1, expected.len());
    }

    #[test]
    fn test_term_event_mouse_keeps_coordinates_and_modifiers() {
        // Type guard only: it fails to compile if the payload stops carrying
        // the full `MouseEvent` (kind / modifiers / 0-based coords), which is
        // what selection & scrollbar build on. The actual forwarding in
        // `spawn_event_stream` is covered end-to-end by
        // `tests/tui_event_stream_events.rs` (pty-fed real sequences).
        let event = TermEvent::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 42,
            row: 7,
            modifiers: crossterm::event::KeyModifiers::SHIFT,
        });
        match event {
            TermEvent::Mouse(mouse) => {
                assert_eq!(mouse.kind, crossterm::event::MouseEventKind::ScrollUp);
                assert_eq!(mouse.column, 42);
                assert_eq!(mouse.row, 7);
                assert_eq!(mouse.modifiers, crossterm::event::KeyModifiers::SHIFT);
            }
            other => panic!("expected a mouse event, got {other:?}"),
        }
    }

    /// 没起过事件流（向导 / 初始化失败路径）：收尾是 no-op——不 panic、不动 flag
    /// （接驳与 raw mode 此前也没装 / 没开，两步还原都是空操作）。
    #[test]
    fn teardown_without_an_event_stream_is_a_noop() {
        end_tui_session().expect("teardown without a session");
    }
}
