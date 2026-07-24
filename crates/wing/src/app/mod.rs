//! Application state machine and main event loop.

pub mod ask_flow;
pub mod constants;
pub mod goal;
pub mod intent;
pub mod popup_state;
pub mod render_context;
pub mod replay;
pub mod runner;
pub mod transport;
pub mod turn_state;

pub use intent::AppIntent;

use anyhow::Result;
use crossterm::cursor::Hide;
use crossterm::cursor::MoveTo;
use crossterm::cursor::Show;
use crossterm::execute;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;

use self::transport::GatewayEndpoint;
use self::transport::Transport;
use self::transport::backoff;
use self::transport::try_reconnect;
use crate::protocol::WingEvent;
use crate::tui::TermEvent;
use crate::tui::WingTerminal;
use crate::tui::is_quit_key;
use crate::ui::cells::ask_msg::AskMessage;
use crate::ui::cells::diff_view::DiffView;
use crate::ui::cells::todo_msg::TodoMessage;
use crate::ui::cells::tool_call::ToolCallBlock;
use crate::ui::chat_view::ChatCell;
use crate::ui::chat_view::ChatView;
use crate::ui::chat_view::ChatViewWidget;
use crate::ui::chat_view::ComposerTail;
use crate::ui::header::build_header_lines;
use crate::ui::input_area::InputAction;
use crate::ui::input_area::InputArea;
use crate::ui::input_area::cursor_screen_pos;
use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::candidate_request_for;
use crate::ui::status_bar::StatusBar;
use crate::ui::status_bar::StatusData;
use crate::ui::status_bar::TurnUsage;
use crate::ui::toast::Toast;
use crate::ui::toast::ToastKind;
use crate::ui::toast::render_toast;
use crate::util::title;
use title::AttentionKind;

use self::constants::CLEAR_COMMAND;
use self::constants::COPY_COMMAND;
use self::constants::GOAL_COMMAND;
use self::constants::GOAL_EXIT_COMMAND;
use self::constants::NEW_COMMAND;
use self::constants::TOOL_BASH;
use self::constants::TOOL_TODO;
use self::popup_state::PopupState;
use self::render_context::RenderContext;
use self::turn_state::TurnState;

use crate::config::AppConfig;
use crate::config::ThemePalette;

/// Threshold for wide-mode status bar (shows cumulative usage details).
const WIDE_THRESHOLD: u16 = 100;

/// Application state.
pub struct App {
    pub status: StatusData,
    pub chat: ChatView,
    pub input: InputArea,
    pub session_id: String,
    /// Whether the app should exit.
    pub should_quit: bool,
    /// Pending side-effect intents. Drained by runner after each draw cycle.
    intents: Vec<AppIntent>,
    /// Visible chat area height (updated during draw).
    visible_height: usize,
    /// Terminal width (updated during draw).
    terminal_width: u16,
    /// Ctrl+C press count for double-press quit.
    ctrl_c_count: u8,
    /// Last Ctrl+C timestamp (for double-press detection).
    ctrl_c_last: Option<std::time::Instant>,
    /// Render context — tracks current turn's active cells.
    ctx: RenderContext,
    /// Popup state (active popup + candidate cache + dedup).
    pub(crate) popup: PopupState,
    /// Turn state (working flag + timer + spinner + usage).
    turn: TurnState,
    /// Last tick instant for measuring real dt between TermEvent::Tick.
    last_tick: std::time::Instant,
    /// Active toast notification (lazy-expired in draw).
    toast: Option<Toast>,
    /// Queued ask selections (when agent requires a choice from menu).
    /// Concurrent asks queue up; the front entry is the active one.
    ask_selections: std::collections::VecDeque<crate::ui::ask_select::AskSelection>,
    /// Queued multi-question ask flows (AskUserQuestion with questions array).
    /// Concurrent asks queue up; the front entry is the active one.
    ask_flows: std::collections::VecDeque<ask_flow::AskFlow>,
    /// Whether the terminal window/tab currently has focus.
    /// Default `true` — terminals that don't support focus events
    /// will never send FocusLost, so BEL is never triggered.
    focused: bool,
    /// User configuration.
    config: AppConfig,
    /// Resolved theme palette.
    palette: ThemePalette,
    /// Whether the gateway connection is active.
    connected: bool,
    /// Goal orchestration state (None = normal mode).
    pub(crate) goal: Option<goal::GoalState>,
    /// TUI 启动时的工作目录，用于 /new 创建 session 时传递 workspace。
    launch_workspace: Option<String>,
}

// ---------------------------------------------------------------------------
// Command parsing helpers
// ---------------------------------------------------------------------------

/// Classified result of parsing a boolean-style argument.
#[derive(Debug)]
enum BoolArg {
    /// No argument provided (empty string).
    Empty,
    /// `"on"`, `"true"`, or `"1"`.
    On,
    /// `"off"`, `"false"`, or `"0"`.
    Off,
    /// Any other value (already lowercased).
    Other(String),
}

/// Classify a boolean toggle argument.
fn parse_bool_arg(raw: &str) -> BoolArg {
    match raw.to_lowercase().as_str() {
        "on" | "true" | "1" => BoolArg::On,
        "off" | "false" | "0" => BoolArg::Off,
        "" => BoolArg::Empty,
        _ => BoolArg::Other(raw.to_lowercase()),
    }
}

/// Strip `prefix` from `text`, trim, and return `Some` if non-empty.
fn parse_string_arg(text: &str, prefix: &str) -> Option<String> {
    let value = text.strip_prefix(prefix)?.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

impl App {
    pub fn new(session_id: String, config: AppConfig, launch_workspace: Option<String>) -> Self {
        let palette = ThemePalette::from_config(&config.colors);
        let max_input_lines = config.layout.max_input_lines;
        let mut chat = ChatView::new();
        chat.set_header(build_header_lines(&palette));
        Self {
            status: StatusData::default(),
            chat,
            input: InputArea::with_max_lines("今天构建什么？".into(), max_input_lines),
            session_id,
            should_quit: false,
            intents: Vec::new(),
            visible_height: 20,
            terminal_width: 80,
            ctrl_c_count: 0,
            ctrl_c_last: None,
            ctx: RenderContext::new(),
            popup: PopupState::default(),
            turn: TurnState::default(),
            last_tick: std::time::Instant::now(),
            toast: None,
            ask_selections: std::collections::VecDeque::new(),
            ask_flows: std::collections::VecDeque::new(),
            focused: true,
            config,
            palette,
            connected: true,
            goal: None,
            launch_workspace,
        }
    }

    /// Whether the terminal is wide enough for detailed status bar.
    fn is_wide(&self) -> bool {
        self.terminal_width >= WIDE_THRESHOLD
    }

    /// Clear all queued ask state (selections + flows) and remove their
    /// cells from chat. Called when a turn ends or is interrupted — the
    /// backend cancels all feedback waiters at the same time.
    fn clear_ask_state(&mut self) {
        if self.ask_selections.is_empty() && self.ask_flows.is_empty() {
            return;
        }
        for sel in self.ask_selections.drain(..) {
            self.chat.remove_ask(&sel.tool_call_id);
        }
        for flow in self.ask_flows.drain(..) {
            self.chat.remove_ask(&flow.tool_call_id);
        }
        self.input.placeholder = "今天构建什么？".into();
    }

    /// Refresh the input placeholder to reflect the active (front) ask state.
    ///
    /// Selection takes precedence over flow: while a selection is active its
    /// key handler captures all keys, so the flow cannot be answered yet.
    fn refresh_ask_placeholder(&mut self) {
        if self.ask_selections.front().is_some() {
            self.input.placeholder = "↑↓ select · Enter confirm".into();
        } else if let Some(flow) = self.ask_flows.front() {
            self.input.placeholder = format!("Question {} — type your answer...", flow.progress());
        } else {
            self.input.placeholder = "今天构建什么？".into();
        }
    }

    /// Common cleanup at the end of an agent turn (Done / Interrupted / Error).
    ///
    /// Resets turn state, render context, and copy candidates.
    /// Callers handle their own specific follow-up (title, toast, etc.).
    ///
    /// **Ordering note**: `refresh_copy_candidates()` runs *inside* this method,
    /// so any chat mutations by the caller (e.g. `clear_ask_state`,
    /// `chat.push(ErrorMessage)`) happen *after* the copy cache is snapshot.
    /// Currently safe because `collect_assistant_messages` only collects
    /// `AssistantMessage` cells, which are unaffected by these mutations.
    fn finish_turn(&mut self) {
        self.turn.finish();
        self.ctx.reset();
        self.refresh_copy_candidates();
    }

    /// Workdir last-component label for the terminal title suffix (e.g. `myproject`).
    fn dir_label(&self) -> Option<String> {
        title::dir_label(self.status.workdir.as_deref())
    }

    /// Notify the user via OSC 9 + attention title when the terminal is not focused.
    fn notify_unfocused(&mut self, message: String, kind: AttentionKind) {
        if !self.focused {
            self.push_intent(AppIntent::Notify(message));
            self.push_intent(AppIntent::SetTitle(title::title_attention(
                kind,
                self.dir_label().as_deref(),
            )));
        }
    }

    /// Show a toast notification. Returns remaining duration for timer setup.
    pub fn show_toast(&mut self, toast: Toast) -> std::time::Duration {
        let remaining = toast.remaining();
        self.toast = Some(toast);
        remaining
    }

    /// Clear the active toast (including persistent toasts).
    pub fn clear_toast(&mut self) {
        self.toast = None;
    }

    /// Submit text from the input area: try frontend command, else send as message.
    ///
    /// Returns `true` if the text was consumed (either as a command or message).
    fn submit_message(&mut self, text: &str) -> bool {
        // Multi-question ask flow: intercept submission to advance the
        // front flow (concurrent asks are answered in arrival order).
        if !self.ask_flows.is_empty() {
            return self.handle_ask_flow_submit(text);
        }
        if self.try_frontend_command(text) {
            return true;
        }
        // Goal mode: route user input through Goal state machine.
        if let Some(goal) = self.goal.as_mut() {
            self.chat.push(ChatCell::UserMessage(text.to_string()));
            let actions = goal.on_user_input(text);
            self.execute_goal_actions(actions);
            self.turn.usage = TurnUsage::default();
            return true;
        }
        self.chat.push(ChatCell::UserMessage(text.to_string()));
        self.push_intent(AppIntent::SendMessage {
            content: text.to_string(),
            tool_call_id: None,
        });
        self.turn.usage = TurnUsage::default();
        true
    }

    /// Handle submission during a multi-question ask flow.
    ///
    /// Advances the front flow to the next question or sends the final
    /// structured response (addressed by the flow's tool_call_id).
    fn handle_ask_flow_submit(&mut self, text: &str) -> bool {
        // Reject whitespace-only input (honors "must provide an answer" invariant).
        let text = text.trim();
        if text.is_empty() {
            return true; // consumed but no-op
        }
        // Advance the front flow and extract needed data to avoid borrow conflicts.
        let Some(result) = self.ask_flows.front_mut().map(|flow| {
            let advance_result = flow.advance(text);
            (
                advance_result,
                flow.tool_call_id.clone(),
                flow.current_idx,
                flow.answers.clone(),
                flow.len(),
                flow.progress(),
            )
        }) else {
            return false;
        };
        let (advance_result, tool_call_id, idx, answers, total, progress) = result;

        match advance_result {
            None => {
                // More questions to answer — update the AskMessage cell.
                self.chat.update_ask_progress(&tool_call_id, idx, answers);
                self.input.placeholder = format!("Question {progress} — type your answer...");
            }
            Some(json) => {
                // All questions answered — send the addressed response and
                // move on to the next queued ask (if any).
                self.ask_flows.pop_front();
                self.chat.update_ask_progress(&tool_call_id, total, answers);
                self.push_intent(AppIntent::SendMessage {
                    content: json,
                    tool_call_id: Some(tool_call_id),
                });
                self.refresh_ask_placeholder();
            }
        }
        true
    }

    /// Push a side-effect intent for the runner to execute after draw.
    fn push_intent(&mut self, intent: AppIntent) {
        self.intents.push(intent);
    }

    /// Drain all pending intents. Called by the runner after each draw cycle.
    pub fn drain_intents(&mut self) -> Vec<AppIntent> {
        std::mem::take(&mut self.intents)
    }

    /// Execute GoalActions produced by the GoalState state machine.
    ///
    /// Translates pure logic actions into AppIntents (side-effects) and
    /// chat mutations.
    pub(crate) fn execute_goal_actions(&mut self, actions: Vec<goal::GoalAction>) {
        use goal::GoalAction;
        for action in actions {
            match action {
                GoalAction::SendToExecutor(content) => {
                    let session_id = self.session_id.clone();
                    self.push_intent(AppIntent::GoalSend {
                        session_id,
                        content,
                    });
                }
                GoalAction::SendToChecker(content) => {
                    if let Some(goal) = &self.goal
                        && let Some(checker_id) = &goal.checker_session_id
                    {
                        self.push_intent(AppIntent::GoalSend {
                            session_id: checker_id.clone(),
                            content,
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
    fn exit_goal(&mut self) {
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
    fn is_goal_session(&self, session_id: &str) -> bool {
        self.goal
            .as_ref()
            .and_then(|g| g.checker_session_id.as_deref())
            .is_some_and(|id| id == session_id)
    }

    /// Determine which Goal role a session_id corresponds to.
    fn goal_role_for_session(&self, session_id: Option<&str>) -> Option<goal::GoalRole> {
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

    /// Update the gateway connection state.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
        self.status.connected = connected;
    }

    /// Try to handle `text` as a frontend-only magic command.
    ///
    /// Returns `true` if the command was recognized and handled locally;
    /// `false` if it should be sent to the gateway as usual.
    fn try_frontend_command(&mut self, text: &str) -> bool {
        // /copy accepts optional index from SubCommand popup completion.
        if text == COPY_COMMAND || text.starts_with("/copy ") {
            self.handle_copy_command(text);
            return true;
        }
        // /goal <prompt> — activate Goal orchestration.
        if text == GOAL_COMMAND || text.starts_with("/goal ") {
            self.handle_goal_command(text);
            return true;
        }
        // /goal-exit — exit Goal mode.
        if text == GOAL_EXIT_COMMAND {
            self.handle_goal_exit_command();
            return true;
        }
        match text {
            CLEAR_COMMAND => {
                self.chat.clear();
                self.refresh_copy_candidates();
                self.show_toast(Toast::info(
                    "Chat cleared",
                    std::time::Duration::from_secs(2),
                ));
                true
            }
            NEW_COMMAND => {
                self.push_intent(AppIntent::CreateSession {
                    workspace: self.launch_workspace.clone(),
                });
                true
            }
            _ => self.try_http_command(text),
        }
    }

    /// Try to handle `text` as an HTTP-migrated magic command.
    ///
    /// Commands that were previously sent via WS silent are now intercepted
    /// and converted to HTTP API intents.
    fn try_http_command(&mut self, text: &str) -> bool {
        match text {
            "/context" => {
                self.push_intent(AppIntent::ShowContextInfo);
                true
            }
            "/skills" => {
                self.push_intent(AppIntent::ShowSkillsInfo);
                true
            }
            "/model" => {
                self.push_intent(AppIntent::FetchModels);
                true
            }
            "/agents" => {
                self.push_intent(AppIntent::FetchAgents);
                true
            }
            _ if text.starts_with("/model ") => {
                match parse_string_arg(text, "/model") {
                    Some(model) => {
                        self.push_intent(AppIntent::set_model(model));
                    }
                    None => {
                        self.push_intent(AppIntent::FetchModels);
                    }
                }
                true
            }
            _ if text.starts_with("/agents ") => {
                match parse_string_arg(text, "/agents") {
                    Some(agent) => {
                        self.push_intent(AppIntent::set_agent(agent));
                    }
                    None => {
                        self.push_intent(AppIntent::FetchAgents);
                    }
                }
                true
            }
            _ if text == "/title" || text.starts_with("/title ") => {
                let args = text.strip_prefix("/title").unwrap().trim();
                if args.is_empty() {
                    let title = self.status.session_name.as_deref().unwrap_or("(not set)");
                    self.show_toast(Toast::info(
                        format!("title: {title}"),
                        std::time::Duration::from_secs(3),
                    ));
                } else {
                    self.push_intent(AppIntent::set_title(args.to_string()));
                }
                true
            }
            _ if text == "/workdir" || text.starts_with("/workdir ") => {
                let args = text.strip_prefix("/workdir").unwrap().trim();
                if args.is_empty() {
                    let wd = self.status.workdir.as_deref().unwrap_or("(not set)");
                    self.show_toast(Toast::info(
                        format!("workdir: {wd}"),
                        std::time::Duration::from_secs(3),
                    ));
                } else {
                    self.push_intent(AppIntent::set_workdir(args.to_string()));
                }
                true
            }
            _ if text == "/think" || text.starts_with("/think ") => {
                let args = text.strip_prefix("/think").unwrap().trim();
                match parse_bool_arg(args) {
                    BoolArg::Empty => {
                        let effort = self.status.reasoning_effort.as_deref().unwrap_or("default");
                        let msg = format!("think: {} (effort: {})", self.status.thinking, effort);
                        self.show_toast(Toast::info(&msg, std::time::Duration::from_secs(3)));
                        true
                    }
                    BoolArg::On => {
                        self.push_intent(AppIntent::set_thinking(true, None));
                        true
                    }
                    BoolArg::Off => {
                        self.push_intent(AppIntent::set_thinking(false, None));
                        true
                    }
                    BoolArg::Other(effort)
                        if matches!(
                            effort.as_str(),
                            "low" | "medium" | "high" | "xhigh" | "max"
                        ) =>
                    {
                        self.push_intent(AppIntent::set_thinking(true, Some(effort)));
                        true
                    }
                    BoolArg::Other(_) => {
                        self.show_toast(Toast::warning(
                            "Usage: /think [on|off|low|medium|high|xhigh|max]",
                            std::time::Duration::from_secs(3),
                        ));
                        true
                    }
                }
            }
            _ if text == "/yolo" || text.starts_with("/yolo ") => {
                let args = text.strip_prefix("/yolo").unwrap().trim();
                match parse_bool_arg(args) {
                    BoolArg::Empty => {
                        let msg = format!("yolo: {}", self.status.yolo);
                        self.show_toast(Toast::info(&msg, std::time::Duration::from_secs(3)));
                        true
                    }
                    BoolArg::On => {
                        self.push_intent(AppIntent::set_yolo(true));
                        true
                    }
                    BoolArg::Off => {
                        self.push_intent(AppIntent::set_yolo(false));
                        true
                    }
                    BoolArg::Other(_) => {
                        self.show_toast(Toast::warning(
                            "Usage: /yolo [on|off]",
                            std::time::Duration::from_secs(3),
                        ));
                        true
                    }
                }
            }
            "/compact" => {
                self.push_intent(AppIntent::CompactSession);
                true
            }
            "/reload" => {
                self.push_intent(AppIntent::ReloadSystem);
                true
            }
            _ if text == "/fork" || text.starts_with("/fork ") => {
                match parse_string_arg(text, "/fork") {
                    Some(uuid) => {
                        self.push_intent(AppIntent::ForkSession { target_uuid: uuid });
                    }
                    None => {
                        self.show_toast(Toast::warning(
                            "Usage: /fork <uuid>",
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
                true
            }
            _ if text == "/rewind" || text.starts_with("/rewind ") => {
                match parse_string_arg(text, "/rewind") {
                    Some(uuid) => {
                        self.push_intent(AppIntent::RewindSession { target_uuid: uuid });
                    }
                    None => {
                        self.show_toast(Toast::warning(
                            "Usage: /rewind <uuid>",
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
                true
            }
            _ if text == "/session"
                || text == "/ss"
                || text.starts_with("/session ")
                || text.starts_with("/ss ") =>
            {
                let id =
                    parse_string_arg(text, "/session").or_else(|| parse_string_arg(text, "/ss"));
                match id {
                    Some(id) => {
                        self.push_intent(AppIntent::ResumeSession { session_id: id });
                    }
                    None => {
                        self.show_toast(Toast::warning(
                            "Usage: /session <id>",
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// `/copy [N]` — copy the N-th (or last) assistant message to clipboard.
    fn handle_copy_command(&mut self, text: &str) {
        use std::time::Duration;

        let index = text
            .strip_prefix("/copy ")
            .and_then(|s| s.trim().parse::<usize>().ok());

        let content = match index {
            Some(n) => self.chat.nth_assistant_text(n).map(String::from),
            None => self.chat.last_assistant_text().map(String::from),
        };

        let Some(content) = content else {
            self.show_toast(Toast::warning(
                "No assistant message to copy",
                Duration::from_secs(2),
            ));
            return;
        };

        self.push_intent(AppIntent::CopyToClipboard(content));
    }

    /// `/goal <prompt>` — activate Goal orchestration mode.
    fn handle_goal_command(&mut self, text: &str) {
        if self.goal.is_some() {
            self.show_toast(Toast::warning(
                "Goal already active, use /goal-exit first",
                std::time::Duration::from_secs(3),
            ));
            return;
        }

        let prompt = text.strip_prefix("/goal ").map(|s| s.trim()).unwrap_or("");
        if prompt.is_empty() {
            self.show_toast(Toast::warning(
                "Usage: /goal <prompt>",
                std::time::Duration::from_secs(3),
            ));
            return;
        }

        let checker_prompt = self
            .config
            .goal
            .checker_system_prompt
            .clone()
            .unwrap_or_else(|| goal::DEFAULT_CHECKER_SYSTEM_PROMPT.to_string());

        let (state, actions) =
            goal::GoalState::new(self.session_id.clone(), prompt.to_string(), checker_prompt);
        self.goal = Some(state);
        self.status.goal_active = true;
        // Show the goal prompt as a user message in chat.
        self.chat
            .push(ChatCell::UserMessage(format!("/goal {prompt}")));
        self.execute_goal_actions(actions);
    }

    /// `/goal-exit` — exit Goal orchestration mode.
    fn handle_goal_exit_command(&mut self) {
        if self.goal.is_none() {
            self.show_toast(Toast::warning(
                "Goal not active",
                std::time::Duration::from_secs(2),
            ));
            return;
        }
        self.exit_goal();
        self.show_toast(Toast::info(
            "Goal mode exited",
            std::time::Duration::from_secs(2),
        ));
    }

    /// Refresh `/copy` candidate cache from current chat state.
    fn refresh_copy_candidates(&mut self) {
        self.popup.cache.copies = self.chat.collect_assistant_messages();
    }

    /// Invalidate the cached session list so the next `/session` popup re-fetches.
    ///
    /// Called after session-mutating operations (resume, create, fork, title
    /// update) succeed, ensuring the popup always shows fresh data.
    pub fn invalidate_session_cache(&mut self) {
        self.popup.cache.sessions.clear();
    }

    /// Update popup state based on current input text.
    ///
    /// Skips requests while the agent is streaming to avoid interference.
    /// HTTP calls are idempotent — no dedup needed.
    pub(crate) fn update_popup(&mut self) {
        use crate::ui::popup::command::PopupAction;

        // Streaming guard: skip requests while agent is busy.
        let streaming = self.turn.working;
        let text = self.input.text().to_string();
        if let Some(action) = self.popup.update_from_input(&text) {
            if streaming {
                return;
            }
            match action {
                PopupAction::FetchModels => {
                    self.push_intent(AppIntent::FetchModels);
                }
                PopupAction::FetchBranches => {
                    self.push_intent(AppIntent::FetchBranches);
                }
                PopupAction::FetchAgents => {
                    self.push_intent(AppIntent::FetchAgents);
                }
                PopupAction::FetchSessionList => {
                    self.push_intent(AppIntent::FetchSessionList);
                }
            }
        }
    }

    /// Apply a background fetch result to app state.
    ///
    /// Called from the main event loop when a spawned fetch task completes.
    /// Discards stale results whose `session_id` doesn't match the current session.
    fn handle_fetch_result(&mut self, result: crate::app::intent::FetchResult) {
        use crate::app::intent::FetchPayload;

        // Session guard: discard results from a previous session.
        if result.session_id != self.session_id {
            tracing::debug!(
                stale = %result.session_id,
                current = %self.session_id,
                "discarding stale fetch result"
            );
            return;
        }

        match result.payload {
            FetchPayload::Info(info) => {
                self.status.model = info.model;
                self.status.total_tokens = info.total_tokens;
                self.status.context_window_tokens = info.context_window_tokens;
                self.status.thinking = info.thinking;
                self.status.reasoning_effort = info.reasoning_effort;
                self.status.yolo = info.yolo;
                self.status.session_name = info.session_name;
                self.status.workdir = info.workdir;
                // Refresh title so the workdir suffix appears once known.
                let dir = self.dir_label();
                let title = if self.turn.working {
                    title::title_working(self.turn.spinner.frame_str(), dir.as_deref())
                } else {
                    title::title_idle(dir.as_deref())
                };
                self.turn.last_title = Some(title.clone());
                self.push_intent(AppIntent::SetTitle(title));
                tracing::info!(model = %self.status.model, "session info received");
            }
            FetchPayload::Commands(resp) => {
                self.popup.cache.commands = resp
                    .commands
                    .into_iter()
                    .map(|c| crate::protocol::CommandInfo {
                        name: c.name,
                        aliases: c.aliases,
                        description: c.description,
                        params: c.params,
                    })
                    .collect();
                self.update_popup();
            }
            FetchPayload::Models(resp) => {
                self.popup.cache.models = resp
                    .models
                    .into_iter()
                    .map(|m| (m, String::new()))
                    .collect();
                self.update_popup();
            }
            FetchPayload::Branches(resp) => {
                self.popup.cache.branches = resp
                    .targets
                    .into_iter()
                    .map(|t| {
                        let preview =
                            crate::ui::cells::tool_call::truncate_by_chars(&t.content, 80);
                        (t.uuid, preview)
                    })
                    .collect();
                self.update_popup();
            }
            FetchPayload::Agents(resp) => {
                self.popup.cache.agents = resp
                    .agents
                    .into_iter()
                    .map(|a| (a, String::new()))
                    .collect();
                self.update_popup();
            }
            FetchPayload::SessionList(resp) => {
                use crate::ui::popup::command::SessionCandidate;
                use crate::ui::popup::selection::SessionStatus;

                // Normalize a path for workdir comparison (strip trailing slashes).
                let norm = |p: &str| {
                    let t = p.trim_end_matches('/');
                    if t.is_empty() {
                        "/".to_string()
                    } else {
                        t.to_string()
                    }
                };
                let launch_norm = norm(self.launch_workspace.as_deref().unwrap_or(""));

                // A session workspace "matches" the launch dir if it equals it or
                // is a subdirectory (prefix match on path components).
                let ws_matches = |ws: &str| {
                    let n = norm(ws);
                    n == launch_norm || n.starts_with(&format!("{launch_norm}/"))
                };

                let mut candidates: Vec<SessionCandidate> = resp
                    .sessions
                    .iter()
                    .map(|s| SessionCandidate {
                        id: s.id.clone(),
                        title: s.name.clone().unwrap_or_default(),
                        workspace: s.workspace.clone().unwrap_or_default(),
                        status: s.status.clone(),
                        last_interaction: s.last_interaction.clone().unwrap_or_default(),
                    })
                    .collect();

                // Stable sort: ① workdir 匹配当前启动目录者优先（前缀匹配）→
                // ② 状态优先级 (waiting > working > idle > inactive) →
                // ③ 保持后端时间降序。
                candidates.sort_by_key(|c| {
                    let ws_mismatch = !ws_matches(&c.workspace);
                    (ws_mismatch, SessionStatus::parse(&c.status).rank())
                });

                self.popup.cache.sessions = candidates;
                self.update_popup();
            }
            FetchPayload::ContextInfo(text) => {
                self.chat
                    .push(crate::ui::chat_view::ChatCell::SystemMessage(text));
            }
            FetchPayload::SkillsInfo(text) => {
                self.chat
                    .push(crate::ui::chat_view::ChatCell::SystemMessage(text));
            }
            FetchPayload::CompactDone {
                original,
                compressed,
            } => {
                self.show_toast(Toast::info(
                    format!("Compact done: {original} → {compressed} tokens"),
                    std::time::Duration::from_secs(3),
                ));
            }
            FetchPayload::Toast { message, is_error } => {
                if is_error {
                    self.show_toast(Toast::error(message, std::time::Duration::from_secs(3)));
                } else {
                    self.show_toast(Toast::info(message, std::time::Duration::from_secs(3)));
                }
            }
        }
    }

    /// Handle a terminal key event.
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        // Ctrl+C: double-press quit.
        if is_quit_key(&key) {
            let now = std::time::Instant::now();
            if let Some(last) = self.ctrl_c_last
                && now.duration_since(last).as_millis() < 500
            {
                self.should_quit = true;
                return;
            }
            self.ctrl_c_last = Some(now);
            self.ctrl_c_count += 1;
            if self.ctrl_c_count == 1 {
                self.show_toast(Toast::warning(
                    "Press Ctrl+C again to quit, Esc to interrupt",
                    std::time::Duration::from_secs(2),
                ));
            }
            return;
        }

        // Ask selection menu: capture all keys when active (queue front).
        if let Some(ask) = self.ask_selections.front_mut() {
            match key.code {
                crossterm::event::KeyCode::Up => {
                    ask.move_up();
                    let id = ask.tool_call_id.clone();
                    let selected = ask.selected;
                    self.chat.update_ask_selection(&id, selected);
                }
                crossterm::event::KeyCode::Down => {
                    ask.move_down();
                    let id = ask.tool_call_id.clone();
                    let selected = ask.selected;
                    self.chat.update_ask_selection(&id, selected);
                }
                crossterm::event::KeyCode::Enter => {
                    let choice = ask.current().map(|s| s.to_string());
                    let id = ask.tool_call_id.clone();
                    if let Some(choice) = choice {
                        self.ask_selections.pop_front();
                        self.chat.remove_ask(&id);
                        self.refresh_ask_placeholder();
                        // Goal mode: send to active session, not app.session_id.
                        if let Some(goal) = &self.goal
                            && let Some(role) = goal.active_role()
                        {
                            let actions = match role {
                                goal::GoalRole::Executor => {
                                    vec![goal::GoalAction::SendToExecutor(choice)]
                                }
                                goal::GoalRole::Checker => {
                                    vec![goal::GoalAction::SendToChecker(choice)]
                                }
                            };
                            self.execute_goal_actions(actions);
                        } else {
                            self.push_intent(AppIntent::SendMessage {
                                content: choice,
                                tool_call_id: Some(id),
                            });
                        }
                    }
                }
                _ => {} // ignore other keys while selection is active
            }
            return;
        }

        // Esc: close popup if active, otherwise clear/interrupt.
        if key.code == crossterm::event::KeyCode::Esc {
            if self.popup.active.is_active() {
                self.popup.active = ActivePopup::None;
                return;
            }
            if !self.input.text().is_empty() {
                self.input.clear();
            } else {
                // Goal mode: interrupt only the active session.
                if let Some(goal) = &self.goal
                    && let Some(role) = goal.active_role()
                {
                    match role {
                        goal::GoalRole::Executor => {
                            self.push_intent(AppIntent::InterruptSession);
                        }
                        goal::GoalRole::Checker => {
                            if let Some(checker_id) = &goal.checker_session_id {
                                self.push_intent(AppIntent::GoalInterrupt {
                                    session_id: checker_id.clone(),
                                });
                            }
                        }
                    }
                } else {
                    self.push_intent(AppIntent::InterruptSession);
                }
                self.show_toast(Toast::info(
                    "Interrupting agent...",
                    std::time::Duration::from_secs(2),
                ));
            }
            self.ctrl_c_count = 0;
            self.ctrl_c_last = None;
            return;
        }

        // Reset Ctrl+C counter on any other key.
        self.ctrl_c_count = 0;
        self.ctrl_c_last = None;

        // Popup navigation keys (only when popup is active and has items).
        if self.handle_popup_key(key) {
            return;
        }

        // If popup is active but has no items, close it (Enter falls through to input).
        if self.popup.active.is_active() && !self.popup.active.has_items() {
            self.popup.active = ActivePopup::None;
        }

        // Scrolling keys for chat view.
        let page = self.visible_height.saturating_sub(2);
        match key.code {
            crossterm::event::KeyCode::PageUp => {
                self.chat.page_up(page);
                return;
            }
            crossterm::event::KeyCode::PageDown => {
                self.chat.page_down(page, self.visible_height);
                return;
            }
            crossterm::event::KeyCode::Up
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.chat.scroll_up(1);
                return;
            }
            crossterm::event::KeyCode::Down
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.chat.scroll_down(1, self.visible_height);
                return;
            }
            // Alternate-scroll mode translates trackpad/wheel into plain
            // Up/Down arrows. Route them to chat scrolling when:
            //   - the user is reading history (not pinned to the bottom), OR
            //   - the input cursor cannot move further in that direction
            //     (e.g. single-line input + Up → scroll history, like a shell).
            // Otherwise fall through to the input area for cursor movement.
            crossterm::event::KeyCode::Up
                if !key.modifiers.intersects(
                    crossterm::event::KeyModifiers::CONTROL
                        | crossterm::event::KeyModifiers::ALT
                        | crossterm::event::KeyModifiers::SHIFT,
                ) && (!self.chat.is_at_bottom() || !self.input.can_move_up()) =>
            {
                self.chat.scroll_up(3);
                return;
            }
            crossterm::event::KeyCode::Down
                if !key.modifiers.intersects(
                    crossterm::event::KeyModifiers::CONTROL
                        | crossterm::event::KeyModifiers::ALT
                        | crossterm::event::KeyModifiers::SHIFT,
                ) && (!self.chat.is_at_bottom() || !self.input.can_move_down()) =>
            {
                self.chat.scroll_down(3, self.visible_height);
                return;
            }
            crossterm::event::KeyCode::Home
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.chat.jump_top();
                return;
            }
            crossterm::event::KeyCode::End
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.chat.jump_bottom();
                return;
            }
            _ => {}
        }

        // Everything else goes to the input area.
        let text_before = self.input.text();
        match self.input.handle_key(key, self.terminal_width) {
            InputAction::Submit(text) => {
                self.popup.active = ActivePopup::None;
                self.submit_message(&text);
            }
            InputAction::Escape | InputAction::None => {
                // Update popup based on new text.
                self.update_popup();
            }
        }
        // Any content edit brings the input back into view (it may have
        // been scrolled out while the user was reading history).
        if self.input.text() != text_before {
            self.chat.jump_bottom();
        }
    }

    /// Handle popup navigation keys. Returns `true` if the key was consumed.
    fn handle_popup_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.popup.active.is_active() || !self.popup.active.has_items() {
            return false;
        }
        match key.code {
            crossterm::event::KeyCode::Up => {
                self.popup.active.move_up();
                true
            }
            crossterm::event::KeyCode::Down => {
                self.popup.active.move_down();
                true
            }
            crossterm::event::KeyCode::Tab => {
                if let Some(completion) = self.popup.active.completion_text() {
                    self.input.set_text(&completion);
                    self.update_popup();
                }
                true
            }
            crossterm::event::KeyCode::Enter => {
                if self.popup.active.should_submit() {
                    // Complete the input from popup selection, then route through
                    // submit_message() — the single command dispatch path.
                    match &self.popup.active {
                        ActivePopup::Command {
                            rows,
                            state,
                            filter,
                        } if rows
                            .get(state.selected)
                            .is_some_and(|r| candidate_request_for(&r.name).is_none()) =>
                        {
                            let row = &rows[state.selected];
                            let typed_cmd = format!("/{}", filter);
                            if typed_cmd != row.name {
                                let original = self.input.text();
                                let args = original.find(' ').map(|p| &original[p..]).unwrap_or("");
                                self.input.set_text(&format!("{}{}", row.name, args));
                            }
                        }
                        _ => {
                            if let Some(completion) = self.popup.active.completion_text() {
                                self.input.set_text(&completion);
                            }
                        }
                    }
                    self.popup.active = ActivePopup::None;
                    let text = self.input.expand_and_get_text();
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        self.submit_message(&text);
                    }
                    self.input.clear();
                } else {
                    if let Some(completion) = self.popup.active.completion_text() {
                        self.input.set_text(&completion);
                        self.update_popup();
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Handle a gateway event.
    fn handle_event(&mut self, event: WingEvent) {
        // Defense-in-depth: skip events from other sessions.
        // Protects against backend events emitted without scope="session".
        //
        // Session lifecycle events (SyncSession) are always accepted —
        // their session_id may intentionally differ from the current one.
        //
        // Goal mode: also accept events from the checker session.
        if !matches!(event, WingEvent::SyncSession { .. })
            && let Some(event_sid) = event.session_id()
            && event_sid != self.session_id
            && !self.is_goal_session(event_sid)
        {
            tracing::debug!(
                event_type = %event.event_type(),
                event_session = event_sid,
                current_session = %self.session_id,
                "skipping event from different session"
            );
            return;
        }

        tracing::debug!(event_type = %event.event_type(), "handling event");

        match event {
            // ---- Lifecycle events ----
            WingEvent::Delivered { .. } => {
                // Transport-level ack — does NOT enter working state.
                // Working is triggered by TurnStarted (agent-level event).
                tracing::debug!("message delivered");
            }
            WingEvent::TurnStarted { .. } => {
                // Agent begins processing a user message.
                // Guard: only initialize timer if not already working
                // (Ask answers may trigger a second turn within the same logical flow).
                tracing::debug!("turn started, was_working={}", self.turn.working);
                self.turn.start();
                self.turn.last_result = None;
                self.chat.reset_thinking_count();
                // Set title to working state with initial spinner frame.
                let working_title = title::title_working(
                    self.turn.spinner.frame_str(),
                    self.dir_label().as_deref(),
                );
                self.turn.last_title = Some(working_title.clone());
                self.push_intent(AppIntent::SetTitle(working_title));
            }
            WingEvent::Done { .. } => {
                self.finish_turn();
                self.clear_ask_state();
                // Restore idle title — but if the user is not focused and a
                // TurnResult just set an attention title (✓/⚠), preserve it
                // until the user refocuses (Focus handler restores idle).
                let had_result = self.turn.last_result.take().is_some();
                if self.focused || !had_result {
                    self.push_intent(AppIntent::SetTitle(title::title_idle(
                        self.dir_label().as_deref(),
                    )));
                }
            }
            WingEvent::Interrupted { meta, .. } => {
                self.finish_turn();
                self.clear_ask_state();
                // Goal mode: record which role was interrupted.
                let goal_role = self.goal_role_for_session(meta.session_id.as_deref());
                if let Some(goal) = self.goal.as_mut()
                    && let Some(role) = goal_role
                {
                    let actions = goal.on_interrupted(role);
                    self.execute_goal_actions(actions);
                }
                self.show_toast(Toast::info(
                    "Agent interrupted",
                    std::time::Duration::from_secs(2),
                ));
                // Restore idle title (no BEL — user triggered this).
                self.push_intent(AppIntent::SetTitle(title::title_idle(
                    self.dir_label().as_deref(),
                )));
            }
            WingEvent::Error { message, .. } => {
                self.finish_turn();
                self.chat.push(ChatCell::ErrorMessage(message.clone()));
                self.notify_unfocused(message.clone(), AttentionKind::Error);
            }

            // ---- Reasoning events ----
            WingEvent::Reasoning { content, .. } => {
                self.chat.append_to_last_thinking(&content);
                self.ctx.current_thinking = Some(self.chat.len().saturating_sub(1));
                // Increment thinking event count for hidden mode indicator.
                self.chat.increment_thinking_count();
            }

            // ---- Text events ----
            WingEvent::Text { content, .. } => {
                self.ctx.current_thinking = None;
                self.chat.append_to_last_assistant(&content);
                self.ctx.current_assistant = Some(self.chat.len().saturating_sub(1));
                self.ctx.last_usage_target = self.ctx.current_assistant;
            }

            // ---- Tool events ----
            WingEvent::ToolCall {
                tool_name,
                tool_args,
                tool_call_id,
                ..
            } => {
                self.ctx.current_thinking = None;
                self.ctx.current_assistant = None;

                let mut block =
                    ToolCallBlock::new(tool_name.clone(), tool_args.clone(), tool_call_id.clone());
                // Start timer for Bash tools.
                if tool_name == TOOL_BASH {
                    block.started_at = Some(std::time::Instant::now());
                }
                let idx = self.chat.len();
                self.chat.push(ChatCell::ToolCall(block));
                self.ctx.register_tool_call(tool_call_id, idx);

                tracing::debug!(tool = %tool_name, "tool call started");
            }
            WingEvent::ToolCallResult {
                tool_name,
                tool_args,
                tool_call_id,
                tool_result,
                tool_success,
                ..
            } => {
                self.handle_tool_result(
                    tool_name,
                    tool_args,
                    tool_call_id,
                    tool_result,
                    tool_success,
                );
            }

            // ---- Diff events ----
            WingEvent::DiffContent {
                path,
                old_text,
                new_text,
                ..
            } => {
                let diff = DiffView::new(path, old_text, new_text);
                self.chat.push(ChatCell::Diff(diff));
            }

            // ---- Metrics events ----
            WingEvent::LlmCallMetrics {
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                tokens_per_sec,
                first_chunk_rt_ms,
                ..
            } => {
                self.status.session_prompt_tokens += prompt_tokens;
                self.status.session_completion_tokens += completion_tokens;
                self.status.session_cached_tokens += cached_tokens;

                self.turn.usage = TurnUsage {
                    prompt_tokens,
                    completion_tokens,
                    cached_tokens,
                    tokens_per_sec,
                    ttft_ms: first_chunk_rt_ms,
                };

                tracing::debug!(
                    prompt_tokens,
                    completion_tokens,
                    cached_tokens,
                    tokens_per_sec,
                    first_chunk_rt_ms,
                    "LLM metrics"
                );
            }

            // ---- Ask events ----
            WingEvent::Ask {
                tool_call_id,
                questions,
                question,
                choices,
                required,
                ..
            } => {
                if !questions.is_empty() {
                    // Multi-question flow (AskUserQuestion tool).
                    let msg = AskMessage::new_multi(tool_call_id.clone(), questions.clone());
                    self.chat.push(ChatCell::Ask(msg));
                    let flow = ask_flow::AskFlow::new(tool_call_id.clone(), questions.clone());
                    self.ask_flows.push_back(flow);
                    self.refresh_ask_placeholder();
                    let notify_text = questions
                        .first()
                        .map(|q| q.question.clone())
                        .unwrap_or_default();
                    self.notify_unfocused(notify_text, AttentionKind::Ask);
                } else {
                    // Legacy single-question (Bash dangerous command confirmation).
                    let msg =
                        AskMessage::new(tool_call_id.clone(), question.clone(), choices.clone());
                    self.chat.push(ChatCell::Ask(msg));
                    if required && !choices.is_empty() {
                        let sel =
                            crate::ui::ask_select::AskSelection::new(tool_call_id.clone(), choices);
                        self.ask_selections.push_back(sel);
                        self.refresh_ask_placeholder();
                        if let Some(front) = self.ask_selections.front()
                            && front.tool_call_id == tool_call_id
                        {
                            self.chat.update_ask_selection(&tool_call_id, 0);
                        }
                    }
                    self.notify_unfocused(question.clone(), AttentionKind::Ask);
                }
                // Bring the ask into view so the user sees it immediately and
                // understands why Up/Down now navigate the selection menu
                // (alternate-scroll translates trackpad into arrow keys).
                self.chat.jump_bottom();
            }

            // ---- Candidate list events (for popup) ----
            // Note: CommandList, ModelList, AgentList, SessionList removed — now via HTTP.
            // BranchTargets is still emitted by /rewind <uuid> after execution.
            WingEvent::BranchTargets { targets, .. } => {
                self.popup.cache.branches = targets
                    .iter()
                    .map(|t| {
                        let preview =
                            crate::ui::cells::tool_call::truncate_by_chars(&t.content, 80);
                        (t.uuid.clone(), preview)
                    })
                    .collect();
                self.update_popup();
            }

            // ---- State events ----
            WingEvent::ContextStats {
                total_tokens,
                context_window_tokens,
                message_count,
                ..
            } => {
                self.status.total_tokens = total_tokens;
                if context_window_tokens > 0 {
                    self.status.context_window_tokens = context_window_tokens;
                }
                tracing::debug!(
                    total_tokens,
                    context_window_tokens,
                    message_count,
                    "context stats"
                );
            }
            WingEvent::SessionStateChanged {
                model,
                thinking,
                reasoning_effort,
                yolo,
                title,
                agent,
                ..
            } => {
                self.status.apply_session_update(
                    model,
                    agent,
                    title,
                    thinking,
                    reasoning_effort,
                    yolo,
                );
            }
            WingEvent::SyncSession {
                session_id,
                messages,
                draft,
                name,
                agent,
                ..
            } => {
                // Goal mode: ignore SyncSession from checker session.
                // We subscribe to checker for events only — it must NOT
                // replace the current session or clear the chat.
                if self.goal.is_some() && self.is_goal_session(&session_id) {
                    tracing::debug!(session_id, "ignoring checker SyncSession in goal mode");
                    return;
                }

                // Update session_id to the new session (Phase 3c).
                self.session_id = session_id;

                // Clear old content + reset render context before replay.
                // SyncSession is a full state replacement — not an append.
                self.chat.clear();
                self.ctx.reset();

                // Replay session history.
                replay::replay_messages(&mut self.chat, &messages);
                tracing::info!(
                    message_count = messages.len(),
                    has_draft = draft.is_some(),
                    "session replayed"
                );

                // Restore draft to input box if present.
                if let Some(draft_text) = draft {
                    self.input.set_text(&draft_text);
                }

                // Update session name if provided.
                if let Some(session_name) = name {
                    self.status.session_name = Some(session_name);
                }

                // Restore model + workdir from agent snapshot.
                if let Some(agent_info) = agent {
                    self.status.model = agent_info.model_name;
                    self.status.workdir = agent_info.workspace;
                }

                self.refresh_copy_candidates();
            }
            // ---- Turn result (rich completion data) ----
            WingEvent::TurnResult {
                subtype,
                is_error,
                duration_ms,
                num_turns,
                result,
                usage,
                meta,
                ..
            } => {
                // Extract total tokens from usage JSON.
                // Note: cached_tokens is a subset of input_tokens (cache hit),
                // so total = input + output (not input + output + cached).
                let total_tokens = usage.as_ref().map(|u| {
                    let get = |key| u.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
                    get("input_tokens") + get("output_tokens")
                });

                // Store turn result for Done handler consumption.
                self.turn.last_result = Some(crate::app::turn_state::TurnResultSummary {
                    subtype: subtype.clone(),
                    is_error,
                    duration_ms,
                    num_turns,
                    result: result.clone(),
                    total_tokens,
                });
                tracing::debug!(
                    subtype = %subtype,
                    is_error,
                    duration_ms,
                    num_turns,
                    "turn result received"
                );

                // Goal mode: drive the orchestration loop.
                let goal_role = self.goal_role_for_session(meta.session_id.as_deref());
                if let Some(goal) = self.goal.as_mut()
                    && let Some(role) = goal_role
                {
                    let actions = goal.on_turn_result(role, result.clone());
                    self.execute_goal_actions(actions);
                }

                // Notify user if terminal is not focused.
                let msg = crate::util::osc9::fmt_turn_result(
                    result.as_deref(),
                    num_turns,
                    duration_ms,
                    total_tokens,
                );
                let kind = if is_error {
                    AttentionKind::Error
                } else {
                    AttentionKind::Done
                };
                self.notify_unfocused(msg, kind);
            }
            _ => {
                tracing::debug!(
                    event_type = %event.event_type(),
                    "unhandled event"
                );
            }
        }
    }

    /// Handle tool call results with tool-specific routing.
    ///
    /// Rendering decisions (hidden, full, truncated) are delegated to
    /// `ToolCallBlock::to_lines()` via the `ToolRenderer` strategy pattern.
    fn handle_tool_result(
        &mut self,
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        tool_result: String,
        tool_success: bool,
    ) {
        // Special handling for TodoWrite — render as TodoMessage cell.
        if tool_name == TOOL_TODO
            && tool_success
            && let Some(todo) = TodoMessage::from_tool_args(&tool_args)
        {
            self.chat.push(ChatCell::Todo(todo));
            tracing::debug!("todo updated");
            return;
        }

        // Unified path: set result on existing ToolCallBlock, or create orphan.
        if let Some(idx) = self.ctx.get_tool_call_index(&tool_call_id) {
            self.chat
                .set_tool_result_by_index(idx, tool_result, tool_success);
        } else {
            let mut block = ToolCallBlock::new(tool_name, tool_args, tool_call_id);
            block.set_result(tool_result, tool_success);
            self.chat.push(ChatCell::ToolCall(block));
        }
    }

    /// Draw the UI.
    fn draw(&mut self, terminal: &mut WingTerminal) -> Result<()> {
        let mut chat_height: u16 = 0;

        // Extract render context before the draw closure borrows self.
        let palette = self.palette;
        let layout = self.config.layout.clone();
        let thinking_mode = self.config.rendering.thinking;
        let goal_role_label: Option<String> = self.goal.as_ref().and_then(|g| {
            g.active_role()
                .map(|r| format!("{} {}", r.label(), r.working_verb()))
        });

        terminal.draw(|frame| {
            let area = frame.area();
            self.terminal_width = area.width;

            // Layout: status_bar (1) | chat-with-composer-tail (fill).
            // The input, pop-down popup, working line and telemetry bar
            // are rendered as the trailing segment of the scrollable chat
            // content (see ChatViewWidget), not as separate layout blocks.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(area);

            // Status bar (model top-left, cumulative usage, connection).
            frame.render_widget(
                StatusBar::new(&self.status, self.is_wide(), &palette),
                chunks[0],
            );

            // Chat view + composer tail.
            chat_height = chunks[1].height;
            let ctx = crate::render::renderable::CellContext {
                palette: &palette,
                thinking_mode,
                layout: &layout,
            };
            let popup_data = self.popup.active.render_data();
            let tail = ComposerTail {
                input: &mut self.input,
                usage: &self.turn.usage,
                workdir: self.status.workdir.as_deref(),
                working: self.turn.working,
                spinner: &self.turn.spinner,
                started_at: self.turn.started_at,
                role_label: goal_role_label.as_deref(),
                popup: popup_data,
            };
            let widget = ChatViewWidget::new(&mut self.chat, ctx).with_tail(tail);
            frame.render_widget(widget, chunks[1]);

            // Toast overlay (rendered last, on top of everything).
            if let Some(ref toast) = self.toast
                && !toast.is_expired()
            {
                render_toast(toast, area, frame.buffer_mut(), &palette);
            }
        })?;

        // Lazy cleanup expired toast after draw closure.
        if self.toast.as_ref().is_some_and(|t| t.is_expired()) {
            self.toast = None;
        }

        self.visible_height = chat_height as usize;

        // Position cursor in the input area; hide it when the input is
        // scrolled out of view.
        let size = terminal.size()?;
        match self.chat.input_card_rect {
            Some(rect) => {
                let (cursor_x, cursor_y) = cursor_screen_pos(&self.input, &rect);
                let cursor_x = cursor_x.min(size.width.saturating_sub(1));
                let cursor_y = cursor_y.min(size.height.saturating_sub(1));
                execute!(terminal.backend_mut(), Show, MoveTo(cursor_x, cursor_y))?;
            }
            None => {
                execute!(terminal.backend_mut(), Hide)?;
            }
        }

        Ok(())
    }
}

/// Run the main application loop.
pub async fn run_app(
    terminal: &mut WingTerminal,
    transport: Transport,
    session_id: String,
    endpoint: GatewayEndpoint,
    config: AppConfig,
    launch_workspace: Option<String>,
) -> Result<()> {
    let mut app = App::new(session_id, config, launch_workspace);
    let mut term_events = crate::tui::spawn_event_stream();
    let mut transport = Some(transport);
    let mut reconnect_attempt: u32 = 0;
    let mut reconnect_at = std::time::Instant::now();

    // Channel for background fetch results.
    let (fetch_tx, mut fetch_rx) =
        tokio::sync::mpsc::channel::<crate::app::intent::FetchResult>(32);

    // Request system info on startup via HTTP intents.
    app.push_intent(AppIntent::FetchInfo);
    app.push_intent(AppIntent::FetchCommands);

    // Set initial terminal title (workdir suffix appears once FetchInfo lands).
    {
        let writer = terminal.backend_mut();
        let _ = title::set_title(writer, &title::title_idle(app.dir_label().as_deref()));
    }

    loop {
        // Draw.
        if let Err(e) = app.draw(terminal) {
            tracing::error!("draw error: {e}");
        }

        // Execute all pending intents produced during the last event cycle.
        for intent in app.drain_intents() {
            runner::execute_intent(&mut app, &transport, terminal, intent, &fetch_tx).await;
        }

        if app.should_quit {
            break;
        }

        // Compute toast expiry sleep for the select below.
        let toast_remaining = app
            .toast
            .as_ref()
            .filter(|t| !t.is_expired())
            .map(|t| t.remaining());

        // Reconnect sleep (only when disconnected).
        let need_reconnect = transport.is_none();
        let reconnect_sleep = async {
            if need_reconnect {
                let now = std::time::Instant::now();
                if reconnect_at > now {
                    tokio::time::sleep(reconnect_at - now).await;
                }
            } else {
                std::future::pending::<()>().await;
            }
        };

        // Select from term events and gateway events.
        tokio::select! {
            Some(term_event) = term_events.recv() => {
                match term_event {
                    TermEvent::Key(key) => {
                        app.handle_key(key);
                    }
                    TermEvent::Paste(text) => {
                        app.input.insert_str(&text);
                        app.update_popup();
                        // Pasted content brings the input back into view.
                        app.chat.jump_bottom();
                    }
                    TermEvent::Resize(_, _) => {}
                    TermEvent::Focus(focused) => {
                        app.focused = focused;
                        // On focus regain, restore the correct title.
                        if focused {
                            let dir = app.dir_label();
                            let title = if app.turn.working {
                                title::title_working(
                                    app.turn.spinner.frame_str(),
                                    dir.as_deref(),
                                )
                            } else {
                                title::title_idle(dir.as_deref())
                            };
                            app.push_intent(AppIntent::SetTitle(title));
                        }
                    }
                    TermEvent::Tick => {
                        let now = std::time::Instant::now();
                        let dt = now.duration_since(app.last_tick);
                        app.last_tick = now;
                        if app.turn.working {
                            app.turn.spinner.tick(dt);
                            // Update title only when spinner frame changed.
                            let new_title = title::title_working(
                                app.turn.spinner.frame_str(),
                                app.dir_label().as_deref(),
                            );
                            if app.turn.last_title.as_deref() != Some(&new_title) {
                                app.turn.last_title = Some(new_title.clone());
                                app.push_intent(AppIntent::SetTitle(new_title));
                            }
                        }
                        // Refresh Bash tool timers.
                        app.chat.tick_bash_timers();
                    }
                }
            }
            event = async {
                transport.as_mut().unwrap().ws.recv_event().await
            }, if transport.is_some() => {
                match event {
                    Some(e) => app.handle_event(e),
                    None => {
                        // Disconnected.
                        transport = None;
                        app.set_connected(false);
                        app.show_toast(Toast::persistent(
                            "⚡ Connection lost — reconnecting...",
                            ToastKind::Warning,
                        ));
                        reconnect_attempt = 0;
                        reconnect_at = std::time::Instant::now() + backoff(0);
                    }
                }
            }
            // Reconnect timer (only fires when disconnected).
            _ = reconnect_sleep => {
                match try_reconnect(&endpoint, &app.session_id).await {
                    Ok(new_transport) => {
                        transport = Some(new_transport);
                        app.set_connected(true);
                        app.clear_toast();
                        app.show_toast(Toast::info(
                            "Reconnected!",
                            std::time::Duration::from_secs(2),
                        ));
                        reconnect_attempt = 0;

                        // Re-request info and commands via HTTP intents.
                        app.push_intent(AppIntent::FetchInfo);
                        app.push_intent(AppIntent::FetchCommands);
                    }
                    Err(e) => {
                        reconnect_attempt += 1;
                        reconnect_at = std::time::Instant::now() + backoff(reconnect_attempt);
                        tracing::warn!(
                            "reconnect attempt {reconnect_attempt} failed: {e}"
                        );
                    }
                }
            }
            // Toast expiry: trigger a redraw to clear the toast overlay.
            _ = async {
                if let Some(d) = toast_remaining {
                    tokio::time::sleep(d).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                // Toast expired — next draw() will lazy-cleanup.
            }
            // Background fetch results (non-blocking HTTP queries).
            Some(result) = fetch_rx.recv() => {
                app.handle_fetch_result(result);
            }
            else => {
                break;
            }
        }
    }

    // Restore terminal title on exit.
    {
        let writer = terminal.backend_mut();
        let _ = title::set_title(writer, "");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::ui::popup::command::SessionCandidate;

    /// Create a minimal App for command dispatch testing.
    fn test_app() -> App {
        App::new("test-session".into(), AppConfig::default(), None)
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

    /// Assert that `text` is consumed as a command (not sent to LLM).
    fn assert_consumed(text: &str) {
        let mut app = test_app();
        assert!(
            app.try_frontend_command(text),
            "expected '{text}' to be consumed as a command"
        );
    }

    /// Assert that `text` is NOT consumed (falls through to LLM).
    fn assert_not_consumed(text: &str) {
        let mut app = test_app();
        assert!(
            !app.try_frontend_command(text),
            "expected '{text}' to fall through to LLM"
        );
    }

    // ── Dispatch completeness: all known commands are consumed ──

    #[test]
    fn test_frontend_commands_consumed() {
        for cmd in [
            "/clear",
            "/new",
            "/copy",
            "/copy 1",
            "/goal do something",
            "/goal-exit",
        ] {
            assert_consumed(cmd);
        }
    }

    #[test]
    fn test_http_commands_consumed() {
        for cmd in [
            "/context",
            "/skills",
            "/model",
            "/agents",
            "/model gpt-4o",
            "/agents coder",
            "/title",
            "/title my session",
            "/workdir",
            "/workdir /tmp",
            "/think",
            "/think on",
            "/think off",
            "/think high",
            "/yolo",
            "/yolo on",
            "/yolo off",
            "/compact",
            "/reload",
            "/fork abc-123",
            "/rewind def-456",
            "/session sess-789",
            "/ss sess-789",
        ] {
            assert_consumed(cmd);
        }
    }

    // ── Bare commands with no args are consumed (usage toast) ──

    #[test]
    fn test_bare_commands_consumed() {
        for cmd in ["/fork", "/rewind", "/session", "/ss"] {
            assert_consumed(cmd);
        }
    }

    // ── Trailing space with empty args is consumed (not sent to LLM) ──

    #[test]
    fn test_trailing_space_consumed() {
        for cmd in [
            "/fork ",
            "/rewind ",
            "/session ",
            "/ss ",
            "/model ",
            "/agents ",
        ] {
            assert_consumed(cmd);
        }
    }

    // ── Unknown commands fall through to LLM ──

    #[test]
    fn test_unknown_commands_fall_through() {
        for cmd in ["hello world", "/unknown", "/forkabc", "fork abc", "/titlex"] {
            assert_not_consumed(cmd);
        }
    }

    // ── Intent correctness for key commands ──

    #[test]
    fn test_fork_produces_intent() {
        let mut app = test_app();
        app.try_frontend_command("/fork uuid-123");
        let intents = app.drain_intents();
        assert!(intents.iter().any(
            |i| matches!(i, AppIntent::ForkSession { target_uuid } if target_uuid == "uuid-123")
        ));
    }

    #[test]
    fn test_rewind_produces_intent() {
        let mut app = test_app();
        app.try_frontend_command("/rewind uuid-456");
        let intents = app.drain_intents();
        assert!(intents.iter().any(
            |i| matches!(i, AppIntent::RewindSession { target_uuid } if target_uuid == "uuid-456")
        ));
    }

    #[test]
    fn test_session_produces_intent() {
        let mut app = test_app();
        app.try_frontend_command("/session sess-abc");
        let intents = app.drain_intents();
        assert!(intents.iter().any(
            |i| matches!(i, AppIntent::ResumeSession { session_id } if session_id == "sess-abc")
        ));
    }

    #[test]
    fn test_ss_alias_produces_intent() {
        let mut app = test_app();
        app.try_frontend_command("/ss sess-def");
        let intents = app.drain_intents();
        assert!(intents.iter().any(
            |i| matches!(i, AppIntent::ResumeSession { session_id } if session_id == "sess-def")
        ));
    }

    #[test]
    fn test_model_produces_intent() {
        let mut app = test_app();
        app.try_frontend_command("/model gpt-4o");
        let intents = app.drain_intents();
        assert!(intents.iter().any(
            |i| matches!(i, AppIntent::UpdateSession { model: Some(m), .. } if m == "gpt-4o")
        ));
    }

    #[test]
    fn test_title_set_produces_intent() {
        let mut app = test_app();
        app.try_frontend_command("/title my project");
        let intents = app.drain_intents();
        assert!(intents.iter().any(
            |i| matches!(i, AppIntent::UpdateSession { title: Some(t), .. } if t == "my project")
        ));
    }

    #[test]
    fn test_bare_title_no_intent() {
        let mut app = test_app();
        app.try_frontend_command("/title");
        let intents = app.drain_intents();
        assert!(
            intents.is_empty(),
            "bare /title should only show toast, no intent"
        );
    }

    #[test]
    fn test_bare_fork_no_intent() {
        let mut app = test_app();
        app.try_frontend_command("/fork");
        let intents = app.drain_intents();
        assert!(
            intents.is_empty(),
            "bare /fork should only show usage toast, no intent"
        );
    }
}
