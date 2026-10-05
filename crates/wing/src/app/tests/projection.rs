//! Event projection lane tests — `WingEvent` into chat / turn / status.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::*;
use crate::protocol::AskQuestion;
use crate::protocol::EventMeta;
use crate::protocol::SessionStatus;
use crate::protocol::WingEvent;
use crate::shared::panels::ask::AskPanel;
use crate::shared::panels::ask::PanelMode;
use crate::ui::chat_view::ChatCell;

fn utc_ago(secs: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
}

// ── notice events: warn without faking end-of-turn ──

fn notice_event(level: Option<&str>, message: &str) -> WingEvent {
    WingEvent::Notice {
        level: level.unwrap_or("info").to_string(),
        message: message.to_string(),
        attempt: Some(1),
        max_attempts: Some(3),
        retry_in_s: Some(6.0),
        meta: EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: Some("test-session".into()),
            request_id: "r".into(),
        },
    }
}

fn turn_started_event() -> WingEvent {
    WingEvent::TurnStarted {
        meta: EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: Some("test-session".into()),
            request_id: "r".into(),
        },
    }
}

/// The regression: a retry notice must NOT end the turn (the backend is
/// still working on it) — it is a warning cell, not an error cell.
#[test]
fn test_notice_keeps_turn_running_and_renders_warning() {
    let mut app = test_app();
    app.handle_event(turn_started_event());
    let _ = app.drain_intents();
    assert!(app.turn.working, "turn started");

    app.handle_event(notice_event(
        Some("warning"),
        "generate 调用失败 (1/3): TimeoutError: stalled, 6s 后重试",
    ));

    assert!(app.turn.working, "a notice must not finish the turn");
    match app.chat.cells.last().map(|c| c.cell()) {
        Some(ChatCell::WarningMessage(text)) => {
            assert!(text.contains("stalled"), "{text}");
            assert!(text.contains("attempt 1/3"), "{text}");
        }
        other => panic!("expected WarningMessage, got {other:?}"),
    }
    // No OSC 9 notification intent (an error would have sent one).
    assert!(
        !app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::Notify(_))),
        "notice must not raise a desktop notification"
    );
}

#[test]
fn test_notice_missing_level_degrades_to_system_message() {
    let mut app = test_app();
    app.handle_event(notice_event(None, "something happened"));
    assert!(
        matches!(
            app.chat.cells.last().map(|c| c.cell()),
            Some(ChatCell::SystemMessage(text)) if text.contains("something happened")
        ),
        "unknown/missing level must degrade to a plain system message"
    );
    // Also covers an outright unknown level string.
    app.handle_event(notice_event(Some("weird"), "still ok"));
    assert!(matches!(
        app.chat.cells.last().map(|c| c.cell()),
        Some(ChatCell::SystemMessage(_))
    ));
}

#[test]
fn test_notice_without_retry_fields_renders_message_only() {
    let mut app = test_app();
    app.handle_event(WingEvent::Notice {
        level: "warning".into(),
        message: "degraded".into(),
        attempt: None,
        max_attempts: None,
        retry_in_s: None,
        meta: EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: Some("test-session".into()),
            request_id: "r".into(),
        },
    });
    match app.chat.cells.last().map(|c| c.cell()) {
        Some(ChatCell::WarningMessage(text)) => assert_eq!(text, "degraded"),
        other => panic!("expected WarningMessage, got {other:?}"),
    }
}

/// `error` keeps its "this turn is over" semantics (unchanged by notice).
#[test]
fn test_error_still_finishes_turn() {
    let mut app = test_app();
    app.handle_event(turn_started_event());
    let _ = app.drain_intents();
    app.handle_event(WingEvent::Error {
        message: "boom".into(),
        status_code: 500,
        error_code: None,
        detail: None,
        meta: EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: Some("test-session".into()),
            request_id: "r".into(),
        },
    });
    assert!(!app.turn.working, "error still finishes the turn");
    assert!(matches!(
        app.chat.cells.last().map(|c| c.cell()),
        Some(ChatCell::ErrorMessage(text)) if text == "boom"
    ));
}

fn cell_kinds(app: &App) -> Vec<&'static str> {
    app.chat
        .cells
        .iter()
        .map(|c| match c.cell() {
            ChatCell::UserMessage(_) => "user",
            ChatCell::AssistantMessage(_) => "assistant",
            ChatCell::SystemMessage(_) => "system",
            ChatCell::Thinking(_) => "thinking",
            ChatCell::ToolCall(_) => "tool_call",
            ChatCell::Diff(_) => "diff",
            ChatCell::Ask(_) => "ask",
            ChatCell::Todo(_) => "todo",
            ChatCell::Separator => "separator",
            _ => "other",
        })
        .collect()
}

#[test]
fn test_sync_midturn_restores_working_and_elapsed() {
    // Content in flight (status: working) → working; elapsed from
    // turn_started_at (not recounted from the resume moment).
    let mut app = test_app();
    let uncommitted = serde_json::json!({
        "role": "assistant",
        "content": "partial answer",
    });
    app.handle_event(sync_event(
        vec![serde_json::json!({"role": "user", "content": "hi"})],
        Some(uncommitted),
        vec![],
        vec![],
        Some(utc_ago(5)),
    ));

    assert!(app.turn.working, "mid-turn resume must enter working state");
    let started = app.turn.started_at.expect("started_at set");
    let elapsed = started.elapsed().as_secs();
    assert!(
        (3..=7).contains(&elapsed),
        "elapsed should reflect turn_started_at (~5s), got {elapsed}s"
    );
    // uncommitted rendered via replay_messages (assistant cell present)
    assert!(cell_kinds(&app).contains(&"assistant"));
}

#[test]
fn test_sync_working_status_with_empty_projections_restores_working() {
    // The regression: the turn is in flight but nothing is finalized yet — the
    // first LLM call of a round is still in flight (TTFT / compaction await) or
    // we are exactly at a round boundary. `uncommitted` / `uncommitted_tools`
    // are empty in *both* phases, yet the snapshot says `working`; taking
    // "empty projection" for "idle" left the spinner / title / timer off while
    // live events kept arriving.
    let mut app = test_app();
    let _ = app.drain_intents();
    app.handle_event(sync_event_with_status(
        SessionStatus::Working,
        vec![serde_json::json!({"role": "user", "content": "hi"})],
        None,
        vec![],
        vec![],
        Some(utc_ago(5)),
    ));

    assert!(
        app.turn.working,
        "a working status must enter working state even with empty projections"
    );
    let started = app.turn.started_at.expect("started_at set");
    let elapsed = started.elapsed().as_secs();
    assert!(
        (3..=7).contains(&elapsed),
        "elapsed should reflect turn_started_at (~5s), got {elapsed}s"
    );
    // The terminal title switched to the working frame (spinner glyph + wing).
    let title = app.drain_intents().into_iter().find_map(|i| match i {
        AppIntent::SetTitle(t) => Some(t),
        _ => None,
    });
    let title = title.expect("working resume sets the title");
    assert!(
        title.starts_with('⠋') && title.ends_with("wing"),
        "title should carry a spinner frame, got: {title}"
    );
}

#[test]
fn test_sync_waiting_status_restores_working() {
    // A pending ask: the turn is in flight, blocked on user input. Live parity
    // — the live path keeps the spinner up while the ask panel is open.
    let mut app = test_app();
    app.handle_event(sync_event_with_status(
        SessionStatus::Waiting,
        vec![],
        None,
        vec![],
        vec![],
        Some(utc_ago(2)),
    ));

    assert!(app.turn.working, "waiting is a turn in flight");
}

#[test]
fn test_sync_idle_status_beats_content_presence() {
    // The status is authoritative: a snapshot that says `idle` must not enter
    // working just because a projection is present (the content is still
    // replayed — it is the *turn state* that follows the status).
    let mut app = test_app();
    app.handle_event(sync_event_with_status(
        SessionStatus::Idle,
        vec![],
        Some(serde_json::json!({"role": "assistant", "content": "committed elsewhere"})),
        vec![],
        vec![],
        None,
    ));

    assert!(!app.turn.working, "idle status wins over content presence");
    assert!(app.turn.started_at.is_none());
    assert!(
        cell_kinds(&app).contains(&"assistant"),
        "content still replays"
    );
}

#[test]
fn test_sync_idle_status_clears_the_previous_turns_working_state() {
    // The mirror image of the regression: a full view replacement under a
    // running turn (session switch / reconnect) with an idle snapshot. The
    // previous session's `Done` never arrives (foreign-session events are
    // filtered), so the snapshot has to clear spinner / elapsed / title.
    let mut app = test_app();
    app.handle_event(turn_started_event());
    assert!(app.turn.working, "turn started");
    let _ = app.drain_intents();

    app.handle_event(sync_event_with_status(
        SessionStatus::Idle,
        vec![],
        None,
        vec![],
        vec![],
        None,
    ));

    assert!(!app.turn.working, "idle snapshot must clear working state");
    assert!(app.turn.started_at.is_none(), "elapsed anchor cleared");
    let title = app.drain_intents().into_iter().find_map(|i| match i {
        AppIntent::SetTitle(t) => Some(t),
        _ => None,
    });
    assert_eq!(
        title.as_deref(),
        Some("☾ wing"),
        "title must go back to idle"
    );
}

#[test]
fn test_sync_titles_use_the_new_sessions_workdir() {
    // The title carries the workdir suffix: when a sync restates the turn state
    // it must be the *new* session's label — the metadata travels in the same
    // snapshot and is restored before the title is composed (an idle sync has
    // no spinner tick to self-correct on).
    let mut app = test_app();
    app.handle_event(turn_started_event());
    let _ = app.drain_intents();

    let mut sync = sync_event_with_status(SessionStatus::Idle, vec![], None, vec![], vec![], None);
    if let WingEvent::SyncSession { agent, .. } = &mut sync {
        *agent = Some(Box::new(crate::protocol::AgentInfo {
            model_name: "test-model".into(),
            system_prompt: None,
            tools: vec![],
            skills: vec![],
            rules: vec![],
            workspace: Some("/home/me/other-project".into()),
            provider_name: None,
            model_display_name: None,
        }));
    }
    app.handle_event(sync);

    let title = app
        .drain_intents()
        .into_iter()
        .find_map(|i| match i {
            AppIntent::SetTitle(t) => Some(t),
            _ => None,
        })
        .expect("idle title restored");
    assert_eq!(
        title, "☾ wing [other-project]",
        "title must use the new session's workdir"
    );
    assert_eq!(
        app.status.workdir.as_deref(),
        Some("/home/me/other-project"),
        "the snapshot's metadata is restored"
    );
}

#[test]
fn test_sync_uncommitted_tools_alone_restores_working() {
    // uncommitted_tools non-empty (args still streaming) → working too.
    let mut app = test_app();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![serde_json::json!({
            "tool_call_id": "tc-1",
            "tool_name": "Bash",
            "args_fragment": "{\"command\": \"sl",
        })],
        vec![],
        Some(utc_ago(2)),
    ));
    assert!(app.turn.working);
    // The streaming tool cell was built via the live ToolCallStream branch.
    assert!(cell_kinds(&app).contains(&"tool_call"));
}

#[test]
fn test_sync_idle_session_not_working() {
    // 6.13: idle session (no uncommitted, no tools) → stays idle.
    let mut app = test_app();
    app.handle_event(sync_event(
        vec![serde_json::json!({"role": "user", "content": "done earlier"})],
        None,
        vec![],
        vec![],
        None,
    ));
    assert!(
        !app.turn.working,
        "idle resume must NOT enter working state"
    );
    assert!(app.turn.started_at.is_none());
}

/// sync_event with an AgentInfo attached (skills/rules banner source).
fn sync_event_with_agent(
    agent: Option<crate::protocol::AgentInfo>,
    messages: Vec<serde_json::Value>,
) -> WingEvent {
    let mut ev = sync_event(messages, None, vec![], vec![], None);
    if let WingEvent::SyncSession {
        agent: agent_slot, ..
    } = &mut ev
    {
        *agent_slot = agent.map(Box::new);
    }
    ev
}

#[test]
fn test_sync_renders_skills_rules_banner() {
    // Loaded-skills/rules summary: one SystemMessage at the top of the
    // chat, counts from AgentInfo, details left to /skills.
    let mut app = test_app();
    let agent = crate::protocol::AgentInfo {
        model_name: "test-model".into(),
        system_prompt: None,
        tools: vec![],
        skills: vec!["pdf".into(), "webapp".into()],
        rules: vec!["AGENTS.md".into()],
        workspace: None,
        provider_name: None,
        model_display_name: None,
    };
    app.handle_event(sync_event_with_agent(
        Some(agent),
        vec![serde_json::json!({"role": "user", "content": "hi"})],
    ));

    match app.chat.cells.first().map(|c| c.cell()) {
        Some(ChatCell::SystemMessage(text)) => {
            assert!(
                text.contains("loaded 2 skills, 1 rules"),
                "banner should carry counts, got: {text}"
            );
            assert!(
                text.contains("/skills"),
                "banner should hint the details command, got: {text}"
            );
        }
        other => panic!("expected SystemMessage banner as first cell, got {other:?}"),
    }
    // Replay content is untouched behind the banner.
    assert!(cell_kinds(&app).contains(&"user"));
}

#[test]
fn test_sync_without_agent_omits_banner() {
    // agent: None (e.g. older gateway) → no banner, replay only.
    let mut app = test_app();
    app.handle_event(sync_event_with_agent(
        None,
        vec![serde_json::json!({"role": "user", "content": "hi"})],
    ));
    assert!(
        !cell_kinds(&app).contains(&"system"),
        "no agent info → no banner"
    );
}

#[test]
fn test_sync_restores_model_display_name() {
    // The snapshot's agent carries the declared display label: the status bar
    // renders it, while `model` keeps the call name (identity).
    let mut app = test_app();
    let agent = crate::protocol::AgentInfo {
        model_name: "dfmodel-2026".into(),
        system_prompt: None,
        tools: vec![],
        skills: vec![],
        rules: vec![],
        workspace: None,
        provider_name: Some("qoder".into()),
        model_display_name: Some("DeepSeek-Flash".into()),
    };
    app.handle_event(sync_event_with_agent(Some(agent), vec![]));
    assert_eq!(app.status.model, "dfmodel-2026");
    assert_eq!(
        app.status.model_display_name.as_deref(),
        Some("DeepSeek-Flash")
    );
    assert_eq!(app.status.model_label(), "DeepSeek-Flash");
    assert_eq!(app.status.provider.as_deref(), Some("qoder"));
}

#[test]
fn test_session_state_changed_prefers_display_name_and_tolerates_absent() {
    let mut app = test_app();
    let meta = || crate::protocol::EventMeta {
        created_at: "2026-01-01T00:00:00+00:00".into(),
        session_id: Some("test-session".into()),
        request_id: "r".into(),
    };

    // New gateway: label travels with the model.
    app.handle_event(WingEvent::SessionStateChanged {
        model: Some("dfmodel-2026".into()),
        model_display_name: Some("DeepSeek-Flash".into()),
        thinking: None,
        reasoning_effort: None,
        yolo: None,
        title: None,
        agent: None,
        meta: meta(),
    });
    assert_eq!(app.status.model_label(), "DeepSeek-Flash");

    // Old gateway / undeclared model: model changes, label resets to the call
    // name (a stale label must not describe the new model).
    app.handle_event(WingEvent::SessionStateChanged {
        model: Some("plain-model".into()),
        model_display_name: None,
        thinking: Some(true),
        reasoning_effort: None,
        yolo: None,
        title: None,
        agent: None,
        meta: meta(),
    });
    assert_eq!(app.status.model, "plain-model");
    assert_eq!(app.status.model_display_name, None);
    assert_eq!(app.status.model_label(), "plain-model");
    assert!(app.status.thinking);

    // Model untouched (e.g. yolo toggle): the label stays with its model.
    app.handle_event(WingEvent::SessionStateChanged {
        model: None,
        model_display_name: None,
        thinking: None,
        reasoning_effort: None,
        yolo: Some(true),
        title: None,
        agent: None,
        meta: meta(),
    });
    assert_eq!(app.status.model, "plain-model");
    assert!(app.status.yolo);
}

#[test]
fn test_session_state_changed_falls_back_to_the_local_label_when_omitted() {
    // Old gateway: `/api/models` already ships the declared label, but
    // `session_state_changed` predates the field. The label the user has
    // already seen (optimistic toast / status) must not be dropped to the raw
    // call name just because the event cannot carry it — and no label may be
    // invented for models the snapshot does not know.
    let mut app = test_app();
    app.status.provider = Some("qoder".into());
    app.model_sources = vec![model_group_with_labels(
        "qoder",
        &["dfmodel"],
        &[("dfmodel", "DeepSeek-Flash")],
    )];
    let meta = crate::protocol::EventMeta {
        created_at: "2026-01-01T00:00:00+00:00".into(),
        session_id: Some("test-session".into()),
        request_id: "r".into(),
    };

    app.handle_event(WingEvent::SessionStateChanged {
        model: Some("dfmodel".into()),
        model_display_name: None,
        thinking: None,
        reasoning_effort: None,
        yolo: None,
        title: None,
        agent: None,
        meta: meta.clone(),
    });
    assert_eq!(app.status.model, "dfmodel");
    assert_eq!(
        app.status.model_display_name.as_deref(),
        Some("DeepSeek-Flash"),
        "a known label must survive a gateway that cannot ship it"
    );

    // The local snapshot has nothing for this model → no invented label.
    app.handle_event(WingEvent::SessionStateChanged {
        model: Some("mystery".into()),
        model_display_name: None,
        thinking: None,
        reasoning_effort: None,
        yolo: None,
        title: None,
        agent: None,
        meta,
    });
    assert_eq!(app.status.model_display_name, None);
}

#[test]
fn test_sync_clock_skew_clamps_elapsed() {
    // 6.13: turn_started_at in the future (clock skew) → clamp to ~zero,
    // no panic / wrap-around to a huge value.
    let mut app = test_app();
    app.handle_event(sync_event(
        vec![],
        Some(serde_json::json!({"role": "assistant", "content": "x"})),
        vec![],
        vec![],
        Some(utc_ago(-3600)), // 1h in the future
    ));
    assert!(app.turn.working);
    let started = app.turn.started_at.expect("started_at set");
    assert!(
        started.elapsed().as_secs() < 2,
        "future timestamp must clamp elapsed to ~0, not wrap"
    );
}

#[test]
fn test_sync_clears_stale_ask_panels_before_replay() {
    // Reconnect / session switch while an ask is pending: the
    // live-registered panel must be cleared before the replayed pending
    // ask re-registers — otherwise the deque holds a duplicate and a
    // later answer pops the stale front entry (routed to a dead
    // tool_call_id, swallowing the next ask's answer).
    let mut app = test_app();
    // Simulate a live ask registered before the disconnect.
    app.ask_panels.push_back(AskPanel::new(
        "ask-live".into(),
        vec![AskQuestion {
            id: "q0".into(),
            question: "old session question?".into(),
            header: String::new(),
            multi_select: false,
            options: vec![],
            choices: vec![],
        }],
    ));
    // Re-sync: the still-pending ask is replayed (backend filter keeps
    // it) and re-registered.
    let events = vec![serde_json::json!({
        "type": "ask",
        "tool_call_id": "ask-live",
        "questions": [{"id": "q1", "question": "proceed?", "choices": []}],
    })];
    app.handle_event(sync_event(vec![], None, vec![], events, None));
    assert_eq!(
        app.ask_panels.len(),
        1,
        "replayed ask must be the only registered panel"
    );
    assert_eq!(app.ask_panels[0].tool_call_id, "ask-live");
    // And the replayed payload is the one registered (fresh question).
    assert_eq!(app.ask_panels[0].questions[0].id, "q1");
}

#[test]
fn test_sync_diff_anchors_to_uncommitted_tool_call() {
    // 6.12 (P1-1 regression lock): the tool_use that produced a diff is a
    // *finalized* block → it is in the uncommitted projection → replayed
    // (step 2) before events (step 4) → the diff anchors AFTER its ToolCall
    // cell, not at the tail.
    let mut app = test_app();
    let uncommitted = serde_json::json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{"id": "tc-edit", "name": "Edit", "arguments": {"path": "main.rs"}}],
    });
    let events = vec![serde_json::json!({
        "type": "diff_content",
        "path": "main.rs",
        "old_text": null,
        "new_text": "fn main() {}",
        "tool_call_id": "tc-edit",
    })];
    app.handle_event(sync_event(
        vec![serde_json::json!({"role": "user", "content": "edit it"})],
        Some(uncommitted),
        vec![],
        events,
        Some(utc_ago(1)),
    ));
    // Order: user → ToolCall(from uncommitted) → Diff(anchored, not tail-appended
    // — here tail and anchored coincide, so assert the adjacency explicitly).
    let kinds = cell_kinds(&app);
    assert_eq!(kinds, vec!["user", "tool_call", "diff"]);
}

#[test]
fn test_direct_diff_event_renders_window_with_absolute_lines() {
    // The live `diff_content` branch consumes the windowed payload the
    // same way replay does: window rows verbatim, absolute gutter numbers.
    let mut app = test_app();
    app.handle_event(WingEvent::DiffContent {
        path: "main.rs".into(),
        old_text: Some("line 7\nline 8\nline 9\nline 10\nline 11".into()),
        new_text: "line 7\nline 8\nline 9\nLINE TEN\nline 11".into(),
        old_start_line: 7,
        new_start_line: 7,
        tool_call_id: "tc-edit".into(),
        meta: crate::protocol::EventMeta {
            created_at: "2026-01-01T00:00:00".into(),
            session_id: None,
            request_id: "r1".into(),
        },
    });

    let ChatCell::Diff(diff) = app.chat.cells[0].cell() else {
        panic!("expected a Diff cell");
    };
    assert_eq!((diff.old_start_line, diff.new_start_line), (7, 7));
    let text: String = diff
        .to_lines(&crate::config::ThemePalette::default(), 80)
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("@@ -7,5 +7,5 @@"), "{text}");
    assert!(text.contains("    7   7 │   line 7"), "{text}");
    assert!(text.contains("       10 │ + LINE TEN"), "{text}");
}

#[test]
fn test_direct_diff_event_repeats_anchor_under_one_tool_call() {
    // `replace_all` sends one event per match position with the same
    // tool_call_id: each lands after its ToolCall (and after the previous
    // diff sibling), preserving emission order.
    let mut app = test_app();
    app.handle_event(WingEvent::ToolCall {
        tool_name: "Edit".into(),
        tool_args: serde_json::json!({"path": "f.txt"}),
        tool_call_id: "tc-all".into(),
        meta: crate::protocol::EventMeta {
            created_at: "2026-01-01T00:00:00".into(),
            session_id: None,
            request_id: "r0".into(),
        },
    });
    for (i, line) in [10usize, 20, 30].into_iter().enumerate() {
        app.handle_event(WingEvent::DiffContent {
            path: "f.txt".into(),
            old_text: Some(format!("old {i}")),
            new_text: format!("NEW {i}"),
            old_start_line: line,
            new_start_line: line,
            tool_call_id: "tc-all".into(),
            meta: crate::protocol::EventMeta {
                created_at: "2026-01-01T00:00:00".into(),
                session_id: None,
                request_id: format!("r{}", i + 1),
            },
        });
    }

    assert_eq!(cell_kinds(&app), vec!["tool_call", "diff", "diff", "diff"]);
    let starts: Vec<usize> = app
        .chat
        .cells
        .iter()
        .filter_map(|c| match c.cell() {
            ChatCell::Diff(d) => Some(d.old_start_line),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![10, 20, 30]);
}

#[test]
fn test_sync_three_segment_mixed_order() {
    // 6.12: messages → uncommitted → events across a multi-round turn.
    // Committed round (messages) renders first; the in-progress round
    // (uncommitted) next; diffs anchor to whichever ToolCall they belong to.
    let mut app = test_app();
    let messages = vec![
        serde_json::json!({"role": "user", "content": "do two edits"}),
        serde_json::json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "tc-a", "name": "Edit", "arguments": {"path": "a"}}],
        }),
        serde_json::json!({"role": "tool", "tool_call_id": "tc-a", "content": "ok"}),
    ];
    let uncommitted = serde_json::json!({
        "role": "assistant", "content": "",
        "tool_calls": [{"id": "tc-b", "name": "Edit", "arguments": {"path": "b"}}],
    });
    // events in chain order: diff for the committed tc-a AND the in-progress tc-b
    let events = vec![
        serde_json::json!({
            "type": "diff_content", "path": "b", "old_text": null,
            "new_text": "B", "tool_call_id": "tc-b",
        }),
        serde_json::json!({
            "type": "diff_content", "path": "a", "old_text": null,
            "new_text": "A", "tool_call_id": "tc-a",
        }),
    ];
    app.handle_event(sync_event(
        messages,
        Some(uncommitted),
        vec![],
        events,
        Some(utc_ago(1)),
    ));
    // Each diff lands directly after its own ToolCall regardless of event order.
    let kinds = cell_kinds(&app);
    assert_eq!(
        kinds,
        vec!["user", "tool_call", "diff", "tool_call", "diff"]
    );
}

#[test]
fn test_replay_matches_live_cell_sequence_across_two_rounds() {
    // Live-vs-replay parity for the common shape: two rounds that each
    // announce their tool calls in text. The TUI's own tests above assert the
    // replay sequence; this one drives the *live* handlers as well, so a drift
    // in the shared `push` rule (separator placement included) cannot slip
    // through one-sided assertions.
    let meta = crate::protocol::EventMeta {
        created_at: "2026-01-01T00:00:00".into(),
        session_id: None,
        request_id: "r".into(),
    };

    let mut live = test_app();
    live.handle_event(WingEvent::Reasoning {
        content: "turn one".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::Text {
        content: "Listing the directory.".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::ToolCall {
        tool_name: "Bash".into(),
        tool_args: serde_json::json!({"command": "ls"}),
        tool_call_id: "tc-ls".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::ToolCallResult {
        tool_name: "Bash".into(),
        tool_args: serde_json::json!({"command": "ls"}),
        tool_call_id: "tc-ls".into(),
        tool_result: "a.txt".into(),
        tool_success: true,
        model: "test-model".into(),
        tool_media: Vec::new(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::Reasoning {
        content: "turn two".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::Text {
        content: "Now reading a.txt.".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::ToolCall {
        tool_name: "Read".into(),
        tool_args: serde_json::json!({"path": "a.txt"}),
        tool_call_id: "tc-read".into(),
        meta,
    });

    let mut replayed = test_app();
    replayed.handle_event(sync_event(
        vec![
            serde_json::json!({
                "role": "assistant",
                "content": "Listing the directory.",
                "reasoning_content": "turn one",
                "tool_calls": [{"id": "tc-ls", "name": "Bash", "arguments": {"command": "ls"}}],
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "tc-ls", "content": "a.txt"}),
            serde_json::json!({
                "role": "assistant",
                "content": "Now reading a.txt.",
                "reasoning_content": "turn two",
                "tool_calls": [{"id": "tc-read", "name": "Read", "arguments": {"path": "a.txt"}}],
            }),
        ],
        None,
        vec![],
        vec![],
        None,
    ));

    assert_eq!(
        cell_kinds(&live),
        vec![
            "thinking",
            "assistant",
            "tool_call",
            "separator", // turn boundary
            "thinking",
            "assistant",
            "tool_call",
        ]
    );
    assert_eq!(cell_kinds(&replayed), cell_kinds(&live));
}

#[test]
fn test_replay_matches_live_cell_sequence_after_an_anchored_diff() {
    // Replay ≠ live parity for the diff case: resume runs the message pass
    // (and its ReAct separator) before the events pass anchors the diff, so
    // the separator is born where live — the diff already sitting on the
    // tool card when the next round's text arrives — never grows one.
    let meta = crate::protocol::EventMeta {
        created_at: "2026-01-01T00:00:00".into(),
        session_id: None,
        request_id: "r".into(),
    };

    // Live: tool call → its diff → the next round's reasoning + text.
    let mut live = test_app();
    live.handle_event(WingEvent::ToolCall {
        tool_name: "Edit".into(),
        tool_args: serde_json::json!({"path": "a.rs"}),
        tool_call_id: "tc-edit".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::DiffContent {
        path: "a.rs".into(),
        old_text: Some("old".into()),
        new_text: "new".into(),
        old_start_line: 1,
        new_start_line: 1,
        tool_call_id: "tc-edit".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::Reasoning {
        content: "turn two".into(),
        meta: meta.clone(),
    });
    live.handle_event(WingEvent::Text {
        content: "Now verify.".into(),
        meta,
    });

    // Replay: the same conversation through SyncSession.
    let mut replayed = test_app();
    replayed.handle_event(sync_event(
        vec![
            serde_json::json!({
                "role": "assistant", "content": "",
                "tool_calls": [{"id": "tc-edit", "name": "Edit", "arguments": {"path": "a.rs"}}],
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "tc-edit", "content": "ok"}),
            serde_json::json!({
                "role": "assistant",
                "content": "Now verify.",
                "reasoning_content": "turn two",
            }),
        ],
        None,
        vec![],
        vec![serde_json::json!({
            "type": "diff_content", "path": "a.rs", "old_text": "old",
            "new_text": "new", "tool_call_id": "tc-edit",
        })],
        None,
    ));

    assert_eq!(
        cell_kinds(&live),
        vec!["tool_call", "diff", "thinking", "assistant"]
    );
    assert_eq!(cell_kinds(&replayed), cell_kinds(&live));
}

#[test]
fn test_sync_replays_pending_ask_answerable() {
    // 6.14: a pending ask replays as an Ask cell AND registers the reply
    // panel, so the user can answer it (resolves the backend waiter).
    use crossterm::event::KeyCode;

    let mut app = test_app();
    let events = vec![serde_json::json!({
        "type": "ask",
        "tool_call_id": "ask-1",
        "questions": [{"id": "q1", "question": "proceed?", "choices": ["y", "n"]}],
    })];
    app.handle_event(sync_event(vec![], None, vec![], events, None));

    assert!(cell_kinds(&app).contains(&"ask"), "ask cell rendered");
    // Answerable: the panel is registered with the right id, and the
    // legacy choices were normalized into options.
    assert_eq!(app.ask_panels.len(), 1);
    assert_eq!(app.ask_panels[0].tool_call_id, "ask-1");
    assert_eq!(app.ask_panels[0].questions[0].options.len(), 2);
    assert_eq!(app.ask_panels[0].mode, PanelMode::Question);
    // The placeholder says the app is waiting for an answer above.
    assert_eq!(app.input.placeholder, "Answering above · Esc to interrupt");

    // Answer it through the keys: commit the first option, then Submit — the
    // reply goes out addressed by the replayed tool_call_id (same channel as
    // the live path).
    app.handle_key(key(KeyCode::Enter)); // commit "y" + advance to confirm
    app.handle_key(key(KeyCode::Enter)); // Submit
    let sent = app.drain_intents().into_iter().find_map(|i| match i {
        AppIntent::SendMessage {
            content,
            tool_call_id,
            ..
        } => Some((content, tool_call_id)),
        _ => None,
    });
    assert_eq!(sent, Some(("q1: y".to_string(), Some("ask-1".into()))));
    assert!(app.ask_panels.is_empty(), "panel popped after answering");
}

#[test]
fn test_live_ask_event_renders_and_registers_a_question_panel() {
    // The live `WingEvent::Ask` branch normalizes through the same entry as
    // replay: the AskUserQuestion shape → a registered `Question` panel.
    let mut app = test_app();
    app.handle_event(WingEvent::Ask {
        tool_call_id: "ask-live".into(),
        questions: vec![AskQuestion {
            id: "q1".into(),
            header: "H".into(),
            question: "proceed?".into(),
            multi_select: false,
            options: Vec::new(),
            choices: vec!["y".into(), "n".into()],
        }],
        question: String::new(),
        choices: Vec::new(),
        required: false,
        meta: event_meta(),
    });

    assert!(cell_kinds(&app).contains(&"ask"), "ask cell rendered");
    assert_eq!(app.ask_panels.len(), 1);
    assert_eq!(app.ask_panels[0].mode, PanelMode::Question);
    assert_eq!(app.ask_panels[0].tool_call_id, "ask-live");
    assert_eq!(
        app.ask_panels[0].questions[0].options.len(),
        2,
        "legacy choices are normalized into options"
    );
}

#[test]
fn test_live_legacy_required_ask_becomes_a_required_choice_panel() {
    // The retired Bash confirmation shape (still what the backend sends) is
    // normalized into a required-choice panel — the same model and reply
    // channel as every other ask.
    let mut app = test_app();
    app.handle_event(WingEvent::Ask {
        tool_call_id: "ask-bash".into(),
        questions: Vec::new(),
        question: "⚠️ Dangerous command detected:\n```bash\nrm -rf /\n```\nProceed?".into(),
        choices: vec!["y".into(), "n".into(), "yolo".into()],
        required: true,
        meta: event_meta(),
    });

    assert!(cell_kinds(&app).contains(&"ask"), "ask cell rendered");
    assert_eq!(app.ask_panels.len(), 1, "registered for answering");
    assert_eq!(app.ask_panels[0].mode, PanelMode::RequiredChoice);
    assert_eq!(app.ask_panels[0].tool_call_id, "ask-bash");
    assert_eq!(app.ask_panels[0].questions[0].options.len(), 3);
}

#[test]
fn test_live_legacy_non_required_ask_stays_a_static_notice() {
    // A retired ask that is not required is not something the user can answer:
    // it renders, but must never take the keyboard or reply.
    let mut app = test_app();
    app.handle_event(WingEvent::Ask {
        tool_call_id: "ask-notice".into(),
        questions: Vec::new(),
        question: "heads up".into(),
        choices: vec!["a".into()],
        required: false,
        meta: event_meta(),
    });

    assert!(cell_kinds(&app).contains(&"ask"), "ask cell rendered");
    assert!(app.ask_panels.is_empty(), "not registered for answering");
    assert_eq!(app.modal_owner(), None, "no modal took the keyboard");
}

#[test]
fn test_sync_replays_legacy_required_ask_as_a_required_choice_panel() {
    // A pending retired ask arrives again on resume; replay normalizes it
    // through the same entry, so the card is answerable — with the bare
    // option label the backend expects.
    let mut app = test_app();
    let events = vec![serde_json::json!({
        "type": "ask",
        "tool_call_id": "ask-2",
        "question": "dangerous command, proceed?",
        "choices": ["yes", "no"],
        "required": true,
    })];
    app.handle_event(sync_event(vec![], None, vec![], events, None));
    assert!(cell_kinds(&app).contains(&"ask"));
    assert_eq!(app.ask_panels.len(), 1);
    assert_eq!(app.ask_panels[0].tool_call_id, "ask-2");
    assert_eq!(app.ask_panels[0].mode, PanelMode::RequiredChoice);
    assert_eq!(app.ask_panels[0].questions[0].options[0].label, "yes");
}

#[test]
fn test_sync_replays_legacy_non_required_ask_without_registering_it() {
    let mut app = test_app();
    let events = vec![serde_json::json!({
        "type": "ask",
        "tool_call_id": "ask-3",
        "question": "heads up",
        "choices": ["a", "b"],
    })];
    app.handle_event(sync_event(vec![], None, vec![], events, None));
    assert!(cell_kinds(&app).contains(&"ask"), "still displayed");
    assert!(app.ask_panels.is_empty(), "never answerable");
}

fn event_meta() -> crate::protocol::EventMeta {
    crate::protocol::EventMeta {
        created_at: "2025-01-01T00:00:00".into(),
        session_id: Some("test-session".into()),
        request_id: "evt-req".into(),
    }
}

fn accepted_event(origin_request_id: &str) -> WingEvent {
    WingEvent::UserMessageAccepted {
        content: String::new(),
        origin_request_id: origin_request_id.into(),
        meta: event_meta(),
    }
}

#[test]
fn test_accepted_event_promotes_pending() {
    let mut app = test_app();
    assert!(app.submit_message("hello"));
    let pending_id = app.chat.pending[0].request_id.clone();
    app.drain_intents();

    app.handle_event(accepted_event(&pending_id));

    assert!(app.chat.pending.is_empty());
    assert!(
        app.chat
            .cells
            .iter()
            .any(|c| matches!(c.cell(), ChatCell::UserMessage(s) if s == "hello"))
    );
}

#[test]
fn test_accepted_event_unknown_id_ignored() {
    let mut app = test_app();
    assert!(app.submit_message("hello"));
    app.drain_intents();

    app.handle_event(accepted_event("someone-elses-id"));

    // Not ours (another client's request id) — nothing changes.
    assert_eq!(app.chat.pending.len(), 1);
    assert!(app.chat.cells.is_empty());
}

#[test]
fn test_done_flushes_remaining_pending() {
    let mut app = test_app();
    assert!(app.submit_message("hello"));
    app.drain_intents();

    app.handle_event(WingEvent::Done { meta: event_meta() });

    assert!(app.chat.pending.is_empty());
    assert!(
        app.chat
            .cells
            .iter()
            .any(|c| matches!(c.cell(), ChatCell::UserMessage(s) if s == "hello"))
    );
}

#[test]
fn test_interrupted_discards_pending() {
    let mut app = test_app();
    assert!(app.submit_message("hello"));
    app.drain_intents();

    app.handle_event(WingEvent::Interrupted { meta: event_meta() });

    assert!(app.chat.pending.is_empty());
    assert!(
        app.chat
            .cells
            .iter()
            .any(|c| matches!(c.cell(), ChatCell::DiscardedUserMessage(s) if s == "hello")),
        "interrupted pending message must be committed as discarded, not lost"
    );
}

// ── In-app text selection ────────────────────────────────

// ── 思考块的冻结接线（Text / ToolCall / Done / Interrupted）──

/// 触发一次带推理的回合开头，返回思考块是否仍在计时。
fn thinking_active(app: &App) -> Option<bool> {
    app.chat
        .cells
        .iter()
        .find_map(|cached| match cached.cell() {
            ChatCell::Thinking(block) => Some(block.is_active()),
            _ => None,
        })
}

#[test]
fn test_reasoning_freezes_on_text_and_tool_events() {
    // 正文事件 = 思考阶段结束。
    let mut app = test_app();
    app.handle_event(WingEvent::Reasoning {
        content: "let me think".into(),
        meta: event_meta(),
    });
    assert_eq!(thinking_active(&app), Some(true), "推理中");
    app.handle_event(WingEvent::Text {
        content: "answer".into(),
        meta: event_meta(),
    });
    assert_eq!(thinking_active(&app), Some(false), "正文来了就定格");

    // 工具调用事件同样冻结（流式参数与权威调用两个入口）。
    for event in [
        WingEvent::ToolCallStream {
            tool_call_id: "tc".into(),
            tool_name: "Bash".into(),
            args_fragment: "{}".into(),
            is_final: true,
            meta: event_meta(),
        },
        WingEvent::ToolCall {
            tool_name: "Bash".into(),
            tool_args: serde_json::json!({"command": "ls"}),
            tool_call_id: "tc".into(),
            meta: event_meta(),
        },
    ] {
        let mut app = test_app();
        app.handle_event(WingEvent::Reasoning {
            content: "let me think".into(),
            meta: event_meta(),
        });
        app.handle_event(event);
        assert_eq!(thinking_active(&app), Some(false), "工具阶段就定格");
    }
}

#[test]
fn test_reasoning_freezes_on_turn_end_and_interrupt() {
    // 回合结束（Done）：finish_turn 兜底冻结。
    let mut app = test_app();
    app.handle_event(WingEvent::Reasoning {
        content: "long thinking".into(),
        meta: event_meta(),
    });
    app.handle_event(WingEvent::Done { meta: event_meta() });
    assert_eq!(thinking_active(&app), Some(false), "回合结束定格");

    // 打断（Interrupted）：同样定格，折叠行不该永远停在"思考中"。
    let mut app = test_app();
    app.handle_event(WingEvent::Reasoning {
        content: "long thinking".into(),
        meta: event_meta(),
    });
    app.handle_event(WingEvent::Interrupted { meta: event_meta() });
    assert_eq!(thinking_active(&app), Some(false), "打断定格");
}
