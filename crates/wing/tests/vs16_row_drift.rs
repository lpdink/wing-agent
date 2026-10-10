//! The VS16 row drift (#181): after a repaint the terminal must show exactly
//! what the buffer holds.
//!
//! A cell holding an emoji presentation sequence (`U+26A0 U+FE0F` and friends)
//! measures two columns, and `BufferDiff` takes its wide branch for it — but it
//! *also* re-emits the cell the symbol covers, as a workaround for terminals
//! that fail to clear an emoji's trailing cell. `CrosstermBackend::draw` reads
//! that trailing cell as adjacent (`x == last.x + 1`) and skips its `MoveTo`,
//! assuming the symbol it just printed advanced the cursor by one column: the
//! trailing cell and every later cell of the row land one column to the right,
//! and a write that hits half of a CJK glyph makes the terminal drop that whole
//! glyph. The buffer stays correct (which is why selecting or copying shows the
//! text), the screen drifts, and it stays drifted until the next full repaint.
//!
//! The probe below is the wire, not the widgets: a real `Buffer::diff`, the
//! backend's adjacency rule, and a screen that advances its cursor by what it
//! *prints* (including xterm's rule for writing into half of a wide glyph).
//! Every scenario is replayed twice — with the frames exactly as the widgets
//! wrote them, and with the wire pass ([`wing::ui::emoji_width`]) applied, which
//! is what `tui::draw_frame` does in production — so the first arm pins the
//! ratatui regression and the second pins the repair. The drift arm turning
//! clean is *expected* the day a ratatui release carries ratatui#2721; its
//! message says so.

use std::num::NonZeroU16;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use wing::render::markdown::symbol_width;
use wing::ui::emoji_width;

/// Width of every probe frame (the repro in the issue used 10).
const WIDTH: u16 = 12;

/// The emoji of the repro: a base character plus `U+FE0F`.
const EMOJI: &str = "⚠️";

/// One row of a probe frame, laid out the way a widget lays text out.
fn row(text: &str) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, WIDTH, 1));
    buf.set_stringn(0, 0, text, WIDTH as usize, Style::default());
    buf
}

// ── The terminal ────────────────────────────────────────────────

/// A screen that models what a real terminal *shows*.
///
/// One entry per column: `Some(symbol)` is the grapheme whose head sits in that
/// column, `None` a column covered by the wide grapheme to its left. The cursor
/// advances by what was printed, which is the whole point — the backend's model
/// of it is what drifts.
struct Screen {
    width: u16,
    rows: Vec<Vec<Option<String>>>,
    cursor: (u16, u16),
}

impl Screen {
    fn blank(width: u16, height: u16) -> Self {
        Self {
            width,
            rows: vec![vec![Some(" ".to_string()); width as usize]; height as usize],
            cursor: (0, 0),
        }
    }

    fn move_to(&mut self, x: u16, y: u16) {
        self.cursor = (x, y);
    }

    /// Print `symbol` at the cursor and advance it by the columns the symbol
    /// really takes — the terminal's rule, not the backend's assumption.
    fn print(&mut self, symbol: &str) {
        let width = symbol_width(symbol);
        assert!(
            width >= 1,
            "the probe does not model a zero-width symbol: {symbol:?}"
        );
        let (x, y) = self.cursor;
        assert!(
            x + width <= self.width,
            "{symbol:?} printed at column {x} would run past the row"
        );
        for column in x..x + width {
            self.blank_at(column, y);
        }
        self.rows[y as usize][x as usize] = Some(symbol.to_string());
        for column in x + 1..x + width {
            self.rows[y as usize][column as usize] = None;
        }
        let next = x + width;
        self.cursor = if next >= self.width {
            (0, y + 1)
        } else {
            (next, y)
        };
    }

    /// Blank the grapheme covering `column` — half of a wide one included,
    /// which is how a terminal drops the character a write lands on.
    fn blank_at(&mut self, column: u16, row: u16) {
        let row = row as usize;
        let column = column as usize;
        match &self.rows[row][column] {
            // A narrow grapheme occupies its own column only.
            Some(symbol) if symbol_width(symbol) == 1 => {}
            // A wide grapheme's head covers the column to its right as well.
            Some(_) => self.rows[row][column + 1] = Some(" ".to_string()),
            // A covered column: the grapheme before it owns both.
            None => self.rows[row][column - 1] = Some(" ".to_string()),
        }
        self.rows[row][column] = Some(" ".to_string());
    }

    /// What the terminal shows, per column (`None` = covered by the wide
    /// grapheme to the left).
    fn columns(&self, row: u16) -> &[Option<String>] {
        &self.rows[row as usize]
    }
}

// ── The wire ────────────────────────────────────────────────────

/// Write `prev.diff(next)` the way `CrosstermBackend::draw` does.
///
/// The only rule that matters here is its `MoveTo` short-circuit: a cell that
/// follows the previous one at `x + 1` is printed where the cursor already is.
/// That is sound exactly while every printed symbol advanced the cursor by the
/// columns its cell declared.
fn replay(prev: &Buffer, next: &Buffer, screen: &mut Screen) {
    let mut last: Option<(u16, u16)> = None;
    for (x, y, cell) in prev.diff(next) {
        if last != Some((x.wrapping_sub(1), y)) {
            screen.move_to(x, y);
        }
        last = Some((x, y));
        screen.print(cell.symbol());
    }
}

/// A screen showing `frame`, painted from a blank terminal (startup: the diff
/// from the empty buffer sends every cell the frame holds).
fn full_paint(frame: &Buffer) -> Screen {
    let mut screen = Screen::blank(frame.area.width, frame.area.height);
    replay(&Buffer::empty(frame.area), frame, &mut screen);
    screen
}

/// What the buffer says each column of `row` shows (`None` for a column covered
/// by the wide grapheme before it) — the projection a copy would read.
fn buffer_columns(buf: &Buffer, row: u16) -> Vec<Option<String>> {
    let mut columns = vec![None; buf.area.width as usize];
    let mut x = buf.area.x;
    while x < buf.area.right() {
        let symbol = buf[(x, row)].symbol().to_string();
        let width = symbol_width(&symbol);
        columns[(x - buf.area.x) as usize] = Some(symbol);
        x += width.max(1);
    }
    columns
}

/// The columns the screen and the buffer disagree on, as a report.
fn drift(screen: &Screen, buf: &Buffer) -> Vec<String> {
    let mut report = Vec::new();
    for row in 0..buf.area.height {
        let shown = screen.columns(row);
        let held = buffer_columns(buf, row);
        for (column, (shown, held)) in shown.iter().zip(held.iter()).enumerate() {
            if shown != held {
                report.push(format!(
                    "row {row} column {column}: the terminal shows {:?} where the buffer \
                     holds {held:?}",
                    shown.as_deref().unwrap_or("<covered>"),
                ));
            }
        }
    }
    report
}

/// The text a row reads as: every head symbol, covered columns skipped.
fn text_of(columns: &[Option<String>]) -> String {
    columns
        .iter()
        .flatten()
        .cloned()
        .collect::<String>()
        .trim_end()
        .to_string()
}

// ── The scenarios ───────────────────────────────────────────────

/// One scenario, replayed twice.
struct Probe {
    /// The row text the buffer holds after the repaint (what a copy reads).
    expected: String,
    /// The screen's row text after the repaint **without** the wire pass.
    drifted_text: String,
    /// Where that screen disagrees with the buffer (empty = it shows the frame).
    drifted: Vec<String>,
    /// The same two, with the pass applied to both frames.
    pinned_text: String,
    pinned: Vec<String>,
}

/// Repaint `after` over `before` on a screen, with and without the wire pass.
fn repaint(before: &str, after: &str) -> Probe {
    let (prev, next) = (row(before), row(after));

    let mut screen = full_paint(&prev);
    replay(&prev, &next, &mut screen);
    let drifted_text = text_of(screen.columns(0));
    let drifted = drift(&screen, &next);

    // The pass is applied to both frames, the way production does it: the
    // previous frame went through `tui::draw_frame` too, so its cells carry the
    // pin the screen in front of the user was painted from.
    let (mut prev, mut next) = (prev, next);
    emoji_width::pin(&mut prev);
    emoji_width::pin(&mut next);
    let mut screen = full_paint(&prev);
    replay(&prev, &next, &mut screen);
    let pinned_text = text_of(screen.columns(0));
    let pinned = drift(&screen, &next);

    Probe {
        expected: text_of(&buffer_columns(&next, 0)),
        drifted_text,
        drifted,
        pinned_text,
        pinned,
    }
}

/// Assert the repaired arm: the terminal shows the buffer, column for column.
fn assert_in_step(probe: &Probe) {
    assert_eq!(
        probe.pinned_text, probe.expected,
        "the repaired row must read what the buffer holds"
    );
    assert!(
        probe.pinned.is_empty(),
        "the wire pass must keep the row in step:\n{}",
        probe.pinned.join("\n")
    );
}

/// Assert the drift arm shows the documented artifact: `text` on the screen,
/// at least one column off the buffer — and that the pass repairs it.
///
/// A *clean* drift arm means ratatui stopped re-sending the emoji's trailing
/// cell (ratatui#2721): the replay is then sound without the wire pass, and
/// `ui::emoji_width` is a candidate for removal — see #181. Nothing is broken
/// about that day; this assertion is the reminder, not a bug.
fn assert_drifted(probe: &Probe, text: &str) {
    assert_eq!(
        probe.drifted_text, text,
        "the drifted row should read {text:?}"
    );
    assert!(
        !probe.drifted.is_empty(),
        "ratatui no longer desyncs the backend on an emoji repaint (ratatui#2721): \
         re-evaluate whether `ui::emoji_width` is still needed (#181)"
    );
    assert_in_step(probe);
}

/// Assert a repaint that was never broken: the terminal shows the buffer both
/// with and without the pass.
fn assert_clean(probe: &Probe) {
    assert_eq!(
        probe.drifted_text, probe.expected,
        "this repaint is not supposed to drift"
    );
    assert!(probe.drifted.is_empty(), "{}", probe.drifted.join("\n"));
    assert_in_step(probe);
}

#[test]
fn an_emoji_arriving_in_text_shifts_the_rest_of_the_row() {
    // `AB好` → `A⚠️好`: the emoji's trailing cell is re-sent (the cell holds a
    // CJK head in the previous frame), so the backend skips its `MoveTo` and
    // `好` is printed one column to the right — a hole in the row, while the
    // buffer still holds the text.
    let probe = repaint("AB好", &format!("A{EMOJI}好"));
    assert_drifted(&probe, &format!("A{EMOJI} 好"));
    assert_eq!(probe.drifted.len(), 3, "{}", probe.drifted.join("\n"));
}

#[test]
fn an_emoji_arriving_in_cjk_text_loses_a_character() {
    // `AB世界` → `A⚠️世界`: the offset makes `界` land on the right half of `世`,
    // which the terminal drops — the "missing character" of the issue.
    let probe = repaint("AB世界", &format!("A{EMOJI}世界"));
    assert_drifted(&probe, &format!("A{EMOJI}  界"));
    assert_eq!(probe.drifted.len(), 2, "{}", probe.drifted.join("\n"));
}

#[test]
fn a_cjk_only_reflow_stays_in_step() {
    // The control: a wide glyph *moving* re-sends its own head and nothing
    // else, so the backend's `MoveTo` puts the cursor back and the row survives.
    let mut before = row("AB世界");
    let mut after = row("A时世界");
    assert_eq!(emoji_width::pin(&mut before), 0, "nothing to pin");
    assert_eq!(emoji_width::pin(&mut after), 0, "nothing to pin");
    assert_clean(&repaint("AB世界", "A时世界"));
}

#[test]
fn an_emoji_appended_at_the_end_of_a_row_stays_in_step() {
    // The other control: on a pure append the trailing cell did not change its
    // symbol, so the diff never re-sends it and nothing shifts.
    let mut appended = row(&format!("AB{EMOJI}"));
    assert_eq!(
        emoji_width::pin(&mut appended),
        1,
        "the pass still pins the cell (the frame is normalized, not the diff)"
    );
    assert_clean(&repaint("AB", &format!("AB{EMOJI}")));
}

#[test]
fn an_emoji_that_moves_one_column_shifts_the_rest_of_the_row() {
    // The emoji in *both* frames, one column apart: its head is re-sent (it
    // moved) and its trailing cell changed its symbol (from `好`'s head to a
    // blank), which is the pair of conditions the issue names. The emoji being
    // there before does not make the repaint safe.
    let probe = repaint(&format!("A{EMOJI}好"), &format!("AB{EMOJI}好"));
    assert_drifted(&probe, &format!("AB{EMOJI} 好"));
    assert_eq!(probe.drifted.len(), 3, "{}", probe.drifted.join("\n"));
}

#[test]
fn the_draw_entry_point_pins_the_cell_the_terminal_gets() {
    // `tui::draw_frame` is what `App::draw` and the setup wizard call: a frame
    // drawn through it must carry the pin all the way into the cell the backend
    // writes, or the repair never reaches a terminal.
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 1)).expect("test terminal");
    wing::tui::draw_frame(&mut terminal, |frame| {
        frame.render_widget(Paragraph::new(format!("A{EMOJI}世界")), frame.area());
    })
    .expect("draw the frame");

    let cell = &terminal.backend().buffer()[(1, 0)];
    assert_eq!(cell.symbol(), EMOJI, "the frame really holds the emoji");
    assert_eq!(
        cell.diff_option,
        CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap()),
        "the declared width is the columns the terminal advances"
    );
}
