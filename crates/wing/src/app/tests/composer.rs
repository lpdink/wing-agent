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

/// One row of the **card** as text (the float's margins excluded).
fn card_row(buf: &ratatui::buffer::Buffer, composer: ratatui::layout::Rect, row: u16) -> String {
    (composer.x..composer.right())
        .map(|x| buf[(x, row)].symbol())
        .collect()
}

#[test]
fn test_the_card_frames_the_draft() {
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);

    let width = composer.width as usize;
    assert_eq!(
        card_row(&buf, composer, composer.y),
        format!("╭{}╮", "─".repeat(width - 2)),
        "an idle top rail is a plain rule:\n{}",
        frame_text(&buf)
    );
    assert_eq!(
        card_row(&buf, composer, composer.y + 1),
        format!(
            "│ ❯ hello world{}│",
            " ".repeat(pad(width, "│ ❯ hello world"))
        ),
        "the draft sits inside the frame, behind the prompt glyph:\n{}",
        frame_text(&buf)
    );
    assert_eq!(
        card_row(&buf, composer, composer.bottom() - 1),
        format!("╰{}╯", "─".repeat(width - 2)),
        "the meta rail closes the card (nothing to report here):\n{}",
        frame_text(&buf)
    );
    // The card floats: the margins outside it are never touched.
    for y in composer.y..composer.bottom() {
        assert_eq!(buf[(composer.x - 1, y)].symbol(), " ", "left margin at {y}");
        assert_eq!(
            buf[(composer.right(), y)].symbol(),
            " ",
            "right margin at {y}"
        );
    }
}

#[test]
fn test_the_card_grows_with_the_draft_and_keeps_the_frame() {
    let mut app = app_with_draft("one\ntwo\nthree");
    let (buf, composer) = draw_frame(&mut app, 40, 12);
    assert_eq!(composer.height, 3 + 2, "three text rows inside the frame");

    // The prompt glyph opens the draft; the following rows line up with the
    // text it introduced, and every row carries both vertical borders — the
    // card is a box, not a rule pair.
    let width = composer.width as usize;
    assert_eq!(
        card_row(&buf, composer, composer.y + 1),
        format!("│ ❯ one{}│", " ".repeat(pad(width, "│ ❯ one")))
    );
    assert_eq!(
        card_row(&buf, composer, composer.y + 2),
        format!("│   two{}│", " ".repeat(pad(width, "│   two")))
    );
    assert_eq!(
        card_row(&buf, composer, composer.y + 3),
        format!("│   three{}│", " ".repeat(pad(width, "│   three")))
    );
    for y in composer.y..composer.bottom() {
        let row = card_row(&buf, composer, y);
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
    let top = card_row(&buf, composer, composer.y);

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
    let bottom = card_row(&buf, composer, composer.bottom() - 1);

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
fn test_the_card_keeps_the_terminal_background() {
    let mut app = app_with_draft("hello world");
    let (buf, composer) = draw_frame(&mut app, 40, 10);
    let palette = crate::config::ThemePalette::default();

    // No fill anywhere: the card is a boundary, not a surface. Every cell of
    // it — frame, rails and the draft between them — stays on whatever
    // background the terminal has (the float's margins included).
    for y in composer.y..composer.bottom() {
        for x in composer.x - 1..composer.right() + 1 {
            assert_eq!(
                buf[(x, y)].bg,
                ratatui::style::Color::Reset,
                "({x},{y}) must not be filled"
            );
        }
    }

    // The frame and its rules are one quiet gray — no accent on the edges,
    // with or without a draft.
    for y in composer.y..composer.bottom() {
        assert_eq!(buf[(composer.x, y)].fg, palette.dim, "left edge at {y}");
        assert_eq!(
            buf[(composer.right() - 1, y)].fg,
            palette.dim,
            "right edge at {y}"
        );
    }

    // The *prompt glyph* is where "there is something to send" lives: the one
    // accent the card carries, and dim again while the draft is empty.
    let (text_x, text_y, _) = composer_text(&app);
    assert_eq!(
        buf[(text_x - 2, text_y)].fg,
        palette.accent,
        "❯ with a draft"
    );

    let mut app = app_with_draft("");
    let (buf, _) = draw_frame(&mut app, 40, 10);
    let (text_x, text_y, _) = composer_text(&app);
    assert_eq!(
        buf[(text_x - 2, text_y)].fg,
        palette.dim,
        "❯ on an empty draft"
    );
}

#[test]
fn test_a_held_keyboard_ghosts_the_draft() {
    let mut app = app_with_draft("hello world");
    // An ask panel owns the keyboard: every key would go to it, so the card
    // must stop looking ready for one.
    app.ask_panels
        .push_back(required_choice_panel("call-1", &["y", "n"]));
    let (buf, _) = draw_frame(&mut app, 40, 10);
    let palette = crate::config::ThemePalette::default();
    let (text_x, text_y, _) = composer_text(&app);

    assert!(app.composer_typing_blocked(), "the panel takes the keys");
    assert_eq!(
        buf[(text_x, text_y)].fg,
        palette.dim,
        "the frozen draft is drawn as inactive text"
    );
    // The panel closes: the draft is live again, in the message's own color,
    // and its glyph takes the accent back.
    app.ask_panels.clear();
    let (buf, _) = draw_frame(&mut app, 40, 10);
    let (text_x, text_y, _) = composer_text(&app);
    assert_eq!(buf[(text_x, text_y)].fg, palette.text);
    assert_eq!(buf[(text_x - 2, text_y)].fg, palette.accent);
}

/// A squeezed composer (the terminal is too short for the whole layout) gets
/// fewer rows than the height request asked for, so the card degrades to a
/// bare text area — and the editor must wrap at *those* columns. The chrome
/// the editor reads and the one the widget drew with are the same object, so
/// a click or an arrow key can never address a row the screen does not have.
#[test]
fn test_a_squeezed_card_wraps_at_the_columns_it_was_drawn_with() {
    let mut app = app_with_draft("a draft long enough to wrap somewhere");
    let (_, composer) = draw_frame(&mut app, 20, 6);
    let chrome = app.input.chrome();

    assert!(
        !chrome.card,
        "no room for a frame at {composer:?}: the composer degrades"
    );
    assert_eq!(
        (chrome.text_width, chrome.text_rows),
        (composer.width, composer.height),
        "the editor wraps at exactly what was drawn"
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
