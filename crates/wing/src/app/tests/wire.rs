//! The wire contract of the frames this app draws.
//!
//! `App::draw` renders the whole TUI into one buffer and hands it to
//! `tui::draw_frame`, which normalizes it (see [`crate::ui::emoji_width`])
//! before ratatui diffs it against the previous frame. Both halves matter: the
//! pass is what keeps a VS16 emoji from shifting a row on the terminal — the
//! `vs16_row_drift` integration probe locks the shape — and `App::draw` is the
//! path every frame of this frontend takes, so a cell that reaches the backend
//! from here has been normalized.

use super::support::*;
use crate::ui::chat_view::ChatCell;

/// The emoji of the regression: a base character plus `U+FE0F`, two columns.
const EMOJI: &str = "⚠️";

/// Every cell of the last frame whose symbol carries a variation selector, with
/// its column and row.
fn emoji_cells(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<(u16, u16)> {
    let buf = terminal.backend().buffer();
    (buf.area.y..buf.area.bottom())
        .flat_map(|y| (buf.area.x..buf.area.right()).map(move |x| (x, y)))
        .filter(|&(x, y)| buf[(x, y)].symbol().contains('\u{FE0F}'))
        .collect()
}

#[test]
fn test_a_drawn_frame_pins_the_wide_emoji_it_sends() {
    let mut app = test_app();
    app.clear_welcome();
    app.chat
        .push(ChatCell::AssistantMessage(format!("A{EMOJI}世界")));
    let mut terminal = test_terminal(40, 8);
    draw(&mut app, &mut terminal);

    let emoji = emoji_cells(&terminal);
    assert_eq!(emoji.len(), 1, "one emoji in the frame, got {emoji:?}");
    let (x, y) = emoji[0];
    let cell = &terminal.backend().buffer()[(x, y)];
    assert_eq!(cell.symbol(), EMOJI);
    assert_eq!(
        cell.diff_option,
        ratatui::buffer::CellDiffOption::ForcedWidth(
            std::num::NonZeroU16::new(2).expect("two columns")
        ),
        "the cell the backend writes declares the width the terminal advances"
    );
}

#[test]
fn test_a_drawn_frame_leaves_everything_else_alone() {
    // The pass is not a blanket rewrite of the frame: a frame without an emoji
    // presentation sequence carries no directive at all (nothing in this tree
    // sets one outside the OSC8 injection and the image placements, and this
    // frame has neither).
    let mut app = test_app();
    app.clear_welcome();
    app.chat
        .push(ChatCell::AssistantMessage("世界 hello".into()));
    let mut terminal = test_terminal(40, 8);
    draw(&mut app, &mut terminal);

    let buf = terminal.backend().buffer();
    assert!(emoji_cells(&terminal).is_empty(), "the guard: no VS16 cell");
    for y in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            assert_eq!(
                buf[(x, y)].diff_option,
                ratatui::buffer::CellDiffOption::None,
                "({x},{y}) carries no directive"
            );
        }
    }
}
