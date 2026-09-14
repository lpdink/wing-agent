//! Modal ownership lane tests — keyboard routing and the Escape ladder.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::modal::ChatScrollAction;
use crate::app::modal::KeyRoute;
use crate::app::modal::ModalOwner;
use crate::app::*;
use crate::protocol::AskQuestion;
use crate::ui::chat_view::ChatCell;
use crate::ui::input_area::helpers::PREFIX_WIDTH;
use crate::ui::popup::command::SessionCandidate;

#[test]
fn test_ask_panel_key_flow_submits_header_answer() {
    // Arrow keys move the cursor, Enter advances to the confirm page and
    // Submit sends the `header: answer` reply addressed by tool_call_id.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    let mut app = test_app();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![],
        vec![serde_json::json!({
            "type": "ask",
            "tool_call_id": "ask-9",
            "questions": [{
                "id": "theme",
                "header": "配色",
                "question": "which theme?",
                "options": [{"label": "浅色"}, {"label": "深色"}],
            }],
        })],
        None,
    ));
    assert_eq!(app.ask_panels.len(), 1);

    app.handle_key(key(KeyCode::Down)); // cursor → 深色
    app.handle_key(key(KeyCode::Enter)); // advance → confirm page
    app.handle_key(key(KeyCode::Enter)); // Submit

    let sent = app.drain_intents().into_iter().find_map(|i| match i {
        AppIntent::SendMessage {
            content,
            tool_call_id,
            ..
        } => Some((content, tool_call_id)),
        _ => None,
    });
    assert_eq!(sent, Some(("配色: 深色".to_string(), Some("ask-9".into()))));
    assert!(app.ask_panels.is_empty(), "panel popped after submit");
    let finished = app.chat.cells.iter().any(|c| {
        matches!(
            c.cell(),
            ChatCell::Ask(msg) if msg
                .panel
                .as_ref()
                .is_some_and(|p| p.finished == Some(ask_panel::PanelFinish::Submitted))
        )
    });
    assert!(finished, "cell keeps the submitted summary");
}

#[test]
fn test_ask_panel_escape_interrupts_and_cancel_sends_sentinel() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    let mut app = test_app();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![],
        vec![serde_json::json!({
            "type": "ask",
            "tool_call_id": "ask-10",
            "questions": [{"id": "q1", "header": "H", "question": "go?", "options": [{"label": "y"}]}],
        })],
        None,
    ));

    // Esc is never consumed by the panel — it reaches the interrupt ladder.
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.ask_panels.len(),
        1,
        "panel survives until interrupted event"
    );
    assert!(
        app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::InterruptSession)),
        "Esc must interrupt while the panel is active"
    );

    // Cancel path: → to the confirm page, ↓ to Cancel, Enter.
    app.handle_key(key(KeyCode::Right));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    let sent = app.drain_intents().into_iter().find_map(|i| match i {
        AppIntent::SendMessage { content, .. } => Some(content),
        _ => None,
    });
    assert_eq!(sent.as_deref(), Some(ask_panel::ASK_CANCEL_CONTENT));
    assert!(app.ask_panels.is_empty());
}

#[test]
fn test_model_panel_esc_closes_without_interrupt_or_request() {
    let mut app = test_app();
    app.model_sources = vec![model_group("p", &["m1"])];
    app.try_frontend_command("/model");
    app.drain_intents();
    app.handle_key(key(crossterm::event::KeyCode::Esc));
    assert!(app.model_panel.is_none(), "Esc closes the panel");
    assert!(
        picker_cell(&app).is_none(),
        "the transient picker cell disappears on close"
    );
    let intents = app.drain_intents();
    assert!(
        !intents
            .iter()
            .any(|i| matches!(i, AppIntent::InterruptSession)),
        "panel Esc is not a turn interrupt"
    );
    assert!(
        !intents
            .iter()
            .any(|i| matches!(i, AppIntent::UpdateSession { .. })),
        "closing sends no model request"
    );
}

#[test]
fn test_model_panel_swallows_keys_but_lets_page_keys_scroll() {
    let mut app = test_app();
    app.model_sources = vec![model_group("p", &["m1"])];
    app.try_frontend_command("/model");
    app.drain_intents();
    // Typing is consumed by the panel — the composer stays empty.
    app.handle_key(key(crossterm::event::KeyCode::Char('x')));
    assert!(app.input.text().is_empty());
    assert!(app.model_panel.is_some());
    // PageUp/PageDown still scroll the chat.
    app.visible_height = 20;
    app.chat.scroll_offset = 50;
    app.chat.scroll_up(0); // leave auto-scroll
    app.handle_key(key(crossterm::event::KeyCode::PageUp));
    assert_eq!(app.chat.scroll_offset, 32, "page scroll reaches the chat");
    assert!(app.model_panel.is_some(), "panel stays open");
}

#[test]
fn test_scroll_down_rearms_autoscroll_at_bottom() {
    let mut app = test_app();
    app.visible_height = 20;
    app.chat.last_total = 100;
    app.chat.scroll_offset = 75;
    app.chat.scroll_up(0); // leave the bottom: auto_scroll=false, offset 75

    app.handle_mouse(wheel_down());
    assert_eq!(app.chat.scroll_offset, 78);
    assert!(!app.chat.is_at_bottom(), "still above the bottom edge");

    // Next step reaches offset 80 == max_scroll → auto-scroll re-armed.
    app.handle_mouse(wheel_down());
    assert!(app.chat.is_at_bottom());
    assert_eq!(app.chat.scroll_offset, 80);
}

#[test]
fn test_plain_arrows_never_scroll_the_chat() {
    let mut app = test_app();
    app.visible_height = 20;
    app.chat.last_total = 100;
    app.chat.scroll_offset = 80;
    app.chat.scroll_up(5); // reading history: offset 75, auto_scroll=false

    // The removed heuristic scrolled the chat here; now Up only moves
    // the composer cursor (a single-line composer cannot move).
    app.handle_key(key(crossterm::event::KeyCode::Up));
    assert_eq!(app.chat.scroll_offset, 75);
    assert!(!app.chat.is_at_bottom());

    // Same for Down on a single-line composer.
    app.handle_key(key(crossterm::event::KeyCode::Down));
    assert_eq!(app.chat.scroll_offset, 75);
    assert!(!app.chat.is_at_bottom());
}

#[test]
fn test_plain_up_moves_composer_cursor_while_reading() {
    let mut app = test_app();
    app.visible_height = 20;
    app.chat.last_total = 100;
    app.chat.scroll_offset = 60;
    app.chat.scroll_up(5); // reading history: offset 55
    app.input.set_text("first\nsecond");
    let area = ratatui::layout::Rect::new(0, 0, 40, 4);
    assert_eq!(app.input.cursor_screen_pos(&area).1, 1, "cursor on line 2");

    app.handle_key(key(crossterm::event::KeyCode::Up));
    assert_eq!(
        app.input.cursor_screen_pos(&area).1,
        0,
        "Up belongs to the composer"
    );
    assert_eq!(app.chat.scroll_offset, 55, "chat must not scroll");
    assert!(!app.chat.is_at_bottom());
}

#[test]
fn test_keyboard_scroll_keys_follow_the_same_contract() {
    let mut app = test_app();
    app.visible_height = 20;
    app.chat.last_total = 100;
    app.chat.scroll_offset = 80;
    app.chat.jump_bottom();

    // PageUp leaves the follow state...
    app.handle_key(key(crossterm::event::KeyCode::PageUp));
    assert!(!app.chat.is_at_bottom());
    let reading = app.chat.scroll_offset;
    assert!(reading < 80);

    // Ctrl+Down walks back down one line at a time (no re-arm yet).
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert_eq!(app.chat.scroll_offset, reading + 1);
    assert!(!app.chat.is_at_bottom());

    // Ctrl+End jumps to the bottom and re-arms the follow state.
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::End,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(app.chat.is_at_bottom());

    // PageDown walks back to the bottom edge and re-arms as well
    // (page = visible_height - 2 = 18).
    app.chat.scroll_down(100, app.visible_height); // offset 80, follow armed
    assert!(app.chat.is_at_bottom());
    app.handle_key(key(crossterm::event::KeyCode::PageUp));
    assert_eq!(app.chat.scroll_offset, 62);
    assert!(!app.chat.is_at_bottom(), "PageUp leaves the follow state");
    app.handle_key(key(crossterm::event::KeyCode::PageDown));
    assert_eq!(app.chat.scroll_offset, 80, "one page reaches the bottom");
    assert!(
        app.chat.is_at_bottom(),
        "PageDown to the bottom re-arms the follow state"
    );
}

#[test]
fn test_edit_does_not_jump_to_bottom() {
    let mut app = test_app();
    app.chat.scroll_up(5); // user is reading history
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(app.input.text(), "x");
    assert!(
        !app.chat.is_at_bottom(),
        "typing must not yank the view back to the bottom while reading history"
    );
}

// ── Pending message lifecycle (user_message_accepted) ──

#[test]
fn test_composer_pointer_is_ignored_while_a_modal_owns_the_keyboard() {
    /// One keyboard-owning modal: a label plus the state that arms it.
    type Case = (&'static str, Box<dyn Fn(&mut App)>);

    let cases: Vec<Case> = vec![
        (
            "ask panel",
            Box::new(|app: &mut App| {
                app.ask_panels.push_back(ask_panel::AskPanel::new(
                    "ask-1".into(),
                    vec![AskQuestion {
                        id: "q1".into(),
                        header: "H".into(),
                        question: "which?".into(),
                        multi_select: false,
                        options: vec![],
                        choices: vec![],
                    }],
                ));
            }),
        ),
        (
            "legacy ask menu",
            Box::new(|app: &mut App| {
                app.ask_selections
                    .push_back(crate::ui::ask_select::AskSelection::new(
                        "ask-legacy".into(),
                        vec!["one".into(), "two".into()],
                    ));
            }),
        ),
        (
            "model panel",
            Box::new(|app: &mut App| {
                app.model_sources = vec![model_group("p", &["m1", "m2"])];
                app.try_frontend_command("/model");
            }),
        ),
        (
            "command popup",
            Box::new(|app: &mut App| {
                // The popup is armed by a slash draft (`update_popup` is
                // what the key path runs), so this case keeps that draft.
                app.input.set_text("/");
                app.update_popup();
            }),
        ),
    ];

    for (label, arm) in cases {
        let mut app = app_with_draft("hello world");
        let mut terminal = test_terminal(40, 12);
        draw(&mut app, &mut terminal);
        arm(&mut app);
        app.drain_intents();
        draw(&mut app, &mut terminal);
        let composer = app.input.rendered_area();
        assert!(app.composer_contains(composer.x + PREFIX_WIDTH, composer.y));

        let at = (composer.x + PREFIX_WIDTH + 4, composer.y);
        let cursor_before = (app.input.cursor_row, app.input.cursor_col);
        assert_eq!(
            app.handle_mouse(press(at)),
            MouseOutcome::Ignored,
            "{label}: the press is ignored"
        );
        assert!(
            !app.selection.is_press_active(),
            "{label}: no selection starts"
        );
        assert_eq!(
            app.handle_mouse(drag((composer.x + PREFIX_WIDTH + 8, composer.y))),
            MouseOutcome::Ignored,
            "{label}: the drag is ignored"
        );
        assert_eq!(
            app.handle_mouse(release((composer.x + PREFIX_WIDTH + 8, composer.y))),
            MouseOutcome::Ignored,
            "{label}: the release is ignored"
        );
        assert_eq!(
            (app.input.cursor_row, app.input.cursor_col),
            cursor_before,
            "{label}: the cursor stays put"
        );
        assert!(app.drain_intents().is_empty(), "{label}: nothing is copied");

        // The chat band keeps its own contract while a panel is open.
        let band = app.chat.geometry().area;
        assert_eq!(
            app.handle_mouse(press((band.x + 2, band.y + 1))),
            MouseOutcome::Immediate,
            "{label}: chat drags still work"
        );
        assert_eq!(app.selection.region(), Some(SelectionRegion::Chat));
        app.handle_mouse(release((band.x + 2, band.y + 1)));
        app.drain_intents();
    }
}

#[test]
fn test_composer_pointer_works_while_an_invisible_popup_is_armed() {
    // `/zzz` matches no command: the popup stays armed but draws nothing
    // (height 0) and no longer consumes keys — so it must not block the
    // composer either, or clicks would die in an idle-looking UI.
    let mut app = test_app();
    app.chat.set_header(Vec::new());
    app.input.set_text("/zzz");
    app.update_popup();
    assert!(app.popup.active.is_active(), "the popup is still armed");
    assert_eq!(app.popup.active.height(), 0, "but it draws nothing");
    assert!(!app.popup.active.is_must_select_empty());

    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    let at = (composer.x + PREFIX_WIDTH + 2, composer.y);
    assert_eq!(
        app.handle_mouse(press(at)),
        MouseOutcome::Immediate,
        "an invisible popup does not own the composer"
    );
    assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Immediate);
    assert_eq!(
        (app.input.cursor_row, app.input.cursor_col),
        (0, 2),
        "the click placed the cursor"
    );
    assert!(app.drain_intents().is_empty());
}

#[test]
fn test_composer_pointer_stays_blocked_by_an_itemless_must_select_popup() {
    // `/session zzz` filters every candidate away, but the popup stays up
    // (invisible) to intercept Enter: its Enter must not reach the draft,
    // so the composer pointer stays off-limits as well.
    let mut app = app_with_draft("/session zzz");
    app.popup.cache.sessions = vec![SessionCandidate {
        id: "s1".into(),
        title: "Test".into(),
        workspace: "/tmp".into(),
        status: "idle".into(),
        last_interaction: "2025-01-01T00:00:00Z".into(),
    }];
    app.update_popup();
    assert_eq!(app.popup.active.height(), 0, "no candidate is visible");
    assert!(
        app.popup.active.is_must_select_empty(),
        "but Enter has to stay intercepted"
    );

    let mut terminal = test_terminal(40, 12);
    draw(&mut app, &mut terminal);
    let composer = app.input.rendered_area();
    let cursor_before = (app.input.cursor_row, app.input.cursor_col);
    let at = (composer.x + PREFIX_WIDTH + 3, composer.y);
    assert_eq!(app.handle_mouse(press(at)), MouseOutcome::Ignored);
    assert_eq!(
        app.handle_mouse(release(at)),
        MouseOutcome::Ignored,
        "the release is a no-op as well"
    );
    assert_eq!((app.input.cursor_row, app.input.cursor_col), cursor_before);
    assert!(app.drain_intents().is_empty());
}

// ── Ownership: one declaration for keyboard, pointer and priority ───────

/// A single question, for building an AskUserQuestion panel.
fn test_question() -> AskQuestion {
    AskQuestion {
        id: "theme".into(),
        question: "which theme?".into(),
        header: String::new(),
        multi_select: false,
        options: Vec::new(),
        choices: Vec::new(),
    }
}

/// An app with a *visible* command popup armed.
fn app_with_visible_popup() -> App {
    let mut app = test_app();
    app.popup.cache.commands = vec![crate::protocol::CommandInfo {
        name: "model".into(),
        aliases: Vec::new(),
        description: String::new(),
        params: String::new(),
    }];
    app.input.set_text("/mo");
    app.update_popup();
    assert!(app.popup.active.height() > 0, "the popup is on screen");
    app
}

/// An app with an *invisible* (armed, no candidates) command popup.
fn app_with_invisible_popup() -> App {
    let mut app = test_app();
    app.input.set_text("/zzz");
    app.update_popup();
    assert!(app.popup.active.is_active());
    assert_eq!(app.popup.active.height(), 0);
    app
}

/// An app with the `/model` picker open.
fn app_with_model_panel() -> App {
    let mut app = test_app();
    app.model_sources = vec![model_group("p", &["m1"])];
    app.open_model_panel();
    assert!(app.model_panel.is_some());
    app
}

/// An app with an AskUserQuestion panel queued.
fn app_with_ask_panel() -> App {
    let mut app = test_app();
    app.register_ask_panel("ask-1", &[test_question()], &[], true);
    assert_eq!(app.ask_panels.len(), 1);
    app
}

/// An app with a legacy ask menu queued.
fn app_with_legacy_ask() -> App {
    let mut app = test_app();
    app.ask_selections
        .push_back(crate::ui::ask_select::AskSelection::new(
            "ask-2".into(),
            vec!["y".into(), "n".into()],
        ));
    app
}

#[test]
fn test_modal_priority_is_declared_in_one_place() {
    // Each layer added here outranks the previous one — the order is the
    // declaration order of `ModalOwner`, nothing else.
    let mut app = app_with_visible_popup();
    assert_eq!(app.modal_owner(), Some(ModalOwner::Popup));
    assert!(app.composer_pointer_blocked());

    app.model_sources = vec![model_group("p", &["m1"])];
    app.open_model_panel();
    assert_eq!(
        app.modal_owner(),
        Some(ModalOwner::ModelPicker),
        "the picker outranks the popup"
    );
    assert!(app.composer_pointer_blocked());

    app.register_ask_panel("ask-1", &[test_question()], &[], true);
    assert_eq!(
        app.modal_owner(),
        Some(ModalOwner::AskPanel),
        "the ask panel outranks the picker"
    );
    // Two modals up at once: the guard stays consistent with the owner.
    assert!(app.composer_pointer_blocked());

    app.ask_selections
        .push_back(crate::ui::ask_select::AskSelection::new(
            "ask-2".into(),
            vec!["y".into()],
        ));
    assert_eq!(
        app.modal_owner(),
        Some(ModalOwner::AskSelection),
        "the legacy menu outranks everything"
    );
    assert!(app.composer_pointer_blocked());
}

#[test]
fn test_key_routing_of_the_idle_app() {
    use crossterm::event::KeyModifiers;

    let app = test_app();
    assert_eq!(app.modal_owner(), None);
    assert_eq!(
        app.route_key(&key(crossterm::event::KeyCode::Esc)),
        KeyRoute::EscLadder
    );
    assert_eq!(
        app.route_key(&key(crossterm::event::KeyCode::PageUp)),
        KeyRoute::ChatScroll(ChatScrollAction::PageUp)
    );
    assert_eq!(
        app.route_key(&key(crossterm::event::KeyCode::PageDown)),
        KeyRoute::ChatScroll(ChatScrollAction::PageDown)
    );
    assert_eq!(
        app.route_key(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            KeyModifiers::CONTROL
        )),
        KeyRoute::ChatScroll(ChatScrollAction::Bottom)
    );
    assert_eq!(
        app.route_key(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )),
        KeyRoute::Quit
    );
    assert_eq!(
        app.route_key(&key(crossterm::event::KeyCode::Char('x'))),
        KeyRoute::Composer
    );
    // Plain arrows belong to the composer; the wheel has its own channel.
    assert_eq!(
        app.route_key(&key(crossterm::event::KeyCode::Up)),
        KeyRoute::Composer
    );
}

#[test]
fn test_key_routing_reserves_esc_and_page_keys_for_the_app() {
    use crossterm::event::KeyCode;

    // A visible popup owns the navigation keys …
    let app = app_with_visible_popup();
    assert_eq!(app.route_key(&key(KeyCode::Down)), KeyRoute::PopupNav);
    assert_eq!(app.route_key(&key(KeyCode::Tab)), KeyRoute::PopupNav);
    // … but Esc still belongs to the app's ladder (it closes the popup).
    assert_eq!(app.route_key(&key(KeyCode::Esc)), KeyRoute::EscLadder);

    // An armed-but-empty popup owns the Enter rung instead.
    let app = app_with_invisible_popup();
    assert_eq!(app.route_key(&key(KeyCode::Enter)), KeyRoute::PopupEmpty);

    // The ask panel owns everything except the reserved keys.
    let app = app_with_ask_panel();
    assert_eq!(app.route_key(&key(KeyCode::Char('x'))), KeyRoute::AskPanel);
    assert_eq!(app.route_key(&key(KeyCode::Enter)), KeyRoute::AskPanel);
    assert_eq!(app.route_key(&key(KeyCode::Esc)), KeyRoute::EscLadder);
    assert_eq!(
        app.route_key(&key(KeyCode::PageDown)),
        KeyRoute::ChatScroll(ChatScrollAction::PageDown)
    );

    // The picker keeps Esc for itself — closing it is not an interrupt.
    let app = app_with_model_panel();
    assert_eq!(
        app.route_key(&key(KeyCode::Char('x'))),
        KeyRoute::ModelPicker
    );
    assert_eq!(app.route_key(&key(KeyCode::Esc)), KeyRoute::ModelPicker);
    assert_eq!(
        app.route_key(&key(KeyCode::PageUp)),
        KeyRoute::ChatScroll(ChatScrollAction::PageUp)
    );

    // The legacy menu swallows every key, Esc included.
    let app = app_with_legacy_ask();
    assert_eq!(app.route_key(&key(KeyCode::Esc)), KeyRoute::AskSelection);
    assert_eq!(app.route_key(&key(KeyCode::PageUp)), KeyRoute::AskSelection);
}

#[test]
fn test_pointer_guard_is_derived_from_modal_ownership() {
    // No modal: the composer is free.
    assert!(!test_app().composer_pointer_blocked());

    // An invisible popup owns the Enter rung but cannot eat clicks.
    let invisible = app_with_invisible_popup();
    assert_eq!(invisible.modal_owner(), Some(ModalOwner::Popup));
    assert!(!invisible.composer_pointer_blocked());

    // A visible popup blocks.
    assert!(app_with_visible_popup().composer_pointer_blocked());

    // Every other modal blocks, visible or not.
    assert!(app_with_ask_panel().composer_pointer_blocked());
    assert!(app_with_model_panel().composer_pointer_blocked());
    assert!(app_with_legacy_ask().composer_pointer_blocked());
}

// ── Popup rungs: the chat keeps its scroll keys ─────────────────────────

#[test]
fn test_page_keys_still_scroll_the_chat_while_a_popup_is_up() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    // A popup only declines a key — it takes the navigation keys and never the
    // chat's page keys, whether its candidates are visible or not.
    for (label, mut app) in [
        ("visible candidates", app_with_visible_popup()),
        ("armed but empty", app_with_invisible_popup()),
    ] {
        app.visible_height = 20;
        app.chat.last_total = 100;
        app.chat.scroll_offset = 80;
        app.chat.jump_bottom();

        app.handle_key(key(KeyCode::PageUp));
        assert!(
            !app.chat.is_at_bottom(),
            "{label}: PageUp leaves the bottom"
        );
        let reading = app.chat.scroll_offset;
        assert!(reading < 80, "{label}: PageUp scrolled the chat");

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(
            app.chat.scroll_offset, 0,
            "{label}: Ctrl+Home jumps to the top"
        );

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert!(app.chat.is_at_bottom(), "{label}: Ctrl+End re-arms follow");
    }
}

#[test]
fn test_ctrl_arrows_step_the_chat_only_when_the_popup_declines_them() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    // Pre-lane behaviour, kept verbatim: with candidates on screen the popup
    // matches the arrow key whatever the modifier, so Ctrl+Down moves the popup
    // selection; with no candidates the popup declines it and the chat steps
    // one line down.
    let mut app = app_with_visible_popup();
    app.visible_height = 20;
    app.chat.last_total = 100;
    app.chat.scroll_offset = 80;
    app.chat.jump_bottom();
    app.handle_key(key(KeyCode::PageUp)); // the chat keeps its page keys
    let reading = app.chat.scroll_offset;
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
    assert_eq!(
        app.chat.scroll_offset, reading,
        "the visible popup owns the arrow key"
    );

    let mut app = app_with_invisible_popup();
    app.visible_height = 20;
    app.chat.last_total = 100;
    app.chat.scroll_offset = 80;
    app.chat.jump_bottom();
    app.handle_key(key(KeyCode::PageUp));
    let reading = app.chat.scroll_offset;
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
    assert_eq!(
        app.chat.scroll_offset,
        reading + 1,
        "an empty popup declines the key and the chat steps one line"
    );
}

#[test]
fn test_escape_closing_the_popup_leaves_the_quit_gesture_alone() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let ctrl_c = || KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    // The popup consumed the Escape, not the app: the Ctrl+C double-press pair
    // survives it (pre-lane behaviour, kept).
    let mut app = app_with_visible_popup();
    app.handle_key(ctrl_c());
    assert_eq!(app.ctrl_c_count, 1);
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.popup.active.is_active(), "the popup closed");
    assert_eq!(app.ctrl_c_count, 1, "the gesture was not reset");
    app.handle_key(ctrl_c());
    assert!(app.should_quit, "the second press within 500 ms quits");

    // An Escape that reaches the app's own ladder *does* reset the gesture
    // (here: clearing the draft).
    let mut app = app_with_draft("hello");
    app.handle_key(ctrl_c());
    assert_eq!(app.ctrl_c_count, 1);
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.ctrl_c_count, 0, "clearing the draft resets the gesture");
    app.handle_key(ctrl_c());
    assert!(!app.should_quit, "the pair was broken");
}

// ── Two modals at once: the chain hands the key over ────────────────────

#[test]
fn test_escape_with_the_picker_under_an_ask_panel_closes_the_picker() {
    use crossterm::event::KeyCode;

    // The ask panel keeps Esc for the app, and the picker is the next layer in
    // the chain: Esc closes the picker and never reaches the interrupt ladder.
    let mut app = app_with_ask_panel();
    app.model_sources = vec![model_group("p", &["m1"])];
    app.open_model_panel();
    app.drain_intents();
    assert!(app.model_panel.is_some());
    assert_eq!(app.ask_panels.len(), 1);

    app.handle_key(key(KeyCode::Esc));

    assert!(app.model_panel.is_none(), "the picker closed");
    assert_eq!(app.ask_panels.len(), 1, "the ask panel is untouched");
    assert!(
        !app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::InterruptSession)),
        "the picker's Esc is not an interrupt"
    );
}

#[test]
fn test_page_keys_reach_the_chat_through_both_modals() {
    use crossterm::event::KeyCode;

    // Both modals decline the page keys, so they fall through to the chat.
    let mut app = app_with_ask_panel();
    app.model_sources = vec![model_group("p", &["m1"])];
    app.open_model_panel();
    app.visible_height = 20;
    app.chat.last_total = 100;
    app.chat.scroll_offset = 80;
    app.chat.jump_bottom();

    app.handle_key(key(KeyCode::PageUp));
    assert!(!app.chat.is_at_bottom(), "PageUp scrolled the chat");
    assert!(app.model_panel.is_some(), "the picker stays open");
    assert_eq!(app.ask_panels.len(), 1, "the ask panel stays open");
}
