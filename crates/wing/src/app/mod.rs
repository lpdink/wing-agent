//! Application state machine and main event loop.

pub mod ask_panel;
pub mod constants;
pub mod goal;
pub mod intent;
pub mod model_panel;
pub mod popup_state;
pub mod render_context;
pub mod replay;
pub mod runner;
pub mod selection_panel;
pub mod transport;
pub mod turn_state;

pub use intent::AppIntent;

use anyhow::Result;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;

use self::transport::GatewayEndpoint;
use self::transport::Transport;
use self::transport::backoff;
use self::transport::connect_transport;
use crate::protocol::{AskQuestion, EventMeta, WingEvent};
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
use crate::ui::chat_view::render_info_separator;
use crate::ui::header::build_header_lines;
use crate::ui::input_area::InputAction;
use crate::ui::input_area::InputArea;
use crate::ui::input_area::InputAreaWidget;
use crate::ui::input_area::cursor_screen_pos;
use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::candidate_request_for;
use crate::ui::popup::command::is_must_select_command;
use crate::ui::popup::command::parse_slash_input;
use crate::ui::popup::selection::SelectionPopup;
use crate::ui::spinner::WorkingIndicatorWidget;
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
    /// Force a full repaint on the next draw.
    ///
    /// ratatui is a diff-based renderer: it only repaints cells whose buffer
    /// value changed. When the physical terminal is disturbed out-of-band
    /// (focus regain, resize), its screen no longer matches ratatui's back
    /// buffer and the diff leaves stale glyphs behind ("rendering residue").
    /// Setting this flag makes the next draw call `Terminal::clear` first,
    /// which resets the back buffer so the whole screen is repainted.
    needs_full_redraw: bool,
    /// Chat (or any non-input UI state) changed since the last draw —
    /// coalesced by the frame gate.
    chat_dirty: bool,
    /// User input arrived — bypasses the frame gate (immediate draw).
    input_dirty: bool,
    /// Timestamp of the last executed draw (frame-gate reference).
    last_draw: std::time::Instant,
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
    /// Queued AskUserQuestion panels (multi-question / multi-select asks).
    /// Concurrent asks queue up; the front entry is the active one.
    ask_panels: std::collections::VecDeque<ask_panel::AskPanel>,
    /// `/model` selection panel (None = closed). Modal while open.
    model_panel: Option<model_panel::ModelPanel>,
    /// Last successful `/api/models` response — `/model` opens the panel from
    /// this cache instantly and refreshes in the background.
    model_sources: Vec<wing_api_client::models::ProviderModels>,
    /// `/model` was requested but the sources were not yet fetched (no cache
    /// at the time of request).  When true, a successful FetchModels result
    /// auto-opens the panel even though the user has not yet seen one.
    /// Cleared on any explicit panel close (Esc, apply) and on delivery.
    model_panel_pending: bool,
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
            needs_full_redraw: true,
            chat_dirty: false,
            input_dirty: false,
            last_draw: std::time::Instant::now(),
            ctrl_c_count: 0,
            ctrl_c_last: None,
            ctx: RenderContext::new(),
            popup: PopupState::default(),
            turn: TurnState::default(),
            last_tick: std::time::Instant::now(),
            toast: None,
            ask_selections: std::collections::VecDeque::new(),
            ask_panels: std::collections::VecDeque::new(),
            model_panel: None,
            model_sources: Vec::new(),
            model_panel_pending: false,
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

    /// Clear all queued ask state (selections + panels) and remove their
    /// cells from chat. Called when a turn ends or is interrupted — the
    /// backend cancels all feedback waiters at the same time.
    fn clear_ask_state(&mut self) {
        if self.ask_selections.is_empty() && self.ask_panels.is_empty() {
            return;
        }
        for sel in self.ask_selections.drain(..) {
            self.chat.remove_ask(&sel.tool_call_id);
        }
        for panel in self.ask_panels.drain(..) {
            self.chat.remove_ask(&panel.tool_call_id);
        }
        self.input.placeholder = "今天构建什么？".into();
    }

    /// Refresh the input placeholder to reflect the active (front) ask state.
    ///
    /// Selection takes precedence over panel: while a selection is active its
    /// key handler captures all keys, so the panel cannot be answered yet.
    fn refresh_ask_placeholder(&mut self) {
        if self.ask_selections.front().is_some() {
            self.input.placeholder = "↑↓ select · Enter confirm".into();
        } else if self.ask_panels.front().is_some() {
            self.input.placeholder = "Answering above · Esc to interrupt".into();
        } else {
            self.input.placeholder = "今天构建什么？".into();
        }
    }

    /// Register the answerable state for a replayed pending ask.
    ///
    /// Resume replay builds the Ask *cell* in `replay_events` (chain-ordered
    /// with diffs); this makes it *interactive* by registering the same reply
    /// channel the live path uses — an `AskPanel`, or a legacy required
    /// `AskSelection` — so answering routes to `post(tool_call_id)` and
    /// resolves the backend waiter. The live `WingEvent::Ask` branch keeps
    /// its own inline registration; this mirrors only the panel state, not
    /// the cell push / notification.
    fn register_ask_panel(
        &mut self,
        tool_call_id: &str,
        questions: &[AskQuestion],
        choices: &[String],
        required: bool,
    ) {
        if !questions.is_empty() {
            let panel = ask_panel::AskPanel::new(tool_call_id.to_string(), questions.to_vec());
            self.chat.update_ask_panel(tool_call_id, panel.clone());
            self.ask_panels.push_back(panel);
        } else if required && !choices.is_empty() {
            self.ask_selections
                .push_back(crate::ui::ask_select::AskSelection::new(
                    tool_call_id.to_string(),
                    choices.to_vec(),
                ));
            if let Some(front) = self.ask_selections.front()
                && front.tool_call_id == tool_call_id
            {
                self.chat.update_ask_selection(tool_call_id, 0);
            }
        }
        self.refresh_ask_placeholder();
    }

    /// Sync the front panel's state into its chat cell (render snapshot).
    fn sync_front_panel(&mut self) {
        if let Some(panel) = self.ask_panels.front() {
            let id = panel.tool_call_id.clone();
            let panel = panel.clone();
            self.chat.update_ask_panel(&id, panel);
        }
    }

    /// Send the front panel's final reply and pop it from the queue.
    ///
    /// Goal mode routes to the active session (executor/checker), mirroring
    /// the legacy selection path.
    fn finish_ask_panel(&mut self, content: String) {
        let Some(panel) = self.ask_panels.pop_front() else {
            return;
        };
        let tool_call_id = panel.tool_call_id.clone();
        if let Some(goal) = &self.goal
            && let Some(role) = goal.active_role()
        {
            let actions = match role {
                goal::GoalRole::Executor => vec![goal::GoalAction::SendToExecutor {
                    content,
                    tool_call_id: Some(tool_call_id),
                }],
                goal::GoalRole::Checker => vec![goal::GoalAction::SendToChecker {
                    content,
                    tool_call_id: Some(tool_call_id),
                }],
            };
            self.execute_goal_actions(actions);
        } else {
            self.push_intent(AppIntent::SendMessage {
                content,
                tool_call_id: Some(tool_call_id),
                request_id: crate::protocol::generate_request_id(),
            });
        }
        self.refresh_ask_placeholder();
    }

    /// Mark the UI as changed (coalesced to ~60fps by the frame gate).
    /// Call after any side effect that may have mutated visible state
    /// outside the event handlers — intent execution, toasts, focus changes.
    fn mark_dirty(&mut self) {
        self.chat_dirty = true;
    }

    /// Frame-gated draw decision.
    ///
    /// WS stream events (deltas, tool updates, …) only mark the chat
    /// dirty and are coalesced to [`MIN_FRAME_INTERVAL`] — the draw rate
    /// decouples from the delta event rate (3000 tokens/s ≈ 50 events/s
    /// would otherwise mean 50 full draws/s). User input, resize and full
    /// repaints bypass the gate and draw immediately.
    fn should_draw_now(&mut self) -> bool {
        let immediate = self.needs_full_redraw || self.input_dirty;
        if immediate {
            self.input_dirty = false;
            self.chat_dirty = false;
            return true;
        }
        if !self.chat_dirty {
            return false;
        }
        let elapsed = self.last_draw.elapsed();
        if draw_gate(elapsed) {
            self.chat_dirty = false;
            return true;
        }
        false
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
        // Turn-end reconcile: install the full reference render for all
        // streaming cells (converges any incremental drift, frees stream
        // state). The actual render happens at the next draw, where the
        // terminal width is known.
        self.chat.finalize_streams();
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
        // A toast IS a visible change — always schedule a draw (the frame
        // gate coalesces bursts).
        self.chat_dirty = true;
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
        // Normal path: the message only moves up into chat history when the
        // model actually receives it (user_message_accepted). Until then it
        // queues in the pending area below in-flight streaming output.
        let request_id = crate::protocol::generate_request_id();
        self.chat.push_pending(request_id.clone(), text.to_string());
        self.push_intent(AppIntent::SendMessage {
            content: text.to_string(),
            tool_call_id: None,
            request_id,
        });
        self.turn.usage = TurnUsage::default();
        true
    }

    /// Handle a bracketed paste event: routed to the active ask panel's inline
    /// editor when one is up (panel is modal); the modal model panel drops it;
    /// otherwise it goes to the composer.
    fn handle_paste(&mut self, text: &str) {
        if !self.ask_panels.is_empty() {
            if let Some(panel) = self.ask_panels.front_mut() {
                panel.insert_paste(text);
            }
            self.sync_front_panel();
            return;
        }
        // The model panel is modal and owns no text field — paste is dropped.
        if self.model_panel.is_some() {
            return;
        }
        self.input.insert_str(text);
        self.update_popup();
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
            // `/model [ignored]` — args are ignored (BREAKING): model selection
            // goes through the panel so the (provider, model) pair is explicit.
            _ if text == "/model" || text.starts_with("/model ") => {
                self.open_model_panel();
                true
            }
            "/agents" => {
                self.push_intent(AppIntent::FetchAgents);
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
            _ if text == "/compact" || text.starts_with("/compact ") => {
                // `/compact` 或 `/compact <侧重指令>` —— 指令附加到压缩 prompt。
                let args = text.strip_prefix("/compact").unwrap().trim();
                let instruction = if args.is_empty() {
                    None
                } else {
                    Some(args.to_string())
                };
                self.push_intent(AppIntent::CompactSession { instruction });
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

    /// Open the `/model` panel: cache-first (instant open, background refresh)
    /// when models were fetched before, fetch-first otherwise. Refused while a
    /// turn is running — the model must not change under an executing turn.
    fn open_model_panel(&mut self) {
        if self.turn.working {
            self.show_toast(Toast::warning(
                "Can't switch model while the agent is working",
                std::time::Duration::from_secs(3),
            ));
            return;
        }
        if !self.model_sources.is_empty() {
            // Open now from cache; the fetch below refreshes in place.
            let panel =
                model_panel::ModelPanel::new(self.model_sources.clone(), self.current_model_pair());
            self.present_model_panel(panel);
            // The popup yields to the modal panel (never both at once).
            self.popup.active = ActivePopup::None;
        } else {
            self.show_toast(Toast::info(
                "Loading models…",
                std::time::Duration::from_secs(2),
            ));
            self.model_panel_pending = true;
        }
        self.push_intent(AppIntent::FetchModels);
    }

    /// Show a freshly built picker panel: store it as the interactive state
    /// and render it at the tail of the transcript (like the Ask panel).
    fn present_model_panel(&mut self, panel: model_panel::ModelPanel) {
        self.model_panel_pending = false;
        self.chat.show_model_picker(panel.clone());
        self.model_panel = Some(panel);
    }

    /// Mirror the app-owned picker state into its chat cell (render snapshot).
    fn sync_model_panel_cell(&mut self) {
        if let Some(panel) = self.model_panel.as_ref() {
            self.chat.update_model_picker(panel.clone());
        }
    }

    /// Close the picker without changing the model (Esc): drop both the
    /// interactive state and its transient cell.
    fn close_model_panel(&mut self) {
        self.model_panel_pending = false;
        self.model_panel = None;
        self.chat.remove_model_picker();
    }

    /// The session's active `(provider, model)` pair — both must be known
    /// (a model without a provider cannot be preselected unambiguously).
    fn current_model_pair(&self) -> Option<(&str, &str)> {
        let provider = self.status.provider.as_deref().filter(|p| !p.is_empty())?;
        let model = self.status.model.as_str();
        if model.is_empty() || model == "unknown" {
            return None;
        }
        Some((provider, model))
    }

    /// Apply the pair chosen in the model panel: close it, dispatch the
    /// explicit `(provider, model)` update and give immediate feedback.
    /// Refuses while a turn is running (defense-in-depth — the panel is
    /// already guarded against opening mid-turn, but a race via SyncSession
    /// / fork could start a turn while the panel is visible).
    fn apply_model_selection(&mut self, provider: String, model: String) {
        if self.turn.working {
            self.close_model_panel();
            self.show_toast(Toast::warning(
                "Can't switch model while the agent is working",
                std::time::Duration::from_secs(3),
            ));
            return;
        }
        self.close_model_panel();
        self.push_intent(AppIntent::set_model(model.clone(), Some(provider.clone())));
        self.show_toast(Toast::info(
            format!("Model: {model} ({provider})"),
            std::time::Duration::from_secs(3),
        ));
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

        // The modal model panel owns keyboard input — no popup while open.
        if self.model_panel.is_some() {
            return;
        }

        // Streaming guard: skip requests while agent is busy.
        let streaming = self.turn.working;
        let text = self.input.text().to_string();
        if let Some(action) = self.popup.update_from_input(&text) {
            if streaming {
                return;
            }
            match action {
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
                // Update model; clear provider when the model changes, since
                // Info does not carry provider info and the old provider
                // may be stale (e.g. the model was changed via another path).
                if info.model != self.status.model {
                    self.status.provider = None;
                }
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
                if resp.providers.is_empty() {
                    // Nothing to choose from — tell the user, keep any open
                    // panel as it was.
                    self.show_toast(Toast::warning(
                        "No models available",
                        std::time::Duration::from_secs(3),
                    ));
                } else {
                    self.model_sources = resp.providers;
                    if let Some(panel) = self.model_panel.as_mut() {
                        // Panel already open: refresh in place, keeping the
                        // current page and cursor, and mirror it into its cell.
                        panel.set_sources(self.model_sources.clone());
                        self.sync_model_panel_cell();
                    } else if self.model_panel_pending {
                        // The user typed `/model` before the cache was
                        // available — deliver the panel now that the fetch
                        // completed.  Clear the flag so a subsequent fetch
                        // (refresh) does NOT reopen after the user closes it.
                        self.model_panel_pending = false;
                        let panel = model_panel::ModelPanel::new(
                            self.model_sources.clone(),
                            self.current_model_pair(),
                        );
                        self.present_model_panel(panel);
                    }
                    // If the panel was closed (Esc) while the fetch was in
                    // flight, do NOT reopen it — the user's explicit action
                    // takes priority over the stale fetch result.
                }
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
                                    vec![goal::GoalAction::SendToExecutor {
                                        content: choice,
                                        tool_call_id: Some(id),
                                    }]
                                }
                                goal::GoalRole::Checker => {
                                    vec![goal::GoalAction::SendToChecker {
                                        content: choice,
                                        tool_call_id: Some(id),
                                    }]
                                }
                            };
                            self.execute_goal_actions(actions);
                        } else {
                            self.push_intent(AppIntent::SendMessage {
                                content: choice,
                                tool_call_id: Some(id),
                                request_id: crate::protocol::generate_request_id(),
                            });
                        }
                    }
                }
                _ => {} // ignore other keys while selection is active
            }
            return;
        }

        // Ask panel: capture all keys while active (queue front) — except the
        // keys the app keeps: Esc (global interrupt ladder below, so the user
        // can interrupt at any time inside the panel) and PageUp/PageDown
        // (chat still scrolls while the panel is modal).
        let panel_owns_key = !matches!(
            key.code,
            crossterm::event::KeyCode::Esc
                | crossterm::event::KeyCode::PageUp
                | crossterm::event::KeyCode::PageDown
        );
        if panel_owns_key && !self.ask_panels.is_empty() {
            let action = self
                .ask_panels
                .front_mut()
                .map(|panel| panel.handle_key(key))
                .unwrap_or(ask_panel::PanelAction::None);
            self.sync_front_panel();
            if let ask_panel::PanelAction::Reply(content) = action {
                self.finish_ask_panel(content);
            }
            return;
        }

        // Model panel: modal while open. Esc closes the panel (it is not part
        // of any turn, so it must NOT reach the interrupt ladder below);
        // PageUp/PageDown stay available for chat scrolling; every other key
        // is consumed by the panel.
        if self.model_panel.is_some()
            && !matches!(
                key.code,
                crossterm::event::KeyCode::PageUp | crossterm::event::KeyCode::PageDown
            )
        {
            let action = self
                .model_panel
                .as_mut()
                .map(|panel| panel.handle_key(key))
                .unwrap_or(model_panel::ModelPanelAction::None);
            match action {
                model_panel::ModelPanelAction::Apply { provider, model } => {
                    self.apply_model_selection(provider, model);
                }
                model_panel::ModelPanelAction::Cancel => {
                    self.close_model_panel();
                }
                // Navigation keeps the panel open — mirror the new cursor /
                // page into the chat cell.
                model_panel::ModelPanelAction::None => self.sync_model_panel_cell(),
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
        // 例外：must-select 命令保持 popup——Enter 给显式"无匹配候选"反馈，
        // 绝不落到自由文本发送（参数必须来自候选）。
        if self.popup.active.is_active() && !self.popup.active.has_items() {
            if self.popup.active.is_must_select_empty() {
                if key.code == crossterm::event::KeyCode::Enter {
                    self.show_toast(Toast::warning(
                        "No matching candidates — adjust the argument and select from the popup",
                        std::time::Duration::from_secs(4),
                    ));
                    return;
                }
            } else {
                self.popup.active = ActivePopup::None;
            }
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
        match self.input.handle_key(key, self.terminal_width) {
            InputAction::Submit(text) => {
                // Submitting a message pins the view to the bottom so the
                // sent message and the agent's reply come into view.
                self.chat.jump_bottom();
                // must-select 命令：参数必须来自候选选择。popup 若被关闭
                //（如 Esc），重开 popup 而非发送自由文本——消灭未定义请求
                //（如 /fork 携带未经验证的 uuid）。
                if let Some((cmd, _)) = parse_slash_input(&text)
                    && is_must_select_command(cmd)
                {
                    self.input.set_text(&text);
                    self.update_popup();
                    if self.popup.active.is_active() {
                        return;
                    }
                    self.input.clear();
                }
                self.popup.active = ActivePopup::None;
                self.submit_message(&text);
            }
            InputAction::Escape | InputAction::None => {
                // Update popup based on new text.
                self.update_popup();
            }
        }
        // NOTE: editing the composer must NOT yank the view back to the
        // bottom — the composer is a fixed block, and the user may be
        // reading history (e.g. item 456 of a review) while typing.
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
            WingEvent::UserMessageAccepted {
                origin_request_id, ..
            } => {
                // The message has actually been fed to the model — only now
                // may it move up into chat history. Untracked ids (goal
                // orchestration sends, other clients) are ignored: this
                // client does not render them.
                if !self.chat.promote_pending(&origin_request_id) {
                    tracing::debug!(
                        origin_request_id,
                        "user_message_accepted for untracked message, ignoring"
                    );
                }
            }
            WingEvent::Done { .. } => {
                // Turn-end safety net: promote anything still queued (e.g.
                // accepted events lost across a disconnect/reconnect).
                self.chat.promote_all_pending();
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
                // Interrupt clears the backend inbox — pending messages never
                // reached the model. Commit them as discarded (dim + struck
                // through) rather than silently vanishing.
                self.chat.discard_all_pending();
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
            WingEvent::ToolCallStream {
                tool_call_id,
                tool_name,
                args_fragment,
                is_final,
                ..
            } => {
                self.ctx.current_thinking = None;
                self.ctx.current_assistant = None;

                if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
                    // Update existing streaming cell
                    self.chat
                        .append_tool_args_fragment_by_index(idx, &args_fragment);
                    if is_final {
                        self.chat.set_tool_status_by_index(
                            idx,
                            crate::ui::cells::tool_call::ToolStatus::Pending,
                        );
                    }
                } else {
                    // Create new streaming cell
                    let mut block = ToolCallBlock::new_streaming(tool_name, tool_call_id.clone());
                    block.append_args_fragment(&args_fragment);
                    if is_final {
                        block.status = crate::ui::cells::tool_call::ToolStatus::Pending;
                    }
                    self.chat.push(ChatCell::ToolCall(block));
                }
            }
            WingEvent::ToolCall {
                tool_name,
                tool_args,
                tool_call_id,
                ..
            } => {
                self.ctx.current_thinking = None;
                self.ctx.current_assistant = None;

                // If a streaming cell already exists for this id, update it
                if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
                    self.chat.update_tool_args_by_index(idx, tool_args);
                    self.chat.set_tool_status_by_index(
                        idx,
                        crate::ui::cells::tool_call::ToolStatus::Pending,
                    );
                    // Start timer for Bash tools on execution start.
                    if tool_name == TOOL_BASH {
                        self.chat.set_tool_started_at_by_index(idx);
                    }
                } else {
                    let mut block =
                        ToolCallBlock::new(tool_name.clone(), tool_args, tool_call_id.clone());
                    // Start timer for Bash tools.
                    if tool_name == TOOL_BASH {
                        block.started_at = Some(std::time::Instant::now());
                    }
                    self.chat.push(ChatCell::ToolCall(block));
                }

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
                tool_call_id,
                ..
            } => {
                let diff = DiffView::new(path, old_text, new_text);
                // Anchor the diff directly after the ToolCall cell that
                // produced it — concurrent edits complete out of order, so
                // appending would interleave diffs arbitrarily. Unknown id
                // (older gateway / lost ToolCall event) falls back to append.
                if let Err(cell) = self
                    .chat
                    .insert_after_tool_call(&tool_call_id, ChatCell::Diff(diff))
                {
                    self.chat.push(*cell);
                }
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
                    // Multi-question panel (AskUserQuestion tool).
                    let panel = ask_panel::AskPanel::new(tool_call_id.clone(), questions.clone());
                    let msg = AskMessage::new_panel(tool_call_id.clone(), panel.clone());
                    self.chat.push(ChatCell::Ask(msg));
                    self.ask_panels.push_back(panel);
                    self.refresh_ask_placeholder();
                    let notify_text = questions
                        .first()
                        .map(|q| q.question.clone())
                        .unwrap_or_default();
                    self.notify_unfocused(notify_text, AttentionKind::Ask);
                } else {
                    // Legacy single-question (Bash dangerous command confirmation).
                    let msg = AskMessage::new_legacy(
                        tool_call_id.clone(),
                        question.clone(),
                        choices.clone(),
                    );
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
                uncommitted,
                uncommitted_tools,
                events,
                turn_started_at,
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

                // Loaded skills/rules banner: one summary line pinned at the
                // top of the chat, mirrored on every sync (connect / resume /
                // switch / fork) — counts only; details stay behind /skills
                // so the sync payload carries lists, not rendered blobs.
                if let Some(agent_info) = &agent {
                    let line = format!(
                        "loaded {} skills, {} rules · /skills for details",
                        agent_info.skills.len(),
                        agent_info.rules.len()
                    );
                    self.chat.push(ChatCell::SystemMessage(line));
                }

                // Ask flows are session-scoped interactive state, not rendered
                // history: without this, a reconnect / session switch while an
                // ask is pending would leave the live-registered flow in the
                // deque AND re-register it from the replayed pending ask —
                // the duplicate stale front entry would later swallow the
                // answer of a subsequent ask (posted to a dead tool_call_id).
                self.clear_ask_state();
                // The model panel targets the previous session (preselect +
                // apply go to its session_id) — a sync means the session
                // changed, so the panel must not survive it.
                self.model_panel = None;

                // Restore working state BEFORE feeding uncommitted content, so
                // the spinner / Bash timers / terminal title reflect an
                // in-progress turn (a mid-turn resume). `turn_started_at`
                // restores the real elapsed instead of recounting from resume.
                let turn_instant = turn_started_at
                    .as_deref()
                    .and_then(turn_state::instant_from_utc_iso);
                let mid_turn = uncommitted.is_some() || !uncommitted_tools.is_empty();
                if mid_turn {
                    self.turn.start();
                    if let Some(instant) = turn_instant {
                        self.turn.started_at = Some(instant);
                    }
                    let working_title = title::title_working(
                        self.turn.spinner.frame_str(),
                        self.dir_label().as_deref(),
                    );
                    self.turn.last_title = Some(working_title.clone());
                    self.push_intent(AppIntent::SetTitle(working_title));
                }

                // Replay order: messages → uncommitted → uncommitted_tools →
                // events. This makes diff anchoring structural: the tool_use
                // block that produced a diff is a *finalized* block, so it is
                // built as a ToolCall cell by step 1/2 before step 4 applies
                // the diff — the anchor always exists first.
                //
                // 1. Committed Message projections (text/thinking/ToolCall cells).
                replay::replay_messages(&mut self.chat, &messages);
                // 2. Uncommitted assistant Message projection — SAME replay path
                //    (a single Message payload, not a list). Never fed to
                //    handle_event as a pseudo-event.
                if let Some(uncommitted_msg) = &uncommitted {
                    replay::replay_messages(&mut self.chat, std::slice::from_ref(uncommitted_msg));
                    // A mid-execution Bash card (finalized tool_use, no result
                    // yet) shows elapsed anchored to the turn start.
                    if let Some(instant) = turn_instant {
                        self.chat.mark_pending_bash_running(instant);
                    }
                }
                // 3. Unfinished tool calls' raw args fragments — through the
                //    EXISTING live ToolCallStream branch (client-side partial
                //    parse via partial_json.rs); zero new rendering logic. The
                //    subsequent live tool_call_stream deltas append seamlessly.
                for tool in &uncommitted_tools {
                    let tool_call_id = tool
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if tool_call_id.is_empty() {
                        continue;
                    }
                    let tool_name = tool
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let args_fragment = tool
                        .get("args_fragment")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    self.handle_event(WingEvent::ToolCallStream {
                        tool_call_id: tool_call_id.to_string(),
                        tool_name: tool_name.to_string(),
                        args_fragment: args_fragment.to_string(),
                        is_final: false,
                        // session_id None → bypasses the cross-session filter
                        // (this is the session we just switched to).
                        meta: EventMeta {
                            created_at: String::new(),
                            session_id: None,
                            request_id: String::new(),
                        },
                    });
                }
                // 4. Durable fact events (diff anchored onto cells above; ask
                //    rendered as an answerable card). replay_events builds the
                //    ask cells; register their reply state so a resumed pending
                //    ask is answerable through the same channel as live.
                for ask in replay::replay_events(&mut self.chat, &events) {
                    self.register_ask_panel(
                        &ask.tool_call_id,
                        &ask.questions,
                        &ask.choices,
                        ask.required,
                    );
                }

                tracing::info!(
                    message_count = messages.len(),
                    has_uncommitted = uncommitted.is_some(),
                    uncommitted_tools_count = uncommitted_tools.len(),
                    event_count = events.len(),
                    mid_turn,
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

                // Restore model + provider + workdir from agent snapshot.
                if let Some(agent_info) = agent {
                    self.status.model = agent_info.model_name;
                    self.status.provider = agent_info.provider_name;
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
        // Special handling for TodoWrite — render as TodoMessage cell
        // anchored directly after its ToolCall cell. Concurrent tool
        // execution makes results arrive out of order, so appending would
        // misplace the todo list below unrelated cells.
        if tool_name == TOOL_TODO
            && tool_success
            && let Some(todo) = TodoMessage::from_tool_args(&tool_args)
        {
            let todo_cell = ChatCell::Todo(todo);
            if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
                // Mark the ToolCall cell Success (mirrors replay behavior;
                // the early return previously left it Pending forever).
                self.chat
                    .set_tool_result_by_index(idx, tool_result, tool_success);
                // Cannot fail — the ToolCall cell was just located above.
                let _ = self.chat.insert_after_tool_call(&tool_call_id, todo_cell);
            } else {
                // Orphan result (lost ToolCall event) — mirror the unified
                // path below: show the ToolCall block, then the todo list.
                let mut block = ToolCallBlock::new(tool_name, tool_args, tool_call_id);
                block.set_result(tool_result, tool_success);
                self.chat.push(ChatCell::ToolCall(block));
                self.chat.push(todo_cell);
            }
            tracing::debug!("todo updated");
            return;
        }

        // Unified path: set result on existing ToolCallBlock, or create orphan.
        if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
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
        // Out-of-band terminal disturbance (focus regain / resize) — see
        // `needs_full_redraw`. Reset the back buffer so this draw repaints
        // the whole screen and resyncs with the terminal.
        if self.needs_full_redraw {
            terminal.clear()?;
            self.needs_full_redraw = false;
        }

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

            // Layout: status (1) | chat (fill) | [working] | info bar | input | [popup].
            // The composer (working line + info separator + input + popup) is a
            // fixed block below the scrollable chat viewport — it stays in view
            // regardless of the chat scroll position.
            let input_h = self.input.height(area.width);
            let popup_h = self.popup.height();
            let mut constraints = vec![
                Constraint::Length(1), // status bar
                Constraint::Min(3),    // chat view (min 3 rows)
            ];
            if self.turn.working {
                constraints.push(Constraint::Length(1)); // working indicator
            }
            constraints.push(Constraint::Length(1)); // info separator
            constraints.push(Constraint::Length(input_h)); // input area
            if popup_h > 0 {
                constraints.push(Constraint::Length(popup_h)); // pop-down command popup
            }
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            // Status bar (model top-left, cumulative usage, connection).
            frame.render_widget(
                StatusBar::new(&self.status, self.is_wide(), &palette),
                chunks[0],
            );

            // Chat view — the scrollable viewport only.
            chat_height = chunks[1].height;
            let ctx = crate::render::renderable::CellContext {
                palette: &palette,
                thinking_mode,
                layout: &layout,
            };
            frame.render_widget(ChatViewWidget::new(&mut self.chat, ctx), chunks[1]);

            // Fixed composer block below the chat viewport.
            let mut idx = 2;

            // Working indicator (when a turn is running).
            if self.turn.working {
                if let Some(started_at) = self.turn.started_at {
                    frame.render_widget(
                        WorkingIndicatorWidget::new(&self.turn.spinner, started_at, &palette)
                            .with_role_label(goal_role_label.as_deref()),
                        chunks[idx],
                    );
                }
                idx += 1;
            }

            // Info separator (workdir · usage · scroll position).
            render_info_separator(
                self.status.workdir.as_deref(),
                &self.turn.usage,
                self.chat.content_height(),
                chat_height as usize,
                self.chat.scroll_position(),
                &palette,
                chunks[idx],
                frame.buffer_mut(),
            );
            idx += 1;

            // Input area — always visible; record its rect for cursor placement.
            let input_rect = chunks[idx];
            frame.render_widget(InputAreaWidget::new(&mut self.input, &palette), input_rect);
            idx += 1;

            // Pop-down command popup (below the input).
            if popup_h > 0
                && let Some((rows, state, filter)) = self.popup.active.render_data()
            {
                frame.render_widget(
                    SelectionPopup::new(rows, state, filter, &palette),
                    chunks[idx],
                );
            }

            // Position the input cursor *inside* the render pass so ratatui
            // owns cursor show/hide/move. The previous post-draw
            // `execute!(Show/Hide, MoveTo)` wrote to the backend out-of-band,
            // which desyncs ratatui's cursor tracking and is explicitly
            // discouraged by ratatui.
            let (cursor_x, cursor_y) = cursor_screen_pos(&self.input, &input_rect);
            let cursor_x = cursor_x.min(area.width.saturating_sub(1));
            let cursor_y = cursor_y.min(area.height.saturating_sub(1));
            frame.set_cursor_position((cursor_x, cursor_y));

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

        Ok(())
    }
}

/// Minimum spacing between chat-dirty draws (≈60fps frame budget).
const MIN_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Pure frame-gate predicate: a chat-dirty draw is due iff at least
/// [`MIN_FRAME_INTERVAL`] has elapsed since the last draw.
fn draw_gate(elapsed_since_last_draw: std::time::Duration) -> bool {
    elapsed_since_last_draw >= MIN_FRAME_INTERVAL
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
    // Session recovery pending: the transport (WS) is up but the session
    // event subscription isn't re-established yet. Cleared once
    // `recover_session` succeeds.
    let mut recovery_pending = false;
    let mut retry_attempt: u32 = 0;
    let mut retry_at = std::time::Instant::now();

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
        // Draw — frame-gated: streaming deltas coalesce, input draws
        // immediately, idle iterations skip the draw entirely.
        if app.should_draw_now() {
            if let Err(e) = app.draw(terminal) {
                tracing::error!("draw error: {e}");
            }
            app.last_draw = std::time::Instant::now();
        }

        // Execute all pending intents produced during the last event cycle.
        // Intents may mutate visible state (session switch + replay, popup
        // status, toasts on failure) — mark dirty uniformly instead of
        // relying on each intent remembering to.
        for intent in app.drain_intents() {
            runner::execute_intent(&mut app, &transport, terminal, intent, &fetch_tx).await;
            app.mark_dirty();
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

        // Retry sleep (fires when disconnected or session recovery is pending).
        let need_retry = transport.is_none() || recovery_pending;
        let retry_sleep = async {
            if need_retry {
                let now = std::time::Instant::now();
                if retry_at > now {
                    tokio::time::sleep(retry_at - now).await;
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
                        app.input_dirty = true;
                    }
                    TermEvent::Paste(text) => {
                        app.handle_paste(&text);
                        app.input_dirty = true;
                    }
                    TermEvent::Resize(_, _) => {
                        // ratatui auto-resizes its buffers on the next draw;
                        // force a full repaint so the resized screen is fully
                        // resynced with the terminal.
                        app.needs_full_redraw = true;
                    }
                    TermEvent::Focus(focused) => {
                        app.focused = focused;
                        app.mark_dirty();
                        // On focus regain, restore the correct title.
                        if focused {
                            // The terminal re-shows its surface on focus
                            // regain and may have drifted from ratatui's
                            // buffer (observed: stale glyphs on the status
                            // bar after switching tabs). Force a full repaint.
                            app.needs_full_redraw = true;
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
                            // Refresh Bash tool timers. Only meaningful while a
                            // turn is active (a tool can only be pending mid-turn);
                            // gating here keeps idle sessions from scanning cells.
                            app.chat.tick_bash_timers();
                            // Spinner animation / tool timers changed the UI —
                            // ONLY while a turn is active. An idle session's
                            // ticks draw nothing (idle iterations skip
                            // drawing entirely); toasts are static (their
                            // expiry has its own timer arm).
                            app.chat_dirty = true;
                        }
                    }
                }
            }
            event = async {
                transport.as_mut().unwrap().ws.recv_event().await
            }, if transport.is_some() => {
                match event {
                    Some(e) => {
                        app.handle_event(e);
                        // Any gateway event may have mutated chat state;
                        // coalesced by the frame gate.
                        app.chat_dirty = true;
                    }
                    None => {
                        // Disconnected.
                        transport = None;
                        app.set_connected(false);
                        app.show_toast(Toast::persistent(
                            "⚡ Connection lost — reconnecting...",
                            ToastKind::Warning,
                        ));
                        app.chat_dirty = true;
                        retry_attempt = 0;
                        retry_at = std::time::Instant::now() + backoff(0);
                    }
                }
            }
            // Retry timer: reconnect transport first, then recover the
            // session over it (two decoupled phases — session failures never
            // dispose of the WebSocket).
            _ = retry_sleep => {
                if transport.is_none() {
                    // Phase 1: transport-level reconnect. One WebSocket for
                    // everything that follows, including phase-2 retries.
                    match connect_transport(&endpoint).await {
                        Ok(new_transport) => {
                            transport = Some(new_transport);
                            recovery_pending = true;
                            retry_attempt = 0;
                            // Try session recovery immediately.
                            retry_at = std::time::Instant::now();
                        }
                        Err(e) => {
                            retry_attempt += 1;
                            retry_at = std::time::Instant::now() + backoff(retry_attempt);
                            tracing::warn!(
                                "reconnect attempt {retry_attempt} failed: {e:#}"
                            );
                        }
                    }
                } else if let Some(t) = transport.as_ref() {
                    // Phase 2: session recovery over the existing WebSocket.
                    // A resume 404 (session never persisted / deleted) makes
                    // this silently start a fresh session — like first launch.
                    // The pushed SyncSession updates App state (session_id,
                    // chat replay), exactly like the `/new` command.
                    let workspace = app.launch_workspace.clone();
                    match t.recover_session(&app.session_id, workspace.as_deref()).await {
                        Ok(()) => {
                            recovery_pending = false;
                            app.set_connected(true);
                            app.clear_toast();
                            app.show_toast(Toast::info(
                                "Reconnected!",
                                std::time::Duration::from_secs(2),
                            ));
                            // The session may have been silently replaced —
                            // the popup list must re-fetch.
                            app.invalidate_session_cache();

                            // Re-request info and commands via HTTP intents.
                            app.push_intent(AppIntent::FetchInfo);
                            app.push_intent(AppIntent::FetchCommands);
                        }
                        Err(e) => {
                            retry_attempt += 1;
                            retry_at = std::time::Instant::now() + backoff(retry_attempt);
                            tracing::warn!(
                                "session recovery attempt {retry_attempt} failed: {e:#}"
                            );
                        }
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
                app.chat_dirty = true;
            }
            // Background fetch results (non-blocking HTTP queries).
            Some(result) = fetch_rx.recv() => {
                app.handle_fetch_result(result);
                app.chat_dirty = true;
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
    use crate::app::selection_panel::SelectionPanel;
    use crate::config::AppConfig;
    use crate::ui::popup::command::SessionCandidate;

    /// Create a minimal App for command dispatch testing.
    fn test_app() -> App {
        App::new("test-session".into(), AppConfig::default(), None)
    }

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

    /// Build a SyncSession event for the test session.
    fn sync_event(
        messages: Vec<serde_json::Value>,
        uncommitted: Option<serde_json::Value>,
        uncommitted_tools: Vec<serde_json::Value>,
        events: Vec<serde_json::Value>,
        turn_started_at: Option<String>,
    ) -> WingEvent {
        WingEvent::SyncSession {
            session_id: "test-session".into(),
            messages,
            uncommitted,
            uncommitted_tools,
            events,
            turn_started_at,
            agent: None,
            name: None,
            draft: None,
            meta: EventMeta {
                created_at: "2026-01-01T00:00:00+00:00".into(),
                session_id: Some("test-session".into()),
                request_id: "r".into(),
            },
        }
    }

    fn utc_ago(secs: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
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
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn test_sync_midturn_restores_working_and_elapsed() {
        // 6.13: uncommitted non-null → working; elapsed from turn_started_at
        // (not recounted from the resume moment).
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
        app.ask_panels.push_back(ask_panel::AskPanel::new(
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
    fn test_sync_replays_pending_ask_answerable() {
        // 6.14: a pending ask replays as an Ask cell AND registers the reply
        // panel, so the user can answer it (resolves the backend waiter).
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
    }

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
    fn test_sync_replays_legacy_required_ask_selection() {
        // Legacy single-question required ask → AskSelection registered.
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
        assert_eq!(app.ask_selections.len(), 1);
        assert_eq!(app.ask_selections[0].tool_call_id, "ask-2");
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
            "/compact keep architecture decisions",
            "/reload",
            "/fork abc-123",
            "/rewind def-456",
            "/session sess-789",
            "/ss sess-789",
        ] {
            assert_consumed(cmd);
        }
    }

    // ── /compact instruction parsing ────────────────────

    #[test]
    fn test_compact_command_parses_instruction() {
        // `/compact <侧重>` → instruction rides on the intent.
        let mut app = test_app();
        assert!(app.try_frontend_command("/compact keep architecture decisions and pending TODOs"));
        let intents = app.drain_intents();
        assert!(
            matches!(
                &intents[..],
                [AppIntent::CompactSession {
                    instruction: Some(i)
                }] if i == "keep architecture decisions and pending TODOs"
            ),
            "expected CompactSession with instruction, got {intents:?}"
        );

        // Bare `/compact` → default strategy (no instruction).
        let mut app = test_app();
        assert!(app.try_frontend_command("/compact"));
        let intents = app.drain_intents();
        assert!(
            matches!(
                &intents[..],
                [AppIntent::CompactSession { instruction: None }]
            ),
            "expected CompactSession without instruction, got {intents:?}"
        );
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
    fn test_model_command_opens_panel_instead_of_direct_switch() {
        // 4.6/4.7: `/model <name>` no longer switches directly — it opens the
        // panel exactly like `/model` (BREAKING).
        let mut app = test_app();
        app.model_sources = vec![model_group("p", &["gpt-4o"])];
        app.try_frontend_command("/model gpt-4o");
        assert!(app.model_panel.is_some(), "panel must open");
        assert_eq!(app.status.model, "unknown", "no direct model change");
        let intents = app.drain_intents();
        assert!(
            !intents
                .iter()
                .any(|i| matches!(i, AppIntent::UpdateSession { .. })),
            "no direct switch request: {intents:?}"
        );
        assert!(intents.iter().any(|i| matches!(i, AppIntent::FetchModels)));
    }

    // ── /model panel (4.7) ──────────────────────────────────────

    fn model_group(provider: &str, models: &[&str]) -> wing_api_client::models::ProviderModels {
        wing_api_client::models::ProviderModels {
            provider: provider.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn feed_models(app: &mut App, providers: Vec<wing_api_client::models::ProviderModels>) {
        let session_id = app.session_id.clone();
        app.handle_fetch_result(crate::app::intent::FetchResult {
            session_id,
            payload: crate::app::intent::FetchPayload::Models(
                wing_api_client::models::ModelsResponse { providers },
            ),
        });
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// The rendered picker cell's panel snapshot, if the cell is present.
    fn picker_cell(app: &App) -> Option<&model_panel::ModelPanel> {
        app.chat.cells.iter().find_map(|c| match c.cell() {
            ChatCell::ModelPicker(panel) => Some(panel),
            _ => None,
        })
    }

    #[test]
    fn test_model_opens_panel_instantly_from_cache_and_preselects() {
        let mut app = test_app();
        app.model_sources = vec![model_group("p", &["m1", "m2"])];
        app.status.provider = Some("p".into());
        app.status.model = "m2".into();
        assert!(app.try_frontend_command("/model"));
        let panel = app.model_panel.as_ref().expect("panel opens from cache");
        assert_eq!(panel.current_page(), 0);
        assert_eq!(panel.cursor(), 1, "preselected the current model");
        assert_eq!(panel.committed_at(0), Some(1), "● on the current model");
        // Rendered in the transcript (like the Ask panel), not near the input.
        let rendered = picker_cell(&app).expect("picker cell shown in the chat");
        assert_eq!(rendered.cursor(), 1);
        let intents = app.drain_intents();
        assert!(
            intents.iter().any(|i| matches!(i, AppIntent::FetchModels)),
            "background refresh requested"
        );
    }

    #[test]
    fn test_model_refused_while_working() {
        let mut app = test_app();
        app.model_sources = vec![model_group("p", &["m1"])];
        app.turn.working = true;
        assert!(app.try_frontend_command("/model"));
        assert!(app.model_panel.is_none(), "no panel while working");
        assert!(
            !app.drain_intents()
                .iter()
                .any(|i| matches!(i, AppIntent::FetchModels))
        );
        assert!(app.toast.is_some(), "refusal toast shown");
    }

    #[test]
    fn test_model_panel_applies_explicit_provider_for_same_name_model() {
        // Regression: two providers expose the same model name — applying the
        // second one must carry the SECOND provider explicitly.
        let mut app = test_app();
        app.model_sources = vec![
            model_group("dashscope", &["shared"]),
            model_group("dashscope-openai", &["shared"]),
        ];
        app.try_frontend_command("/model");
        app.drain_intents();
        app.handle_key(key(crossterm::event::KeyCode::Right)); // → second provider
        app.handle_key(key(crossterm::event::KeyCode::Enter)); // apply
        assert!(app.model_panel.is_none(), "panel closes on apply");
        assert!(
            picker_cell(&app).is_none(),
            "the transient picker cell disappears on apply"
        );
        let intents = app.drain_intents();
        assert!(
            intents.iter().any(|i| matches!(
                i,
                AppIntent::UpdateSession { model: Some(m), provider: Some(p), .. }
                    if m == "shared" && p == "dashscope-openai"
            )),
            "explicit provider required, got {intents:?}"
        );
    }

    #[test]
    fn test_model_fetch_opens_panel_and_refreshes_in_place() {
        let mut app = test_app();
        assert!(app.try_frontend_command("/model"));
        assert!(app.model_panel.is_none(), "no cache → wait for the fetch");
        feed_models(&mut app, vec![model_group("p", &["m1", "m2"])]);
        assert_eq!(app.model_panel.as_ref().unwrap().page_count(), 1);
        assert!(
            picker_cell(&app).is_some(),
            "the fetched result shows the picker cell"
        );
        // Move the cursor, then refresh: page and cursor stay put.
        app.handle_key(key(crossterm::event::KeyCode::Down));
        assert_eq!(app.model_panel.as_ref().unwrap().cursor(), 1);
        assert_eq!(
            picker_cell(&app).unwrap().cursor(),
            1,
            "the cell mirrors the navigation"
        );
        feed_models(&mut app, vec![model_group("p", &["m1", "m2", "m3"])]);
        assert_eq!(
            app.model_panel.as_ref().unwrap().cursor(),
            1,
            "in-place refresh keeps the cursor"
        );
        assert_eq!(
            picker_cell(&app).unwrap().models().len(),
            3,
            "the open cell refreshes in place"
        );
    }

    #[test]
    fn test_model_fetch_empty_shows_toast_and_no_panel() {
        let mut app = test_app();
        assert!(app.try_frontend_command("/model"));
        feed_models(&mut app, vec![]);
        assert!(app.model_panel.is_none());
        assert!(app.toast.is_some(), "empty result gives a hint");
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

    // ── Composer pinning & scroll routing ────────────────────

    #[test]
    fn test_scroll_down_rearms_autoscroll_at_bottom() {
        let mut app = test_app();
        app.visible_height = 20;
        app.chat.last_total = 100;
        app.chat.scroll_offset = 75;
        app.chat.scroll_up(0); // leave the bottom: auto_scroll=false, offset 75

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        app.handle_key(key.clone());
        assert_eq!(app.chat.scroll_offset, 78);
        assert!(!app.chat.is_at_bottom(), "still above the bottom edge");

        // Next step reaches offset 80 == max_scroll → auto-scroll re-armed.
        app.handle_key(key);
        assert!(app.chat.is_at_bottom());
        assert_eq!(app.chat.scroll_offset, 80);
    }

    #[test]
    fn test_submit_jumps_to_bottom_and_queues_pending() {
        let mut app = test_app();
        app.chat.scroll_up(5); // reading history: auto_scroll=false
        app.input.set_text("hi");
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(
            app.chat.is_at_bottom(),
            "submit must pin the view to the bottom"
        );
        // Not committed to history yet — queued until the model accepts it.
        assert!(
            app.chat
                .cells
                .iter()
                .all(|c| !matches!(c.cell(), ChatCell::UserMessage(_))),
            "submitted message must not enter history before acceptance"
        );
        assert_eq!(app.chat.pending.len(), 1);
        assert!(matches!(
            app.chat.pending[0].cell.cell(),
            ChatCell::PendingUserMessage(s) if s == "hi"
        ));
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
    fn test_submit_intent_carries_pending_request_id() {
        let mut app = test_app();
        assert!(app.submit_message("hello"));

        assert_eq!(app.chat.pending.len(), 1);
        let pending_id = app.chat.pending[0].request_id.clone();

        let intents = app.drain_intents();
        let Some(AppIntent::SendMessage {
            request_id,
            tool_call_id,
            ..
        }) = intents.first()
        else {
            panic!("expected SendMessage intent");
        };
        assert_eq!(request_id, &pending_id);
        assert!(tool_call_id.is_none());
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

        // Not ours (goal orchestration / other client) — nothing changes.
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
}
