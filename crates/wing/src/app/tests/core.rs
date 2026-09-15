//! Composition-root tests — frame gate and session-scoped caches.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::*;
use crate::ui::popup::command::SessionCandidate;

#[test]
fn test_draw_gate_frame_interval() {
    use std::time::Duration;
    // Within the frame interval: coalesce.
    assert!(!draw_gate(Duration::from_millis(5)));
    assert!(!draw_gate(Duration::from_millis(15)));
    // At/after the interval: due.
    assert!(draw_gate(Duration::from_millis(16)));
    assert!(draw_gate(Duration::from_millis(50)));
}

#[test]
fn test_should_draw_now_input_bypasses_gate() {
    let mut app = test_app();
    // The fresh app wants a full first repaint — consume it.
    app.needs_full_redraw = false;
    // Just drew — frame gate closed.
    app.last_draw = std::time::Instant::now();
    app.chat_dirty = true;
    assert!(!app.should_draw_now(), "chat-only change coalesces");

    // Input arrives: immediate draw, gate bypassed.
    app.input_dirty = true;
    assert!(app.should_draw_now());
    assert!(!app.input_dirty, "input flag consumed");
    assert!(!app.chat_dirty, "chat flag consumed with the draw");

    // Nothing pending: no draw.
    assert!(!app.should_draw_now());
}

#[test]
fn test_should_draw_now_chat_dirty_frame_due() {
    let mut app = test_app();
    app.needs_full_redraw = false;
    app.chat_dirty = true;
    // Simulate the last draw 20ms ago — frame due.
    app.last_draw = std::time::Instant::now() - std::time::Duration::from_millis(20);
    assert!(app.should_draw_now());
    assert!(!app.chat_dirty, "dirty consumed by the draw");
    assert!(!app.should_draw_now());
}

#[test]
fn test_should_draw_now_full_redraw_immediate() {
    let mut app = test_app();
    app.last_draw = std::time::Instant::now();
    app.needs_full_redraw = true;
    assert!(app.should_draw_now());
}

#[test]
fn test_invalidate_session_cache_clears_sessions() {
    let mut app = test_app();
    // Populate cache with a dummy session.
    app.popup.cache.sessions = vec![SessionCandidate {
        id: "s1".into(),
        title: "Test".into(),
        workspace: "/tmp".into(),
        status: "idle".into(),
        last_interaction: "2025-01-01T00:00:00Z".into(),
    }];
    assert!(app.popup.cache.has_sessions());

    app.invalidate_session_cache();
    assert!(!app.popup.cache.has_sessions());
    assert!(app.popup.cache.sessions.is_empty());
}

#[test]
fn test_invalidate_session_cache_noop_when_empty() {
    let mut app = test_app();
    assert!(!app.popup.cache.has_sessions());
    app.invalidate_session_cache();
    assert!(!app.popup.cache.has_sessions());
}

// ── SyncSession replay: uncommitted projection, working state, ask ──
