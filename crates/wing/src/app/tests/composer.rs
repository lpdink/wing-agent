//! The composer card's rendering contract, asserted on the frame the app
//! really drew (`TestBackend`).
//!
//! These are the tests that say what the bottom of the screen *looks like*:
//! the frame around the draft, the two rails inside it, and the two rows the
//! card used to spend above itself (the working indicator and the info
//! separator) now being part of it.

use std::time::Instant;

use super::support::*;
use crate::app::App;
use crate::app::MouseOutcome;
use crate::ui::chat_view::ChatCell;

/// Draw the app and return `(frame, the composer block it recorded)`.
fn draw_frame(
    app: &mut App,
    width: u16,
    height: u16,
) -> (ratatui::buffer::Buffer, ratatui::layout::Rect) {
    let mut terminal = test_terminal(width, height);
    draw(app, &mut terminal);
    let composer = app.input.rendered_area();
    (terminal.backend().buffer().clone(), composer)
}

/// Spaces between a row's content and its right border.
fn pad(width: usize, content: &str) -> usize {
    width - content.chars().count() - 1
}

/// One row of the frame as text.
fn row_text(buf: &ratatui::buffer::Buffer, row: u16) -> String {
    (buf.area.x..buf.area.right())
        .map(|x| buf[(x, row)].symbol())
        .collect()
}

#[test]
fn test_the_card_frames_the_draft() {
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);

    assert_eq!(
        row_text(&buf, composer.y),
        format!("╭{}╮", "─".repeat(38)),
        "an idle top rail is a plain rule:\n{}",
        frame_text(&buf)
    );
    assert_eq!(
        row_text(&buf, composer.y + 1),
        format!("│ ❯ hello world{}│", " ".repeat(pad(40, "│ ❯ hello world"))),
        "the draft sits inside the frame, behind the prompt glyph:\n{}",
        frame_text(&buf)
    );
    assert_eq!(
        row_text(&buf, composer.bottom() - 1),
        format!("╰{}╯", "─".repeat(38)),
        "the meta rail closes the card (nothing to report here):\n{}",
        frame_text(&buf)
    );
}

#[test]
fn test_the_card_grows_with_the_draft_and_keeps_the_frame() {
    let mut app = app_with_draft("one\ntwo\nthree");
    let (buf, composer) = draw_frame(&mut app, 40, 12);
    assert_eq!(composer.height, 3 + 2, "three text rows inside the frame");

    // The prompt glyph opens the draft; the following rows line up with the
    // text it introduced, and every row carries both vertical borders — the
    // card is a box, not a rule pair.
    assert_eq!(
        row_text(&buf, composer.y + 1),
        format!("│ ❯ one{}│", " ".repeat(pad(40, "│ ❯ one")))
    );
    assert_eq!(
        row_text(&buf, composer.y + 2),
        format!("│   two{}│", " ".repeat(pad(40, "│   two")))
    );
    assert_eq!(
        row_text(&buf, composer.y + 3),
        format!("│   three{}│", " ".repeat(pad(40, "│   three")))
    );
    for y in composer.y..composer.bottom() {
        let row = row_text(&buf, y);
        let (left, right) = (row.chars().next(), row.chars().last());
        assert!(
            matches!(left, Some('╭' | '│' | '╰')) && matches!(right, Some('╮' | '│' | '╯')),
            "row {y} must be closed on both sides: {row}"
        );
    }
}

#[test]
fn test_the_activity_rail_replaces_the_working_row() {
    let mut app = app_with_draft("queued while the agent works");
    app.turn.working = true;
    app.turn.started_at = Some(Instant::now());
    let (buf, composer) = draw_frame(&mut app, 60, 12);
    let top = row_text(&buf, composer.y);

    assert!(
        top.starts_with("╭─ ⠋ Working..."),
        "the spinner opens the top rail:\n{}",
        frame_text(&buf)
    );
    assert!(
        top.contains("Esc to interrupt") && top.ends_with('╮'),
        "the interrupt hint rides the rail, the dashes fill the rest:\n{}",
        frame_text(&buf)
    );
    // A running turn costs no row of its own: the card is still frame + text.
    assert_eq!(
        composer.height, 3,
        "one text row, two borders — the indicator is not a row"
    );
    assert_eq!(
        app.geometry.chat_band().bottom(),
        composer.y,
        "the band ends where the card starts"
    );
}

#[test]
fn test_the_meta_rail_carries_the_session_read_out() {
    let mut app = app_with_draft("draft");
    app.status.workdir = Some("/tmp/ws".into());
    app.chat.push(ChatCell::AssistantMessage(
        (0..40)
            .map(|i| format!("msg {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    let (buf, composer) = draw_frame(&mut app, 80, 12);
    let bottom = row_text(&buf, composer.bottom() - 1);

    assert!(
        bottom.starts_with("╰─ /tmp/ws"),
        "the workdir leads the meta rail:\n{}",
        frame_text(&buf)
    );
    assert!(
        bottom.contains("100%"),
        "the scroll read-out settles on the right:\n{}",
        frame_text(&buf)
    );
}

#[test]
fn test_the_rails_are_not_text() {
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    let (text_x, text_y, _) = composer_text(&app);

    // The pointer's claim covers the whole block (the app dispatches to the
    // composer), but the mapping — which is what a press turns into — finds no
    // text on the frame.
    assert!(app.composer_contains(composer.x + 4, composer.y));
    assert!(app.composer_contains(composer.x + 4, composer.bottom() - 1));
    assert_eq!(
        app.handle_mouse(press((composer.x + 4, composer.y))),
        MouseOutcome::Ignored,
        "a press on the top rail changes nothing:\n{}",
        frame_text(&buf)
    );
    assert!(!app.selection.is_press_active());

    // The draft's first cell is where the card's frame says it is.
    assert_eq!(text_y, composer.y + 1);
    assert_eq!(text_x, composer.x + 4);
    assert_eq!(buf[(text_x, text_y)].symbol(), "h");
}
