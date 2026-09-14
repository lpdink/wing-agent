//! Selection lifecycle tests — follow freeze, invalidation, edge scroll.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::*;
use crate::ui::chat_view::ChatCell;
use crate::ui::input_area::helpers::PREFIX_WIDTH;

#[test]
fn test_composer_selection_survives_chat_structure_changes() {
    let mut app = app_with_draft("hello world");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();

    app.handle_mouse(press((composer.x + PREFIX_WIDTH, composer.y)));
    app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 4, composer.y)));
    // Streaming output, a queued message and a full rebuild: none of it
    // moves the draft's logical coordinates.
    app.chat
        .push(ChatCell::AssistantMessage("streaming".into()));
    app.chat.push_pending("req-1".into(), "queued".into());
    app.chat.clear();
    app.chat.push(ChatCell::UserMessage("rebuilt".into()));
    draw(&mut app, &mut terminal);

    assert!(
        app.selection.is_press_active(),
        "the composer selection is not affected by the chat's structure"
    );
    app.handle_mouse(release((composer.x + PREFIX_WIDTH + 4, composer.y)));
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "hello"),
        other => panic!("expected the draft fragment, got {other:?}"),
    }
}

#[test]
fn test_composer_navigation_keys_keep_the_selection() {
    let mut app = app_with_draft("hello world");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();

    app.handle_mouse(press((composer.x + PREFIX_WIDTH, composer.y)));
    app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 4, composer.y)));
    draw(&mut app, &mut terminal);

    // Moving the cursor is not editing the draft: the selection stays.
    app.handle_key(key(crossterm::event::KeyCode::Left));
    app.handle_key(key(crossterm::event::KeyCode::Home));
    draw(&mut app, &mut terminal);
    assert!(
        app.selection.is_press_active(),
        "navigation keys must not abort the selection"
    );
    assert!(
        !reversed_cells(&terminal).is_empty(),
        "the highlight is still painted"
    );

    app.handle_mouse(release((composer.x + PREFIX_WIDTH + 4, composer.y)));
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "hello"),
        other => panic!("expected the draft fragment, got {other:?}"),
    }
}

#[test]
fn test_drag_freezes_follow_across_frames_and_release_keeps_reading() {
    // Tall content, so the view is pinned at the bottom edge — which is
    // exactly where the freeze has to survive: the render re-arms
    // `auto_scroll` whenever the offset sits at the bottom edge, so the
    // frame drawn right after the press is the one that used to undo it.
    let mut app = app_with_tall_message();
    app.chat
        .push(ChatCell::AssistantMessage("streaming".into()));
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    assert!(app.chat.is_at_bottom(), "pinned to the bottom on load");
    let band = app.chat.geometry().area;

    app.handle_mouse(press((band.x + 2, band.y + 1)));
    let frozen = app.chat.scroll_position();
    assert!(
        !app.chat.is_at_bottom(),
        "the drag freezes the follow state"
    );

    // The run loop always draws a frame after handling the press — the
    // frame must not re-arm follow (regression: the render's "scrolled to
    // the bottom" branch used to undo the freeze here).
    draw(&mut app, &mut terminal);
    assert!(
        app.chat.is_follow_frozen(),
        "the frame after the press must not lift the freeze"
    );
    assert!(
        !app.chat.is_at_bottom(),
        "the frame after the press must not re-arm follow"
    );
    assert_eq!(
        app.chat.scroll_position(),
        frozen,
        "the frame after the press must not move the frozen view"
    );

    // Streaming delta: the existing assistant cell grows. No new cell, no
    // width change — neither the selection nor the frozen view may move.
    let growth = (0..20)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.chat.append_to_last_assistant(&format!("\n{growth}"));
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.chat.scroll_position(),
        frozen,
        "new content must not yank the frozen view"
    );
    assert!(
        app.chat.content_height() > app.geometry.chat_height(),
        "the content must have outgrown the band"
    );
    assert!(
        app.selection.is_press_active(),
        "pure text growth must not abort the selection"
    );

    app.handle_mouse(release((band.x + 4, band.y + 1)));
    assert!(
        !app.chat.is_at_bottom(),
        "released above the bottom edge → stay in reading mode"
    );
}

#[test]
fn test_drag_from_the_left_edge_skips_the_cell_padding() {
    // Dragging from the chat band's left edge covers the two columns a
    // user-message cell fills with background padding — the copy skips
    // them instead of pasting two phantom spaces.
    let mut app = app_with_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;

    app.handle_mouse(press((band.x, band.y + 1)));
    draw(&mut app, &mut terminal);
    app.handle_mouse(drag((band.x + 12, band.y + 1)));
    draw(&mut app, &mut terminal);
    app.handle_mouse(release((band.x + 12, band.y + 1)));

    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "hello world"),
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
}

#[test]
fn test_drag_stops_on_the_character_under_the_pointer() {
    // The pointer selects the character it rests on: stopping on the final
    // `d` copies through it (and the highlight covers it), while a click
    // stays zero-width (covered by the click test above).
    let mut app = app_with_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;

    app.handle_mouse(press((band.x + 2, band.y + 1)));
    draw(&mut app, &mut terminal);
    // Column 12 is the `d` of "hello world" (text starts at column 2).
    app.handle_mouse(drag((band.x + 12, band.y + 1)));
    draw(&mut app, &mut terminal);
    assert_eq!(
        reversed_cells(&terminal),
        (band.x + 2..band.x + 13)
            .map(|x| (x, band.y + 1))
            .collect::<Vec<_>>(),
        "the highlighted span ends after the character under the pointer"
    );

    app.handle_mouse(release((band.x + 12, band.y + 1)));
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, "hello world"),
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
}

#[test]
fn test_release_at_the_bottom_rearms_follow() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    assert!(app.chat.is_at_bottom(), "pinned to the bottom on load");
    let band = app.chat.geometry().area;

    app.handle_mouse(press((band.x + 2, band.y + 1)));
    app.handle_mouse(drag((band.x + 6, band.y + 3)));
    draw(&mut app, &mut terminal);
    app.handle_mouse(release((band.x + 6, band.y + 3)));

    assert!(
        app.chat.is_at_bottom(),
        "a release at the bottom edge re-arms the follow state"
    );
}

#[test]
fn test_release_above_the_bottom_keeps_the_reading_state() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    app.chat.scroll_up(5);
    draw(&mut app, &mut terminal);
    assert!(!app.chat.is_at_bottom());
    let reading = app.chat.scroll_position();
    let band = app.chat.geometry().area;

    app.handle_mouse(press((band.x + 2, band.y + 1)));
    app.handle_mouse(drag((band.x + 8, band.y + 4)));
    draw(&mut app, &mut terminal);
    app.handle_mouse(release((band.x + 8, band.y + 4)));

    assert!(!app.chat.is_at_bottom(), "still above the bottom edge");
    assert_eq!(app.chat.scroll_position(), reading);
}

#[test]
fn test_structural_change_aborts_the_selection() {
    /// One abort rule: a label for the assertion messages plus the
    /// mutation that makes the structure move under the anchor.
    type AbortCase = (&'static str, Box<dyn Fn(&mut App)>);

    // Width (resize), cell count, pending count and a full content rebuild
    // all shift virtual rows under the anchor — the selection must be
    // dropped, not pointed at different text.
    let cases: Vec<AbortCase> = vec![
        (
            "width",
            Box::new(|app: &mut App| {
                // Handled by drawing into a differently sized terminal.
                let _ = app;
            }),
        ),
        (
            "cell count",
            Box::new(|app: &mut App| {
                app.chat.push(ChatCell::SystemMessage("new cell".into()));
            }),
        ),
        (
            "pending count",
            Box::new(|app: &mut App| {
                app.chat.push_pending("req-1".into(), "queued".into());
            }),
        ),
        (
            "rebuild",
            Box::new(|app: &mut App| {
                // Session switch / compaction / rewind replay: same cell
                // count, completely different content.
                let text = (0..30)
                    .map(|i| format!("other-{i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                app.chat.clear();
                app.chat.push(ChatCell::UserMessage(text));
            }),
        ),
    ];

    for (name, mutate) in cases {
        let mut app = app_with_tall_message();
        let mut terminal = test_terminal(40, 12);
        draw(&mut app, &mut terminal);
        let band = app.chat.geometry().area;
        app.handle_mouse(press((band.x + 2, band.y + 1)));
        app.handle_mouse(drag((band.x + 6, band.y + 2)));
        assert!(app.selection.is_press_active(), "{name}: drag in flight");

        if name == "width" {
            // A resize: the frame whose width no longer matches the one the
            // anchor was taken with is the frame that aborts — assert on
            // *that* buffer, not on the stale one from before the press.
            let mut wider = test_terminal(60, 12);
            draw(&mut app, &mut wider);
            assert!(
                reversed_cells(&wider).is_empty(),
                "{name}: the aborted frame must not paint a highlight"
            );
            // Re-draw the original width: the abort has already happened,
            // so the highlight stays gone.
            draw(&mut app, &mut terminal);
        } else {
            mutate(&mut app);
            draw(&mut app, &mut terminal);
        }

        assert!(
            !app.selection.is_press_active(),
            "{name}: the selection must be aborted"
        );
        assert!(
            reversed_cells(&terminal).is_empty(),
            "{name}: the highlight must be cleared"
        );
        assert_eq!(
            app.handle_mouse(release((band.x + 6, band.y + 2))),
            MouseOutcome::Ignored,
            "{name}: a release after the abort is a no-op"
        );
        assert!(
            app.drain_intents().is_empty(),
            "{name}: nothing may be copied"
        );
    }
}

#[test]
fn test_focus_loss_aborts_the_selection() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;

    app.handle_mouse(press((band.x + 2, band.y + 1)));
    app.handle_mouse(drag((band.x + 6, band.y + 3)));
    assert!(app.selection.is_press_active());

    // The release of a drag that left the window never arrives.
    app.cancel_selection();
    assert!(!app.selection.is_press_active());
    assert!(
        app.chat.is_at_bottom(),
        "the frozen follow state is restored"
    );
    draw(&mut app, &mut terminal);
    assert!(reversed_cells(&terminal).is_empty());
}

#[test]
fn test_edge_autoscroll_steps_one_line_and_stops_at_the_edge() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    // Reading history: the view has room to move in both directions.
    app.chat.scroll_up(10);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    assert!(!app.chat.is_at_bottom());

    // Drag onto the band's last row → downward auto-scroll arms.
    app.handle_mouse(press((band.x + 2, band.y + 2)));
    app.handle_mouse(drag((band.x + 2, band.bottom() - 1)));
    assert_eq!(app.selection.auto_scroll(), 1);
    let before = app.chat.scroll_position();
    assert!(app.tick_selection_autoscroll(), "a step redraws");
    assert_eq!(app.chat.scroll_position(), before + 1, "one line per tick");
    assert_eq!(app.selection.auto_scroll(), 1, "still armed");
    assert!(
        !app.chat.is_at_bottom(),
        "the drag keeps the follow state frozen while it steps"
    );

    // Reaching the content edge stops the step instead of spinning.
    app.chat.scroll_down(1000, app.geometry.chat_height());
    let bottom = app.chat.scroll_position();
    assert!(!app.tick_selection_autoscroll(), "nothing left to scroll");
    assert_eq!(app.chat.scroll_position(), bottom, "the view stays put");
    assert_eq!(app.selection.auto_scroll(), 0, "the timer is disarmed");
    assert!(
        app.selection_autoscroll_at.is_none(),
        "the deadline is disarmed"
    );

    // Drag onto the band's first row → upward auto-scroll arms.
    app.handle_mouse(drag((band.x + 2, band.y)));
    assert_eq!(app.selection.auto_scroll(), -1);
    let before = app.chat.scroll_position();
    assert!(app.tick_selection_autoscroll());
    assert_eq!(app.chat.scroll_position(), before - 1);

    // And it stops at the top edge too.
    app.chat.jump_top();
    assert!(!app.tick_selection_autoscroll());
    assert_eq!(app.selection.auto_scroll(), 0);
}

#[test]
fn test_edge_autoscroll_extends_the_selection_with_the_rows() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    app.chat.scroll_up(10);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;

    app.handle_mouse(press((band.x + 2, band.y + 4)));
    app.handle_mouse(drag((band.x + 2, band.bottom() - 1)));
    let before = app.selection.bounds().expect("a drag produced bounds");
    let (anchor_row, focus_before) = (before.0.row, before.1.row);

    assert!(app.tick_selection_autoscroll());
    let after = app.selection.bounds().expect("still selected");
    let focus_after = after.1.row;
    assert!(
        focus_after > focus_before,
        "the focus follows the rows scrolling by: {focus_before} -> {focus_after}"
    );
    assert_eq!(
        after.0.row, anchor_row,
        "the anchor is content-anchored and does not move"
    );

    // Moving the pointer away from the edge disarms the step.
    app.handle_mouse(drag((band.x + 4, band.y + 3)));
    assert_eq!(app.selection.auto_scroll(), 0);

    // Releasing stops it for good.
    app.handle_mouse(drag((band.x + 4, band.bottom() - 1)));
    assert_eq!(app.selection.auto_scroll(), 1);
    app.handle_mouse(release((band.x + 4, band.bottom() - 1)));
    assert_eq!(app.selection.auto_scroll(), 0);
    assert!(!app.selection.is_press_active());
}

#[test]
fn test_hover_and_horizontal_wheel_stay_inert_during_a_drag() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    app.handle_mouse(press((band.x + 2, band.y + 1)));
    app.handle_mouse(drag((band.x + 6, band.y + 2)));

    let offset = app.chat.scroll_position();
    let focus = app.selection.bounds();
    for kind in [
        crossterm::event::MouseEventKind::Moved,
        crossterm::event::MouseEventKind::ScrollLeft,
        crossterm::event::MouseEventKind::ScrollRight,
    ] {
        assert_eq!(
            app.handle_mouse(mouse_at(kind, (band.x + 9, band.y + 4))),
            MouseOutcome::Ignored
        );
    }
    assert_eq!(app.chat.scroll_position(), offset);
    assert_eq!(app.selection.bounds(), focus, "the selection is untouched");
}

// ── Scrollbar × text selection × links: three gestures, one band ────

/// A press on the bar is the bar's: it drags the view, starts no selection
/// and copies nothing. The selection machinery is back in charge as soon as
/// the drag is over.
#[test]
fn test_press_on_the_bar_drags_the_bar_and_starts_no_selection() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    let geom = app.scrollbar_geometry().expect("content overflows");
    assert_eq!(
        app.geometry.chat_band().right() - band.right(),
        scrollbar::SCROLLBAR_GUTTER,
        "the content area is the band minus the gutter"
    );
    assert_eq!(
        geom.column,
        app.geometry.chat_band().right() - 1,
        "the bar owns the band's last column, inside that gutter"
    );
    assert!(app.chat.is_at_bottom(), "pinned to the bottom on load");

    // Press the top of the track: the view jumps, the bar is grabbed, and
    // the drag freeze / copy path is never entered.
    assert_eq!(
        app.handle_mouse(press((geom.column, geom.track_top))),
        MouseOutcome::Immediate
    );
    assert!(app.scrollbar.dragging);
    assert_eq!(app.chat.scroll_position(), 0);
    assert!(
        !app.chat.is_at_bottom(),
        "a track click leaves the follow state"
    );
    assert!(
        !app.selection.is_press_active(),
        "a bar press must not start a selection"
    );
    draw(&mut app, &mut terminal);
    assert!(
        reversed_cells(&terminal).is_empty(),
        "a bar press paints no highlight"
    );

    // The drag keeps the thumb under the pointer (proportional), and no
    // selection is involved either way.
    assert_eq!(
        app.handle_mouse(drag((geom.column, geom.track_bottom))),
        MouseOutcome::Coalesced
    );
    assert_eq!(app.chat.scroll_position(), geom.max_scroll());
    assert!(app.chat.is_at_bottom(), "the bottom edge re-arms follow");
    assert!(!app.selection.is_press_active());

    // Release ends the drag: nothing was selected, so nothing is copied.
    assert_eq!(
        app.handle_mouse(release((geom.column, geom.track_bottom))),
        MouseOutcome::Immediate
    );
    assert!(!app.scrollbar.dragging);
    assert!(app.drain_intents().is_empty(), "a bar drag copies nothing");

    // The band is the selection's again right after the bar let go.
    assert_eq!(
        app.handle_mouse(press((band.x + 2, band.y + 1))),
        MouseOutcome::Immediate
    );
    assert!(app.selection.is_press_active(), "the next press selects");
    assert!(!app.scrollbar.dragging);
}

/// A label of digits that fills the assistant content width **exactly**: the
/// band these tests draw at (40 columns) minus the scrollbar gutter, minus
/// the two-column `⦁ ` prefix, one cell per grapheme.
///
/// Derived from the gutter on purpose — the tests that use it are about the
/// *edges* of the content area, so they have to follow that knob instead of
/// pinning a column count (a hardcoded length silently stops reaching the
/// edge the moment the gutter changes).
fn content_filling_label() -> String {
    let width = 40usize - scrollbar::SCROLLBAR_GUTTER as usize - 2;
    (0..width)
        .map(|i| char::from(b'0' + (i % 10) as u8))
        .collect()
}

/// A link label that fills the assistant content width exactly, so the
/// link's last column is the last column the content owns — the gutter and
/// the bar share the row right of it. The label sits on a row of its own
/// (`see ` plus the label is wider than the content).
///
/// Filler above overflows the band (the bar needs something to scroll) and
/// the link cell is last, so the row stays on screen while the view is
/// pinned to the bottom.
fn app_with_link_at_the_content_edge() -> App {
    let label = content_filling_label();
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    for i in 0..4 {
        app.chat.push(ChatCell::UserMessage(format!("filler {i}")));
    }
    app.chat.push(ChatCell::AssistantMessage(format!(
        "see [{label}](https://example.com)"
    )));
    app
}

/// The bar takes the press at its own column. It sits in the gutter, a
/// couple of blank columns right of the content, and the dispatch order
/// that makes the bar win (it sees the event before the chat does) still
/// has to hold.
#[test]
fn test_press_on_the_bar_never_opens_the_link_beside_it() {
    let mut app = app_with_link_at_the_content_edge();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let content = app.chat.geometry().area;
    let geom = app.scrollbar_geometry().expect("content overflows");
    let (row, links) = app
        .chat
        .frame_links()
        .first()
        .expect("frame has a link")
        .clone();
    let link = &links[0];
    assert_eq!(
        link.end - 1,
        content.right() - 1,
        "the link must stop at the content's last column"
    );
    assert!(
        geom.column > content.right(),
        "the bar's column is right of the content, inside the gutter"
    );

    assert_eq!(
        app.handle_mouse(press((geom.column, row))),
        MouseOutcome::Immediate
    );
    assert!(app.scrollbar.dragging, "the press belongs to the bar");
    assert!(
        app.mouse_link.is_none(),
        "a bar press must not record the link underneath"
    );
    assert_eq!(
        app.handle_mouse(release((geom.column, row))),
        MouseOutcome::Immediate
    );
    assert!(!app.scrollbar.dragging);
    assert!(
        app.drain_intents().is_empty(),
        "the bar drag opens nothing and copies nothing"
    );
}

/// …while the content's own last column is still the link's: the bar claims
/// the gutter, not the content — a click at the content edge opens the link
/// instead of being swallowed by the bar's neighbourhood.
#[test]
fn test_click_at_the_content_edge_still_opens_the_link() {
    let mut app = app_with_link_at_the_content_edge();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let content = app.chat.geometry().area;
    let geom = app.scrollbar_geometry().expect("content overflows");
    let (row, links) = app
        .chat
        .frame_links()
        .first()
        .expect("frame has a link")
        .clone();
    let link = &links[0];

    // The last column the content owns — the gutter starts right of it.
    let at = (content.right() - 1, row);
    assert!(at.0 < geom.column, "the content edge is left of the bar");
    assert!(
        link.start <= at.0 && link.end - 1 >= at.0,
        "inside the link"
    );
    assert_eq!(app.handle_mouse(press(at)), MouseOutcome::Immediate);
    assert!(
        !app.scrollbar.dragging,
        "off the bar, the bar stays out of it"
    );
    assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Immediate);
    match app.drain_intents().as_slice() {
        [AppIntent::OpenLink(target)] => assert_eq!(target, "https://example.com"),
        other => panic!("expected exactly one open intent, got {other:?}"),
    }
}

/// A row that fills the content area is copied whole, bar or no bar: the
/// copy bound is the content width, so the last column of the content must
/// not be dropped. (A bound that stopped one column early would silently
/// lose that character.)
#[test]
fn test_last_column_is_selectable_when_the_content_fits() {
    let label = content_filling_label();
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    app.chat.push(ChatCell::AssistantMessage(label.to_string()));
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    assert!(
        app.scrollbar_geometry().is_none(),
        "one row of content fits the band"
    );
    let band = app.chat.geometry().area;
    let row = band.y;

    // Two columns in: the `⦁ ` cell prefix is chrome, the label is not.
    assert_eq!(
        app.handle_mouse(press((band.x + 2, row))),
        MouseOutcome::Immediate
    );
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.handle_mouse(drag((band.right() - 1, row))),
        MouseOutcome::Coalesced
    );
    draw(&mut app, &mut terminal);
    assert_eq!(
        app.handle_mouse(release((band.right() - 1, row))),
        MouseOutcome::Immediate
    );
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => assert_eq!(text, &label),
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
}

/// A multi-row drag copies every row only up to the bar's column: the bar's
/// glyph must not land in the paste, and neither must the padding spaces in
/// front of it (`trim_end` stops at the glyph, not at the content).
#[test]
fn a_multi_row_copy_ends_before_the_bar_column() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    let geom = app.scrollbar_geometry().expect("content overflows");

    app.handle_mouse(press((band.x + 2, band.y + 1)));
    draw(&mut app, &mut terminal);
    // Drag down over three rows, ending on the bar's own column.
    app.handle_mouse(drag((geom.column, band.y + 3)));
    draw(&mut app, &mut terminal);
    app.handle_mouse(release((geom.column, band.y + 3)));

    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => {
            assert!(text.contains("line-"), "copied the chat rows: {text:?}");
            assert!(!text.contains('│'), "the bar must not be copied: {text:?}");
            for line in text.lines() {
                assert_eq!(line, line.trim_end(), "trailing padding in {line:?}");
            }
        }
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
}

/// …and the highlight leaves that column alone too, on every selected row
/// (not just on the row the drag started or ended in).
#[test]
fn a_multi_row_highlight_never_inverts_the_bar_column() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    let geom = app.scrollbar_geometry().expect("content overflows");

    app.handle_mouse(press((band.x + 2, band.y + 1)));
    draw(&mut app, &mut terminal);
    app.handle_mouse(drag((geom.column, band.y + 3)));
    draw(&mut app, &mut terminal);

    let painted = reversed_cells(&terminal);
    assert!(
        painted.iter().any(|&(x, _)| x > band.x + 2),
        "the drag painted something: {painted:?}"
    );
    assert!(
        !painted.iter().any(|&(x, _)| x == geom.column),
        "the bar's column must stay out of the highlight: {painted:?}"
    );
}

/// A press beside the bar belongs to the chat band: it starts a drag
/// selection and never moves the view. A drag that wanders over the bar's
/// column is still the selection's — the bar only claims presses that land
/// on the bar itself — and the copied span stops short of the bar, so no
/// bar glyph can end up in the text.
#[test]
fn test_press_beside_the_bar_selects_and_leaves_the_bar_alone() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    let geom = app.scrollbar_geometry().expect("content overflows");
    let row = band.y + 1;
    let offset = app.chat.scroll_position();

    assert_eq!(
        app.handle_mouse(press((band.x + 2, row))),
        MouseOutcome::Immediate,
        "two columns in is chat content"
    );
    assert!(app.selection.is_press_active());
    assert!(!app.scrollbar.dragging, "the press is not a bar drag");
    assert_eq!(
        app.chat.scroll_position(),
        offset,
        "starting a selection never scrolls"
    );
    draw(&mut app, &mut terminal);

    // The drag crosses the bar column: still the selection's flood, and
    // still no scrolling.
    assert_eq!(
        app.handle_mouse(drag((geom.column, row))),
        MouseOutcome::Coalesced
    );
    assert!(!app.scrollbar.dragging);
    assert_eq!(app.chat.scroll_position(), offset, "the drag never scrolls");
    draw(&mut app, &mut terminal);

    // Release copies the span — the bar's own column is chrome, so the
    // text never picks up its glyph.
    assert_eq!(
        app.handle_mouse(release((geom.column, row))),
        MouseOutcome::Immediate
    );
    match app.drain_intents().as_slice() {
        [AppIntent::CopyToClipboard(text)] => {
            assert!(
                text.contains("line-") || text.contains("msg"),
                "copied from the chat band: {text:?}"
            );
            assert!(
                !text.contains('│'),
                "a bar glyph must not be copied: {text:?}"
            );
        }
        other => panic!("expected exactly one clipboard intent, got {other:?}"),
    }
}

#[tokio::test]
async fn test_selection_autoscroll_timer_fires_only_when_armed() {
    // Armed with a deadline: the arm completes on its own (the run loop
    // then steps the view one line).
    tokio::select! {
        () = selection_autoscroll_tick(Some(std::time::Instant::now())) => {}
        () = tokio::time::sleep(SELECTION_AUTOSCROLL_DELAY * 20) => {
            panic!("an armed timer must fire");
        }
    }

    // Disarmed: the arm never completes on its own — the loop stays parked
    // instead of spinning at the tick rate.
    let parked = tokio::time::timeout(
        SELECTION_AUTOSCROLL_DELAY * 3,
        selection_autoscroll_tick(None),
    )
    .await;
    assert!(
        parked.is_err(),
        "a disarmed timer must not wake the event loop"
    );
}

#[tokio::test]
async fn test_selection_autoscroll_deadline_survives_busy_iterations() {
    // Regression: `select!` rebuilds this arm on every loop iteration, so
    // a *relative* sleep would restart on each incoming event and, with
    // streaming events arriving every few milliseconds, would never fire.
    // The absolute deadline must be reached regardless of how often the
    // loop turns over.
    let deadline = std::time::Instant::now() + SELECTION_AUTOSCROLL_DELAY;
    let mut next = Some(deadline);
    let mut fires = 0;
    while std::time::Instant::now() < deadline + SELECTION_AUTOSCROLL_DELAY * 2 {
        tokio::select! {
            () = selection_autoscroll_tick(next) => {
                fires += 1;
                next = Some(std::time::Instant::now() + SELECTION_AUTOSCROLL_DELAY);
            }
            // An event arrives far faster than the tick interval: the arm
            // is dropped and rebuilt with the *same* deadline.
            () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
        }
    }
    assert!(
        fires >= 2,
        "the absolute deadline must keep firing despite frequent loop iterations, got {fires}"
    );
}
