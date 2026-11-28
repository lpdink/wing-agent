use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::layout::Position;
use serde_json::json;
use tokio::time::Instant;
use wing_api_client::models::{SessionInfo, SessionListResponse};

use super::support::*;
use crate::app::intent::{FetchPayload, FetchResult};
use crate::app::mouse::PointerOwner;
use crate::app::{App, AppIntent, MouseOutcome};
use crate::config::{AppConfig, SidebarSide};
use crate::protocol::{SessionStatus, WingEvent};
use crate::ui::chat_view::ChatCell;
use crate::ui::session_sidebar::RowAction;

fn row(id: &str, parent: Option<&str>, status: &str, last_interaction: &str) -> SessionInfo {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "workspace": "/projects/wing-agent",
        "parent_session_id": parent,
        "status": status,
        "last_interaction": last_interaction,
    }))
    .unwrap()
}

fn finish(app: &mut App, from: &str, result: Result<Vec<SessionInfo>, String>) {
    app.handle_fetch_result(FetchResult {
        session_id: from.into(),
        payload: FetchPayload::SessionList(result.map(|sessions| SessionListResponse { sessions })),
    });
}

fn refresh(app: &mut App, sessions: Vec<SessionInfo>) {
    assert!(app.session_sidebar.begin_fetch(None));
    finish(app, "test-session", Ok(sessions));
}

fn visible_hit(app: &App, id: &str, action: RowAction) -> Option<(u16, u16)> {
    let frame = &app.session_sidebar.frame;
    (frame.area.y..frame.area.bottom())
        .flat_map(|y| (frame.area.x..frame.area.right()).map(move |x| (x, y)))
        .find(|&(x, y)| frame.hit(Position::new(x, y)) == Some((id, action)))
}

fn click(app: &mut App, at: (u16, u16)) -> MouseOutcome {
    app.handle_mouse(press(at));
    app.handle_mouse(release(at))
}

#[test]
fn startup_toggle_and_resize_use_the_conversation_width() {
    let mut app = app_with_draft(&"x".repeat(78));
    let mut wide = test_terminal(104, 24);
    draw(&mut app, &mut wide);
    assert!(app.session_sidebar.frame.area.is_empty());
    assert!(app.session_sidebar.deadline(true).is_none());
    let original = app.input.rendered_area();

    app.handle_key(key(KeyCode::F(2)));
    app.handle_key(KeyEvent::new_with_kind(
        KeyCode::F(2),
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));
    draw(&mut app, &mut wide);
    let left = app.session_sidebar.frame.area;
    assert_eq!((left.x, left.y, left.width), (0, 1, 30));
    assert_eq!(app.geometry.chat_band().x, left.right());
    assert!(app.input.rendered_area().x >= left.right());
    assert!(app.input.rendered_area().height > original.height);
    assert_eq!(app.input.text(), "x".repeat(78));

    app.apply_settings_preview(&json!({"layout": {"session_sidebar": {"side": "right"}}}));
    draw(&mut app, &mut wide);
    let right = app.session_sidebar.frame.area;
    assert_eq!(right.right(), 104);
    assert_eq!(app.geometry.chat_band().right(), right.x);
    assert!(app.input.rendered_area().right() <= right.x);
    assert_eq!(
        app.pointer_owner(right.x, 3),
        Some(PointerOwner::SessionSidebar)
    );

    let mut narrow = test_terminal(99, 24);
    draw(&mut app, &mut narrow);
    assert!(app.session_sidebar.frame.area.is_empty());
    assert!(app.session_sidebar.deadline(true).is_none());
    assert_eq!(app.geometry.chat_band().width, 99);
    draw(&mut app, &mut wide);
    assert_eq!(app.session_sidebar.frame.area, right);

    app.handle_key(key(KeyCode::F(2)));
    draw(&mut app, &mut wide);
    assert!(app.session_sidebar.frame.area.is_empty());
    assert_eq!(app.input.rendered_area(), original);
    assert_eq!(app.config.layout.session_sidebar.side, SidebarSide::Right);
    assert!(!app.config.layout.session_sidebar.default_open);

    let mut narrow = test_terminal(104, 7);
    draw(&mut app, &mut narrow);
    app.handle_key(key(KeyCode::F(2)));
    assert!(app.toast.is_some());
    draw(&mut app, &mut narrow);
    assert!(app.session_sidebar.frame.area.is_empty());
}

#[test]
fn right_sidebar_discloses_children_and_switches_through_the_snapshot_commit() {
    let config: AppConfig = serde_json::from_value(json!({
        "layout": {"session_sidebar": {"default_open": true, "side": "right"}},
        "colors": {"preset": "light"},
    }))
    .unwrap();
    let mut app = App::new("test-session".into(), config, None);
    let mut terminal = test_terminal(120, 20);
    app.input.set_text("keep this draft");
    let mut parent = row("Project", None, "inactive", "2026-01-01T14:30:00");
    parent.tags.push("pin".into());
    refresh(
        &mut app,
        vec![
            parent,
            row("Waiting", Some("Project"), "waiting", "2026-01-01T14:24:00"),
            row("Running", Some("Project"), "working", "2026-01-01T14:26:00"),
        ],
    );
    draw(&mut app, &mut terminal);
    let text = frame_text(terminal.backend().buffer());
    assert!(text.contains("? Waiting"), "{text}");
    assert!(text.contains("● Running"), "{text}");
    assert!(text.contains("Project ★"), "{text}");
    assert!(text.contains("wing-agent · 01-01 14:30"), "{text}");
    let current = visible_hit(&app, "test-session", RowAction::Open).unwrap();
    assert_eq!(
        terminal.backend().buffer()[current].bg,
        app.palette().surface
    );
    assert_eq!(click(&mut app, current), MouseOutcome::Ignored);

    let disclosure = visible_hit(&app, "Project", RowAction::ToggleChildren).unwrap();
    assert_eq!(click(&mut app, disclosure), MouseOutcome::Immediate);
    draw(&mut app, &mut terminal);
    assert!(visible_hit(&app, "Waiting", RowAction::Open).is_none());
    let text = frame_text(terminal.backend().buffer());
    assert!(text.contains("▸2 ?1 ●1"), "{text}");
    assert!(app.drain_intents().is_empty());

    let disclosure = visible_hit(&app, "Project", RowAction::ToggleChildren).unwrap();
    click(&mut app, disclosure);
    draw(&mut app, &mut terminal);
    let (x, y) = visible_hit(&app, "Waiting", RowAction::Open).unwrap();
    assert_eq!(click(&mut app, (x, y + 1)), MouseOutcome::Immediate);
    assert!(
        matches!(app.drain_intents().as_slice(), [AppIntent::ResumeSession { session_id }] if session_id == "Waiting")
    );
    assert_eq!(app.session_id, "test-session");
    assert_eq!(app.input.text(), "keep this draft");

    app.await_session_snapshot("Waiting".into());
    assert_eq!(click(&mut app, (x, y)), MouseOutcome::Ignored);
    let mut event =
        sync_event_with_status(SessionStatus::Waiting, vec![], None, vec![], vec![], None);
    if let WingEvent::SyncSession(snapshot) = &mut event {
        snapshot.session_id = "Waiting".into();
    }
    app.handle_event(event);
    draw(&mut app, &mut terminal);
    assert_eq!(app.session_id, "Waiting");
    let current = visible_hit(&app, "Waiting", RowAction::Open).unwrap();
    assert_eq!(
        terminal.backend().buffer()[current].bg,
        app.palette().surface
    );
    let text = frame_text(terminal.backend().buffer());
    assert!(text.contains("? Waiting"), "{text}");

    app.status.session_name = Some("Stale title".into());
    app.status.workdir = Some("/projects/stale-workspace".into());
    app.await_session_snapshot("empty-session".into());
    let mut event = sync_event(vec![], None, vec![], vec![], None);
    if let WingEvent::SyncSession(snapshot) = &mut event {
        snapshot.session_id = "empty-session".into();
    }
    app.handle_event(event);
    draw(&mut app, &mut terminal);
    let text = frame_text(terminal.backend().buffer());
    assert!(text.contains("empty-session"), "{text}");
    assert!(!text.contains("Stale title"), "{text}");
    assert!(!text.contains("stale-workspace"), "{text}");
}

#[test]
fn wheel_routing_and_press_identity_survive_refreshes() {
    let mut app = app_with_message();
    app.chat
        .push(ChatCell::AssistantMessage("history\n".repeat(80)));
    app.toggle_session_sidebar();
    let mut terminal = test_terminal(104, 14);
    let mut sessions: Vec<_> = (0..20)
        .map(|i| row(&format!("row-{i:02}"), None, "idle", ""))
        .collect();
    sessions.push(row("test-session", None, "idle", "2026-01-01T10:00:00"));
    refresh(&mut app, sessions.clone());
    draw(&mut app, &mut terminal);
    assert!(frame_text(terminal.backend().buffer()).contains("1–6/21"));
    let chat = app.chat.scroll_position();
    let mut wheel = wheel_up();
    wheel.column = 60;
    wheel.row = 3;
    app.handle_mouse(wheel);
    assert_ne!(app.chat.scroll_position(), chat);
    let chat = app.chat.scroll_position();

    wheel.column = 4;
    wheel.kind = MouseEventKind::ScrollDown;
    app.handle_mouse(wheel);
    draw(&mut app, &mut terminal);
    assert_eq!(app.chat.scroll_position(), chat);
    assert_eq!(visible_hit(&app, "row-02", RowAction::Open).unwrap().1, 2);
    assert!(frame_text(terminal.backend().buffer()).contains("4–9/21"));
    let pressed = visible_hit(&app, "row-03", RowAction::Open).unwrap();
    app.handle_mouse(press(pressed));
    let mut reordered = sessions;
    reordered[19].status = "waiting".into();
    reordered[4].status = "waiting".into();
    reordered.swap_remove(3);
    refresh(&mut app, reordered);
    draw(&mut app, &mut terminal);
    assert_eq!(visible_hit(&app, "row-02", RowAction::Open).unwrap().1, 2);
    assert_eq!(app.handle_mouse(release(pressed)), MouseOutcome::Ignored);
    assert!(app.drain_intents().is_empty());

    let at = visible_hit(&app, "row-02", RowAction::Open).unwrap();
    app.handle_mouse(press(at));
    app.handle_mouse(drag((at.0 + 3, at.1)));
    assert_eq!(app.handle_mouse(release(at)), MouseOutcome::Ignored);
    app.set_connected(false);
    assert_eq!(click(&mut app, at), MouseOutcome::Ignored);
    assert!(app.drain_intents().is_empty());
}

#[test]
fn polling_coalesces_with_popup_and_discards_pre_reconnect_responses() {
    let mut app = app_with_message();
    app.toggle_session_sidebar();
    let mut terminal = test_terminal(105, 18);
    draw(&mut app, &mut terminal);
    assert!(app.session_sidebar.deadline(true).unwrap() <= Instant::now());
    assert!(app.session_sidebar.begin_fetch(None));
    assert!(app.session_sidebar.deadline(true).is_none());
    assert!(!app.session_sidebar.begin_fetch(Some("test-session")));
    finish(
        &mut app,
        "old-session",
        Ok(vec![row("Other", None, "working", "")]),
    );
    assert!(app.popup.cache.sessions_fetched);
    assert_eq!(app.popup.cache.sessions[0].id, "Other");
    draw(&mut app, &mut terminal);
    let remaining = app.session_sidebar.deadline(true).unwrap() - Instant::now();
    assert!(remaining.as_secs_f64() > 5.0 && remaining.as_secs_f64() <= 8.0);

    assert!(app.session_sidebar.begin_fetch(None));
    finish(&mut app, "test-session", Err("timeout".into()));
    draw(&mut app, &mut terminal);
    let text = frame_text(terminal.backend().buffer());
    assert!(text.contains("更 新 失 败"), "{text}");
    assert!(text.contains("Other"), "{text}");
    let remaining = app.session_sidebar.deadline(true).unwrap() - Instant::now();
    assert!(remaining.as_secs_f64() > 13.0 && remaining.as_secs_f64() <= 16.0);
    for _ in 0..3 {
        assert!(app.session_sidebar.begin_fetch(None));
        finish(&mut app, "test-session", Err("timeout".into()));
    }
    let remaining = app.session_sidebar.deadline(true).unwrap() - Instant::now();
    assert!(remaining.as_secs_f64() > 55.0 && remaining.as_secs_f64() <= 60.0);
    assert!(app.session_sidebar.begin_fetch(Some("test-session")));
    app.set_connected(false);
    assert!(app.session_sidebar.deadline(false).is_none());
    draw(&mut app, &mut terminal);
    let text = frame_text(terminal.backend().buffer());
    assert!(text.contains("离 线"), "{text}");
    app.set_connected(true);
    finish(&mut app, "test-session", Ok(vec![]));
    assert!(matches!(
        app.drain_intents().as_slice(),
        [AppIntent::FetchSessionList]
    ));
    assert_eq!(app.popup.cache.sessions[0].id, "Other");
    assert!(app.session_sidebar.deadline(true).unwrap() <= Instant::now());

    refresh(&mut app, vec![row("Idle", None, "idle", "")]);
    draw(&mut app, &mut terminal);
    let remaining = app.session_sidebar.deadline(true).unwrap() - Instant::now();
    assert!(remaining.as_secs_f64() > 24.0 && remaining.as_secs_f64() <= 30.0);
    let text = frame_text(terminal.backend().buffer());
    assert!(text.contains("Idle"));
    assert!(!text.contains("更 新 失 败"));
    assert_eq!(app.popup.cache.sessions[0].id, "Other");

    app.toggle_session_sidebar();
    draw(&mut app, &mut terminal);
    assert!(app.session_sidebar.deadline(true).is_none());
}

#[test]
fn mutations_during_an_inflight_list_request_keep_a_follow_up() {
    let mut app = app_with_message();
    app.toggle_session_sidebar();
    let mut terminal = test_terminal(105, 18);
    draw(&mut app, &mut terminal);
    assert!(app.session_sidebar.begin_fetch(Some("test-session")));
    app.invalidate_session_cache();
    finish(
        &mut app,
        "test-session",
        Ok(vec![row("Old name", None, "idle", "")]),
    );
    assert!(!app.popup.cache.sessions_fetched);
    assert!(matches!(
        app.drain_intents().as_slice(),
        [AppIntent::FetchSessionList]
    ));
    assert!(app.session_sidebar.deadline(true).unwrap() <= Instant::now());

    assert!(app.session_sidebar.begin_fetch(Some("test-session")));
    finish(
        &mut app,
        "test-session",
        Ok(vec![row("New name", None, "idle", "")]),
    );
    assert_eq!(app.popup.cache.sessions[0].id, "New name");
    assert!(app.drain_intents().is_empty());
}

#[test]
fn a_sidebar_switch_closes_the_old_picker_and_keeps_the_draft() {
    let mut app = app_with_message();
    let mut terminal = test_terminal(104, 22);
    draw(&mut app, &mut terminal);
    app.toggle_session_sidebar();
    refresh(&mut app, vec![row("Other", None, "idle", "")]);
    open_session_popup(&mut app, 3);
    app.drain_intents();
    draw(&mut app, &mut terminal);
    assert!(app.popup.height() > 0);
    let target = visible_hit(&app, "Other", RowAction::Open).unwrap();
    click(&mut app, target);
    assert!(
        matches!(app.drain_intents().as_slice(), [AppIntent::ResumeSession { session_id }] if session_id == "Other")
    );
    app.await_session_snapshot("Other".into());
    let mut event = sync_event(vec![], None, vec![], vec![], None);
    if let WingEvent::SyncSession(snapshot) = &mut event {
        snapshot.session_id = "Other".into();
    }
    app.handle_event(event);
    assert_eq!(app.popup.height(), 0);
    assert_eq!(app.input.text(), "/ss");
}

#[test]
fn the_current_row_uses_the_list_until_sync_and_shows_live_asks() {
    let mut app = app_with_message();
    let mut terminal = test_terminal(104, 22);
    draw(&mut app, &mut terminal);
    app.toggle_session_sidebar();
    app.await_session_snapshot("test-session".into());
    finish(
        &mut app,
        "test-session",
        Ok(vec![row("unexpected", None, "idle", "")]),
    );
    draw(&mut app, &mut terminal);
    assert!(visible_hit(&app, "unexpected", RowAction::Open).is_none());

    refresh(&mut app, vec![row("test-session", None, "working", "")]);
    draw(&mut app, &mut terminal);
    let (x, y) = visible_hit(&app, "test-session", RowAction::Open).unwrap();
    assert_eq!(
        terminal.backend().buffer()[(x + 3, y)].fg,
        app.palette().warning
    );

    app.handle_event(sync_event(vec![], None, vec![], vec![], None));
    app.register_ask_panel(required_choice_panel("ask", &["yes", "no"]));
    draw(&mut app, &mut terminal);
    let (x, y) = visible_hit(&app, "test-session", RowAction::Open).unwrap();
    assert_eq!(terminal.backend().buffer()[(x + 3, y)].symbol(), "?");
}
