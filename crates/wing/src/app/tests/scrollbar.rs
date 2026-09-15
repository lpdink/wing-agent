//! Wheel, overlay scrollbar and frame-painting tests.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::*;
use crate::ui::chat_view::ChatCell;
use crate::ui::status_bar::TurnUsage;
use ratatui::layout::Rect;

#[test]
fn test_wheel_scrolls_three_lines_and_rearms_at_bottom() {
    let mut app = test_app();
    set_chat_height(&mut app, 20);
    app.chat.last_total = 100;
    app.chat.scroll_offset = 80; // bottom edge == max_scroll (80)
    app.chat.jump_bottom();
    assert!(app.chat.is_at_bottom());

    // Wheel up: 3 lines per event, leaves the follow state.
    assert_eq!(
        app.handle_mouse(wheel_up()),
        MouseOutcome::Immediate,
        "the wheel changed the view"
    );
    assert_eq!(app.chat.scroll_offset, 77);
    assert!(!app.chat.is_at_bottom(), "wheel up starts reading history");

    app.handle_mouse(wheel_up());
    assert_eq!(app.chat.scroll_offset, 74);

    // Wheel down: 3 lines per event; reaching the bottom edge re-arms.
    app.handle_mouse(wheel_down());
    assert_eq!(app.chat.scroll_offset, 77);
    assert!(!app.chat.is_at_bottom(), "still above the bottom edge");

    app.handle_mouse(wheel_down());
    assert_eq!(app.chat.scroll_offset, 80);
    assert!(app.chat.is_at_bottom(), "back at the bottom → follow");
}

#[test]
fn test_wheel_up_clamps_at_top_and_stays_unpinned() {
    let mut app = test_app();
    set_chat_height(&mut app, 20);
    app.chat.last_total = 100;
    app.chat.scroll_offset = 1;
    for _ in 0..5 {
        app.handle_mouse(wheel_up());
    }
    assert_eq!(app.chat.scroll_offset, 0);
    assert!(!app.chat.is_at_bottom());
}

#[test]
fn test_non_wheel_mouse_events_are_ignored() {
    let mut app = test_app();
    set_chat_height(&mut app, 20);
    app.chat.last_total = 100;
    app.chat.scroll_offset = 50;
    app.chat.scroll_up(0); // leave the bottom without moving the offset

    // No frame has been drawn, so the chat band has no geometry yet: press
    // / drag / release cannot start a selection (they only ever do inside
    // the band — see the text-selection tests) and hover / horizontal
    // wheel are never a chat gesture at all.
    for kind in [
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Moved,
        crossterm::event::MouseEventKind::ScrollLeft,
        crossterm::event::MouseEventKind::ScrollRight,
    ] {
        assert_eq!(
            app.handle_mouse(wheel(kind)),
            MouseOutcome::Ignored,
            "{kind:?} must report no view change"
        );
    }
    assert_eq!(
        app.chat.scroll_offset, 50,
        "press/drag/release/hover/horizontal wheel must not scroll the chat"
    );
    assert!(
        !app.chat.is_at_bottom(),
        "non-wheel events must not re-arm the follow state either"
    );
}

// ── Overlay scrollbar ────────────────────────────────────────────────

/// Did the gesture ask for a redraw?
///
/// The scrollbar tests assert on *what changed*, not on how urgently the
/// frame should be repainted — the outcome's kind is the frame gate's
/// concern (see [`MouseOutcome`]).
fn redrew(outcome: MouseOutcome) -> bool {
    outcome != MouseOutcome::Ignored
}

fn row_text(buf: &ratatui::buffer::Buffer) -> String {
    (buf.area.x..buf.area.right())
        .map(|x| buf[(x, buf.area.y)].symbol())
        .collect()
}

/// App with a chat viewport (the rect `draw` would have recorded) and a
/// content height that overflows it — i.e. the scrollbar exists.
fn app_with_scrollbar(area: Rect, content: usize, offset: usize) -> App {
    let mut app = test_app();
    app.geometry.record_chat_band(area);
    app.chat.last_total = content;
    app.chat.scroll_offset = offset;
    app.chat.scroll_up(0); // reading state, offset unchanged
    app
}

#[test]
fn test_scrollbar_track_click_jumps_and_syncs_the_info_separator() {
    // Chat area offset from the origin, so geometry offsets are covered.
    let area = Rect::new(0, 1, 80, 20);
    let mut app = app_with_scrollbar(area, 100, 0);
    let geom = app.scrollbar_geometry().expect("content overflows");
    assert_eq!(geom.column, 79, "rightmost column of the chat area");

    // Click the very bottom of the track → end of the history.
    assert!(
        redrew(app.handle_mouse(press((geom.column, geom.track_bottom)))),
        "a track click is a view change"
    );
    assert_eq!(app.chat.scroll_position(), geom.max_scroll());
    assert!(app.chat.is_at_bottom(), "the bottom edge re-arms follow");

    // The info separator renders from the same state, so the frame that
    // follows the click already shows the new position.
    let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 1));
    let sep_area = Rect::new(0, 0, 80, 1);
    render_info_separator(
        None,
        &TurnUsage::default(),
        app.chat.content_height(),
        app.geometry.chat_height(),
        app.chat.scroll_position(),
        &app.palette,
        sep_area,
        &mut buf,
    );
    let rendered = row_text(&buf);
    assert!(rendered.contains("100/100"), "pos/total:\n{rendered}");
    assert!(rendered.contains("100%"), "percent:\n{rendered}");

    // Click the top of the track → back to the beginning, reading state.
    assert!(redrew(
        app.handle_mouse(press((geom.column, geom.track_top)))
    ));
    assert_eq!(app.chat.scroll_position(), 0);
    assert!(!app.chat.is_at_bottom());
}

/// Dragging the thumb keeps it 1:1 with the pointer (the mapping is
/// proportional — see `scrollbar::offset_for_row`), and the row clamps into
/// the track so a drag that leaves the chat area keeps dragging along the
/// extremes instead of jumping or stalling.
#[test]
fn test_scrollbar_drag_follows_the_pointer_and_clamps_outside_the_track() {
    let area = Rect::new(0, 0, 80, 20);
    let mut app = app_with_scrollbar(area, 100, 40);
    let geom = app.scrollbar_geometry().expect("content overflows");

    // Grab the last row of the thumb: pressing must not jump.
    let grab_row = geom.thumb_top + geom.thumb_height() - 1;
    assert!(redrew(app.handle_mouse(press((geom.column, grab_row)))));
    assert_eq!(app.chat.scroll_position(), 40, "no jump when grabbing");

    // Dragged far above the chat area → clamped to the top, still dragging.
    assert!(redrew(app.handle_mouse(drag((geom.column, 0)))));
    assert_eq!(app.chat.scroll_position(), 0);

    // …and far below it → clamped to the bottom, follow re-armed.
    assert!(redrew(app.handle_mouse(drag((geom.column, 9999)))));
    assert_eq!(app.chat.scroll_position(), geom.max_scroll());
    assert!(app.chat.is_at_bottom());

    // The column is ignored while dragging (the row is what maps).
    assert!(redrew(app.handle_mouse(drag((0, geom.track_top)))));
    assert_eq!(app.chat.scroll_position(), 0);

    // Release on the bar: the drag ends, but the pointer still hovers it.
    assert!(redrew(
        app.handle_mouse(release((geom.column, geom.track_top)))
    ));
    assert!(!app.scrollbar.dragging, "release ends the drag");
    assert!(app.scrollbar.hovered, "the pointer is still on the bar");

    // A bare drag (no press) must not move anything afterwards.
    assert!(!redrew(app.handle_mouse(drag((geom.column, 9999)))));
    assert_eq!(app.chat.scroll_position(), 0);

    // Moving off the bar drops the hover look again.
    assert!(redrew(
        app.handle_mouse(hover((geom.column - 1, geom.track_top)))
    ));
    assert!(!app.scrollbar.is_active());
}

#[test]
fn test_scrollbar_hover_lights_up_only_on_its_own_column() {
    let area = Rect::new(0, 0, 80, 20);
    let mut app = app_with_scrollbar(area, 100, 40);
    let geom = app.scrollbar_geometry().expect("content overflows");

    // Moving onto the bar column lights it up (the caller draws at once).
    assert!(redrew(app.handle_mouse(hover((geom.column, 5)))));
    assert!(app.scrollbar.is_active());
    // The exact same cell again changes nothing → no redraw.
    assert!(!redrew(app.handle_mouse(hover((geom.column, 5)))));
    // One column to the left is chat content, not the bar.
    assert!(redrew(app.handle_mouse(hover((geom.column - 1, 5)))));
    assert!(!app.scrollbar.is_active());
    // …and a row outside the chat area is not the bar either.
    assert!(!redrew(
        app.handle_mouse(hover((geom.column, geom.track_bottom + 1)))
    ));
    // Hovering never scrolls.
    assert_eq!(app.chat.scroll_position(), 40);
    assert!(!app.chat.is_at_bottom());
}

#[test]
fn test_wheel_over_the_scrollbar_still_scrolls_the_chat() {
    let area = Rect::new(0, 0, 80, 20);
    let mut app = app_with_scrollbar(area, 100, 40);
    let geom = app.scrollbar_geometry().expect("content overflows");

    // Pointer rests on the bar: it is lit up, and the wheel still belongs
    // to the chat view (the wheel arm is matched before the bar sees the
    // event — the bar has no wheel handling at all).
    assert!(redrew(app.handle_mouse(hover((geom.column, 5)))));
    assert!(app.scrollbar.is_active());
    assert!(redrew(app.handle_mouse(mouse_at(
        crossterm::event::MouseEventKind::ScrollUp,
        (geom.column, 5)
    ))));
    assert_eq!(app.chat.scroll_position(), 37, "3 lines per notch");
    assert!(redrew(app.handle_mouse(mouse_at(
        crossterm::event::MouseEventKind::ScrollDown,
        (geom.column, 5)
    ))));
    assert_eq!(app.chat.scroll_position(), 40);

    // Same while dragging: the wheel is not swallowed either.
    app.handle_mouse(press((geom.column, geom.thumb_top)));
    app.handle_mouse(mouse_at(
        crossterm::event::MouseEventKind::ScrollDown,
        (geom.column, 5),
    ));
    assert_eq!(
        app.chat.scroll_position(),
        43,
        "the wheel re-targets the chat"
    );
}

#[test]
fn test_scrollbar_is_absent_when_the_content_fits() {
    let area = Rect::new(0, 0, 80, 20);
    // Exactly one screen of content: no bar, so the rightmost column is
    // plain chat text and none of the mouse gestures do anything.
    let mut app = app_with_scrollbar(area, 20, 0);
    assert!(app.scrollbar_geometry().is_none());
    let column = area.right() - 1;
    for event in [
        hover((column, 3)),
        press((column, 3)),
        drag((column, 19)),
        release((column, 19)),
    ] {
        assert!(
            !redrew(app.handle_mouse(event)),
            "{:?} must be a no-op",
            event.kind
        );
    }
    assert_eq!(app.chat.scroll_position(), 0);

    // Content grows → the bar appears; the state is dropped again as soon
    // as it stops overflowing (clear / compaction).
    app.chat.last_total = 100;
    assert!(redrew(app.handle_mouse(hover((column, 3)))));
    assert!(app.scrollbar.is_active());
    app.chat.last_total = 5;
    assert!(
        redrew(app.handle_mouse(hover((column, 3)))),
        "cleanup is a redraw"
    );
    assert!(!app.scrollbar.is_active(), "no bar, no interaction state");
}

#[test]
fn test_scrollbar_press_missing_the_bar_clears_the_state() {
    let area = Rect::new(0, 0, 80, 20);
    let mut app = app_with_scrollbar(area, 100, 40);
    let geom = app.scrollbar_geometry().expect("content overflows");

    app.handle_mouse(hover((geom.column, 5)));
    assert!(app.scrollbar.is_active());
    // A press one column to the left (chat content) drops the hover look
    // instead of starting a drag.
    assert!(redrew(app.handle_mouse(press((geom.column - 1, 5)))));
    assert!(!app.scrollbar.is_active());
    assert_eq!(app.chat.scroll_position(), 40, "the press did not scroll");
}

#[test]
fn test_clear_scrollbar_interaction_is_idempotent() {
    // Focus loss path: the button-up may be delivered elsewhere.
    let area = Rect::new(0, 0, 80, 20);
    let mut app = app_with_scrollbar(area, 100, 40);
    let geom = app.scrollbar_geometry().expect("content overflows");

    app.handle_mouse(press((geom.column, geom.thumb_top)));
    assert!(app.scrollbar.is_active());
    assert!(app.clear_scrollbar_interaction());
    assert!(!app.scrollbar.is_active());
    assert!(
        !app.clear_scrollbar_interaction(),
        "cleaning up twice is a no-op"
    );
    // The stray drag that follows cannot move the view any more.
    let before = app.chat.scroll_position();
    assert!(!redrew(app.handle_mouse(drag((geom.column, 9999)))));
    assert_eq!(app.chat.scroll_position(), before);
}

#[test]
fn test_scrollbar_works_while_a_panel_is_open() {
    let mut app = test_app();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![],
        vec![serde_json::json!({
            "type": "ask",
            "tool_call_id": "ask-scrollbar",
            "questions": [{
                "id": "q1",
                "header": "H",
                "question": "which?",
                "options": [{"label": "a"}, {"label": "b"}],
            }],
        })],
        None,
    ));
    assert_eq!(app.ask_panels.len(), 1);

    let area = Rect::new(0, 0, 80, 20);
    app.geometry.record_chat_band(area);
    app.chat.last_total = 100;
    app.chat.scroll_offset = 40;
    app.chat.scroll_up(0);
    let geom = app.scrollbar_geometry().expect("content overflows");
    let cursor_before = app.ask_panels.front().unwrap().states[0].cursor;

    app.handle_mouse(press((geom.column, geom.track_bottom)));
    assert_eq!(app.chat.scroll_position(), geom.max_scroll());
    assert_eq!(
        app.ask_panels.front().unwrap().states[0].cursor,
        cursor_before,
        "the bar must not touch panel state"
    );
    assert_eq!(app.ask_panels.len(), 1, "panel stays open");
}

// ── Wire level: what a real terminal does with the emitted frames ────

/// A stand-in for the terminal the frames are written to.
///
/// It consumes the very stream `CrosstermBackend::draw` writes —
/// `(x, y, cell)` triples — and checks the one property that keeps a frame
/// from smearing: **the cursor has to be where the backend thinks it is at
/// every print**.
///
/// The backend skips its `MoveTo` when the next cell sits at `last_x + 1`,
/// a shortcut that only holds while every printed symbol advances the
/// cursor by exactly what the cell claimed (`Cell::cell_width()`). A cell
/// whose claimed width disagrees with what the terminal renders prints one
/// column off instead, and every following cell of that row inherits the
/// error — the "scroll and the text smears" class of artifact, which a
/// `TestBackend` cannot see because it never models a cursor.
struct TerminalSim {
    width: u16,
    /// Where the terminal really is (after the last print).
    cursor: (u16, u16),
    /// Where the backend believes it is — crossterm's `last_pos`, which is
    /// local to one `draw` call.
    assumed: Option<(u16, u16)>,
    /// Prints that landed somewhere other than the backend meant.
    desync: Vec<String>,
    /// Every cell the frames emitted, `<frame>`-annotated, for assertions
    /// and debugging.
    emitted: Vec<(u16, u16, String, u16)>,
}

impl TerminalSim {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            cursor: (0, height - 1),
            assumed: None,
            desync: Vec::new(),
            emitted: Vec::new(),
        }
    }

    /// The terminal's side of one `Print(cell)`.
    fn write(&mut self, x: u16, y: u16, symbol: &str) {
        // What the terminal renders: the visible text, control sequences
        // (an injected OSC8 link) included in the string but not printed.
        let rendered = crate::render::markdown::links::strip_osc8(symbol);
        let width = crate::ui::selection::grapheme_width(&rendered);
        let skip_move = self.assumed == Some((x.wrapping_sub(1), y));
        let at = if skip_move { self.cursor } else { (x, y) };
        if at != (x, y) {
            self.desync.push(format!(
                "printed {symbol:?} at {at:?} while the backend meant ({x}, {y})"
            ));
        }
        let next = at.0 + width;
        self.cursor = if next >= self.width {
            (next - self.width, at.1 + 1)
        } else {
            (next, at.1)
        };
        self.assumed = Some((x, y));
    }
}

impl ratatui::backend::Backend for TerminalSim {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        // `last_pos` is local to one draw call in the real backend.
        self.assumed = None;
        use ratatui::buffer::CellWidth;
        for (x, y, cell) in content {
            let symbol = cell.symbol().to_string();
            let width = cell.cell_width();
            self.emitted.push((x, y, symbol.clone(), width));
            self.write(x, y, &symbol);
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        Ok(self.cursor.into())
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        self.cursor = position.into().into();
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn clear_region(&mut self, _clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        Ok(())
    }

    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        Ok(ratatui::layout::Size::new(self.width, 24))
    }

    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        Ok(ratatui::backend::WindowSize {
            columns_rows: ratatui::layout::Size::new(self.width, 24),
            pixels: ratatui::layout::Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Frame with links, CJK and enough content to scroll — the shape that
/// reproduced the artifact in the wild.
fn app_with_chinese_links() -> App {
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    for i in 0..3 {
        app.chat
            .push(ChatCell::AssistantMessage(format!("above {i}")));
    }
    app.chat.push(ChatCell::AssistantMessage(
        "• Markdown 链接：[百度首页](https://www.baidu.com)、[wing-agent 仓库](https://github.com/lpdink/wing-agent)".into(),
    ));
    for i in 0..12 {
        app.chat
            .push(ChatCell::AssistantMessage(format!("below {i}")));
    }
    app
}

/// Scrolling must not smear: every cell of the partial repaint has to land
/// where the buffer says — wide graphemes inside links included.
#[test]
fn scrolling_linked_cjk_keeps_the_terminal_in_step() {
    let mut app = app_with_chinese_links();
    let mut terminal = ratatui::Terminal::new(TerminalSim::new(40, 12)).expect("test terminal");
    app.draw(&mut terminal).expect("first frame");

    // A wheel notch scrolls three lines: the next frame is a partial
    // repaint, which is where a width mismatch starts to smear.
    app.handle_mouse(mouse_at(crossterm::event::MouseEventKind::ScrollUp, (5, 5)));
    app.draw(&mut terminal).expect("scrolled frame");

    assert!(
        terminal.backend().desync.is_empty(),
        "the terminal cursor drifted from the backend's model:\n{}",
        terminal.backend().desync.join("\n")
    );
}

// ── Overlay scrollbar: whole-frame checks through a real Terminal ────

/// The frame as text (one line per row), for assertion messages.
fn frame_text(buf: &ratatui::buffer::Buffer) -> String {
    (buf.area.y..buf.area.bottom())
        .map(|row| {
            let line: String = (buf.area.x..buf.area.right())
                .map(|x| buf[(x, row)].symbol())
                .collect();
            format!("{row:>2} |{line}|\n")
        })
        .collect()
}

/// Bar glyphs are unambiguous outside the chat (`│` is shared with every
/// border, `┃` / `█` are not).
fn is_bar_glyph(symbol: &str) -> bool {
    matches!(symbol, "┃" | "█")
}

/// A chat long enough to overflow the viewport, drawn through ratatui's
/// `TestBackend`.
fn draw_frame(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    app.draw(&mut terminal).expect("draw");
    terminal.backend().buffer().clone()
}

fn long_chat_app() -> App {
    let mut app = test_app();
    for i in 0..40 {
        app.chat
            .push(ChatCell::AssistantMessage(format!("msg {i}")));
    }
    app
}

/// Whole-frame contract: the bar owns the chat area's rightmost column,
/// one glyph per chat row, and nothing else in the frame.
#[test]
fn test_draw_paints_the_scrollbar_only_on_the_chat_areas_last_column() {
    let mut app = long_chat_app();
    let buf = draw_frame(&mut app, 80, 24);
    let chat = app.geometry.chat_band();
    let column = chat.right() - 1;
    assert!(chat.height > 3, "chat viewport: {chat:?}");
    assert_eq!(column, 79, "the chat spans the full width");

    // Every chat row carries a bar glyph on the chat area's last column…
    for row in chat.y..chat.bottom() {
        let symbol = buf[(column, row)].symbol();
        assert!(
            matches!(symbol, "│" | "┃" | "█"),
            "row {row} must carry the bar, got {symbol:?}\n{}",
            frame_text(&buf)
        );
    }
    // …the composer rows below it keep their own content (the info
    // separator paints a rule across the whole row)…
    assert_eq!(
        buf[(column, chat.bottom())].symbol(),
        "─",
        "info separator row\n{}",
        frame_text(&buf)
    );
    assert!(
        !matches!(buf[(column, 0)].symbol(), "│" | "┃" | "█"),
        "status bar row must not look like the bar\n{}",
        frame_text(&buf)
    );
    // …and no other cell of the frame carries a bar-only glyph.
    for row in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            if is_bar_glyph(buf[(x, row)].symbol()) {
                assert_eq!(
                    x,
                    column,
                    "bar glyph outside the chat column at ({x},{row})\n{}",
                    frame_text(&buf)
                );
            }
        }
    }
}

/// The gutter is content-free: nothing is drawn in the columns the bar
/// reserves, on any row. This is the shape that used to end flush against
/// the bar — a user message (full-width background) plus a CJK paragraph
/// (no spaces, so the greedy wrap fills every line's budget exactly).
#[test]
fn test_draw_keeps_chat_content_out_of_the_scrollbar_gutter() {
    let mut app = test_app();
    app.chat.push(ChatCell::UserMessage("中文输入框".into()));
    app.chat.push(ChatCell::AssistantMessage(
        "这段中文没有空格，每一行都会被换行器顶满，正是以前最后一个字形被滑轮裁掉一半的形态。"
            .repeat(3),
    ));
    for i in 0..20 {
        app.chat
            .push(ChatCell::AssistantMessage(format!("pad {i}")));
    }
    let buf = draw_frame(&mut app, 80, 24);
    assert!(
        app.scrollbar_geometry().is_some(),
        "content must overflow for the gutter check to mean anything"
    );

    let chat = app.geometry.chat_band();
    let bar_column = chat.right() - 1;
    let content_right = chat.right() - scrollbar::SCROLLBAR_GUTTER;
    for row in chat.y..chat.bottom() {
        for x in content_right..bar_column {
            let cell = &buf[(x, row)];
            assert_eq!(
                cell.symbol(),
                " ",
                "content leaked into the gutter at ({x},{row})\n{}",
                frame_text(&buf)
            );
            assert_eq!(
                cell.bg,
                ratatui::style::Color::Reset,
                "background leaked into the gutter at ({x},{row})\n{}",
                frame_text(&buf)
            );
        }
    }
}

/// The bar is painted before the toast, but the toast keeps a one-column
/// right margin — they share rows and never the bar's column, so a frame
/// with both must show both.
#[test]
fn test_draw_keeps_the_bar_and_the_toast_in_the_same_frame() {
    let mut app = long_chat_app();
    app.toast = Some(Toast::info(
        "hello toast",
        std::time::Duration::from_secs(30),
    ));
    let buf = draw_frame(&mut app, 80, 24);
    let chat = app.geometry.chat_band();
    let column = chat.right() - 1;

    let rendered = frame_text(&buf);
    assert!(rendered.contains("hello toast"), "toast text:\n{rendered}");
    // The toast sits at y = 1 (below the status bar); the bar rows it
    // spans must still carry the bar, i.e. the overlay pass that draws the
    // toast does not erase it.
    for row in chat.y..chat.y + 3 {
        assert!(
            matches!(buf[(column, row)].symbol(), "│" | "┃" | "█"),
            "the bar must survive the toast pass on row {row}\n{rendered}"
        );
    }
    // The toast's own border column is untouched by the bar (row 2 is a
    // vertical border row; row 1 is the top border corner).
    let toast_border = 80 - 2;
    assert_eq!(
        buf[(toast_border, 2)].symbol(),
        "│",
        "toast right border\n{rendered}"
    );
}

/// Spec: the interaction state is dropped in the same frame the bar
/// disappears with, not on the next mouse event.
#[test]
fn test_draw_clears_the_scrollbar_state_when_the_bar_disappears() {
    let mut app = long_chat_app();
    draw_frame(&mut app, 80, 24);
    let geom = app.scrollbar_geometry().expect("content overflows");
    app.handle_mouse(hover((geom.column, geom.track_top)));
    app.handle_mouse(press((geom.column, geom.thumb_top)));
    assert!(app.scrollbar.is_active());

    // Content shrinks below the viewport (clear / compaction): the next
    // frame must drop the highlight without waiting for a mouse event.
    app.chat.clear();
    app.chat.push(ChatCell::AssistantMessage("short".into()));
    draw_frame(&mut app, 80, 24);
    assert!(
        !app.scrollbar.is_active(),
        "the frame that hides the bar clears its interaction state"
    );
}

#[test]
fn test_wheel_scrolls_chat_while_ask_panel_is_open() {
    let mut app = test_app();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![],
        vec![serde_json::json!({
            "type": "ask",
            "tool_call_id": "ask-wheel",
            "questions": [{
                "id": "q1",
                "header": "H",
                "question": "which?",
                "options": [{"label": "a"}, {"label": "b"}],
            }],
        })],
        None,
    ));
    assert_eq!(app.ask_panels.len(), 1);

    set_chat_height(&mut app, 20);
    app.chat.last_total = 100;
    app.chat.scroll_offset = 50;
    app.chat.scroll_up(0); // leave auto-scroll

    let cursor_before = app.ask_panels.front().unwrap().states[0].cursor;
    app.handle_mouse(wheel_down());
    assert_eq!(app.chat.scroll_offset, 53, "the wheel reaches the chat");
    assert_eq!(
        app.ask_panels.front().unwrap().states[0].cursor,
        cursor_before,
        "the wheel must not move the panel selection"
    );
    assert_eq!(app.ask_panels.len(), 1, "panel stays open");

    // Plain Down still belongs to the panel (not to chat scrolling).
    app.handle_key(key(crossterm::event::KeyCode::Down));
    assert_eq!(app.chat.scroll_offset, 53);
    assert_ne!(
        app.ask_panels.front().unwrap().states[0].cursor,
        cursor_before
    );
}

#[test]
fn test_wheel_scrolls_chat_while_model_panel_is_open() {
    let mut app = test_app();
    app.model_sources = vec![model_group("p", &["m1", "m2"])];
    app.try_frontend_command("/model");
    app.drain_intents();
    assert!(app.model_panel.is_some());

    set_chat_height(&mut app, 20);
    app.chat.last_total = 100;
    app.chat.scroll_offset = 50;
    app.chat.scroll_up(0); // leave auto-scroll

    let cursor_before = app.model_panel.as_ref().unwrap().cursor();
    app.handle_mouse(wheel_down());
    assert_eq!(app.chat.scroll_offset, 53, "the wheel reaches the chat");
    assert_eq!(app.model_panel.as_ref().unwrap().cursor(), cursor_before);
    assert!(app.model_panel.is_some(), "panel stays open");

    // Plain Down navigates the panel and leaves the chat alone.
    app.handle_key(key(crossterm::event::KeyCode::Down));
    assert_eq!(app.chat.scroll_offset, 53);
    assert_ne!(app.model_panel.as_ref().unwrap().cursor(), cursor_before);
}

#[test]
fn test_wheel_scrolls_chat_while_command_popup_is_open() {
    let mut app = test_app();
    app.handle_key(key(crossterm::event::KeyCode::Char('/')));
    assert!(app.popup.active.has_items(), "slash opens the popup");

    set_chat_height(&mut app, 20);
    app.chat.last_total = 100;
    app.chat.scroll_offset = 50;
    app.chat.scroll_up(0); // leave auto-scroll

    let selected_before = app.popup.active.selected_name().map(str::to_string);
    assert!(selected_before.is_some(), "popup has a selection");
    app.handle_mouse(wheel_down());
    assert_eq!(app.chat.scroll_offset, 53, "the wheel reaches the chat");
    assert_eq!(
        app.popup.active.selected_name().map(str::to_string),
        selected_before,
        "the wheel must not move the popup selection"
    );
    assert!(app.popup.active.has_items(), "popup stays open");

    // Plain Down moves the popup selection instead.
    app.handle_key(key(crossterm::event::KeyCode::Down));
    assert_eq!(app.chat.scroll_offset, 53);
    assert_ne!(
        app.popup.active.selected_name().map(str::to_string),
        selected_before
    );
}
