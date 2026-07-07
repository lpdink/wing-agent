//! Application state machine and main event loop.

pub mod constants;
pub mod popup_state;
pub mod render_context;
pub mod replay;
pub mod turn_state;

use anyhow::Result;
use crossterm::cursor::MoveTo;
use crossterm::cursor::Show;
use crossterm::execute;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;

use crate::gateway::GatewayClient;
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
use crate::ui::toast::render_toast;

use self::constants::CLEAR_COMMAND;
use self::constants::COPY_COMMAND;
use self::constants::HELP_COMMAND;
use self::constants::INFO_COMMAND;
use self::constants::INIT_INFO_REQUEST_ID;
use self::constants::INTERRUPT_COMMAND;
use self::constants::POPUP_AGENTS_REQUEST_ID;
use self::constants::POPUP_HELP_REQUEST_ID;
use self::constants::POPUP_MODEL_REQUEST_ID;
use self::constants::POPUP_REWIND_REQUEST_ID;
use self::constants::POPUP_SESSION_REQUEST_ID;
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
    /// Pending message to send to gateway (set by input handling).
    pending_send: Option<String>,
    /// Pending silent request for fetching candidates.
    pending_silent: Option<(String, String)>,
    /// Pending clipboard text to write via OSC52.
    pending_clipboard: Option<String>,
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
    popup: PopupState,
    /// Turn state (working flag + timer + spinner + usage).
    turn: TurnState,
    /// Last tick instant for measuring real dt between TermEvent::Tick.
    last_tick: std::time::Instant,
    /// Active toast notification (lazy-expired in draw).
    toast: Option<Toast>,
    /// Active ask selection (when agent requires a choice from menu).
    ask_selection: Option<crate::ui::ask_select::AskSelection>,
    /// User configuration.
    config: AppConfig,
    /// Resolved theme palette.
    palette: ThemePalette,
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
            pending_send: None,
            pending_silent: None,
            pending_clipboard: None,
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
            config,
            palette,
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
    }

    /// Show a toast notification. Returns remaining duration for timer setup.
    pub fn show_toast(&mut self, toast: Toast) -> std::time::Duration {
        let remaining = toast.remaining();
        self.toast = Some(toast);
        remaining
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

        self.pending_clipboard = Some(content);
    }

    /// Refresh `/copy` candidate cache from current chat state.
    fn refresh_copy_candidates(&mut self) {
        self.popup.cache.copies = self.chat.collect_assistant_messages();
    }

    /// Update popup state based on current input text.
    ///
    /// Skips silent requests while the agent is streaming to avoid interference.
    /// Uses a sent-request set to prevent duplicate silent requests.
    fn update_popup(&mut self) {
        // Streaming guard: skip silent requests while agent is busy.
        let streaming = self.turn.working;
        let text = self.input.text().to_string();
        if let Some(request) = self.popup.update_from_input(&text) {
            if streaming {
                return;
            }
            let req_id = format!("_popup_{}", request.replace(' ', "_"));
            // Dedup: don't re-send if already pending.
            if self.popup.should_send_request(&req_id) {
                self.popup.mark_sent(req_id.clone());
                self.pending_silent = Some((request, req_id));
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
                        self.pending_send = Some(choice);
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
                self.pending_send = Some(INTERRUPT_COMMAND.to_string());
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
                if self.try_frontend_command(&text) {
                    return;
                }
                self.chat.push(ChatCell::UserMessage(text.clone()));
                self.pending_send = Some(text);
                self.turn.usage = TurnUsage::default();
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
                        if self.try_frontend_command(&text) {
                            self.input.clear();
                            return true;
                        }
                        self.chat.push(ChatCell::UserMessage(text.clone()));
                        self.pending_send = Some(text);
                        self.turn.usage = TurnUsage::default();
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
        // Session lifecycle events (NewSession/SyncSession) are always accepted —
        // they drive session switching and their session_id may intentionally
        // differ from the current one (e.g. NewSession carries the old session_id).
        if !matches!(
            event,
            WingEvent::NewSession { .. } | WingEvent::SyncSession { .. }
        ) && let Some(event_sid) = event.session_id()
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
            WingEvent::SystemInfo {
                model,
                api_url: _,
                tools,
                total_tokens,
                context_window_tokens,
                thinking,
                session_name,
                ..
            } => {
                self.status.model = model.clone();
                self.status.total_tokens = total_tokens;
                self.status.context_window_tokens = context_window_tokens;
                self.status.thinking = thinking;
                self.status.session_name = session_name;
                tracing::info!(
                    model = %self.status.model,
                    tools = ?tools,
                    "system info received"
                );
            }
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
                self.chat.reset_thinking_count();
            }
            WingEvent::Done { .. } => {
                self.turn.finish();
                self.ctx.reset();
                self.clear_ask_selection();
                self.refresh_copy_candidates();
            }
            WingEvent::Interrupted { .. } => {
                self.turn.finish();
                self.ctx.reset();
                self.clear_ask_selection();
                self.refresh_copy_candidates();
                self.show_toast(Toast::info(
                    "Agent interrupted",
                    std::time::Duration::from_secs(2),
                ));
            }
            WingEvent::Error { message, .. } => {
                self.turn.finish();
                self.ctx.reset();
                self.chat.push(ChatCell::ErrorMessage(message));
                self.refresh_copy_candidates();
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
                question,
                choices,
                required,
                ..
            } => {
                let msg = AskMessage::new(question, choices.clone());
                self.chat.push(ChatCell::Ask(msg));
                if required && !choices.is_empty() {
                    let sel = crate::ui::ask_select::AskSelection::new(choices);
                    self.ask_selection = Some(sel);
                    self.input.placeholder = "↑↓ select · Enter confirm".into();
                    // Set initial cursor on the AskMessage in chat view.
                    self.chat.update_last_ask_selection(0);
                }
            }

            // ---- Candidate list events (for popup) ----
            WingEvent::CommandList { commands, .. } => {
                self.popup.cache.commands = commands;
                self.popup.clear_dedup(POPUP_HELP_REQUEST_ID);
                self.update_popup();
            }
            WingEvent::ModelList { models, .. } => {
                self.popup.cache.models = models.into_iter().map(|m| (m, String::new())).collect();
                self.popup.clear_dedup(POPUP_MODEL_REQUEST_ID);
                self.update_popup();
            }
            WingEvent::SessionList { sessions, .. } => {
                self.popup.cache.sessions = sessions
                    .iter()
                    .map(|s| {
                        let desc = s.name.as_deref().unwrap_or("").to_string();
                        (s.id.clone(), desc)
                    })
                    .collect();
                self.popup.clear_dedup(POPUP_SESSION_REQUEST_ID);
                self.update_popup();
            }
            WingEvent::BranchTargets { targets, .. } => {
                self.popup.cache.branches = targets
                    .iter()
                    .map(|t| {
                        let preview =
                            crate::ui::cells::tool_call::truncate_by_chars(&t.content, 80);
                        (t.uuid.clone(), preview)
                    })
                    .collect();
                self.popup.clear_dedup(POPUP_REWIND_REQUEST_ID);
                self.update_popup();
            }
            WingEvent::AgentList { agents, .. } => {
                self.popup.cache.agents = agents.into_iter().map(|a| (a, String::new())).collect();
                self.popup.clear_dedup(POPUP_AGENTS_REQUEST_ID);
                self.update_popup();
            }

            // ---- State events ----
            WingEvent::System { content, .. } => {
                self.show_toast(Toast::info(content, std::time::Duration::from_secs(3)));
            }
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
            WingEvent::ModelSwitched { new_model, .. } => {
                self.status.model = new_model.clone();
            }
            WingEvent::ThinkToggled { enabled, .. } => {
                self.status.thinking = enabled;
            }
            WingEvent::NewSession {
                new_session_id,
                agent,
                ..
            } => {
                self.session_id = new_session_id;
                self.chat.clear();
                self.ctx.reset();
                self.status = StatusData::default();
                if let Some(agent_info) = agent {
                    self.status.model = agent_info.model_name;
                }
                self.turn.usage = TurnUsage::default();
                self.popup.reset();
            }
            WingEvent::SyncSession {
                messages,
                draft,
                name,
                agent,
                ..
            } => {
                // Clear old content + reset render context before replay.
                // SyncSession is a full state replacement — not an append.
                // This is critical for /rewind which does NOT emit NewSessionEvent.
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
            WingEvent::SessionUpdated { name, .. } => {
                // Update session attributes (e.g. renamed by /title).
                if let Some(session_name) = name {
                    self.status.session_name = Some(session_name);
                }
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
    mut gateway: GatewayClient,
    session_id: String,
    config: AppConfig,
) -> Result<()> {
    let mut app = App::new(session_id, config);
    let mut term_events = crate::tui::spawn_event_stream();

    // Request system info on startup.
    if let Err(e) = gateway
        .send_silent(gateway.session_id(), INFO_COMMAND, INIT_INFO_REQUEST_ID)
        .await
    {
        tracing::warn!("failed to send /info: {e}");
    }
    // Fetch dynamic command list for popup.
    if let Err(e) = gateway
        .send_silent(gateway.session_id(), HELP_COMMAND, POPUP_HELP_REQUEST_ID)
        .await
    {
        tracing::warn!("failed to send /help: {e}");
    }

    let mut gateway_alive = true;

    loop {
        // Draw.
        if let Err(e) = app.draw(terminal) {
            tracing::error!("draw error: {e}");
        }

        // Flush pending clipboard write via OSC52.
        if let Some(text) = app.pending_clipboard.take() {
            let writer = terminal.backend_mut();
            match crate::util::clipboard::copy_to_clipboard(writer, &text) {
                Ok(()) => {
                    app.show_toast(Toast::info("Copied!", std::time::Duration::from_secs(2)));
                }
                Err(e) => {
                    tracing::warn!("clipboard copy failed: {e}");
                    app.show_toast(Toast::warning(
                        format!("Copy failed: {e}"),
                        std::time::Duration::from_secs(3),
                    ));
                }
            }
        }

        if gateway_alive {
            // Check for pending send.
            if let Some(text) = app.pending_send.take()
                && let Err(e) = gateway.send_message(&app.session_id, &text).await
            {
                tracing::error!("failed to send message: {e}");
                app.chat
                    .push(ChatCell::ErrorMessage(format!("Send failed: {e}")));
            }

            // Check for pending silent request (candidate fetching).
            if let Some((content, req_id)) = app.pending_silent.take()
                && let Err(e) = gateway
                    .send_silent(&app.session_id, &content, &req_id)
                    .await
            {
                tracing::warn!("failed to send silent request: {e}");
            }
        } else {
            // Drain any pending sends — they can't go anywhere.
            app.pending_send.take();
            app.pending_silent.take();
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

        // Select from term events and gateway events.
        // After gateway disconnects, use pending() to keep the branch valid
        // without busy-looping.
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
                    TermEvent::Tick => {
                        let now = std::time::Instant::now();
                        let dt = now.duration_since(app.last_tick);
                        app.last_tick = now;
                        if app.turn.working {
                            app.turn.spinner.tick(dt);
                        }
                        // Refresh Bash tool timers.
                        app.chat.tick_bash_timers();
                    }
                }
            }
            event = gateway.recv_event(), if gateway_alive => {
                match event {
                    Some(e) => app.handle_event(e),
                    None => {
                        gateway_alive = false;
                        app.chat.push(ChatCell::ErrorMessage(
                            "⚡ Connection to gateway lost. Please restart wing.".into(),
                        ));
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
            else => {
                break;
            }
        }
    }

    Ok(())
}
