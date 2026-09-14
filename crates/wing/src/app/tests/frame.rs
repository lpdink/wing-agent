//! The frame contract and the pointer priority — the two structures this
//! change made explicit.
//!
//! * `draw` records the geometry the input paths read; these tests render a
//!   real frame (`TestBackend`) and check the record against the layout, the
//!   widgets and the derived scrollbar geometry;
//! * the pointer's order of ownership is declared once
//!   ([`crate::app::mouse::POINTER_PRIORITY`]); these tests pin that order on a
//!   frame that has every region at once;
//! * the selection's invalidation rule is one predicate; these tests check that
//!   the frame and the release both read *that* predicate.

use super::support::*;
use crate::app::mouse::POINTER_PRIORITY;
use crate::app::mouse::PointerOwner;
use crate::app::*;
use crate::ui::chat_view::ChatCell;
use crate::ui::scrollbar;
use crate::ui::selection::SelectionRegion;

/// A frame with every region at once: a chat that overflows (so the overlay
/// scrollbar exists) and a draft in the composer.
fn app_with_bar_and_draft() -> App {
    let mut app = app_with_draft("hello world");
    app.chat.push(ChatCell::AssistantMessage(
        (0..40)
            .map(|i| format!("msg {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    app
}

#[test]
fn test_draw_records_the_frame_it_laid_out() {
    let mut app = app_with_bar_and_draft();
    let mut terminal = test_terminal(80, 24);
    draw(&mut app, &mut terminal);

    // Whole frame: the terminal it was drawn into.
    assert_eq!(app.geometry.width(), 80);

    // Layout, as an independent fact: status bar row 0, chat band from row 1,
    // then the info separator, then the composer — nothing else claims rows.
    let band = app.geometry.chat_band();
    let composer = app.geometry.composer_rect();
    assert_eq!(band.y, 1, "the band starts right below the status bar");
    assert_eq!(band.x, 0);
    assert_eq!(band.width, 80, "the band spans the terminal");
    assert_eq!(
        composer.y,
        band.bottom() + 1,
        "the info separator is the row right under the band (bottom is exclusive),          the composer the one below it"
    );
    assert_eq!(
        band.height + 1 + 1 + composer.height,
        24,
        "status + band + separator + composer fill the terminal"
    );

    // Chat band vs. the rect the chat widget recorded: the content area plus
    // the gutter — two independent recorders agreeing on one frame.
    let content = app.chat.geometry().area;
    assert_eq!(band.x, content.x);
    assert_eq!(band.y, content.y);
    assert_eq!(band.height, content.height);
    assert_eq!(band.width - content.width, scrollbar::SCROLLBAR_GUTTER);
    assert_eq!(
        app.geometry.chat_height(),
        band.height as usize,
        "the band's height is what the page keys and the wheel step against"
    );

    // The bar is derived over that band: its column is the terminal's last one
    // and its track spans the band's rows.
    let geom = app.scrollbar_geometry().expect("content overflows");
    assert_eq!(geom.column, 79, "the terminal's last column");
    assert_eq!(geom.track_top, band.y);
    assert_eq!(geom.track_bottom, band.bottom() - 1);

    // Composer: the contract's block *is* the rect the widget recorded for
    // itself — the claim and the pointer mapping read the same screen.
    assert_eq!(composer, app.input.rendered_area());
    assert_eq!(composer.width, 80);
    for (x, y) in [
        (composer.x, composer.y),
        (composer.right() - 1, composer.bottom() - 1),
    ] {
        assert!(app.composer_contains(x, y), "({x},{y}) is the composer's");
    }
    assert!(!app.composer_contains(composer.right(), composer.y));
    assert!(!app.composer_contains(composer.x, composer.bottom()));

    // The two claimable regions are disjoint — the priority chain says a press
    // is offered to one of them, never to a position that is both.
    assert!(!band.intersects(composer));
}

#[test]
fn test_geometry_is_rebuilt_by_every_frame() {
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let narrow = app.geometry.chat_band();
    assert_eq!(app.geometry.width(), 40);

    // A resize: the next frame records the new terminal and the band with it.
    let mut wide = test_terminal(100, 24);
    draw(&mut app, &mut wide);
    assert_eq!(app.geometry.width(), 100);
    let wide_band = app.geometry.chat_band();
    assert!(
        wide_band.width > narrow.width,
        "the band follows the terminal: {narrow:?} → {wide_band:?}"
    );
    assert!(wide_band.height > narrow.height);

    // A taller composer eats into the band — the layout is the frame's, not a
    // cached guess.
    app.input.set_text("one\ntwo\nthree");
    draw(&mut app, &mut wide);
    let shorter = app.geometry.chat_band();
    assert!(
        shorter.height < wide_band.height,
        "the composer grew, so the band shrank: {wide_band:?} → {shorter:?}"
    );
    assert_eq!(shorter.y, wide_band.y, "still right below the status bar");
    assert_eq!(app.geometry.chat_height(), shorter.height as usize);
    assert!(
        app.input.rendered_area().y > shorter.bottom(),
        "the composer sits below the band"
    );
    assert_eq!(
        app.geometry.width(),
        100,
        "…and the frame is still the one that was drawn"
    );
}

#[test]
fn test_unframed_geometry_claims_nothing_and_keeps_the_editor_width() {
    // Nothing has been drawn: no region can be claimed — a press *and* a drag
    // and a release must all be no-ops, exactly like the unwritten frame.
    let mut app = app_with_bar_and_draft();
    assert!(app.pointer_owner(2, 2).is_none());
    assert!(app.pointer_owner(5, 11).is_none());
    assert_eq!(app.handle_mouse(press((2, 2))), MouseOutcome::Ignored);
    assert_eq!(app.handle_mouse(drag((2, 2))), MouseOutcome::Ignored);
    assert_eq!(app.handle_mouse(release((2, 2))), MouseOutcome::Ignored);
    assert!(!app.selection.is_press_active());
    assert!(app.drain_intents().is_empty());

    // The pre-first-frame contract, pinned: the composer's editor keeps the
    // canonical width it had before the first frame (the value the replaced
    // field started with) …
    assert_eq!(app.geometry.width(), 80);
    // … while the band is empty (the replaced `visible_height` started at 20 —
    // deliberately not carried over, see `FrameGeometry::default`) and the
    // composer accepts nothing.
    assert_eq!(app.geometry.chat_height(), 0);
    assert_eq!(app.geometry.chat_band(), ratatui::layout::Rect::default());
    assert!(!app.composer_contains(0, 11));
    assert!(app.scrollbar_geometry().is_none(), "no band, no bar");

    // What the empty band *means* for the paths that read its height. Only the
    // tests can get here: `run_app` draws before it reads an event, and with no
    // content yet the two implementations agree anyway (max_scroll == 0).
    let mut app = test_app();
    app.chat.last_total = 100;
    app.chat.scroll_offset = 78;
    app.chat.scroll_up(0); // reading state, offset unchanged

    // The wheel: up is height-independent, down steps its three lines and
    // clamps at the *content height* (`max_scroll = last_total - 0`), not at
    // the 20-row band the replaced field assumed.
    app.handle_mouse(wheel_up());
    assert_eq!(app.chat.scroll_position(), 75);
    app.handle_mouse(wheel_down());
    assert_eq!(app.chat.scroll_position(), 78);
    app.chat.scroll_offset = 98;
    app.handle_mouse(wheel_down());
    assert_eq!(
        app.chat.scroll_position(),
        100,
        "clamped at the content height"
    );
    assert!(
        app.chat.is_at_bottom(),
        "the zero-tall bottom edge is the content end"
    );

    // The page keys: `page = chat_height() - 2` saturates to 0, so a page step
    // moves nothing in either direction (the follow state is still left behind
    // — that part does not depend on the height).
    let mut app = test_app();
    app.chat.last_total = 100;
    app.chat.scroll_offset = 90;
    app.chat.scroll_up(0);
    app.handle_key(key(crossterm::event::KeyCode::PageUp));
    assert_eq!(
        app.chat.scroll_position(),
        90,
        "a zero-height page steps nothing"
    );
    app.handle_key(key(crossterm::event::KeyCode::PageDown));
    assert_eq!(app.chat.scroll_position(), 90, "…in either direction");
    assert!(
        !app.chat.is_at_bottom(),
        "and it does not re-arm follow either"
    );
}

#[test]
fn test_pointer_priority_is_declared_once_and_in_order() {
    // The declaration itself: the array order **is** the priority order.
    assert_eq!(
        POINTER_PRIORITY,
        [
            PointerOwner::Scrollbar,
            PointerOwner::Composer,
            PointerOwner::Chat
        ]
    );

    let mut app = app_with_bar_and_draft();
    let mut terminal = test_terminal(80, 24);
    draw(&mut app, &mut terminal);
    let band = app.geometry.chat_band();
    let content = app.chat.geometry().area;
    let geom = app.scrollbar_geometry().expect("content overflows");
    let composer = app.input.rendered_area();

    // 1. The bar's own column, inside the band's rows: the bar wins over the
    //    band it overprints — the position *is* inside the band rect (which
    //    includes the gutter) and still resolves to the bar, because the
    //    Scrollbar rung is declared above the Chat one.
    assert!(
        band.contains(ratatui::layout::Position::new(geom.column, geom.track_top)),
        "the bar's column belongs to the band rect: {band:?}"
    );
    assert_eq!(
        app.pointer_owner(geom.column, geom.track_top),
        Some(PointerOwner::Scrollbar)
    );
    assert_eq!(
        app.pointer_chain(geom.column, geom.track_top)
            .collect::<Vec<_>>(),
        vec![PointerOwner::Scrollbar],
        "the chain holds the claiming owners, in declaration order"
    );
    // 2. The band elsewhere: the chat (its content rect, gutter excluded).
    assert_eq!(
        app.pointer_owner(content.x, content.y),
        Some(PointerOwner::Chat)
    );
    assert_eq!(
        app.pointer_owner(content.right() - 1, content.bottom() - 1),
        Some(PointerOwner::Chat),
        "the content's last column is still the chat's, not the bar's"
    );
    // The gutter's blank column belongs to nobody: the bar owns only its own
    // column, and the chat only its content rect.
    assert_eq!(app.pointer_owner(content.right(), content.y), None);
    assert_eq!(
        app.pointer_chain(content.right(), content.y).count(),
        0,
        "nobody claims the gutter's blank column"
    );
    // 3. The composer block, below the band.
    assert_eq!(
        app.pointer_owner(composer.x + 1, composer.y),
        Some(PointerOwner::Composer)
    );
    // 4. Outside every region (the status bar row): nobody.
    assert_eq!(app.pointer_owner(band.x, 0), None);

    // The press path consumes exactly these verdicts: the bar's position grabs
    // the bar (and starts no selection), the composer's position starts a
    // composer selection.
    assert_eq!(
        app.handle_mouse(press((geom.column, geom.track_top))),
        MouseOutcome::Immediate
    );
    assert!(app.scrollbar.dragging);
    assert!(!app.selection.is_press_active());
    app.handle_mouse(release((geom.column, geom.track_top)));

    assert_eq!(
        app.handle_mouse(press((composer.x + 1, composer.y))),
        MouseOutcome::Immediate
    );
    assert_eq!(app.selection.region(), Some(SelectionRegion::Composer));
}

#[test]
fn test_at_most_one_owner_claims_any_position() {
    // The invariant behind "the order only decides ties": on the frame that
    // holds every region at once, no position is claimed twice. If a future
    // claim starts overlapping another (say the chat band claiming the gutter),
    // this test goes red and the declaration order becomes load-bearing — which
    // is exactly what it is declared for.
    let mut app = app_with_bar_and_draft();
    let mut terminal = test_terminal(80, 24);
    draw(&mut app, &mut terminal);
    assert!(
        app.scrollbar_geometry().is_some(),
        "the bar must exist for its claim to take part"
    );

    let mut claimed = 0usize;
    for row in 0..24 {
        for column in 0..80 {
            let owners: Vec<PointerOwner> = app.pointer_chain(column, row).collect();
            assert!(
                owners.len() <= 1,
                "({column},{row}) is claimed by {owners:?} — the priority order \
                 would have to decide, and the chain is documented as disjoint"
            );
            claimed += owners.len();
        }
    }
    assert!(
        claimed >= 4,
        "the frame must claim something (band, bar and composer rows), got {claimed}"
    );
}

#[test]
fn test_the_composer_claim_reads_the_modal_lane_guard() {
    let mut app = app_with_draft("hello world");
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    let at = (composer.x + 1, composer.y);
    let band = app.geometry.chat_band();
    assert_eq!(app.pointer_owner(at.0, at.1), Some(PointerOwner::Composer));

    // A keyboard-owning modal: the same position drops out of the chain …
    app.ask_selections
        .push_back(crate::ui::ask_select::AskSelection::new(
            "ask-legacy".into(),
            vec!["one".into(), "two".into()],
        ));
    draw(&mut app, &mut terminal);
    assert!(
        app.composer_pointer_blocked(),
        "the guard is the modal lane's, not re-derived here"
    );
    assert_eq!(app.pointer_owner(at.0, at.1), None);
    // … while the chat band (and the wheel) stay the chat's.
    assert_eq!(
        app.pointer_owner(band.x + 1, band.y + 1),
        Some(PointerOwner::Chat)
    );
    assert_eq!(
        app.handle_mouse(press((at.0, at.1))),
        MouseOutcome::Ignored,
        "a press under a modal starts nothing"
    );
    assert!(!app.selection.is_press_active());
}

#[test]
fn test_the_invalidation_rule_is_one_predicate() {
    // Chat trigger — a structural change lands between two frames: the
    // predicate sees it, and the frame acts on it.
    let mut app = app_with_tall_message();
    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    app.handle_mouse(press((band.x + 2, band.y + 1)));
    app.handle_mouse(drag((band.x + 6, band.y + 2)));
    assert!(
        !app.selection
            .needs_abort(|region| app.selection_fingerprint(region)),
        "an untouched frame matches the recorded fingerprint"
    );

    app.chat.push(ChatCell::SystemMessage("new cell".into()));
    assert!(
        app.selection
            .needs_abort(|region| app.selection_fingerprint(region)),
        "the predicate sees the stale fingerprint"
    );
    draw(&mut app, &mut terminal);
    assert!(
        !app.selection.is_press_active(),
        "…and the frame aborts the gesture"
    );
    assert!(app.drain_intents().is_empty(), "nothing is copied");

    // Chat trigger — the same change landing in the release's own loop
    // iteration (no frame in between): the release reads the same predicate,
    // and copies nothing, where the normal path would copy the span. The frame
    // after the press is what snapshots the rows the copy would read.
    let mut app = app_with_tall_message();
    draw(&mut app, &mut terminal);
    app.handle_mouse(press((band.x + 2, band.y + 1)));
    app.handle_mouse(drag((band.x + 6, band.y + 2)));
    draw(&mut app, &mut terminal);
    app.chat.push(ChatCell::SystemMessage("new cell".into()));
    assert_eq!(
        app.handle_mouse(release((band.x + 6, band.y + 2))),
        MouseOutcome::Immediate,
        "the release reads the same predicate"
    );
    assert!(!app.selection.is_press_active());
    assert!(
        app.drain_intents().is_empty(),
        "a stale gesture copies nothing"
    );

    // Chat non-trigger — streamed text growth rewrites a cell without moving
    // any row, so the fingerprint stays equal.
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    app.chat.push(ChatCell::AssistantMessage(
        (0..30)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    draw(&mut app, &mut terminal);
    let band = app.chat.geometry().area;
    app.handle_mouse(press((band.x + 2, band.y + 1)));
    app.handle_mouse(drag((band.x + 6, band.y + 2)));
    app.chat
        .append_to_last_assistant("\nand more streaming text");
    assert!(
        !app.selection
            .needs_abort(|region| app.selection_fingerprint(region)),
        "streaming growth must not abort the drag"
    );

    // Composer — the other variant of the same predicate: an edit moves the
    // text under the anchor, nothing else does.
    let mut app = app_with_draft("hello world");
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    app.handle_mouse(press((composer.x + 3, composer.y)));
    app.handle_mouse(drag((composer.x + 7, composer.y)));
    assert!(
        !app.selection
            .needs_abort(|region| app.selection_fingerprint(region)),
        "an untouched draft is stable"
    );
    app.input.set_text("changed");
    assert!(
        app.selection
            .needs_abort(|region| app.selection_fingerprint(region)),
        "an edit invalidates a draft-anchored gesture"
    );
}
