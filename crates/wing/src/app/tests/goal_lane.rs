//! Goal lane tests — session identity, roles, actions.

use super::support::*;
use crate::app::*;
use crate::protocol::AskQuestion;
use crate::protocol::EventMeta;
use crate::protocol::WingEvent;
use crate::shared::goal_role::GoalRole;
use crate::shared::panels::ask::AskPanel;
use crate::ui::chat_view::ChatCell;

/// An app in Goal mode with a created checker session.
fn app_in_goal_mode() -> App {
    let mut app = test_app();
    app.try_frontend_command("/goal build the thing");
    app.drain_intents();
    app.goal
        .as_mut()
        .expect("goal active")
        .on_checker_created("checker-session".into());
    app
}

/// Every goal intent produced so far.
fn goal_intents(app: &mut App) -> Vec<AppIntent> {
    app.drain_intents()
        .into_iter()
        .filter(|i| {
            matches!(
                i,
                AppIntent::GoalSend { .. }
                    | AppIntent::GoalCreateChecker { .. }
                    | AppIntent::GoalUnsubscribe { .. }
            )
        })
        .collect()
}

// ── Session identity ────────────────────────────────────────────────────

#[test]
fn test_goal_roles_are_session_scoped() {
    let app = app_in_goal_mode();
    assert_eq!(
        app.goal_role_for_session(Some(&app.session_id.clone())),
        Some(GoalRole::Executor),
        "the current session is the executor"
    );
    assert_eq!(
        app.goal_role_for_session(Some("checker-session")),
        Some(GoalRole::Checker)
    );
    assert_eq!(
        app.goal_role_for_session(Some("someone-else")),
        None,
        "unrelated sessions have no role"
    );
    assert_eq!(app.goal_role_for_session(None), None);

    assert!(app.is_goal_session("checker-session"));
    assert!(!app.is_goal_session(&app.session_id.clone()));
}

#[test]
fn test_checker_events_reach_the_projection_without_stealing_the_session() {
    let mut app = app_in_goal_mode();
    let before = app.session_id.clone();

    // A turn event from the checker session is projected (it drives the loop).
    app.handle_event(WingEvent::TurnStarted {
        meta: EventMeta {
            created_at: String::new(),
            session_id: Some("checker-session".into()),
            request_id: String::new(),
        },
    });
    assert!(
        app.turn.working,
        "checker events are projected in goal mode"
    );

    // A SyncSession from the checker must NOT replace the current session.
    let mut checker_sync = sync_event(vec![], None, vec![], vec![], None);
    if let WingEvent::SyncSession { session_id, .. } = &mut checker_sync {
        *session_id = "checker-session".into();
    }
    app.handle_event(checker_sync);
    assert_eq!(app.session_id, before, "the executor session stays current");
}

#[test]
fn test_unrelated_session_events_are_still_dropped_in_goal_mode() {
    let mut app = app_in_goal_mode();
    app.handle_event(WingEvent::TurnStarted {
        meta: EventMeta {
            created_at: String::new(),
            session_id: Some("someone-else".into()),
            request_id: String::new(),
        },
    });
    assert!(
        !app.turn.working,
        "only executor / checker sessions project"
    );
}

// ── Commands ────────────────────────────────────────────────────────────

#[test]
fn test_goal_command_starts_the_orchestration() {
    let mut app = test_app();
    assert!(app.try_frontend_command("/goal build the thing"));

    assert!(app.goal.is_some(), "goal state installed");
    assert!(app.status.goal_active, "status bar reflects goal mode");
    assert!(
        app.chat
            .cells
            .iter()
            .any(|c| matches!(c.cell(), ChatCell::UserMessage(t) if t == "/goal build the thing")),
        "the goal prompt is shown as a user message"
    );
    let intents = goal_intents(&mut app);
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, AppIntent::GoalCreateChecker { .. })),
        "checker creation requested: {intents:?}"
    );
    assert!(
        intents.iter().any(
            |i| matches!(i, AppIntent::GoalSend { session_id, .. } if *session_id == app.session_id)
        ),
        "the initial prompt goes to the executor"
    );
}

#[test]
fn test_bare_goal_shows_usage_and_stays_inactive() {
    let mut app = test_app();
    assert!(app.try_frontend_command("/goal"));
    assert!(app.goal.is_none());
    assert!(app.toast.is_some(), "usage hint shown");
    assert!(goal_intents(&mut app).is_empty());
}

#[test]
fn test_goal_command_refused_while_already_active() {
    let mut app = app_in_goal_mode();
    let round = app.goal.as_ref().unwrap().round;

    assert!(app.try_frontend_command("/goal another thing"));
    assert_eq!(
        app.goal.as_ref().unwrap().round,
        round,
        "the running goal is untouched"
    );
    assert!(app.toast.is_some(), "refusal hint shown");
    assert!(
        goal_intents(&mut app).is_empty(),
        "no second checker, no second prompt"
    );
}

#[test]
fn test_goal_exit_clears_state_and_unsubscribes_checker() {
    let mut app = app_in_goal_mode();
    assert!(app.try_frontend_command("/goal-exit"));

    assert!(app.goal.is_none(), "goal state cleared");
    assert!(!app.status.goal_active, "status bar reset");
    let intents = goal_intents(&mut app);
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, AppIntent::GoalUnsubscribe { session_id } if session_id == "checker-session")),
        "checker unsubscribed: {intents:?}"
    );
}

#[test]
fn test_goal_exit_without_goal_only_warns() {
    let mut app = test_app();
    assert!(app.try_frontend_command("/goal-exit"));
    assert!(app.goal.is_none());
    assert!(app.toast.is_some());
    assert!(goal_intents(&mut app).is_empty());
}

// ── Action execution ────────────────────────────────────────────────────

#[test]
fn test_goal_complete_finishes_the_turn_and_celebrates() {
    let mut app = app_in_goal_mode();
    app.turn.working = true;
    app.drain_intents();

    app.execute_goal_actions(vec![
        goal::GoalAction::GoalComplete {
            reason: "all green".into(),
        },
        goal::GoalAction::ExitGoal,
    ]);

    assert!(!app.turn.working, "the stall-proof turn reset ran");
    assert!(app.toast.is_some(), "completion toast shown");
    let intents = app.drain_intents();
    assert!(
        intents.iter().any(|i| matches!(i, AppIntent::SetTitle(_))),
        "title restored: {intents:?}"
    );
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, AppIntent::GoalUnsubscribe { .. })),
        "the ExitGoal action also ran"
    );
}

#[test]
fn test_goal_actions_are_translated_into_intents_and_cells() {
    let mut app = app_in_goal_mode();
    app.drain_intents();
    let cells = app.chat.cells.len();

    app.execute_goal_actions(vec![
        goal::GoalAction::PushSeparator {
            role: GoalRole::Checker,
            round: 1,
        },
        goal::GoalAction::SendToChecker {
            content: "review this".into(),
            tool_call_id: None,
        },
    ]);

    assert!(
        matches!(
            app.chat.cells.last().map(|c| c.cell()),
            Some(ChatCell::GoalSeparator {
                role: GoalRole::Checker,
                round: 1
            })
        ),
        "separator pushed into the transcript"
    );
    assert!(app.chat.cells.len() > cells);
    let intents = app.drain_intents();
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, AppIntent::GoalSend { session_id, .. } if session_id == "checker-session")),
        "checker send addressed to the checker session: {intents:?}"
    );
}

#[test]
fn test_ask_answer_routes_to_the_active_goal_role() {
    // Goal mode: an ask answered while the checker is working must reach the
    // checker session (not the executor), carrying the tool_call_id.
    let mut app = app_in_goal_mode();
    app.goal
        .as_mut()
        .unwrap()
        .on_turn_result(GoalRole::Executor, Some("done".into()));
    app.register_ask_panel(AskPanel::new(
        "ask-1".into(),
        vec![AskQuestion {
            id: "theme".into(),
            question: "which?".into(),
            header: String::new(),
            multi_select: false,
            options: Vec::new(),
            choices: Vec::new(),
        }],
    ));
    app.drain_intents();

    app.finish_ask_panel("y".into());

    let intents = app.drain_intents();
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, AppIntent::GoalSend { session_id, .. } if session_id == "checker-session")),
        "answer routed to the active role: {intents:?}"
    );
}
