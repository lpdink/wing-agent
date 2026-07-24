//! Goal orchestration — executor/checker loop state machine.
//!
//! Pure logic, no I/O. All methods return `Vec<GoalAction>` which the App
//! translates into intents (side-effects). This module can be removed entirely
//! to strip Goal support from the TUI.

/// Which agent is involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalRole {
    Executor,
    Checker,
}

impl GoalRole {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Executor => "Executor",
            Self::Checker => "Checker",
        }
    }

    pub fn working_verb(&self) -> &'static str {
        match self {
            Self::Executor => "working",
            Self::Checker => "reviewing",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Executor => "⚡",
            Self::Checker => "🔍",
        }
    }
}

/// Current phase of the Goal loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalPhase {
    /// Waiting for checker session to be created.
    CreatingChecker,
    /// Executor is processing.
    ExecutorWorking,
    /// Checker is reviewing.
    CheckerWorking,
    /// Interrupted by user (ESC). Records which role was interrupted.
    Interrupted(GoalRole),
    /// Goal completed successfully.
    Completed,
}

/// Actions produced by the state machine for the App to execute.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalAction {
    /// Send a message to the executor session.
    SendToExecutor {
        content: String,
        /// Set when this send answers a pending Ask event — routes to the
        /// ask's feedback waiter instead of the session inbox.
        tool_call_id: Option<String>,
    },
    /// Send a message to the checker session.
    SendToChecker {
        content: String,
        /// Set when this send answers a pending Ask event — routes to the
        /// ask's feedback waiter instead of the session inbox.
        tool_call_id: Option<String>,
    },
    /// Request checker session creation (with system prompt).
    CreateChecker { system_prompt: String },
    /// Push a separator cell into chat view.
    PushSeparator { role: GoalRole, round: u32 },
    /// Goal is complete.
    GoalComplete { reason: String },
    /// Show a toast message.
    Toast(String),
    /// Unsubscribe checker and exit Goal mode.
    ExitGoal,
}

/// Maximum format retries before warning the user.
const MAX_FORMAT_RETRIES: u32 = 2;

/// Goal orchestration state machine.
#[derive(Debug)]
pub struct GoalState {
    /// Current phase.
    pub phase: GoalPhase,
    /// Executor session ID (== app.session_id).
    pub executor_session_id: String,
    /// Checker session ID (set after creation).
    pub checker_session_id: Option<String>,
    /// The user's original goal prompt.
    goal_prompt: String,
    /// Appended user messages (accumulated).
    appends: Vec<String>,
    /// Messages appended while an agent is working (flushed on next round).
    pending_appends: Vec<String>,
    /// Current round number (starts at 1).
    pub round: u32,
    /// Consecutive format errors from checker.
    format_retries: u32,
    /// Checker system prompt (from config or default).
    checker_system_prompt: String,
}

impl GoalState {
    /// Create a new Goal state machine.
    ///
    /// Immediately produces actions to create the checker session and send
    /// the initial prompt to the executor.
    pub fn new(
        executor_session_id: String,
        goal_prompt: String,
        checker_system_prompt: String,
    ) -> (Self, Vec<GoalAction>) {
        let state = Self {
            phase: GoalPhase::CreatingChecker,
            executor_session_id,
            checker_session_id: None,
            goal_prompt,
            appends: Vec::new(),
            pending_appends: Vec::new(),
            round: 1,
            format_retries: 0,
            checker_system_prompt,
        };

        let actions = vec![
            GoalAction::CreateChecker {
                system_prompt: state.checker_system_prompt.clone(),
            },
            GoalAction::PushSeparator {
                role: GoalRole::Executor,
                round: 1,
            },
            GoalAction::SendToExecutor {
                content: state.build_prompt_user(),
                tool_call_id: None,
            },
        ];

        (state, actions)
    }

    /// Called when checker session creation succeeds.
    pub fn on_checker_created(&mut self, session_id: String) -> Vec<GoalAction> {
        self.checker_session_id = Some(session_id);
        self.phase = GoalPhase::ExecutorWorking;
        Vec::new()
    }

    /// Called when checker session creation fails.
    pub fn on_checker_create_failed(&mut self) -> Vec<GoalAction> {
        self.phase = GoalPhase::Completed;
        vec![GoalAction::ExitGoal]
    }

    /// Handle a TurnResult event from either agent.
    pub fn on_turn_result(&mut self, role: GoalRole, result: Option<String>) -> Vec<GoalAction> {
        // Phase guard: only process when the corresponding agent is working.
        let expected_phase = match role {
            GoalRole::Executor => GoalPhase::ExecutorWorking,
            GoalRole::Checker => GoalPhase::CheckerWorking,
        };
        if self.phase != expected_phase {
            return Vec::new();
        }

        // Flush pending appends into the main appends list.
        self.flush_pending_appends();

        match role {
            GoalRole::Executor => self.on_executor_done(result),
            GoalRole::Checker => self.on_checker_done(result),
        }
    }

    /// Handle an Interrupted event.
    ///
    /// Only transitions if the interrupted role matches the current working phase.
    /// This guards against no-op interrupts from the idle session (backend always
    /// emits InterruptedEvent regardless of whether the agent was busy).
    pub fn on_interrupted(&mut self, role: GoalRole) -> Vec<GoalAction> {
        let dominated = matches!(
            (&self.phase, role),
            (GoalPhase::ExecutorWorking, GoalRole::Executor)
                | (GoalPhase::CheckerWorking, GoalRole::Checker)
        );
        if dominated {
            self.phase = GoalPhase::Interrupted(role);
        }
        Vec::new()
    }

    /// Handle user input during Goal mode.
    pub fn on_user_input(&mut self, text: &str) -> Vec<GoalAction> {
        match &self.phase {
            GoalPhase::CreatingChecker => {
                // Checker not yet created — queue for next round.
                self.pending_appends.push(text.to_string());
                Vec::new()
            }
            GoalPhase::ExecutorWorking => {
                // Forward to executor (Ask/permission response). No prompt_user append —
                // intentional追加 happens after ESC interruption.
                vec![GoalAction::SendToExecutor {
                    content: text.to_string(),
                    tool_call_id: None,
                }]
            }
            GoalPhase::CheckerWorking => {
                // Forward to checker (Ask/permission response).
                vec![GoalAction::SendToChecker {
                    content: text.to_string(),
                    tool_call_id: None,
                }]
            }
            GoalPhase::Interrupted(role) => {
                // Resume: append to prompt_user and send to interrupted agent.
                let role = *role;
                self.appends.push(text.to_string());
                let msg = self.build_prompt_user();
                self.phase = match role {
                    GoalRole::Executor => GoalPhase::ExecutorWorking,
                    GoalRole::Checker => GoalPhase::CheckerWorking,
                };
                vec![match role {
                    GoalRole::Executor => GoalAction::SendToExecutor {
                        content: msg,
                        tool_call_id: None,
                    },
                    GoalRole::Checker => GoalAction::SendToChecker {
                        content: msg,
                        tool_call_id: None,
                    },
                }]
            }
            _ => Vec::new(),
        }
    }

    /// Whether the Goal loop is actively working (executor or checker).
    pub fn is_working(&self) -> bool {
        matches!(
            self.phase,
            GoalPhase::ExecutorWorking | GoalPhase::CheckerWorking
        )
    }

    /// The currently active role (for rendering).
    pub fn active_role(&self) -> Option<GoalRole> {
        match &self.phase {
            GoalPhase::ExecutorWorking => Some(GoalRole::Executor),
            GoalPhase::CheckerWorking => Some(GoalRole::Checker),
            GoalPhase::Interrupted(role) => Some(*role),
            _ => None,
        }
    }

    // ================================================================
    // Private helpers
    // ================================================================

    fn on_executor_done(&mut self, result: Option<String>) -> Vec<GoalAction> {
        let exec_result = result.unwrap_or_default();
        let checker_msg = self.build_checker_message(&exec_result);

        self.phase = GoalPhase::CheckerWorking;
        self.format_retries = 0;

        vec![
            GoalAction::PushSeparator {
                role: GoalRole::Checker,
                round: self.round,
            },
            GoalAction::SendToChecker {
                content: checker_msg,
                tool_call_id: None,
            },
        ]
    }

    fn on_checker_done(&mut self, result: Option<String>) -> Vec<GoalAction> {
        let output = result.unwrap_or_default();

        match parse_goal_finish(&output) {
            Some(true) => {
                // Goal complete!
                let reason = parse_reason(&output).unwrap_or_default();
                self.phase = GoalPhase::Completed;
                vec![GoalAction::GoalComplete { reason }, GoalAction::ExitGoal]
            }
            Some(false) => {
                // Not done — send checker feedback to executor.
                let executor_msg = self.build_executor_message(&output);
                self.round += 1;
                self.phase = GoalPhase::ExecutorWorking;
                vec![
                    GoalAction::PushSeparator {
                        role: GoalRole::Executor,
                        round: self.round,
                    },
                    GoalAction::SendToExecutor {
                        content: executor_msg,
                        tool_call_id: None,
                    },
                ]
            }
            None => {
                // Format error — retry or warn.
                self.format_retries += 1;
                if self.format_retries > MAX_FORMAT_RETRIES {
                    // Stall: stop showing a phantom "reviewing…" spinner.
                    // Move to Interrupted so the UI reflects that the loop is
                    // paused awaiting a user decision (ESC /goal-exit or input).
                    self.phase = GoalPhase::Interrupted(GoalRole::Checker);
                    vec![GoalAction::Toast(
                        "Checker format error (max retries exceeded), consider /goal-exit".into(),
                    )]
                } else {
                    // Stay in CheckerWorking, send format reminder.
                    vec![GoalAction::SendToChecker {
                        content: FORMAT_REMINDER.to_string(),
                        tool_call_id: None,
                    }]
                }
            }
        }
    }

    /// Build the structured prompt_user text.
    fn build_prompt_user(&self) -> String {
        let mut text = format!("【任务目标】\n{}", self.goal_prompt);
        if !self.appends.is_empty() {
            text.push_str("\n【追加信息】\n");
            for (i, append) in self.appends.iter().enumerate() {
                text.push_str(&format!("{}. {}", i + 1, append));
                if i < self.appends.len() - 1 {
                    text.push('\n');
                }
            }
        }
        text
    }

    /// Build message for checker: prompt_user + exec_result.
    fn build_checker_message(&self, exec_result: &str) -> String {
        format!(
            "{}\n\n【执行结果】\n{}",
            self.build_prompt_user(),
            exec_result
        )
    }

    /// Build message for executor: prompt_user + checker feedback.
    fn build_executor_message(&self, checker_feedback: &str) -> String {
        format!(
            "{}\n\n【检查者反馈】\n{}",
            self.build_prompt_user(),
            checker_feedback
        )
    }

    /// Move pending_appends into appends.
    fn flush_pending_appends(&mut self) {
        if !self.pending_appends.is_empty() {
            self.appends.append(&mut self.pending_appends);
        }
    }
}

// ================================================================
// Parsing helpers
// ================================================================

/// Parse `<goal_finish>...</goal_finish>` from checker output.
///
/// Uses the **last** occurrence (`rfind`) so that a checker echoing the
/// expected format mid-reasoning doesn't trigger a premature verdict —
/// the contract is "end your response with this format".
///
/// Returns `Some(true)` if value is "true" (case-insensitive),
/// `Some(false)` if the tag exists but value is not "true",
/// `None` if the tag is not found.
pub fn parse_goal_finish(text: &str) -> Option<bool> {
    let start = text.rfind("<goal_finish>")?;
    let after_open = &text[start + "<goal_finish>".len()..];
    let end = after_open.find("</goal_finish>")?;
    let value = after_open[..end].trim().to_lowercase();
    Some(value == "true")
}

/// Parse `<reason>...</reason>` from checker output (last occurrence).
pub fn parse_reason(text: &str) -> Option<String> {
    let start = text.rfind("<reason>")?;
    let after_open = &text[start + "<reason>".len()..];
    let end = after_open.find("</reason>")?;
    Some(after_open[..end].trim().to_string())
}

/// Default checker system prompt.
pub const DEFAULT_CHECKER_SYSTEM_PROMPT: &str = r#"You are a Goal Checker. Your role is to VERIFY whether a task has been completed correctly. You are NOT the executor — do NOT attempt to do the task yourself.

You will receive:
1. A task goal (【任务目标】) describing what should be achieved
2. An execution result (【执行结果】) describing what the executor claims to have done

Your job:
- Use your tools (Bash, Read, Glob, Grep) to independently verify the execution result
- Check files exist, content is correct, commands work, etc.
- Do NOT modify any files or execute the task yourself

When you have finished checking, you MUST end your response with EXACTLY this format:

<goal_finish>true</goal_finish>
<reason>brief explanation of why the goal is met</reason>

OR if the goal is NOT met:

<goal_finish>false</goal_finish>
<reason>brief explanation of what is missing or wrong</reason>

The <goal_finish> tag is MANDATORY. Always include it in your final response."#;

/// Format reminder sent to checker when output lacks required tags.
const FORMAT_REMINDER: &str = "Your last response did not contain the required <goal_finish> tag. You MUST end your response with:\n\n<goal_finish>true</goal_finish>\n<reason>...</reason>\n\nOR\n\n<goal_finish>false</goal_finish>\n<reason>...</reason>\n\nPlease re-state your conclusion with the correct format.";

// ================================================================
// Tests
// ================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_goal() -> (GoalState, Vec<GoalAction>) {
        GoalState::new(
            "exec-session".into(),
            "create hello.py".into(),
            DEFAULT_CHECKER_SYSTEM_PROMPT.into(),
        )
    }

    #[test]
    fn test_new_produces_initial_actions() {
        let (state, actions) = make_goal();
        assert_eq!(state.phase, GoalPhase::CreatingChecker);
        assert_eq!(state.round, 1);
        assert_eq!(actions.len(), 3);
        assert!(matches!(&actions[0], GoalAction::CreateChecker { .. }));
        assert!(matches!(
            &actions[1],
            GoalAction::PushSeparator {
                role: GoalRole::Executor,
                round: 1
            }
        ));
        assert!(matches!(&actions[2], GoalAction::SendToExecutor { .. }));
    }

    #[test]
    fn test_checker_created_transitions_to_executor_working() {
        let (mut state, _) = make_goal();
        let actions = state.on_checker_created("checker-session".into());
        assert!(actions.is_empty());
        assert_eq!(state.phase, GoalPhase::ExecutorWorking);
        assert_eq!(state.checker_session_id.as_deref(), Some("checker-session"));
    }

    #[test]
    fn test_executor_done_sends_to_checker() {
        let (mut state, _) = make_goal();
        state.on_checker_created("checker-session".into());

        let actions = state.on_turn_result(GoalRole::Executor, Some("done".into()));
        assert_eq!(state.phase, GoalPhase::CheckerWorking);
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            GoalAction::PushSeparator {
                role: GoalRole::Checker,
                round: 1
            }
        ));
        assert!(matches!(&actions[1], GoalAction::SendToChecker { .. }));
    }

    #[test]
    fn test_checker_true_completes_goal() {
        let (mut state, _) = make_goal();
        state.on_checker_created("checker-session".into());
        state.on_turn_result(GoalRole::Executor, Some("done".into()));

        let actions = state.on_turn_result(
            GoalRole::Checker,
            Some("<goal_finish>true</goal_finish>\n<reason>file exists</reason>".into()),
        );
        assert_eq!(state.phase, GoalPhase::Completed);
        assert!(
            actions.iter().any(
                |a| matches!(a, GoalAction::GoalComplete { reason } if reason == "file exists")
            )
        );
        assert!(actions.iter().any(|a| matches!(a, GoalAction::ExitGoal)));
    }

    #[test]
    fn test_checker_false_loops_back() {
        let (mut state, _) = make_goal();
        state.on_checker_created("checker-session".into());
        state.on_turn_result(GoalRole::Executor, Some("done".into()));

        let actions = state.on_turn_result(
            GoalRole::Checker,
            Some("<goal_finish>false</goal_finish>\n<reason>missing</reason>".into()),
        );
        assert_eq!(state.phase, GoalPhase::ExecutorWorking);
        assert_eq!(state.round, 2);
        assert!(actions.iter().any(|a| matches!(
            a,
            GoalAction::PushSeparator {
                role: GoalRole::Executor,
                round: 2
            }
        )));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, GoalAction::SendToExecutor { .. }))
        );
    }

    #[test]
    fn test_checker_format_error_retries() {
        let (mut state, _) = make_goal();
        state.on_checker_created("checker-session".into());
        state.on_turn_result(GoalRole::Executor, Some("done".into()));

        // First format error
        let actions = state.on_turn_result(GoalRole::Checker, Some("no tags here".into()));
        assert_eq!(state.phase, GoalPhase::CheckerWorking);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, GoalAction::SendToChecker { .. }))
        );

        // Second format error
        let actions = state.on_turn_result(GoalRole::Checker, Some("still no tags".into()));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, GoalAction::SendToChecker { .. }))
        );

        // Third format error — exceeds max: stalls into Interrupted(Checker).
        let actions = state.on_turn_result(GoalRole::Checker, Some("nope".into()));
        assert!(actions.iter().any(|a| matches!(a, GoalAction::Toast(_))));
        assert_eq!(state.phase, GoalPhase::Interrupted(GoalRole::Checker));
    }

    #[test]
    fn test_interrupted_and_resume() {
        let (mut state, _) = make_goal();
        state.on_checker_created("checker-session".into());

        // Interrupt executor
        let actions = state.on_interrupted(GoalRole::Executor);
        assert!(actions.is_empty());
        assert_eq!(state.phase, GoalPhase::Interrupted(GoalRole::Executor));

        // User input resumes
        let actions = state.on_user_input("use python 3.12");
        assert_eq!(state.phase, GoalPhase::ExecutorWorking);
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], GoalAction::SendToExecutor { content, .. } if content.contains("use python 3.12"))
        );
    }

    #[test]
    fn test_user_input_while_working_forwards_only() {
        let (mut state, _) = make_goal();
        state.on_checker_created("checker-session".into());

        // During working: forwards to active agent, does NOT append to prompt_user.
        let actions = state.on_user_input("y");
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], GoalAction::SendToExecutor { content, .. } if content == "y")
        );
        assert!(state.pending_appends.is_empty());

        // After interruption: appends to prompt_user AND sends.
        state.on_interrupted(GoalRole::Executor);
        let actions = state.on_user_input("use python 3.12");
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], GoalAction::SendToExecutor { content, .. } if content.contains("use python 3.12"))
        );
        assert!(state.appends.contains(&"use python 3.12".to_string()));
    }

    #[test]
    fn test_prompt_user_format() {
        let (mut state, _) = make_goal();
        assert_eq!(state.build_prompt_user(), "【任务目标】\ncreate hello.py");

        state.appends.push("use python 3.12".into());
        state.appends.push("add shebang".into());
        assert_eq!(
            state.build_prompt_user(),
            "【任务目标】\ncreate hello.py\n【追加信息】\n1. use python 3.12\n2. add shebang"
        );
    }

    #[test]
    fn test_parse_goal_finish() {
        assert_eq!(
            parse_goal_finish("<goal_finish>true</goal_finish>"),
            Some(true)
        );
        assert_eq!(
            parse_goal_finish("<goal_finish>True</goal_finish>"),
            Some(true)
        );
        assert_eq!(
            parse_goal_finish("<goal_finish> TRUE </goal_finish>"),
            Some(true)
        );
        assert_eq!(
            parse_goal_finish("<goal_finish>false</goal_finish>"),
            Some(false)
        );
        assert_eq!(
            parse_goal_finish("<goal_finish>no</goal_finish>"),
            Some(false)
        );
        assert_eq!(parse_goal_finish("no tags here"), None);
        assert_eq!(parse_goal_finish("<goal_finish>unclosed"), None);
    }

    #[test]
    fn test_parse_goal_finish_uses_last_occurrence() {
        // Checker echoes the format mid-reasoning, then gives the real verdict last.
        let text = "I should output <goal_finish>true</goal_finish> but first let me verify...\n\
                    After checking, the file is missing.\n\
                    <goal_finish>false</goal_finish>\n<reason>missing</reason>";
        assert_eq!(parse_goal_finish(text), Some(false));
        assert_eq!(parse_reason(text), Some("missing".into()));
    }

    #[test]
    fn test_parse_reason() {
        assert_eq!(
            parse_reason("<reason>file exists and works</reason>"),
            Some("file exists and works".into())
        );
        assert_eq!(parse_reason("no reason tag"), None);
    }

    #[test]
    fn test_checker_message_includes_prompt_and_result() {
        let (mut state, _) = make_goal();
        state.appends.push("extra".into());
        let msg = state.build_checker_message("created hello.py");
        assert!(msg.contains("【任务目标】"));
        assert!(msg.contains("create hello.py"));
        assert!(msg.contains("【追加信息】"));
        assert!(msg.contains("1. extra"));
        assert!(msg.contains("【执行结果】"));
        assert!(msg.contains("created hello.py"));
    }
}
