//! Modal ownership lane — who owns the keyboard, and the Escape ladder.
//!
//! The TUI has three modal layers (ask panel, `/model` picker, command
//! candidate popup) plus the composer. This module declares, **once**, which
//! one owns a key event:
//!
//! * [`App::modal_chain`] — the layers that are up, in priority order (the
//!   declaration order of [`ModalOwner`] *is* the priority order);
//! * [`App::route_key`] — the key → owner decision, including the keys the app
//!   always keeps for itself (Esc, page keys, Ctrl+C). A layer that does not
//!   take a key lets it through to the next one, and the chat keeps its scroll
//!   keys all the way down;
//! * [`App::handle_key`] — a thin ladder that dispatches each route to its
//!   handler.
//!
//! The mouse path reads the same declaration ([`App::composer_pointer_blocked`])
//! instead of re-deriving "is a modal up?" on its own, so the two input
//! channels cannot drift apart.
//!
//! The panel **state machines** (ask panel questions, model picker pages) are
//! neutral and live in `shared/panels/`; what lives here is their App-side
//! lifecycle: the queues, the chat-cell mirroring, and the reply/apply paths.
//!
//! Call directions: [`super::commands`] calls in for the `/model` picker and
//! the composer's submit path; [`super::projection`] calls in for ask
//! registration; this module calls [`super::commands`] (submit / popup
//! refresh) and [`super::goal_lane`] (ask answers in goal mode).

use super::App;
use super::AppIntent;
use super::goal;
use crate::shared::goal_role::GoalRole;
use crate::shared::panels::ask::AskPanel;
use crate::shared::panels::ask::PanelAction;
use crate::shared::panels::picker::ModelPanel;
use crate::shared::panels::picker::ModelPanelAction;
use crate::tui::is_quit_key;
use crate::ui::input_area::InputAction;
use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::candidate_request_for;
use crate::ui::popup::command::is_must_select_command;
use crate::ui::popup::command::parse_slash_input;
use crate::ui::toast::Toast;

/// The modal layer that owns the keyboard right now.
///
/// **Declaration order is the priority order** — see [`App::modal_owner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModalOwner {
    /// Ask panel (queue front): swallows every key except the ones the app
    /// reserves (`Esc`, `PageUp` / `PageDown`). Every ask is a panel — the
    /// retired menu's own layer is gone.
    AskPanel,
    /// `/model` picker: swallows every key except the page keys.
    ModelPicker,
    /// Command candidate popup: only the navigation keys.
    Popup,
}

/// A key the chat viewport owns even while the composer has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatScrollAction {
    /// One viewport page up.
    PageUp,
    /// One viewport page down.
    PageDown,
    /// One line up (Ctrl+Up).
    LineUp,
    /// One line down (Ctrl+Down).
    LineDown,
    /// Jump to the top (Ctrl+Home).
    Top,
    /// Jump to the bottom (Ctrl+End).
    Bottom,
}

/// Where a key event goes, decided before anything is mutated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyRoute {
    /// Ctrl+C — the double-press quit gesture.
    Quit,
    /// The ask panel owns the key.
    AskPanel,
    /// The `/model` picker owns the key.
    ModelPicker,
    /// Escape — the app's own ladder (popup → draft → interrupt).
    EscLadder,
    /// Command popup navigation (`Up` / `Down` / `Tab` / `Enter`); keys the
    /// popup does not consume go on to the composer.
    PopupNav,
    /// The popup is armed but has no candidates: `Enter` on a must-select
    /// command is refused with a toast, otherwise the popup is dropped and the
    /// key continues to the composer.
    PopupEmpty,
    /// The chat viewport owns the key.
    ChatScroll(ChatScrollAction),
    /// Everything else — the composer.
    Composer,
}

/// Keys the app keeps for itself while a modal is up.
fn app_reserved_key(key: &crossterm::event::KeyEvent) -> bool {
    matches!(
        key.code,
        crossterm::event::KeyCode::Esc
            | crossterm::event::KeyCode::PageUp
            | crossterm::event::KeyCode::PageDown
    )
}

/// The chat-scroll meaning of a key, if it has one.
///
/// Plain (unmodified) `Up` / `Down` are NOT scrolling keys: they belong to
/// whatever holds focus (panel navigation, or the composer cursor) — the wheel
/// has its own channel in `handle_mouse`.
fn chat_scroll_action(key: &crossterm::event::KeyEvent) -> Option<ChatScrollAction> {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::PageUp => Some(ChatScrollAction::PageUp),
        KeyCode::PageDown => Some(ChatScrollAction::PageDown),
        KeyCode::Up if control => Some(ChatScrollAction::LineUp),
        KeyCode::Down if control => Some(ChatScrollAction::LineDown),
        KeyCode::Home if control => Some(ChatScrollAction::Top),
        KeyCode::End if control => Some(ChatScrollAction::Bottom),
        _ => None,
    }
}

impl App {
    /// Modal layers that are up, highest priority first.
    ///
    /// The declaration order of [`ModalOwner`] **is** the priority order, and
    /// this is the single place where it is materialized: [`App::modal_owner`]
    /// takes the front, [`App::route_key`] walks the whole chain.
    fn modal_chain(&self) -> impl Iterator<Item = ModalOwner> {
        [
            (!self.ask_panels.is_empty()).then_some(ModalOwner::AskPanel),
            self.model_panel
                .is_some()
                .then_some(ModalOwner::ModelPicker),
            self.popup.active.is_active().then_some(ModalOwner::Popup),
        ]
        .into_iter()
        .flatten()
    }

    /// Which modal layer owns the keyboard right now (`None` = the composer).
    ///
    /// One declaration for every consumer: the keyboard ladder and the
    /// composer's pointer guard. An armed-but-invisible popup counts as a
    /// *keyboard* owner (its "no candidates" Enter behaviour is part of the
    /// composer's route), while the pointer guard additionally requires it to
    /// be visible — see [`App::composer_pointer_blocked`].
    pub(super) fn modal_owner(&self) -> Option<ModalOwner> {
        self.modal_chain().next()
    }

    /// Decide where a key goes — the single place where keyboard ownership is
    /// resolved.
    ///
    /// The modal chain is walked in priority order and **a layer that does not
    /// take the key lets it through to the next one**: the ask panel keeps the
    /// app-reserved keys (Esc, page keys) for whatever is below it, so a
    /// `/model` picker sitting underneath still gets its Esc — and the page
    /// keys still reach the chat.
    pub(super) fn route_key(&self, key: &crossterm::event::KeyEvent) -> KeyRoute {
        if is_quit_key(key) {
            return KeyRoute::Quit;
        }
        for layer in self.modal_chain() {
            match layer {
                ModalOwner::AskPanel if !app_reserved_key(key) => return KeyRoute::AskPanel,
                ModalOwner::ModelPicker
                    if !matches!(
                        key.code,
                        crossterm::event::KeyCode::PageUp | crossterm::event::KeyCode::PageDown
                    ) =>
                {
                    return KeyRoute::ModelPicker;
                }
                // The popup's rungs sit below the Escape rung (Esc closes the
                // popup) and depend on whether it has candidates — see below.
                ModalOwner::Popup => break,
                _ => {}
            }
        }
        // The popup's own rungs: Esc closes it, the navigation keys are the
        // popup's, and the "armed but empty" case gets its own route so its
        // Enter refusal stays explicit. Scroll keys are *not* taken here — the
        // rungs below the popup handle them via `handle_scroll_or_composer_key`.
        if key.code == crossterm::event::KeyCode::Esc {
            return KeyRoute::EscLadder;
        }
        if self.popup.active.is_active() {
            return if self.popup.active.has_items() {
                KeyRoute::PopupNav
            } else {
                KeyRoute::PopupEmpty
            };
        }
        if let Some(action) = chat_scroll_action(key) {
            return KeyRoute::ChatScroll(action);
        }
        KeyRoute::Composer
    }

    /// Whether a keyboard-owning modal currently owns the composer pointer
    /// rules.
    ///
    /// The ask panel, the `/model` panel and the command candidate popup own
    /// the keyboard while they are up: their own inline editors are typing
    /// into the draft and their keys rewrite it. Placing the cursor — or worse,
    /// copying a fragment — under such a modal would race that flow, so
    /// composer pointer interaction is ignored outright.
    ///
    /// A popup only counts when it really takes over, which is not the same as
    /// `ActivePopup::is_active()`: a slash command that matches no candidate
    /// (`/zzz`) or whose candidates have not arrived yet leaves an *armed but
    /// invisible* popup (height 0 ⇒ it draws nothing, and plain keys already
    /// fall through to the composer). Blocking on that would swallow clicks in
    /// a completely idle-looking UI, so only a **visible** popup
    /// (`height() > 0`) counts — plus the itemless must-select case, whose
    /// Enter has to stay intercepted instead of reaching the composer.
    ///
    /// The chat band's drag selection and the wheel are **not** affected
    /// (rolling history while a panel is open has to keep working).
    pub(super) fn composer_pointer_blocked(&self) -> bool {
        match self.modal_owner() {
            None => false,
            Some(ModalOwner::Popup) => {
                self.popup.active.height() > 0 || self.popup.active.is_must_select_empty()
            }
            Some(_) => true,
        }
    }

    /// Handle a terminal key event.
    pub(super) fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        match self.route_key(&key) {
            // Modal owners keep the Ctrl+C counter untouched: the gesture only
            // counts while the app itself is receiving keys.
            KeyRoute::Quit => self.handle_quit_key(key),
            KeyRoute::AskPanel => self.handle_ask_panel_key(key),
            KeyRoute::ModelPicker => self.handle_model_picker_key(key),
            KeyRoute::EscLadder => {
                // The Escape that merely closes the popup is the popup's, not
                // the app's: it leaves the quit gesture alone, exactly like the
                // modal owners above (pre-lane behaviour).
                if !self.handle_escape_key() {
                    self.reset_quit_counter();
                }
            }
            KeyRoute::PopupNav => {
                self.reset_quit_counter();
                // The popup takes only its navigation keys; everything else
                // falls through to the rungs below it.
                if !self.handle_popup_key(key) {
                    self.handle_scroll_or_composer_key(key);
                }
            }
            KeyRoute::PopupEmpty => {
                self.reset_quit_counter();
                // 例外：must-select 命令保持 popup——Enter 给显式"无匹配候选"反馈，
                // 绝不落到自由文本发送（参数必须来自候选）。
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
                self.handle_scroll_or_composer_key(key);
            }
            KeyRoute::ChatScroll(action) => {
                self.reset_quit_counter();
                self.handle_chat_scroll_key(action);
            }
            KeyRoute::Composer => {
                self.reset_quit_counter();
                self.handle_composer_key(key);
            }
        }
    }

    /// The rungs below the popup: the chat viewport keeps its scroll keys, and
    /// everything else is composer input.
    ///
    /// A popup only ever *declines* a key — it takes the navigation keys and
    /// nothing else, so page keys and Ctrl+Home/End must keep scrolling the
    /// chat while a candidate list is on screen (the wheel has the same
    /// contract on the mouse side). This is the old if-chain's order
    /// (popup → popup-empty → chat scroll → composer), kept explicit because
    /// the popup rung cannot forward to the chat rung through `route_key`.
    fn handle_scroll_or_composer_key(&mut self, key: crossterm::event::KeyEvent) {
        if let Some(action) = chat_scroll_action(&key) {
            self.handle_chat_scroll_key(action);
            return;
        }
        self.handle_composer_key(key);
    }

    /// Ctrl+C — first press warns, second press within 500 ms quits.
    fn handle_quit_key(&mut self, _key: crossterm::event::KeyEvent) {
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
    }

    /// Any key that reaches the app's own ladder breaks the quit gesture.
    fn reset_quit_counter(&mut self) {
        self.ctrl_c_count = 0;
        self.ctrl_c_last = None;
    }

    /// Ask panel: capture the key (queue front).
    ///
    /// Every ask is a panel now — the Bash confirmation included — so this is
    /// the only ask key handler. The app-reserved keys (`Esc`,
    /// `PageUp` / `PageDown`) never reach it: [`App::route_key`] keeps them.
    fn handle_ask_panel_key(&mut self, key: crossterm::event::KeyEvent) {
        let action = self
            .ask_panels
            .front_mut()
            .map(|panel| panel.handle_key(key))
            .unwrap_or(PanelAction::None);
        self.sync_front_panel();
        if let PanelAction::Reply(content) = action {
            self.finish_ask_panel(content);
        }
    }

    /// `/model` picker: modal while open.
    ///
    /// `Esc` closes the panel (it is not part of any turn, so it must NOT reach
    /// the interrupt ladder); `PageUp` / `PageDown` stay available for chat
    /// scrolling; every other key is consumed by the panel.
    fn handle_model_picker_key(&mut self, key: crossterm::event::KeyEvent) {
        let action = self
            .model_panel
            .as_mut()
            .map(|panel| panel.handle_key(key))
            .unwrap_or(ModelPanelAction::None);
        match action {
            ModelPanelAction::Apply { provider, model } => {
                self.apply_model_selection(provider, model);
            }
            ModelPanelAction::Cancel => {
                self.close_model_panel();
            }
            // Navigation keeps the panel open — mirror the new cursor /
            // page into the chat cell.
            ModelPanelAction::None => self.sync_model_panel_cell(),
        }
    }

    /// Escape ladder: close popup if active, otherwise clear/interrupt.
    ///
    /// Returns whether the Escape was the popup's alone: that path is the one
    /// Escape that does **not** reach the app's own ladder, so its caller must
    /// leave the Ctrl+C gesture alone (pre-lane behaviour, kept verbatim).
    fn handle_escape_key(&mut self) -> bool {
        if self.popup.active.is_active() {
            self.popup.active = ActivePopup::None;
            return true;
        }
        if !self.input.text().is_empty() {
            self.input.clear();
            return false;
        }
        // Goal mode: interrupt only the active session.
        if let Some(goal) = &self.goal
            && let Some(role) = goal.active_role()
        {
            match role {
                GoalRole::Executor => {
                    self.push_intent(AppIntent::InterruptSession);
                }
                GoalRole::Checker => {
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
        false
    }

    /// The chat viewport owns this key.
    fn handle_chat_scroll_key(&mut self, action: ChatScrollAction) {
        let page = self.geometry.chat_height().saturating_sub(2);
        match action {
            ChatScrollAction::PageUp => self.chat.page_up(page),
            ChatScrollAction::PageDown => self.chat.page_down(page, self.geometry.chat_height()),
            ChatScrollAction::LineUp => self.chat.scroll_up(1),
            ChatScrollAction::LineDown => self.chat.scroll_down(1, self.geometry.chat_height()),
            ChatScrollAction::Top => self.chat.jump_top(),
            ChatScrollAction::Bottom => self.chat.jump_bottom(),
        }
    }

    /// Everything else goes to the input area.
    fn handle_composer_key(&mut self, key: crossterm::event::KeyEvent) {
        match self.input.handle_key(key, self.geometry.width()) {
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

    /// Handle a bracketed paste event: routed to the active ask panel's inline
    /// editor when one is up (panel is modal); the modal model panel drops it;
    /// otherwise it goes to the composer.
    pub(super) fn handle_paste(&mut self, text: &str) {
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

    // -----------------------------------------------------------------------
    // Ask panels — queues, chat-cell mirroring, answers
    // -----------------------------------------------------------------------

    /// Clear all queued ask state and remove their cells from chat. Called when
    /// a turn ends or is interrupted — the backend cancels all feedback waiters
    /// at the same time.
    pub(super) fn clear_ask_state(&mut self) {
        if self.ask_panels.is_empty() {
            return;
        }
        for panel in self.ask_panels.drain(..) {
            self.chat.remove_ask(&panel.tool_call_id);
        }
        self.refresh_ask_placeholder();
    }

    /// Refresh the input placeholder to reflect the active (front) ask state.
    pub(super) fn refresh_ask_placeholder(&mut self) {
        if self.ask_panels.front().is_some() {
            self.input.placeholder = "Answering above · Esc to interrupt".into();
        } else {
            self.input.placeholder = "今天构建什么？".into();
        }
    }

    /// Register a normalized ask panel — the single registration entry, shared
    /// by the live projection and by resume replay.
    ///
    /// Interactive modes queue up (the front is the keyboard owner; answering
    /// routes to `post(tool_call_id)` and resolves the backend waiter). A
    /// [`PanelMode::Notice`](crate::shared::panels::ask::PanelMode::Notice) is
    /// display-only: it was rendered as a chat cell and must never take keys or
    /// answer, so it is not registered at all.
    ///
    /// The cell is the caller's business: the live path pushes it (and mirrors
    /// this very panel), replay has already pushed it in chain order.
    pub(super) fn register_ask_panel(&mut self, panel: AskPanel) {
        if panel.is_interactive() {
            self.ask_panels.push_back(panel);
        }
        self.refresh_ask_placeholder();
    }

    /// Sync the front panel's state into its chat cell (render snapshot).
    pub(super) fn sync_front_panel(&mut self) {
        if let Some(panel) = self.ask_panels.front() {
            let id = panel.tool_call_id.clone();
            let panel = panel.clone();
            self.chat.update_ask_panel(&id, panel);
        }
    }

    /// Send the front panel's final reply and pop it from the queue.
    pub(super) fn finish_ask_panel(&mut self, content: String) {
        let Some(panel) = self.ask_panels.pop_front() else {
            return;
        };
        let tool_call_id = panel.tool_call_id.clone();
        self.reply_to_ask(content, tool_call_id);
        self.refresh_ask_placeholder();
    }

    /// Answer a pending ask — the single reply path (every ask is a panel).
    ///
    /// Goal mode routes to the active session (executor/checker); otherwise the
    /// answer is a normal message carrying the ask's `tool_call_id` so the
    /// backend resolves its waiter. The content is whatever the panel built:
    /// `header: answer` lines for `Question` mode, the bare option label
    /// (`y` / `n` / `yolo`) for `RequiredChoice`.
    fn reply_to_ask(&mut self, content: String, tool_call_id: String) {
        if let Some(goal) = &self.goal
            && let Some(role) = goal.active_role()
        {
            let actions = match role {
                GoalRole::Executor => vec![goal::GoalAction::SendToExecutor {
                    content,
                    tool_call_id: Some(tool_call_id),
                }],
                GoalRole::Checker => vec![goal::GoalAction::SendToChecker {
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
    }

    // -----------------------------------------------------------------------
    // `/model` picker — App-side lifecycle
    // -----------------------------------------------------------------------

    /// Open the `/model` panel: cache-first (instant open, background refresh)
    /// when models were fetched before, fetch-first otherwise. Refused while a
    /// turn is running — the model must not change under an executing turn.
    pub(super) fn open_model_panel(&mut self) {
        if self.turn.working {
            self.show_toast(Toast::warning(
                "Can't switch model while the agent is working",
                std::time::Duration::from_secs(3),
            ));
            return;
        }
        if !self.model_sources.is_empty() {
            // Open now from cache; the fetch below refreshes in place.
            let panel = ModelPanel::new(self.model_sources.clone(), self.current_model_pair());
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
    pub(super) fn present_model_panel(&mut self, panel: ModelPanel) {
        self.model_panel_pending = false;
        self.chat.show_model_picker(panel.clone());
        self.model_panel = Some(panel);
    }

    /// Mirror the app-owned picker state into its chat cell (render snapshot).
    pub(super) fn sync_model_panel_cell(&mut self) {
        if let Some(panel) = self.model_panel.as_ref() {
            self.chat.update_model_picker(panel.clone());
        }
    }

    /// Close the picker without changing the model (Esc): drop both the
    /// interactive state and its transient cell.
    pub(super) fn close_model_panel(&mut self) {
        self.model_panel_pending = false;
        self.model_panel = None;
        self.chat.remove_model_picker();
    }

    /// The session's active `(provider, model)` pair — both must be known
    /// (a model without a provider cannot be preselected unambiguously).
    pub(super) fn current_model_pair(&self) -> Option<(&str, &str)> {
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
    pub(super) fn apply_model_selection(&mut self, provider: String, model: String) {
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
}
