//! Application state machine and main event loop.
//!
//! This file is the **composition root**: it holds the `App` state, runs the
//! main loop and draws. Everything else lives in a lane module with one
//! responsibility:
//!
//! * [`commands`] — slash-command routing (table → handler → fetch projection);
//! * [`projection`] — gateway events projected into chat / turn / status;
//! * [`modal`] — keyboard ownership, the Escape ladder, panel lifecycles;
//! * [`goal_lane`] — the App side of the Goal orchestration (state machine in
//!   [`goal`]).
//!
//! The interaction paths live in their own modules too: [`frame`] records the
//! geometry of the frame just drawn, [`mouse`] routes pointer gestures — its
//! priority chain declared once — and [`selection_session`] owns the drag's
//! lifecycle (anchor, fingerprint, edge auto-scroll).

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

mod commands;
mod frame;
mod goal_lane;
mod modal;
mod mouse;
mod projection;
mod selection_session;

pub use intent::AppIntent;

use anyhow::Result;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;

use self::transport::GatewayEndpoint;
use self::transport::Transport;
use self::transport::backoff;
use self::transport::connect_transport;
use crate::tui::TermEvent;
use crate::tui::WingTerminal;
use crate::ui::chat_view::ChatView;
use crate::ui::chat_view::ChatViewWidget;
use crate::ui::chat_view::render_info_separator;
use crate::ui::header::build_header_lines;
use crate::ui::input_area::InputArea;
use crate::ui::input_area::InputAreaWidget;
use crate::ui::input_area::cursor_screen_pos;
use crate::ui::input_area::pointer;
use crate::ui::popup::selection::SelectionPopup;
use crate::ui::scrollbar;
use crate::ui::selection::SelectionRegion;
use crate::ui::spinner::WorkingIndicatorWidget;
use crate::ui::status_bar::StatusBar;
use crate::ui::status_bar::StatusData;
use crate::ui::toast::Toast;
use crate::ui::toast::ToastKind;
use crate::ui::toast::render_toast;
use crate::util::title;
use title::AttentionKind;

use self::frame::FrameGeometry;
use self::mouse::MouseOutcome;
use self::popup_state::PopupState;
use self::render_context::RenderContext;
use self::selection_session::SelectionSession;
use self::selection_session::selection_autoscroll_tick;
use self::turn_state::TurnState;

use crate::config::AppConfig;
use crate::config::ThemePalette;

/// Threshold for wide-mode status bar (shows cumulative usage details).
const WIDE_THRESHOLD: u16 = 100;

/// Lines scrolled per wheel event. Carried over from before alternate scroll
/// (#28) and validated then; per-event step is deliberately larger than the
/// 1-line keyboard step and smaller than a page. Real-terminal feel
/// (trackpad momentum vs. wheel notches) is calibrated by hand — see the
/// change's Open Questions.
const WHEEL_SCROLL_LINES: usize = 3;

/// Application state.
pub struct App {
    pub status: StatusData,
    pub chat: ChatView,
    pub input: InputArea,
    pub session_id: String,
    /// In-app text selection — one gesture, anchored in the *content*
    /// coordinates of the region it started in (chat band or composer), with
    /// its fingerprint, press-frame link and edge auto-scroll deadline, see
    /// [`crate::ui::selection`] and [`SelectionRegion`].
    selection: SelectionSession,
    /// Whether the app should exit.
    pub should_quit: bool,
    /// Pending side-effect intents. Drained by runner after each draw cycle.
    intents: Vec<AppIntent>,
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
    /// Geometry of the last drawn frame — the contract every input path
    /// (mouse, selection, page keys, composer editing width) reads. Written
    /// by `draw` alone; see [`FrameGeometry`].
    geometry: FrameGeometry,
    /// Overlay scrollbar interaction state (hover / drag).
    scrollbar: scrollbar::ScrollbarState,
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
            selection: SelectionSession::default(),
            should_quit: false,
            intents: Vec::new(),
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
            // Zero-sized until the first draw records the real chat viewport:
            // no bar, no hit testing, before anything is on screen.
            geometry: FrameGeometry::default(),
            scrollbar: scrollbar::ScrollbarState::default(),
        }
    }

    /// Whether the terminal is wide enough for detailed status bar.
    fn is_wide(&self) -> bool {
        self.geometry.width() >= WIDE_THRESHOLD
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

    /// Draw the UI.
    ///
    /// Generic over the backend so tests can drive it with a
    /// `ratatui::backend::TestBackend` and assert on the very frame the user
    /// would see. The selection highlight, its text snapshot and the
    /// scrollbar's clipping / place in the overlay order are all frame-level
    /// properties, not unit properties of the painters; production always
    /// passes the crossterm terminal.
    fn draw<B>(&mut self, terminal: &mut ratatui::Terminal<B>) -> Result<()>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
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
            self.geometry.record_area(area);

            // A selection is anchored to *content* coordinates, which only
            // survive while the content is stable: adding / removing /
            // promoting cells or changing the width shifts the virtual rows
            // under a chat anchor, editing the draft moves the text under a
            // composer anchor. Abort instead of pointing the highlight at
            // different text. Streamed chat text growth does not (it never
            // moves an existing row), so it keeps the selection alive.
            if self
                .selection
                .needs_abort(|region| self.selection_fingerprint(region))
            {
                self.cancel_selection();
            }

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

            // Chat view — the scrollable viewport only. The widget is rendered
            // into the band **minus the scrollbar gutter**, so no cell
            // (markdown text, user-message background, diff tint, tool output)
            // can reach the bar's column however its own width arithmetic
            // works out. The recorded chat band keeps the full width: that is
            // what the bar's geometry and its hit testing are derived from.
            chat_height = chunks[1].height;
            self.geometry.record_chat_band(chunks[1]);
            let ctx = crate::render::renderable::CellContext {
                palette: &palette,
                thinking_mode,
                layout: &layout,
            };
            frame.render_widget(
                ChatViewWidget::new(&mut self.chat, ctx),
                scrollbar::content_area(chunks[1]),
            );

            // Overlay scrollbar. Painted after the chat widget (so it overprints
            // the gutter's blank columns) and before the toast (so a toast is
            // never hidden by it). It takes no layout width: the gutter is
            // reserved unconditionally, so the bar showing up on overflow never
            // reflows the cells. `self.chat` now holds this frame's content
            // height and the effective scroll offset (auto-scroll / clamp
            // included).
            if let Some(geom) = self.scrollbar_geometry() {
                scrollbar::paint(frame.buffer_mut(), &geom, self.scrollbar, &palette);
            } else {
                // No bar this frame (content fits, or nothing drawn yet):
                // drop the interaction state now instead of waiting for the
                // next mouse event, so a later overflow cannot resurrect a
                // stale hover / drag look.
                self.clear_scrollbar_interaction();
            }

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

            // Input area — always visible; the widget records the rect it drew
            // into (for cursor placement and for the composer's pointer mapping:
            // mouse events arrive between frames, so hit testing works off the
            // last frame's rect — same contract as the chat band's geometry).
            let input_rect = chunks[idx];
            self.geometry.record_composer(input_rect);
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
                let toast_area = render_toast(toast, area, frame.buffer_mut(), &palette);
                // The toast paints over the chat band (top-right, below the
                // status bar): its cells are gone, so the link hit boxes under
                // it must go too.
                if let Some(toast_area) = toast_area {
                    self.chat.mask_links(toast_area);
                }
            }

            // In-app text selection — painted after the toast (the selection
            // sits above every overlay) and clipped to *this* frame's rect of
            // the region that owns it. It is a pure `Buffer` patch: merging
            // `REVERSED` into the cell styles the widgets just produced, so no
            // cell / widget code has to know about selections and the
            // background colors survive.
            //
            // The chat pass also snapshots the visible rows: the release event
            // lands between frames, so the copy must come from the frame the
            // user was actually looking at. The composer needs no snapshot —
            // its wrapping is ours, so the copy comes from the draft itself.
            if self.selection.is_press_active() {
                match self.selection.region() {
                    Some(SelectionRegion::Chat) => {
                        self.chat
                            .paint_selection(frame.buffer_mut(), self.selection.state());
                        self.chat.capture_visible_rows(frame.buffer_mut());
                    }
                    Some(SelectionRegion::Composer) => {
                        pointer::paint_selection(
                            frame.buffer_mut(),
                            &self.input,
                            input_rect,
                            self.selection.state(),
                        );
                    }
                    None => {}
                }
            }
        })?;

        // Lazy cleanup expired toast after draw closure.
        if self.toast.as_ref().is_some_and(|t| t.is_expired()) {
            self.toast = None;
        }

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
                    TermEvent::Mouse(mouse) => {
                        // Discrete gestures (a wheel notch, a press, a
                        // release) are direct user actions: draw right away,
                        // bypassing the 16 ms frame gate, like keys do.
                        // Pointer motion and drags arrive in bursts (~100 Hz
                        // on a trackpad) and only mark the chat dirty, so the
                        // frame gate coalesces them. Events that change
                        // nothing (a hover that stays put, a drag that lands
                        // on the same position) ask for no redraw at all — so
                        // their floods never force one.
                        match app.handle_mouse(mouse) {
                            MouseOutcome::Ignored => {}
                            MouseOutcome::Coalesced => app.chat_dirty = true,
                            MouseOutcome::Immediate => app.input_dirty = true,
                        }
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
                        // A drag that leaves the window never reports its
                        // release — drop the in-flight selection so no
                        // highlight (and no frozen follow state) is left
                        // behind.
                        if !focused {
                            app.cancel_selection();
                        }
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
                        } else {
                            // The button-up of an in-flight scrollbar drag may
                            // be delivered to whatever window took the focus —
                            // drop the interaction so the bar does not stay
                            // stuck in its dragged look (and so stray drag
                            // events stop moving the view).
                            app.clear_scrollbar_interaction();
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
                        // Disconnected. Carry the read task's reason into the
                        // toast so the user can tell "gateway restarted" from
                        // "this session's payload exceeds the frame limit".
                        let reason = transport
                            .as_ref()
                            .and_then(|t| t.ws.close_reason())
                            .map(|r| crate::util::osc9::truncate_bytes(&r.describe(), 120));
                        transport = None;
                        app.set_connected(false);
                        let text = match reason {
                            Some(reason) => format!("⚡ Connection lost: {reason} — reconnecting..."),
                            None => "⚡ Connection lost — reconnecting...".to_string(),
                        };
                        app.show_toast(Toast::persistent(text, ToastKind::Warning));
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
            // Selection edge auto-scroll: one line per *absolute* deadline
            // (see `selection_autoscroll_tick` — a relative sleep would be
            // restarted by every incoming event and starve during streaming).
            // The arm carries no state of its own: the deadline is cleared the
            // moment the selection ends or hits the content edge, so nothing is
            // left spinning (and no busy loop when the view cannot move).
            _ = selection_autoscroll_tick(app.selection.deadline()) => {
                if app.tick_selection_autoscroll() {
                    app.input_dirty = true;
                }
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
mod tests;
