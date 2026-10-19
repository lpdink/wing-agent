//! 光标转义的**变化语义** —— 出帧口的 backend 装饰（唯一装配点是
//! [`crate::tui::init_terminal`]）。为什么需要它见 `docs/dev/tui-signals.md`。
//!
//! 契约：
//!
//! 1. **只在变化时声明。** `?25h` / `?25l` 只在「未知 → 某值」与「值 → 另一个值」时发——本
//!    装饰器是参与这套状态机的唯一写者，`tui::enter_sequence` / `leave_sequence` 只在段边界
//!    出现、不参与；`CUP` 只在目标 ≠ **已知**物理位置时发。
//! 2. **物理位置未知时不得省 `CUP`**（红线）：真实光标是输入法候选窗的锚，位置错了就是硬伤。
//!    "已知"只认本装饰器自己设置过的位置——任何可能移动物理光标、或让前进量由终端决定的动作
//!    之后立刻作废（[`CursorGate::forget_position`] 的调用点就是全部清单）。
//! 3. **稳态帧一次 write**：省掉 `?25h` / `CUP` 也就省掉了 `execute!` 的提前 flush，diff 与
//!    `CUP` 同批到达终端，终端看不到半截画面。
//!
//! 覆盖范围**只限经过本装饰器的字节**：`tui/clipboard_protocol.rs` 的读取协议机直写 stdout
//! （OSC 5522，不移动光标）。

use std::cell::Cell;
use std::io;

use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::backend::WindowSize;
use ratatui::buffer::Cell as BufferCell;
use ratatui::layout::Position;
use ratatui::layout::Size;

/// 终端侧光标状态里我们**确信**的那部分。`None` 一律表示未知 —— 未知就照发。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CursorState {
    visible: Option<bool>,
    position: Option<Position>,
    size: Option<Size>,
}

/// [`Backend`] 装饰器：把「每帧声明光标状态」换成「只在变化时声明」。
pub struct CursorGate<B> {
    inner: B,
    state: Cell<CursorState>,
}

impl<B> CursorGate<B> {
    pub const fn new(inner: B) -> Self {
        Self {
            inner,
            state: Cell::new(CursorState {
                visible: None,
                position: None,
                size: None,
            }),
        }
    }

    fn state(&self) -> CursorState {
        self.state.get()
    }

    /// 物理位置不再可信（下一个 `set_cursor_position` 必须真的发）。
    fn forget_position(&self) {
        let mut state = self.state();
        state.position = None;
        self.state.set(state);
    }

    fn set_visible(&self, visible: bool) {
        let mut state = self.state();
        state.visible = Some(visible);
        self.state.set(state);
    }

    fn set_position(&self, position: Position) {
        let mut state = self.state();
        state.position = Some(position);
        self.state.set(state);
    }
}

impl<B: Backend> Backend for CursorGate<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a BufferCell)>,
    {
        // 写出去的格子由终端决定光标落到哪（`CrosstermBackend` 对相邻格省略 `MoveTo`、
        // 宽字符与组合序列的前进量由终端裁定）—— 一格都不能推。
        let mut wrote = false;
        self.inner.draw(content.inspect(|_| wrote = true))?;
        if wrote {
            self.forget_position();
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        if self.state().visible == Some(false) {
            return Ok(());
        }
        self.inner.hide_cursor()?;
        self.set_visible(false);
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        if self.state().visible == Some(true) {
            return Ok(());
        }
        self.inner.show_cursor()?;
        self.set_visible(true);
        Ok(())
    }

    /// `CrosstermBackend` 这条绕过 `writer` 直接发 DSR，答的是**终端此刻的屏幕**——本帧
    /// 还没 flush 出去的字节不在它的账上。所以答案只回给调用方，不记成"已知位置"。
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        if self.state().position == Some(position) {
            return Ok(());
        }
        self.inner.set_cursor_position(position)?;
        self.set_position(position);
        Ok(())
    }

    /// 防御性：ratatui 0.1.1 的 `Terminal` 清屏只走 [`Backend::clear_region`]（全屏视口用
    /// `ClearType::All`），`clear` 在生产路径上没有调用点。
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()?;
        self.forget_position();
        Ok(())
    }

    /// `Terminal::clear()` / `Terminal::resize()` 的必经之路（整屏重画、resize 都从这里过）：
    /// 屏幕内容一清，位置就该重问一遍。
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)?;
        self.forget_position();
        Ok(())
    }

    /// 尺寸是"终端重排过"的唯一信号：重排会把光标挪到终端认为合适的地方。这里不猜
    /// ratatui 在 resize 里是否顺手清了屏 —— 位置一律作废。
    fn size(&self) -> Result<Size, Self::Error> {
        let size = self.inner.size()?;
        let mut state = self.state();
        if state.size != Some(size) {
            state.size = Some(size);
            state.position = None;
        }
        self.state.set(state);
        Ok(size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        let window = self.inner.window_size()?;
        let mut state = self.state();
        if state.size != Some(window.columns_rows) {
            state.size = Some(window.columns_rows);
            state.position = None;
        }
        self.state.set(state);
        Ok(window)
    }

    /// 防御性（当前全屏视口不会走到这里）：换行把光标往下推了。
    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)?;
        if n > 0 {
            self.forget_position();
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

/// 绕过后端直接写的字节（OSC 标题 / 桌面通知 / 程序状态 / 剪贴板复制（OSC 52），以及
/// `restore_terminal` 交给 `leave_sequence` 的收尾序列）不经过 [`Backend`]，里面可能夹着光标
/// 序列：位置作废。`tui/clipboard_protocol.rs` 的**读取**协议机则直写 stdout、不走这里
/// （OSC 5522，不移动光标）。
impl<B: io::Write> io::Write for CursorGate<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.forget_position();
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::widgets::Paragraph;

    use super::*;

    /// 终端侧看到的 write 边界：一次 `flush()` 封一段 = 一次 `write(2)` = 终端可以独立出帧
    /// 的单位。断言"一帧一次 write"靠的就是它。
    #[derive(Clone, Default)]
    struct Wire {
        closed: Rc<RefCell<Vec<Vec<u8>>>>,
        open: Rc<RefCell<Vec<u8>>>,
    }

    impl Wire {
        fn new() -> Self {
            Self::default()
        }

        /// 取走自上次调用以来终端收到的所有 write（丢掉空的）。
        fn take(&self) -> Vec<String> {
            let mut closed = self.closed.borrow_mut();
            std::mem::take(&mut *closed)
                .into_iter()
                .filter(|write| !write.is_empty())
                .map(|write| String::from_utf8_lossy(&write).into_owned())
                .collect()
        }
    }

    impl io::Write for Wire {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.open.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut open = self.open.borrow_mut();
            if !open.is_empty() {
                self.closed.borrow_mut().push(std::mem::take(&mut *open));
            }
            Ok(())
        }
    }

    /// `CrosstermBackend`（出的是真字节）+ 可控尺寸（测试进程没有 tty）。
    struct SizedTerminal {
        inner: CrosstermBackend<Wire>,
        size: Rc<Cell<Size>>,
        cursor: Position,
    }

    impl Backend for SizedTerminal {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(self.cursor)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.cursor = position.into();
            self.inner.set_cursor_position(self.cursor)
        }

        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> io::Result<Size> {
            Ok(self.size.get())
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size.get(),
                pixels: Size::new(0, 0),
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Backend::flush(&mut self.inner)
        }
    }

    impl io::Write for SizedTerminal {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(&mut self.inner)
        }
    }

    fn gate(width: u16, height: u16) -> (CursorGate<SizedTerminal>, Wire, Rc<Cell<Size>>) {
        let wire = Wire::new();
        let size = Rc::new(Cell::new(Size::new(width, height)));
        let backend = SizedTerminal {
            inner: CrosstermBackend::new(wire.clone()),
            size: Rc::clone(&size),
            cursor: Position::ORIGIN,
        };
        (CursorGate::new(backend), wire, size)
    }

    struct Fixture {
        terminal: Terminal<CursorGate<SizedTerminal>>,
        wire: Wire,
    }

    impl Fixture {
        fn new(width: u16, height: u16) -> Self {
            let (gate, wire, _size) = gate(width, height);
            let terminal = Terminal::new(gate).expect("terminal with a fixed size");
            Self { terminal, wire }
        }

        /// 画一帧：`text` 是内容，`cursor` 是这一帧要求的落点（`None` = 隐藏光标）。
        fn frame(&mut self, text: &str, cursor: Option<(u16, u16)>) {
            self.terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new(text), frame.area());
                    if let Some(position) = cursor {
                        frame.set_cursor_position(position);
                    }
                })
                .expect("draw");
        }

        /// 画一帧并把它的字节丢掉（建立"上一帧"的状态）。
        fn settle(&mut self, text: &str, cursor: Option<(u16, u16)>) {
            self.frame(text, cursor);
            let _ = self.wire.take();
        }

        fn writes(&self) -> Vec<String> {
            self.wire.take()
        }
    }

    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    /// 光标落点 `(x, y)` 的 `CUP` 字节（1 起的行列）。
    fn cup(x: u16, y: u16) -> String {
        format!("\x1b[{};{}H", y + 1, x + 1)
    }

    /// 一段 write 里所有 `CUP`（`ESC [ r ; c H`），按出现顺序：diff 自己也用 `MoveTo` 定位，
    /// 所以"落点对不对"要看**最后**一条 `CUP`，看条数没有意义。
    fn cups(write: &str) -> Vec<String> {
        let bytes = write.as_bytes();
        let mut found = Vec::new();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
                let start = i + 2;
                let mut end = start;
                while end < bytes.len() && (bytes[end] == b';' || bytes[end].is_ascii_digit()) {
                    end += 1;
                }
                if end < bytes.len() && bytes[end] == b'H' && bytes[start..end].contains(&b';') {
                    found.push(write[i..=end].to_string());
                    i = end + 1;
                    continue;
                }
            }
            i += 1;
        }
        found
    }

    /// 空 diff 的帧里后端只留下这一串（`CrosstermBackend::draw` 的无条件收尾）。
    const SGR_RESET: &str = "\x1b[39m\x1b[49m\x1b[59m\x1b[0m";

    #[test]
    fn a_second_identical_frame_sends_no_cursor_bytes() {
        let mut fixture = Fixture::new(20, 4);
        fixture.frame("hello", Some((7, 2)));

        // 首帧：可见性未知 → `?25h` 真发一次；落点未知 → 一次 `CUP`。
        let first = fixture.writes();
        let first = first.join("");
        assert_eq!(count(&first, "\x1b[?25h"), 1, "{first:?}");
        assert_eq!(count(&first, &cup(7, 2)), 1, "{first:?}");

        // 同一帧再画一次：一个字都没变 —— 光标字节一条都不该再发。
        fixture.frame("hello", Some((7, 2)));
        assert_eq!(
            fixture.writes(),
            vec![SGR_RESET.to_string()],
            "稳态同位置帧只剩后端的 SGR 复位"
        );
    }

    #[test]
    fn a_moved_cursor_is_positioned_exactly_once() {
        let mut fixture = Fixture::new(20, 4);
        fixture.settle("hello", Some((7, 2)));

        fixture.frame("hello!", Some((8, 2)));
        let writes = fixture.writes();
        assert_eq!(writes.len(), 1, "一帧一次 write：{writes:?}");
        assert_eq!(count(&writes[0], &cup(8, 2)), 1, "恰好一次 CUP：{writes:?}");
        assert_eq!(count(&writes[0], "\x1b[?25h"), 0, "可见性没变：{writes:?}");
        assert!(
            writes[0].ends_with(&cup(8, 2)),
            "CUP 是这一帧的最后一条（diff 之后）：{writes:?}"
        );
    }

    #[test]
    fn hiding_and_showing_is_sent_on_change_only() {
        let mut fixture = Fixture::new(20, 4);
        fixture.settle("hello", Some((1, 1)));

        // 可见 → 隐藏：一次 `?25l`，且这一帧没有落点（不发 CUP）。
        fixture.frame("hello", None);
        let hidden = fixture.writes();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        assert_eq!(count(&hidden[0], "\x1b[?25l"), 1, "{hidden:?}");
        assert_eq!(count(&hidden[0], &cup(1, 1)), 0, "{hidden:?}");

        // 一直隐藏：两条都不再发。
        fixture.frame("hello", None);
        assert_eq!(fixture.writes(), vec![SGR_RESET.to_string()]);

        // 隐藏 → 可见（同一落点）：只发 `?25h`。隐藏不移动物理光标，落点仍是已知的。
        fixture.frame("hello", Some((1, 1)));
        let reshown = fixture.writes();
        assert_eq!(reshown.len(), 1, "{reshown:?}");
        assert_eq!(count(&reshown[0], "\x1b[?25h"), 1, "{reshown:?}");
        assert_eq!(count(&reshown[0], &cup(1, 1)), 0, "{reshown:?}");

        // 稳态：不再发。
        fixture.frame("hello", Some((1, 1)));
        assert_eq!(fixture.writes(), vec![SGR_RESET.to_string()]);

        // 藏起来再挪落点：`?25l` 一次；重新可见时 `?25h` 与新落点各一次。
        fixture.frame("hello", None);
        let rehidden = fixture.writes();
        assert_eq!(count(&rehidden[0], "\x1b[?25l"), 1, "{rehidden:?}");
        fixture.frame("hello", Some((5, 1)));
        let moved = fixture.writes().join("");
        assert_eq!(count(&moved, "\x1b[?25h"), 1, "{moved:?}");
        assert_eq!(count(&moved, &cup(5, 1)), 1, "落点变了就要发：{moved:?}");
    }

    #[test]
    fn a_repaint_reaches_the_terminal_in_one_write() {
        // 根因的回归钉子：diff 与 CUP 必须同批到达，终端看不到「光标停在最后一个被改写的
        // 格子上、而且可见」的中间态（开屏扫光期就是它在牌子上闪）。
        let mut fixture = Fixture::new(20, 4);
        fixture.settle("hello", Some((7, 2)));

        fixture.frame("hallo", Some((7, 2)));
        let writes = fixture.writes();
        assert_eq!(writes.len(), 1, "diff + CUP 一次写：{writes:?}");
        assert!(writes[0].contains('a'), "diff 在这一帧里：{writes:?}");
        assert_eq!(
            count(&writes[0], &cup(7, 2)),
            1,
            "写了格子之后位置未知，落点必须重发（红线）：{writes:?}"
        );
        assert!(writes[0].ends_with(&cup(7, 2)), "{writes:?}");
    }

    #[test]
    fn a_wide_glyph_by_the_row_end_keeps_the_cursor_on_its_row() {
        // 宽字符 / emoji（含 VS16 的 `emoji_width::pin`）的前进量由终端裁定 ⇒ 写过它们之后
        // 位置不可信：落点必须原样重发，且列不得被推到下一行（行尾写满时终端会折行）。
        let mut fixture = Fixture::new(20, 4);
        fixture.settle("AB", Some((5, 0)));

        // 走真正的出帧口（含 wire pass）：两张宽字换掉 "AB"。
        crate::tui::draw_frame(&mut fixture.terminal, |frame| {
            frame.render_widget(Paragraph::new("宽⚠️"), frame.area());
            frame.set_cursor_position((5, 0));
        })
        .expect("draw");
        let writes = fixture.writes();
        assert_eq!(writes.len(), 1, "一帧一次 write：{writes:?}");
        // diff 自己也会发 `MoveTo`；要断言的是**最后一次**定位 = 落点（且在第 1 行）。
        assert_eq!(
            cups(&writes[0]).last(),
            Some(&cup(5, 0)),
            "宽字之后落点必须原样重发、且仍在第 1 行：{writes:?}"
        );
        assert!(writes[0].ends_with(&cup(5, 0)), "{writes:?}");

        // 行尾写满（10 张宽字 = 20 列）：落点是最后一列，CUP 只能是第 20 列——不是第 21 列
        // （终端在行尾写满会折行，折了光标就不在落点上）。
        let full = "宽".repeat(10);
        fixture.frame(&full, Some((19, 0)));
        let writes = fixture.writes();
        assert_eq!(
            cups(&writes[0]).last(),
            Some(&cup(19, 0)),
            "行尾落点必须是第 1 行第 20 列：{writes:?}"
        );
        assert!(writes[0].ends_with(&cup(19, 0)), "{writes:?}");

        // 位置重新已知：同一帧再画不再发光标字节。
        fixture.frame(&full, Some((19, 0)));
        assert_eq!(fixture.writes(), vec![SGR_RESET.to_string()]);
    }

    #[test]
    fn a_clear_forgets_the_position() {
        let (mut gate, wire, _size) = gate(20, 4);
        Backend::show_cursor(&mut gate).expect("show");
        Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
        let _ = wire.take();

        Backend::clear(&mut gate).expect("clear");
        let _ = wire.take();

        Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
        assert_eq!(wire.take(), vec![cup(7, 2)], "清屏之后落点必须重发");
    }

    #[test]
    fn every_backend_side_wipe_forgets_the_position() {
        // `clear_region` 是整屏重画的必经之路（`Terminal::clear` / `resize`），`append_lines`
        // 只有 inline 视口会走；两者既然都写了作废，就各要一条断言。
        for wipe in [
            |gate: &mut CursorGate<SizedTerminal>| {
                Backend::clear_region(gate, ClearType::All).expect("clear_region");
            },
            |gate: &mut CursorGate<SizedTerminal>| {
                Backend::append_lines(gate, 1).expect("append_lines");
            },
        ] {
            let (mut gate, wire, _size) = gate(20, 4);
            Backend::show_cursor(&mut gate).expect("show");
            Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
            let _ = wire.take();

            wipe(&mut gate);
            let _ = wire.take();

            Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
            assert_eq!(
                wire.take(),
                vec![cup(7, 2)],
                "擦过屏 / 换过行，落点必须重发"
            );
        }
    }

    #[test]
    fn a_resize_forgets_the_position() {
        let (mut gate, wire, size) = gate(20, 4);
        // 先问一次尺寸（`autoresize` 每帧就这么问）：建立"已知尺寸"的基线。
        assert_eq!(Backend::size(&gate).expect("size"), Size::new(20, 4));
        Backend::show_cursor(&mut gate).expect("show");
        Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
        let _ = wire.take();

        // 同样的尺寸再问一次：位置仍然可信，不发。
        assert_eq!(Backend::size(&gate).expect("size"), Size::new(20, 4));
        Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
        assert_eq!(wire.take(), Vec::<String>::new(), "尺寸没变，位置仍然可信");

        // 终端重排过：位置不可信。
        size.set(Size::new(20, 6));
        assert_eq!(Backend::size(&gate).expect("size"), Size::new(20, 6));
        Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
        assert_eq!(wire.take(), vec![cup(7, 2)], "尺寸变了，落点必须重发");
    }

    #[test]
    fn a_queried_position_is_not_recorded() {
        // DSR 答的是终端已消费的屏幕，本帧还没 flush 出去的字节不在它的账上 —— 答案不能当
        // "已知位置"用，否则后续那一帧会省掉一次必要的 CUP。
        let (mut gate, wire, _size) = gate(20, 4);
        Backend::show_cursor(&mut gate).expect("show");
        let _ = wire.take();

        // 桩后端答 ORIGIN；若这条被记成"已知"，下面那次同位置就被省掉了。
        assert_eq!(
            Backend::get_cursor_position(&mut gate).expect("query"),
            Position::ORIGIN
        );
        let _ = wire.take();

        Backend::set_cursor_position(&mut gate, Position::ORIGIN).expect("position");
        assert_eq!(wire.take(), vec![cup(0, 0)], "查过一嘴不等于位置已知");
    }

    #[test]
    fn raw_writes_forget_the_position() {
        // OSC 标题 / 桌面通知 / 程序状态都绕过后端直写：里面可能夹着光标序列。
        let (mut gate, wire, _size) = gate(20, 4);
        Backend::show_cursor(&mut gate).expect("show");
        Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
        let _ = wire.take();

        io::Write::write_all(&mut gate, b"\x1b]0;wing\x07").expect("osc");
        let _ = wire.take();

        Backend::set_cursor_position(&mut gate, (7, 2)).expect("position");
        let writes = wire.take();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(
            writes[0].ends_with(&cup(7, 2)),
            "直写之后落点必须重发：{writes:?}"
        );
    }

    #[test]
    fn the_visibility_state_is_tracked_across_frames() {
        // 首帧就要求隐藏（设置面板开着的形态）：`?25l` 一次，之后不再发。
        let mut fixture = Fixture::new(20, 4);
        fixture.frame("hello", None);
        let first = fixture.writes();
        assert_eq!(count(&first[0], "\x1b[?25l"), 1, "{first:?}");
        assert_eq!(count(&first[0], "\x1b[?25h"), 0, "{first:?}");
    }
}
