//! Program-status (OSC 7501) projection tests — gateway events into report
//! intents.
//!
//! The lane is deliberately thin: every test here pins one mapping from a
//! `WingEvent` (or an ask interaction) to the report the terminal receives.

use super::support::*;
use crate::app::*;
use crate::protocol::AskQuestion;
use crate::protocol::EventMeta;
use crate::protocol::SessionStatus;
use crate::protocol::WingEvent;
use crate::util::program_status::BlockKind;
use crate::util::program_status::Reporter;
use crate::util::program_status::State;
use crate::util::program_status::format;

/// An app with reporting forced on — the production constructor reads
/// `WING_PROGRAM_STATUS` once, and the tests must not depend on the
/// developer's environment.
fn report_app() -> App {
    let mut app = test_app();
    app.program_status = Reporter::new(true);
    app
}

/// The program-status reports among the pending intents, in order.
fn reports(app: &mut App) -> Vec<String> {
    app.drain_intents()
        .into_iter()
        .filter_map(|intent| match intent {
            AppIntent::SetProgramStatus(report) => Some(report),
            _ => None,
        })
        .collect()
}

fn meta() -> EventMeta {
    EventMeta {
        created_at: "2026-01-01T00:00:00+00:00".into(),
        session_id: Some("test-session".into()),
        request_id: "r".into(),
    }
}

fn turn_started() -> WingEvent {
    WingEvent::TurnStarted { meta: meta() }
}

fn turn_result(is_error: bool, result: Option<&str>) -> WingEvent {
    WingEvent::TurnResult {
        uuid: String::new(),
        subtype: if is_error {
            "error_during_execution".into()
        } else {
            "success".into()
        },
        is_error,
        result: result.map(str::to_string),
        num_turns: 1,
        duration_ms: 1000,
        usage: None,
        errors: Vec::new(),
        meta: meta(),
    }
}

fn ask_question_event() -> WingEvent {
    WingEvent::Ask {
        tool_call_id: "ask-q".into(),
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
        meta: meta(),
    }
}

fn ask_required_choice_event() -> WingEvent {
    WingEvent::Ask {
        tool_call_id: "ask-bash".into(),
        questions: Vec::new(),
        question: "dangerous command, proceed?".into(),
        choices: vec!["y".into(), "n".into(), "yolo".into()],
        required: true,
        meta: meta(),
    }
}

// ── turn lifecycle ──

#[test]
fn turn_started_reports_working() {
    let mut app = report_app();
    app.handle_event(turn_started());
    assert_eq!(reports(&mut app), vec![format(State::Working, None)]);
}

#[test]
fn repeated_working_reports_once() {
    let mut app = report_app();
    app.handle_event(turn_started());
    assert_eq!(reports(&mut app), vec![format(State::Working, None)]);
    // A second `turn_started` within the same logical flow (an ask answer
    // resumes the turn) must not repeat the record.
    app.handle_event(turn_started());
    assert!(reports(&mut app).is_empty());
}

#[test]
fn turn_result_reports_done_with_the_notification_summary() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    app.handle_event(turn_result(false, Some("Created hello.rs")));
    assert_eq!(
        reports(&mut app),
        vec![format(State::Done, Some("1 turn · 1s\nCreated hello.rs"))]
    );
}

#[test]
fn failing_turn_result_reports_error() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    app.handle_event(turn_result(true, None));
    assert_eq!(
        reports(&mut app),
        vec![format(State::Error, Some("1 turn · 1s"))]
    );
}

#[test]
fn done_after_a_result_keeps_the_terminal_record() {
    let mut app = report_app();
    app.handle_event(turn_started());
    app.handle_event(turn_result(false, Some("ok")));
    let _ = reports(&mut app);
    app.handle_event(WingEvent::Done { meta: meta() });
    assert!(
        reports(&mut app).is_empty(),
        "`done` must not be replaced by `idle` — the result is unseen"
    );
}

#[test]
fn done_without_a_terminal_report_falls_back_to_idle() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    // A turn that ended without any `turn_result` (lost across a reconnect)
    // must not leave the record claiming work.
    app.handle_event(WingEvent::Done { meta: meta() });
    assert_eq!(reports(&mut app), vec![format(State::Idle, None)]);
}

#[test]
fn error_event_reports_error_and_done_keeps_it() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    app.handle_event(WingEvent::Error {
        message: "处理消息失败：异常：boom".into(),
        status_code: 500,
        error_code: None,
        detail: None,
        meta: meta(),
    });
    assert_eq!(
        reports(&mut app),
        vec![format(State::Error, Some("处理消息失败：异常：boom"))]
    );
    // The `Done` that closes this turn must not clobber the error record.
    app.handle_event(WingEvent::Done { meta: meta() });
    assert!(reports(&mut app).is_empty());
}

#[test]
fn interrupted_reports_idle() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    app.handle_event(WingEvent::Interrupted {
        dropped_request_ids: None,
        meta: meta(),
    });
    assert_eq!(reports(&mut app), vec![format(State::Idle, None)]);
}

// ── asks ──

#[test]
fn question_ask_reports_blocked_question() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    app.handle_event(ask_question_event());
    assert_eq!(
        reports(&mut app),
        vec![format(
            State::Blocked(BlockKind::Question),
            Some("proceed?")
        )]
    );
}

#[test]
fn required_choice_ask_reports_blocked_permission() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    // The retired Bash confirmation is an approval gate, not a question.
    app.handle_event(ask_required_choice_event());
    assert_eq!(
        reports(&mut app),
        vec![format(
            State::Blocked(BlockKind::Permission),
            Some("dangerous command, proceed?")
        )]
    );
}

#[test]
fn display_only_ask_reports_nothing() {
    let mut app = report_app();
    app.handle_event(turn_started());
    let _ = reports(&mut app);
    app.handle_event(WingEvent::Ask {
        tool_call_id: "ask-notice".into(),
        questions: Vec::new(),
        question: "heads up".into(),
        choices: vec!["a".into()],
        required: false,
        meta: meta(),
    });
    assert!(
        reports(&mut app).is_empty(),
        "a notice blocks nothing — it must not claim `blocked`"
    );
}

#[test]
fn answering_the_ask_reports_working_again() {
    let mut app = report_app();
    app.handle_event(turn_started());
    app.handle_event(ask_required_choice_event());
    let _ = reports(&mut app);
    app.finish_ask_panel("y".into());
    assert_eq!(
        reports(&mut app),
        vec![format(State::Working, None)],
        "the agent resumes on the answer"
    );
}

#[test]
fn a_queued_ask_keeps_blocking_after_the_answer() {
    // Concurrent tool calls can leave two asks queued; answering the front
    // one leaves the record blocked on the next, and only emptying the queue
    // goes back to `working`.
    let mut app = report_app();
    app.handle_event(turn_started());
    app.handle_event(ask_question_event());
    app.handle_event(ask_required_choice_event());
    let _ = reports(&mut app);

    app.finish_ask_panel("q1: y".into());
    assert_eq!(
        reports(&mut app),
        vec![format(
            State::Blocked(BlockKind::Permission),
            Some("dangerous command, proceed?")
        )],
        "the second panel becomes the front and the record follows it"
    );

    app.finish_ask_panel("y".into());
    assert_eq!(reports(&mut app), vec![format(State::Working, None)]);
}

// ── session snapshots ──

#[test]
fn idle_snapshot_reports_idle() {
    let mut app = report_app();
    app.handle_event(sync_event(vec![], None, vec![], vec![], None));
    assert_eq!(reports(&mut app), vec![format(State::Idle, None)]);
}

#[test]
fn working_snapshot_reports_working() {
    let mut app = report_app();
    app.handle_event(sync_event_with_status(
        SessionStatus::Working,
        vec![],
        Some(serde_json::json!({"role": "assistant", "content": "x"})),
        vec![],
        vec![],
        None,
    ));
    assert_eq!(reports(&mut app), vec![format(State::Working, None)]);
}

#[test]
fn snapshot_with_a_pending_ask_reports_blocked() {
    let mut app = report_app();
    let events = vec![serde_json::json!({
        "type": "ask",
        "tool_call_id": "ask-1",
        "questions": [{"id": "q1", "question": "proceed?", "choices": ["y", "n"]}],
    })];
    app.handle_event(sync_event(vec![], None, vec![], events, None));
    assert_eq!(
        reports(&mut app),
        vec![format(
            State::Blocked(BlockKind::Question),
            Some("proceed?")
        )],
        "a replayed ask queue is what the agent is blocked on"
    );
}

#[test]
fn all_whitespace_ask_text_is_dropped_from_the_message() {
    // A message that is nothing but whitespace / control characters is
    // dropped from the report; the state itself is still reported.
    let mut app = report_app();
    app.handle_event(WingEvent::Ask {
        tool_call_id: "ask-multi".into(),
        questions: Vec::new(),
        question: "\n\n  ".into(),
        choices: vec!["y".into()],
        required: true,
        meta: meta(),
    });
    assert_eq!(
        reports(&mut app),
        vec![format(State::Blocked(BlockKind::Permission), None)],
        "an all-whitespace message is dropped, the state still reported"
    );
}
