//! Mouse interaction tests — press / drag / release, links, composer.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::*;
use crate::ui::chat_view::ChatCell;
use crate::ui::input_area::helpers::PREFIX_WIDTH;

#[test]
fn test_drag_select_copies_the_selected_text() {
    let mut app = app_with_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);

    let band = app.chat.geometry().area;
    assert!(band.height > 1 && app.chat.is_at_bottom());

    assert_eq!(
        app.handle_mouse(press((band.x + 2, band.y + 1))),
        MouseOutcome::Immediate,
        "a press inside the chat band starts a selection"
    );
    // The run loop always draws a frame after handling the press; the drag
    // snap reads the row graphemes that frame snapshotted.
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.handle_mouse(drag((band.x + 12, band.y + 1))),
        MouseOutcome::Coalesced,
        "a drag changes the highlight, coalesced by the frame gate"
    );
    draw(&mut app, &mut terminal);

    // The drag frame carries the highlight over exactly the selected span.
    assert_eq!(
        reversed_cells(&terminal),
        (band.x + 2..band.x + 13)
            .map(|x| (x, band.y + 1))
            .collect::<Vec<_>>()
    );

    assert_eq!(
        app.handle_mouse(release((band.x + 13, band.y + 1))),
        MouseOutcome::Immediate
    );
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "hello world"),
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }

    // Release clears the highlight immediately — no residue, no Esc.
    draw(&mut app, &mut terminal);
    assert!(
        reversed_cells(&terminal).is_empty(),
        "the selection must leave no highlight behind"
    );
    draw(&mut app, &mut terminal);
    assert!(reversed_cells(&terminal).is_empty());
}

#[test]
fn test_click_without_drag_copies_nothing() {
    let mut app = app_with_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;

    assert_eq!(
        app.handle_mouse(press((band.x + 2, band.y + 1))),
        MouseOutcome::Immediate
    );
    assert_eq!(
        app.handle_mouse(release((band.x + 2, band.y + 1))),
        MouseOutcome::Immediate
    );
    assert!(app.drain_intents().is_empty(), "a click selects nothing");
    assert!(
        app.chat.is_at_bottom(),
        "a click at the bottom must not leave the view unfollowed"
    );

    draw(&mut app, &mut terminal);
    assert!(reversed_cells(&terminal).is_empty());
}

/// App whose chat holds one assistant message with a markdown link, so the
/// link's screen row / column can be read back from the frame map.
fn app_with_link() -> App {
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    app.chat.push(ChatCell::AssistantMessage(
        "see [docs](https://example.com) now".into(),
    ));
    app
}

/// App whose chat is taller than the band, with a link in the middle — so
/// scrolling really moves the content under the pointer.
fn app_with_scrollable_link() -> App {
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    for i in 0..4 {
        app.chat.push(ChatCell::UserMessage(format!("above {i}")));
    }
    app.chat.push(ChatCell::AssistantMessage(
        "[docs](https://example.com)".into(),
    ));
    for i in 0..2 {
        app.chat.push(ChatCell::UserMessage(format!("below {i}")));
    }
    app
}

/// Targets of the `OpenLink` intents drained so far (ignores clipboard
/// intents: pressing, scrolling the view and releasing is a *selection*
/// sweep — pre-existing behaviour — but it must never open a link).
fn open_intents(app: &mut App) -> Vec<String> {
    app.drain_intents()
        .into_iter()
        .filter_map(|intent| match intent {
            AppIntent::OpenLink(target) => Some(target),
            _ => None,
        })
        .collect()
}

/// The link's hit box in the drawn frame.
fn link_box(app: &App) -> ((u16, u16), (u16, u16)) {
    let (row, links) = app
        .chat
        .frame_links()
        .first()
        .expect("frame has a link")
        .clone();
    let link = &links[0];
    ((link.start, row), (link.end - 1, row))
}

#[test]
fn test_click_on_a_link_opens_it_without_copying() {
    let mut app = app_with_link();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    let (start, _end) = link_box(&app);

    assert_eq!(app.handle_mouse(press(start)), MouseOutcome::Immediate);
    assert_eq!(app.handle_mouse(release(start)), MouseOutcome::Immediate);
    match app.drain_intents().as_slice() {
        [AppIntent::OpenLink(target)] => assert_eq!(target, "https://example.com"),
        other => panic!("expected exactly one open intent, got {other:?}"),
    }
    assert!(
        app.chat.is_at_bottom(),
        "a click must not leave the view unfollowed"
    );
    draw(&mut app, &mut terminal);
    assert!(
        reversed_cells(&terminal).is_empty(),
        "no highlight for a click"
    );
}

#[test]
fn test_drag_across_a_link_selects_instead_of_opening() {
    let mut app = app_with_link();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    let (start, (end, row)) = link_box(&app);

    assert_eq!(app.handle_mouse(press(start)), MouseOutcome::Immediate);
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.handle_mouse(drag((end, row))),
        MouseOutcome::Coalesced,
        "a drag stays coalesced, even on a link"
    );
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.handle_mouse(release((end, row))),
        MouseOutcome::Immediate
    );

    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => {
            assert!(
                text.contains("docs"),
                "expected the link text, got {text:?}"
            );
        }
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
}

#[test]
fn test_click_outside_a_link_does_nothing() {
    let mut app = app_with_link();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    let ((start, row), _) = link_box(&app);

    // The bullet prefix sits left of the link, plain text right of it.
    for at in [(band.x, row), (band.x + 1, row), (start - 1, row)] {
        assert_eq!(
            app.handle_mouse(press(at)),
            MouseOutcome::Immediate,
            "{at:?}"
        );
        assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Immediate);
        assert!(
            app.drain_intents().is_empty(),
            "a click at {at:?} must not do anything"
        );
    }
}

#[test]
fn test_cancelled_gesture_never_opens_the_link() {
    // Focus loss aborts the gesture; the release that follows must be a
    // no-op even though it lands on the link.
    let mut app = app_with_link();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    let (start, _) = link_box(&app);

    assert_eq!(app.handle_mouse(press(start)), MouseOutcome::Immediate);
    app.cancel_selection();
    assert_eq!(app.handle_mouse(release(start)), MouseOutcome::Ignored);
    assert!(app.drain_intents().is_empty());

    // ...and a structural change (width) does the same through the draw.
    assert_eq!(app.handle_mouse(press(start)), MouseOutcome::Immediate);
    let mut resized = test_terminal(30, 12);
    draw(&mut app, &mut resized);
    assert_eq!(
        app.handle_mouse(release(start)),
        MouseOutcome::Ignored,
        "the width change aborted the gesture"
    );
    assert!(
        app.drain_intents().is_empty(),
        "aborted selection opens nothing"
    );
}

#[test]
fn test_link_click_uses_the_press_frame_snapshot() {
    // The target is the one recorded at press time: the frame in between
    // shows a *different* link under the same pointer, and the click still
    // opens what the user pressed on.
    let mut app = app_with_link();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    let (at, _) = link_box(&app);

    assert_eq!(app.handle_mouse(press(at)), MouseOutcome::Immediate);
    app.chat.cells[0].mutate(|cell| {
        *cell = ChatCell::AssistantMessage("see [docs](https://changed.example) now".into());
    });
    draw(&mut app, &mut terminal);
    assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Immediate);
    match app.drain_intents().as_slice() {
        [AppIntent::OpenLink(target)] => assert_eq!(target, "https://example.com"),
        other => panic!("expected the pressed link, got {other:?}"),
    }
}

#[test]
fn test_release_off_the_anchor_is_not_a_click() {
    // Both halves of "never moved": no drag event *and* the pointer back on
    // the anchor. A release elsewhere (here: the same column after the view
    // scrolled, and a plain different column) opens nothing.
    let mut app = app_with_scrollable_link();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    let (at, _) = link_box(&app);

    assert_eq!(app.handle_mouse(press(at)), MouseOutcome::Immediate);
    app.chat.scroll_up(2);
    draw(&mut app, &mut terminal);
    assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Immediate);
    assert!(
        open_intents(&mut app).is_empty(),
        "the content under the pointer changed: not a click"
    );

    // Back at the bottom (the link is in place again): a release on a
    // different column is rejected by the anchor check.
    app.chat.scroll_down(2, app.geometry.chat_height());
    draw(&mut app, &mut terminal);
    let ((start, row), _) = link_box(&app);
    assert_eq!(
        app.handle_mouse(press((start, row))),
        MouseOutcome::Immediate
    );
    assert_eq!(
        app.handle_mouse(release((start + 6, row))),
        MouseOutcome::Immediate
    );
    assert!(open_intents(&mut app).is_empty());

    // The unchanged press/release pair is still a click (control).
    draw(&mut app, &mut terminal);
    let ((start, row), _) = link_box(&app);
    assert_eq!(
        app.handle_mouse(press((start, row))),
        MouseOutcome::Immediate
    );
    assert_eq!(
        app.handle_mouse(release((start, row))),
        MouseOutcome::Immediate
    );
    assert_eq!(
        open_intents(&mut app),
        vec!["https://example.com".to_string()]
    );
}

#[test]
fn test_wheel_over_a_link_still_scrolls() {
    let mut app = app_with_link();
    let mut terminal = test_terminal(60, 12);
    draw(&mut app, &mut terminal);
    let ((start, row), _) = link_box(&app);
    let before = app.chat.scroll_position();

    app.chat.scroll_up(5);
    let scrolled = app.chat.scroll_position();
    assert_eq!(scrolled, before, "content fits: nothing to scroll");
    assert_eq!(
        app.handle_mouse(mouse_at(
            crossterm::event::MouseEventKind::ScrollUp,
            (start, row)
        )),
        MouseOutcome::Immediate
    );
    assert!(
        app.drain_intents().is_empty(),
        "the wheel never opens a link"
    );
}

#[test]
fn test_press_outside_the_interactive_regions_never_starts_a_selection() {
    let mut app = app_with_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;

    // The status bar belongs to neither region: the press is ignored, no
    // selection starts, and the following drag / release stay inert.
    let status = (band.x + 2, 0);
    assert!(!app.chat.contains_screen(status.0, status.1));
    assert!(!app.composer_contains(status.0, status.1));
    assert_eq!(
        app.handle_mouse(press(status)),
        MouseOutcome::Ignored,
        "press at {status:?} is ignored"
    );
    assert!(!app.selection.is_press_active());
    assert_eq!(
        app.handle_mouse(drag((band.x + 5, band.y + 1))),
        MouseOutcome::Ignored
    );
    assert_eq!(
        app.handle_mouse(release((band.x + 5, band.y + 1))),
        MouseOutcome::Ignored
    );
    assert!(app.drain_intents().is_empty());
}

#[test]
fn test_press_in_the_composer_starts_a_composer_selection() {
    let mut app = app_with_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    let at = (composer.x + 4, composer.y);
    assert!(app.composer_contains(at.0, at.1));
    assert!(
        !app.chat.contains_screen(at.0, at.1),
        "the composer sits below the chat band"
    );

    assert_eq!(app.handle_mouse(press(at)), MouseOutcome::Immediate);
    assert_eq!(
        app.selection.region(),
        Some(SelectionRegion::Composer),
        "the press decides the region"
    );
    assert!(
        app.chat.is_at_bottom(),
        "a composer press must not freeze the chat's follow state"
    );
    assert!(
        app.selection_autoscroll_at.is_none(),
        "the composer has no edge auto-scroll"
    );
}

// ── Composer pointer: drag select, click to place the cursor ──────

#[test]
fn test_composer_drag_selects_and_copies_the_draft() {
    let mut app = app_with_draft("hello world");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    assert_eq!(app.input.line_count(), 1);

    assert_eq!(
        app.handle_mouse(press((composer.x + PREFIX_WIDTH, composer.y))),
        MouseOutcome::Immediate,
        "a press inside the composer starts a selection"
    );
    assert_eq!(
        app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 4, composer.y))),
        MouseOutcome::Coalesced,
        "a drag is a flood: coalesced by the frame gate"
    );
    draw(&mut app, &mut terminal);

    // The drag frame highlights "hello" — five cells right after the
    // prefix, and nothing outside the composer (the `> ` prompt included).
    let reversed = reversed_cells(&terminal);
    assert_eq!(
        reversed,
        (0..5)
            .map(|dx| (composer.x + PREFIX_WIDTH + dx, composer.y))
            .collect::<Vec<_>>()
    );

    assert_eq!(
        app.handle_mouse(release((composer.x + PREFIX_WIDTH + 4, composer.y))),
        MouseOutcome::Immediate
    );
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "hello"),
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
    draw(&mut app, &mut terminal);
    assert!(
        reversed_cells(&terminal).is_empty(),
        "the release must leave no highlight behind"
    );
    assert!(
        app.chat.is_at_bottom(),
        "the composer selection never freezes the chat's follow state"
    );
}

#[test]
fn test_composer_drag_across_a_soft_wrap_copies_one_line() {
    // Width 20 → a text area of 18 columns: 24 characters fold into two
    // visual rows (18 + 6).
    let mut app = app_with_draft("abcdefghijklmnopqrstuvwx");
    let mut terminal = test_terminal(20, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    assert_eq!(composer.height, 2, "the draft wraps into two visual rows");

    let from = (composer.x + PREFIX_WIDTH + 2, composer.y);
    let to = (composer.x + PREFIX_WIDTH + 3, composer.y + 1);
    app.handle_mouse(press(from));
    app.handle_mouse(drag(to));
    draw(&mut app, &mut terminal);

    // The highlight covers the first row from column 2 and the second row
    // up to column 4.
    let reversed = reversed_cells(&terminal);
    assert_eq!(
        reversed,
        (2..18)
            .map(|col| (composer.x + PREFIX_WIDTH + col, composer.y))
            .chain((0..4).map(|col| (composer.x + PREFIX_WIDTH + col, composer.y + 1)))
            .collect::<Vec<_>>()
    );

    app.handle_mouse(release(to));
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(
            text, "cdefghijklmnopqrstuv",
            "a soft wrap is a display fold, not a newline in the draft"
        ),
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
}

#[test]
fn test_composer_drag_beyond_the_region_clamps_to_the_visible_window() {
    // Width 20 → a text area of 18 columns: 24 characters fold into two
    // visual rows (18 + 6).
    let mut app = app_with_draft("abcdefghijklmnopqrstuvwx");
    let mut terminal = test_terminal(20, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    assert_eq!(composer.height, 2);

    // Press on the second row, drag up into the chat band: the pointer
    // stays mapped into the press's region and clamps to its first visible
    // row, so the selection grows to the top of the window.
    app.handle_mouse(press((composer.x + PREFIX_WIDTH + 3, composer.y + 1)));
    assert!(app.mouse_drag(composer.x + PREFIX_WIDTH + 1, composer.y - 8));
    assert_eq!(app.selection.region(), Some(SelectionRegion::Composer));
    app.handle_mouse(release((composer.x + PREFIX_WIDTH + 1, composer.y - 8)));
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "cdefghijklmnopqrstu"),
        other => panic!("expected the clamped draft fragment, got {other:?}"),
    }
}

#[test]
fn test_composer_click_places_the_cursor() {
    let mut app = app_with_draft("hello world");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    // `set_text` leaves the cursor at the end, so the click is a visible move.
    assert_eq!((app.input.cursor_row, app.input.cursor_col), (0, 11));

    let at = (composer.x + PREFIX_WIDTH + 6, composer.y);
    assert_eq!(app.handle_mouse(press(at)), MouseOutcome::Immediate);
    assert_eq!(
        app.handle_mouse(release(at)),
        MouseOutcome::Immediate,
        "a press + release without motion is a click"
    );
    assert_eq!(
        (app.input.cursor_row, app.input.cursor_col),
        (0, 6),
        "the click places the cursor before the character under it"
    );
    assert!(!app.selection.is_press_active());
    assert!(
        app.drain_intents().is_empty(),
        "a click selects nothing and copies nothing"
    );
    // The rendered cursor sits exactly where the pointer was.
    let (cursor_x, cursor_y) = cursor_screen_pos(&app.input, &composer);
    assert_eq!((cursor_x, cursor_y), at, "the cursor follows the click");
    draw(&mut app, &mut terminal);
    assert!(reversed_cells(&terminal).is_empty());
}

#[test]
fn test_composer_click_on_a_wide_character_places_the_cursor_before_it() {
    let mut app = app_with_draft("你好世界");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    // "世" occupies the fifth and sixth cell of the text area; either cell
    // resolves to the position before it (a char index has no half-cell
    // precision).
    for column in [4usize, 5] {
        let at = (composer.x + PREFIX_WIDTH + column as u16, composer.y);
        app.handle_mouse(press(at));
        app.handle_mouse(release(at));
        assert_eq!(
            (app.input.cursor_row, app.input.cursor_col),
            (0, 2),
            "click on cell {column} of 世"
        );
        // A char index has no half-cell precision: the right half of a wide
        // character resolves to its left edge, which is the documented ±1.
        let (cursor_x, _) = cursor_screen_pos(&app.input, &composer);
        let delta = (cursor_x as i32 - at.0 as i32).abs();
        assert!(delta <= 1, "cursor x {cursor_x} vs. click column {}", at.0);
    }
    assert!(app.drain_intents().is_empty());
}

#[test]
fn test_composer_click_on_a_scrolled_window_places_the_cursor() {
    // Width 20 → text width 18, max input lines 10 but only 3 fit in the
    // test layout: the window follows the cursor.
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    let draft = (0..8)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.input.set_text(&draft);
    let mut terminal = test_terminal(20, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    assert!(
        app.input.vertical_scroll > 0,
        "the window scrolled to keep the cursor visible"
    );

    let window_row = composer.height - 1;
    let at = (composer.x + PREFIX_WIDTH + 2, composer.y + window_row);
    app.handle_mouse(press(at));
    app.handle_mouse(release(at));

    let expected_line = app.input.vertical_scroll + window_row as usize;
    assert_eq!(app.input.cursor_row, expected_line);
    assert_eq!(app.input.cursor_col, 2);
}

#[test]
fn test_composer_empty_draft_drag_copies_nothing() {
    // Only the placeholder is on screen: it is not draft content, so a drag
    // over it selects nothing and copies nothing.
    let mut app = app_with_draft("");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();

    app.handle_mouse(press((composer.x + PREFIX_WIDTH, composer.y)));
    app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 6, composer.y)));
    draw(&mut app, &mut terminal);
    assert!(
        reversed_cells(&terminal).is_empty(),
        "the placeholder must not be highlighted"
    );
    app.handle_mouse(release((composer.x + PREFIX_WIDTH + 6, composer.y)));
    assert!(
        app.drain_intents().is_empty(),
        "the placeholder must not be copied"
    );
}

#[test]
fn test_composer_drag_over_line_breaks_copies_nothing() {
    // Dragging from the end of the first line to the start of the empty
    // second one covers nothing but the line break: no bare `\n` may be
    // copied and no `Copied!` may be claimed.
    let mut app = app_with_draft("ab\n");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    assert_eq!(
        composer.height, 2,
        "the empty second line is a row of its own"
    );

    let from = (composer.x + PREFIX_WIDTH + 2, composer.y);
    let to = (composer.x + PREFIX_WIDTH, composer.y + 1);
    app.handle_mouse(press(from));
    app.handle_mouse(drag(to));
    draw(&mut app, &mut terminal);
    assert!(
        reversed_cells(&terminal).is_empty(),
        "a line-break-only span has no cells to highlight"
    );
    app.handle_mouse(release(to));
    assert!(
        app.drain_intents().is_empty(),
        "a line-break-only selection copies nothing"
    );
}

#[test]
fn test_composer_same_cell_jitter_is_still_a_click() {
    let mut app = app_with_draft("hello world");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    let at = (composer.x + PREFIX_WIDTH + 6, composer.y);
    assert_eq!((app.input.cursor_row, app.input.cursor_col), (0, 11));

    // Trackpads report motion inside the same cell: click vs. drag is
    // decided by position, so a jitter must not become a one-character copy.
    app.handle_mouse(press(at));
    assert!(app.mouse_drag(at.0, at.1), "the motion event is tracked");
    assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Immediate);
    assert_eq!(
        (app.input.cursor_row, app.input.cursor_col),
        (0, 6),
        "the jitter is a click: the cursor moves"
    );
    assert!(app.drain_intents().is_empty(), "the jitter copies nothing");

    // Dragging away and back onto the anchor is a zero-width selection too:
    // no copy, and the cursor lands on the release point.
    let mut app = app_with_draft("hello world");
    draw(&mut app, &mut terminal);
    app.handle_mouse(press(at));
    app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 3, composer.y)));
    app.handle_mouse(drag(at));
    assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Immediate);
    assert_eq!((app.input.cursor_row, app.input.cursor_col), (0, 6));
    assert!(app.drain_intents().is_empty());
}

#[test]
fn test_wheel_scrolls_the_chat_during_a_composer_selection() {
    let mut app = app_with_draft("hello world");
    let text = (0..30)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.chat.push(ChatCell::UserMessage(text));
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();

    app.handle_mouse(press((composer.x + PREFIX_WIDTH, composer.y)));
    app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 4, composer.y)));
    let before = app.chat.scroll_position();
    assert!(before >= WHEEL_SCROLL_LINES, "the view has room to scroll");

    // The wheel is an independent channel: it scrolls the chat and leaves
    // the composer selection (and its anchors) alone.
    assert_eq!(app.handle_mouse(wheel_up()), MouseOutcome::Immediate);
    assert_eq!(app.chat.scroll_position(), before - WHEEL_SCROLL_LINES);
    assert!(app.selection.is_press_active());
    assert_eq!(app.selection.region(), Some(SelectionRegion::Composer));

    app.handle_mouse(release((composer.x + PREFIX_WIDTH + 4, composer.y)));
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "hello"),
        other => panic!("expected the draft fragment, got {other:?}"),
    }
}

#[test]
fn test_composer_edit_aborts_the_selection() {
    /// One abort rule: a label plus the mutation that changes the draft.
    type Case = (&'static str, Box<dyn Fn(&mut App)>);

    let cases: Vec<Case> = vec![
        (
            "typing",
            Box::new(|app: &mut App| {
                app.handle_key(key(crossterm::event::KeyCode::Char('x')));
            }),
        ),
        (
            "paste",
            Box::new(|app: &mut App| {
                app.handle_paste("pasted");
            }),
        ),
        (
            "submit",
            Box::new(|app: &mut App| {
                app.handle_key(key(crossterm::event::KeyCode::Enter));
            }),
        ),
        (
            "backspace",
            Box::new(|app: &mut App| {
                app.handle_key(key(crossterm::event::KeyCode::Backspace));
            }),
        ),
        (
            "draft reset",
            Box::new(|app: &mut App| {
                // Session switch / `/new` restores an empty draft — the
                // highlight must not survive into unrelated text.
                app.input.clear();
            }),
        ),
        (
            "width",
            Box::new(|app: &mut App| {
                // Handled by drawing into a differently sized terminal.
                let _ = app;
            }),
        ),
    ];

    for (label, mutate) in cases {
        let mut app = app_with_draft("hello world");
        let mut terminal = test_terminal(40, 12);
        draw(&mut app, &mut terminal);
        let composer = app.input.rendered_area();
        app.handle_mouse(press((composer.x + PREFIX_WIDTH, composer.y)));
        app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 4, composer.y)));
        assert!(app.selection.is_press_active(), "{label}: drag in flight");
        app.drain_intents();

        if label == "width" {
            let mut wider = test_terminal(60, 12);
            draw(&mut app, &mut wider);
            assert!(
                reversed_cells(&wider).is_empty(),
                "{label}: the aborted frame must not paint a highlight"
            );
            draw(&mut app, &mut terminal);
        } else {
            mutate(&mut app);
            draw(&mut app, &mut terminal);
        }

        assert!(
            !app.selection.is_press_active(),
            "{label}: the selection must be aborted"
        );
        assert!(
            reversed_cells(&terminal).is_empty(),
            "{label}: the highlight must be cleared"
        );
        assert_eq!(
            app.handle_mouse(release((composer.x + PREFIX_WIDTH + 4, composer.y))),
            MouseOutcome::Ignored,
            "{label}: a release after the abort is a no-op"
        );
        assert!(
            !app.drain_intents()
                .iter()
                .any(|intent| matches!(intent, AppIntent::CopyToClipboard(_))),
            "{label}: nothing may be copied"
        );
    }
}
