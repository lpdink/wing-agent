//! Goal orchestration lane — the App-side receiver of the Goal state machine.
//!
//! The state machine itself (`GoalState`, [`super::goal`]) is pure logic: it
//! never touches `App` and never performs I/O. This module is its counterpart
//! on the app side and the only place where Goal meets the rest of the TUI:
//!
//! * **session identity** — which session is the executor, which one is the
//!   checker, and whether an incoming event belongs to the Goal at all;
//! * **action execution** — translating `GoalAction`s into `AppIntent`s and
//!   chat mutations;
//! * **commands** — `/goal <prompt>` and `/goal-exit` (routed here by the
//!   command table in [`super::commands`]).
//!
//! Reading "the whole Goal path" therefore means reading `goal.rs` (decisions)
//! together with this file (effects).
//!
//! Call directions: [`super::commands`] routes `/goal` and `/goal-exit` here,
//! [`super::modal`] routes ask answers here, [`super::projection`] drives the
//! state machine with turn results / interrupts; this module only pushes
//! intents and chat cells (no lane calls back into it).

use super::App;
use super::AppIntent;
use super::goal;
use super::title;
use crate::ui::chat_view::ChatCell;
use crate::ui::toast::Toast;
use crate::util::title::AttentionKind;

impl App {
    /// Execute GoalActions produced by the GoalState state machine.
    ///
    /// Translates pure logic actions into AppIntents (side-effects) and
    /// chat mutations.
    pub(super) fn execute_goal_actions(&mut self, actions: Vec<goal::GoalAction>) {
        use goal::GoalAction;
        for action in actions {
            match action {
                GoalAction::SendToExecutor {
                    content,
                    tool_call_id,
                } => {
                    let session_id = self.session_id.clone();
                    self.push_intent(AppIntent::GoalSend {
                        session_id,
                        content,
                        tool_call_id,
                    });
                }
                GoalAction::SendToChecker {
                    content,
                    tool_call_id,
                } => {
                    if let Some(goal) = &self.goal
                        && let Some(checker_id) = &goal.checker_session_id
                    {
                        self.push_intent(AppIntent::GoalSend {
                            session_id: checker_id.clone(),
                            content,
                            tool_call_id,
                        });
                    }
                }
                GoalAction::CreateChecker { system_prompt } => {
                    self.push_intent(AppIntent::GoalCreateChecker { system_prompt });
                }
                GoalAction::PushSeparator { role, round } => {
                    self.chat.push(ChatCell::GoalSeparator { role, round });
                }
                GoalAction::GoalComplete { reason } => {
                    // Reset turn state now — the checker's follow-up Done event
                    // will be filtered out after goal exit, so finish_turn() would
                    // otherwise never run (working indicator + title stuck).
                    self.finish_turn();
                    self.push_intent(AppIntent::SetTitle(title::title_idle(
                        self.dir_label().as_deref(),
                    )));
                    let msg = if reason.is_empty() {
                        "Goal completed ✓".to_string()
                    } else {
                        format!("Goal completed ✓ — {reason}")
                    };
                    self.show_toast(Toast::info(msg, std::time::Duration::from_secs(5)));
                    self.notify_unfocused("Goal completed".into(), AttentionKind::Done);
                }
                GoalAction::Toast(msg) => {
                    self.show_toast(Toast::warning(msg, std::time::Duration::from_secs(4)));
                }
                GoalAction::ExitGoal => {
                    self.exit_goal();
                }
            }
        }
    }

    /// Exit Goal mode: unsubscribe checker, clear state.
    pub(super) fn exit_goal(&mut self) {
        if let Some(goal) = self.goal.take() {
            if let Some(checker_id) = goal.checker_session_id {
                self.push_intent(AppIntent::GoalUnsubscribe {
                    session_id: checker_id,
                });
            }
            self.status.goal_active = false;
        }
    }

    /// Check if a session_id belongs to the active Goal (checker session).
    pub(super) fn is_goal_session(&self, session_id: &str) -> bool {
        self.goal
            .as_ref()
            .and_then(|g| g.checker_session_id.as_deref())
            .is_some_and(|id| id == session_id)
    }

    /// Determine which Goal role a session_id corresponds to.
    pub(super) fn goal_role_for_session(&self, session_id: Option<&str>) -> Option<goal::GoalRole> {
        let goal = self.goal.as_ref()?;
        let sid = session_id?;
        if sid == self.session_id {
            Some(goal::GoalRole::Executor)
        } else if goal.checker_session_id.as_deref() == Some(sid) {
            Some(goal::GoalRole::Checker)
        } else {
            None
        }
    }
}

/// `/goal <prompt>` — activate Goal orchestration mode.
pub(super) fn start_goal_command(app: &mut App, text: &str) -> bool {
    if app.goal.is_some() {
        app.show_toast(Toast::warning(
            "Goal already active, use /goal-exit first",
            std::time::Duration::from_secs(3),
        ));
        return true;
    }

    let prompt = text.strip_prefix("/goal ").map(|s| s.trim()).unwrap_or("");
    if prompt.is_empty() {
        app.show_toast(Toast::warning(
            "Usage: /goal <prompt>",
            std::time::Duration::from_secs(3),
        ));
        return true;
    }

    let checker_prompt = app
        .config
        .goal
        .checker_system_prompt
        .clone()
        .unwrap_or_else(|| goal::DEFAULT_CHECKER_SYSTEM_PROMPT.to_string());

    let (state, actions) =
        goal::GoalState::new(app.session_id.clone(), prompt.to_string(), checker_prompt);
    app.goal = Some(state);
    app.status.goal_active = true;
    // Show the goal prompt as a user message in chat.
    app.chat
        .push(ChatCell::UserMessage(format!("/goal {prompt}")));
    app.execute_goal_actions(actions);
    true
}

/// `/goal-exit` — exit Goal orchestration mode.
pub(super) fn exit_goal_command(app: &mut App, _text: &str) -> bool {
    if app.goal.is_none() {
        app.show_toast(Toast::warning(
            "Goal not active",
            std::time::Duration::from_secs(2),
        ));
        return true;
    }
    app.exit_goal();
    app.show_toast(Toast::info(
        "Goal mode exited",
        std::time::Duration::from_secs(2),
    ));
    true
}
