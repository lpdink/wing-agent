//! Application state machine and main event loop.

pub mod ask_flow;
pub mod constants;
pub mod intent;
pub mod popup_state;
pub mod render_context;
pub mod replay;
pub mod runner;
pub mod transport;
pub mod turn_state;

pub use intent::AppIntent;

use anyhow::Result;
use crossterm::cursor::MoveTo;
use crossterm::cursor::Show;
use crossterm::execute;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;

use self::transport::Transport;
use self::transport::backoff;
use self::transport::try_reconnect;
use crate::protocol::WingEvent;
use crate::tui::MouseAction;
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
use crate::ui::header::build_header_lines;
use crate::ui::input_area::InputAction;
use crate::ui::input_area::InputArea;
use crate::ui::input_area::InputAreaWidget;
use crate::ui::input_area::cursor_screen_pos;
use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::candidate_request_for;
use crate::ui::popup::selection::SelectionPopup;
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
use self::constants::FORK_COMMAND;
use self::constants::NEW_COMMAND;
use self::constants::SESSION_COMMAND;
use self::constants::SS_COMMAND;
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
    /// Active ask selection (when agent requires a choice from menu).
    ask_selection: Option<crate::ui::ask_select::AskSelection>,
    /// Active multi-question ask flow (AskUserQuestion with questions array).
    ask_flow: Option<ask_flow::AskFlow>,
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
    pub fn new(session_id: String, config: AppConfig) -> Self {
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
            ask_selection: None,
            ask_flow: None,
            focused: true,
            config,
            palette,
            connected: true,
        }
    }

    /// Whether the terminal is wide enough for detailed status bar.
    fn is_wide(&self) -> bool {
        self.terminal_width >= WIDE_THRESHOLD
    }

    /// Clear the ask selection and remove the Ask cell from chat.
    fn clear_ask_selection(&mut self) {
        if self.ask_selection.take().is_some() {
            self.chat.remove_last_ask();
            self.input.placeholder = "今天构建什么？".into();
        }
        // Also clear multi-question flow if active (e.g., turn interrupted).
        if self.ask_flow.take().is_some() {
            self.chat.remove_last_ask();
            self.input.placeholder = "今天构建什么？".into();
        }
    }

    /// Common cleanup at the end of an agent turn (Done / Interrupted / Error).
    ///
    /// Resets turn state, render context, and copy candidates.
    /// Callers handle their own specific follow-up (title, toast, etc.).
    ///
    /// **Ordering note**: `refresh_copy_candidates()` runs *inside* this method,
    /// so any chat mutations by the caller (e.g. `clear_ask_selection`,
    /// `chat.push(ErrorMessage)`) happen *after* the copy cache is snapshot.
    /// Currently safe because `collect_assistant_messages` only collects
    /// `AssistantMessage` cells, which are unaffected by these mutations.
    fn finish_turn(&mut self) {
        self.turn.finish();
        self.ctx.reset();
        self.refresh_copy_candidates();
    }

    /// Notify the user via OSC 9 + attention title when the terminal is not focused.
    fn notify_unfocused(&mut self, message: String, kind: AttentionKind) {
        if !self.focused {
            self.push_intent(AppIntent::Notify(message));
            self.push_intent(AppIntent::SetTitle(
                title::title_attention(kind).to_string(),
            ));
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
        // Multi-question ask flow: intercept submission to advance questions.
        if self.ask_flow.is_some() {
            return self.handle_ask_flow_submit(text);
        }
        if self.try_frontend_command(text) {
            return true;
        }
        self.chat.push(ChatCell::UserMessage(text.to_string()));
        self.push_intent(AppIntent::SendMessage {
            content: text.to_string(),
        });
        self.turn.usage = TurnUsage::default();
        true
    }

    /// Handle submission during a multi-question ask flow.
    ///
    /// Advances to the next question or sends the final structured response.
    fn handle_ask_flow_submit(&mut self, text: &str) -> bool {
        // Advance the flow and extract needed data to avoid borrow conflicts.
        let Some(result) = self.ask_flow.as_mut().map(|flow| {
            let advance_result = flow.advance(text);
            (
                advance_result,
                flow.current_idx,
                flow.answers.clone(),
                flow.len(),
                flow.progress(),
            )
        }) else {
            return false;
        };
        let (advance_result, idx, answers, total, progress) = result;

        match advance_result {
            None => {
                // More questions to answer — update the AskMessage cell.
                self.chat.update_last_ask_progress(idx, answers);
                self.input.placeholder = format!("Question {progress} — type your answer...");
            }
            Some(json) => {
                // All questions answered — send structured response.
                self.ask_flow = None;
                self.input.placeholder = "今天构建什么？".into();
                self.chat.update_last_ask_progress(total, answers);
                self.push_intent(AppIntent::SendMessage { content: json });
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
                self.push_intent(AppIntent::CreateSession { workspace: None });
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
                if let Some(model) = parse_string_arg(text, "/model ") {
                    self.push_intent(AppIntent::set_model(model));
                    true
                } else {
                    false
                }
            }
            _ if text.starts_with("/agents ") => {
                if let Some(agent) = parse_string_arg(text, "/agents ") {
                    self.push_intent(AppIntent::set_agent(agent));
                    true
                } else {
                    false
                }
            }
            _ if text.starts_with("/title ") => {
                if let Some(title) = parse_string_arg(text, "/title ") {
                    self.push_intent(AppIntent::set_title(title));
                    true
                } else {
                    false
                }
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
            _ if text.starts_with("/rewind ") => {
                let uuid = text.strip_prefix("/rewind ").unwrap().trim();
                if !uuid.is_empty() {
                    self.push_intent(AppIntent::RewindSession {
                        target_uuid: uuid.to_string(),
                    });
                    true
                } else {
                    false
                }
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

    /// Refresh `/copy` candidate cache from current chat state.
    fn refresh_copy_candidates(&mut self) {
        self.popup.cache.copies = self.chat.collect_assistant_messages();
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
                self.popup.cache.sessions = resp
                    .sessions
                    .iter()
                    .map(|s| (s.id.clone(), s.name.clone().unwrap_or_default()))
                    .collect();
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

        // Ask selection menu: capture all keys when active.
        if self.ask_selection.is_some() {
            let ask = self.ask_selection.as_mut().unwrap();
            match key.code {
                crossterm::event::KeyCode::Up => {
                    ask.move_up();
                    self.chat.update_last_ask_selection(ask.selected);
                }
                crossterm::event::KeyCode::Down => {
                    ask.move_down();
                    self.chat.update_last_ask_selection(ask.selected);
                }
                crossterm::event::KeyCode::Enter => {
                    let choice = ask.current().map(|s| s.to_string());
                    if let Some(choice) = choice {
                        self.clear_ask_selection();
                        self.push_intent(AppIntent::SendMessage { content: choice });
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
                self.push_intent(AppIntent::InterruptSession);
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
                self.popup.active = ActivePopup::None;
                self.submit_message(&text);
            }
            InputAction::Escape | InputAction::None => {
                // Update popup based on new text.
                self.update_popup();
            }
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
                    // Session lifecycle + state update commands: intercept popup
                    // selection and produce intents directly (no input box roundtrip).
                    let lifecycle_intent = if let ActivePopup::SubCommand {
                        command,
                        rows,
                        state,
                        ..
                    } = &self.popup.active
                    {
                        rows.get(state.selected)
                            .and_then(|selected| match command.as_str() {
                                cmd if cmd == FORK_COMMAND => Some(AppIntent::ForkSession {
                                    target_uuid: selected.name.clone(),
                                }),
                                cmd if cmd == SESSION_COMMAND || cmd == SS_COMMAND => {
                                    Some(AppIntent::ResumeSession {
                                        session_id: selected.name.clone(),
                                    })
                                }
                                "/model" => Some(AppIntent::set_model(selected.name.clone())),
                                "/agents" => Some(AppIntent::set_agent(selected.name.clone())),
                                _ => None,
                            })
                    } else {
                        None
                    };
                    if let Some(intent) = lifecycle_intent {
                        self.popup.active = ActivePopup::None;
                        self.input.clear();
                        self.push_intent(intent);
                        return true;
                    }

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
        if !matches!(event, WingEvent::SyncSession { .. })
            && let Some(event_sid) = event.session_id()
            && event_sid != self.session_id
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
                let working_title = title::title_working(self.turn.spinner.frame_str());
                self.turn.last_title = Some(working_title.clone());
                self.push_intent(AppIntent::SetTitle(working_title));
            }
            WingEvent::Done { .. } => {
                self.finish_turn();
                self.clear_ask_selection();
                // Restore idle title — but if the user is not focused and a
                // TurnResult just set an attention title (✓/⚠), preserve it
                // until the user refocuses (Focus handler restores idle).
                let had_result = self.turn.last_result.take().is_some();
                if self.focused || !had_result {
                    self.push_intent(AppIntent::SetTitle(title::title_idle().to_string()));
                }
            }
            WingEvent::Interrupted { .. } => {
                self.finish_turn();
                self.clear_ask_selection();
                self.show_toast(Toast::info(
                    "Agent interrupted",
                    std::time::Duration::from_secs(2),
                ));
                // Restore idle title (no BEL — user triggered this).
                self.push_intent(AppIntent::SetTitle(title::title_idle().to_string()));
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
                questions,
                question,
                choices,
                required,
                ..
            } => {
                if !questions.is_empty() {
                    // Multi-question flow (AskUserQuestion tool).
                    let msg = AskMessage::new_multi(questions.clone());
                    self.chat.push(ChatCell::Ask(msg));
                    let flow = ask_flow::AskFlow::new(questions.clone());
                    self.input.placeholder =
                        format!("Question {} — type your answer...", flow.progress());
                    self.ask_flow = Some(flow);
                    let notify_text = questions
                        .first()
                        .map(|q| q.question.clone())
                        .unwrap_or_default();
                    self.notify_unfocused(notify_text, AttentionKind::Ask);
                } else {
                    // Legacy single-question (Bash dangerous command confirmation).
                    let msg = AskMessage::new(question.clone(), choices.clone());
                    self.chat.push(ChatCell::Ask(msg));
                    if required && !choices.is_empty() {
                        let sel = crate::ui::ask_select::AskSelection::new(choices);
                        self.ask_selection = Some(sel);
                        self.input.placeholder = "↑↓ select · Enter confirm".into();
                        self.chat.update_last_ask_selection(0);
                    }
                    self.notify_unfocused(question.clone(), AttentionKind::Ask);
                }
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

                // Restore model from agent snapshot.
                if let Some(agent_info) = agent {
                    self.status.model = agent_info.model_name;
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
        let mut input_rect = Rect::default();

        // Extract render context before the draw closure borrows self.
        let palette = self.palette;
        let layout = self.config.layout.clone();
        let thinking_mode = self.config.rendering.thinking;

        terminal.draw(|frame| {
            let area = frame.area();
            self.terminal_width = area.width;
            let popup_h = self.popup.height();

            // Layout: status_bar (1) | chat (fill) | [popup] | [working] | input (dynamic)
            let input_h = self.input.height(area.width);
            let mut constraints = vec![
                Constraint::Length(1), // status bar
                Constraint::Min(3),    // chat view (min 3 rows)
            ];
            if popup_h > 0 {
                constraints.push(Constraint::Length(popup_h)); // popup
            }
            if self.turn.working {
                constraints.push(Constraint::Length(1)); // working indicator
            }
            constraints.push(Constraint::Length(input_h)); // input area

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            // Status bar.
            frame.render_widget(
                StatusBar::new(&self.status, self.is_wide(), &palette),
                chunks[0],
            );

            // Chat view (includes header when present).
            chat_height = chunks[1].height;
            let ctx = crate::render::renderable::CellContext {
                palette: &palette,
                thinking_mode,
                layout: &layout,
            };
            let widget = ChatViewWidget::new(&mut self.chat, ctx).with_usage(&self.turn.usage);
            frame.render_widget(widget, chunks[1]);

            // Track chunk index for popup / working / input.
            let mut idx = 2;

            // Popup (if active).
            if popup_h > 0 {
                if let Some((rows, state, filter)) = self.popup.active.render_data() {
                    frame.render_widget(
                        SelectionPopup::new(rows, state, filter, &palette),
                        chunks[idx],
                    );
                }
                idx += 1;
            }

            // Working indicator (if active).
            if self.turn.working {
                if let Some(started_at) = self.turn.started_at {
                    frame.render_widget(
                        crate::ui::spinner::WorkingIndicatorWidget::new(
                            &self.turn.spinner,
                            started_at,
                            &palette,
                        ),
                        chunks[idx],
                    );
                }
                idx += 1;
            }

            // Input area.
            input_rect = chunks[idx];
            frame.render_widget(InputAreaWidget::new(&mut self.input, &palette), chunks[idx]);

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

        // Position cursor in the input area.
        let size = terminal.size()?;
        let (cursor_x, cursor_y) = cursor_screen_pos(&self.input, &input_rect);
        let cursor_x = cursor_x.min(size.width.saturating_sub(1));
        let cursor_y = cursor_y.min(size.height.saturating_sub(1));

        execute!(terminal.backend_mut(), Show, MoveTo(cursor_x, cursor_y))?;

        Ok(())
    }
}

/// Run the main application loop.
pub async fn run_app(
    terminal: &mut WingTerminal,
    transport: Transport,
    session_id: String,
    ws_url: String,
    http_base: String,
    config: AppConfig,
) -> Result<()> {
    let mut app = App::new(session_id, config);
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

    // Set initial terminal title.
    {
        let writer = terminal.backend_mut();
        let _ = title::set_title(writer, title::title_idle());
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
                    TermEvent::Mouse(action) => match action {
                        MouseAction::ScrollUp => {
                            app.chat.scroll_up(3);
                        }
                        MouseAction::ScrollDown => {
                            app.chat.scroll_down(3, app.visible_height);
                        }
                    },
                    TermEvent::Paste(text) => {
                        app.input.insert_str(&text);
                        app.update_popup();
                    }
                    TermEvent::Resize(_, _) => {}
                    TermEvent::Focus(focused) => {
                        app.focused = focused;
                        // On focus regain, restore the correct title.
                        if focused {
                            let title = if app.turn.working {
                                title::title_working(
                                    app.turn.spinner.frame_str(),
                                )
                            } else {
                                title::title_idle().to_string()
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
                match try_reconnect(&ws_url, &http_base, &app.session_id).await {
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
