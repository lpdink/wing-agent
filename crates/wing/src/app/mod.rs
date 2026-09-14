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
mod goal_lane;
mod modal;
mod projection;

pub use intent::AppIntent;

use anyhow::Result;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;

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
use crate::ui::selection::Selection;
use crate::ui::selection::SelectionPoint;
use crate::ui::selection::SelectionRegion;
use crate::ui::spinner::WorkingIndicatorWidget;
use crate::ui::status_bar::StatusBar;
use crate::ui::status_bar::StatusData;
use crate::ui::toast::Toast;
use crate::ui::toast::ToastKind;
use crate::ui::toast::render_toast;
use crate::util::title;
use title::AttentionKind;

use self::popup_state::PopupState;
use self::render_context::RenderContext;
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

/// Step interval of the drag edge auto-scroll: one content line per 50 ms
/// (20 lines/s). Matches the reference implementation's cadence, and is fast
/// enough to feel continuous without skipping rows. The timer only runs while
/// the pointer rests on the chat band's top / bottom row — reaching the
/// content edge stops it (see `App::tick_selection_autoscroll`).
const SELECTION_AUTOSCROLL_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Timer arm of the run loop's `select!` for the drag edge auto-scroll.
///
/// The deadline is **absolute** (`Instant`, set when the drag arms a direction
/// and pushed forward after every step) because `select!` rebuilds this future
/// on every loop iteration: a relative `sleep` would restart on each incoming
/// event and, during streaming (events arrive far faster than the 50 ms tick),
/// would never complete at all. With `None` the arm parks in `pending()`, so
/// nothing wakes the loop while no drag sits on an edge.
async fn selection_autoscroll_tick(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

/// Structural fingerprint of the region a drag started in.
///
/// A selection is anchored to *content* coordinates, which only survive while
/// that content is stable — and the two regions have different notions of
/// "stable":
///
/// * `Chat` — rows move when cells appear / vanish / are promoted, when the
///   whole content is rebuilt (session switch, compaction, rewind) or when the
///   width changes. Streamed text growth does not (it rewrites an existing cell
///   without moving anything), so it must NOT show up here, otherwise every
///   streaming delta would abort a drag.
/// * `Composer` — logical positions survive scrolling and re-wrapping, but not
///   a single edit: the draft is the copy source, so inserting / deleting /
///   pasting / submitting has to drop a highlight that would otherwise point at
///   text the user has just changed. A width change re-wraps the draft and
///   invalidates the pointer ↔ text correspondence, so it counts too.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectionGuard {
    /// Chat band: cell structure, content rebuilds and the frame width.
    Chat {
        /// Number of committed cells.
        cells: usize,
        /// Number of pending (sent, not yet accepted) messages.
        pending: usize,
        /// Terminal width the anchor's columns were measured against.
        width: u16,
        /// Content rebuild counter (`ChatView::structure_epoch`) — catches
        /// session switches / compaction / rewind even when the rebuilt
        /// content ends up with the same cell count.
        rebuilds: u64,
    },
    /// Composer: the draft itself plus the width it was wrapped against.
    Composer {
        /// Current draft text (`InputArea::text`).
        text: String,
        /// Terminal width the visual rows were computed against.
        width: u16,
    },
}

/// How a handled mouse event wants the next frame to happen.
///
/// The wheel, a press and a release are one-shot user actions: they draw
/// immediately (like keys do, bypassing the frame gate). A drag is a *flood* —
/// a touchpad emits motion far above the frame rate, and each drag frame
/// re-snapshots the visible rows — so it is coalesced by the frame gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseOutcome {
    /// Nothing visible changed; no redraw.
    Ignored,
    /// The view changed, but the redraw can wait for the frame gate.
    Coalesced,
    /// Direct user action — draw right away.
    Immediate,
}

/// Application state.
pub struct App {
    pub status: StatusData,
    pub chat: ChatView,
    pub input: InputArea,
    pub session_id: String,
    /// In-app text selection — anchored in the *content* coordinates of the
    /// region it started in (chat band or composer), see
    /// [`crate::ui::selection`] and [`SelectionRegion`].
    selection: Selection,
    /// Structure / content fingerprint captured when the drag started. Any
    /// change means the coordinates may have moved under the anchor, so the
    /// selection is aborted; streamed chat text growth leaves it untouched.
    selection_guard: Option<SelectionGuard>,
    /// Markdown link under the pointer at press time.
    ///
    /// Recorded from the *frame the user pressed on* (never recomputed at
    /// release, which would hit whatever scrolled under the pointer in the
    /// meantime) and opened only when the gesture turns out to be a click —
    /// a drag is a selection, not an open.
    mouse_link: Option<String>,
    /// Absolute deadline of the next drag edge auto-scroll step.
    ///
    /// Absolute (not "sleep 50 ms from now") because the run loop's `select!`
    /// rebuilds its timer arm on every iteration — a relative sleep would be
    /// starved by streaming events. `None` while no drag rests on an edge.
    selection_autoscroll_at: Option<std::time::Instant>,
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
    /// The chat viewport rect of the last frame. The overlay scrollbar is
    /// both painted and hit-tested against this rect only — no cross-frame
    /// cache, so a resize cannot leave the bar interactive where it is not
    /// drawn.
    chat_area: Rect,
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
            selection: Selection::default(),
            selection_guard: None,
            mouse_link: None,
            selection_autoscroll_at: None,
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
            // Zero-sized until the first draw records the real chat viewport:
            // no bar, no hit testing, before anything is on screen.
            chat_area: Rect::default(),
            scrollbar: scrollbar::ScrollbarState::default(),
        }
    }

    /// Whether the terminal is wide enough for detailed status bar.
    fn is_wide(&self) -> bool {
        self.terminal_width >= WIDE_THRESHOLD
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

    /// Handle a mouse event, reporting how the next frame should happen.
    ///
    /// The wheel is an input channel of its own: it always scrolls the chat
    /// view and is never consumed by a panel or popup, so history stays
    /// reachable while the AskUserQuestion panel / model picker / command
    /// popup is open (plain Up/Down stay with the focused widget).
    ///
    /// Left press / drag / release are claimed in order of ownership: the
    /// overlay scrollbar on its own track first — the bar overprints the chat
    /// band's last column, so it has to win there — and otherwise whichever
    /// region the press landed in (see [`App::mouse_press`]): the chat band
    /// keeps its drag-to-copy contract, the composer adds click-to-place-
    /// cursor. A press outside both regions (status bar, popups) is ignored,
    /// exactly like `Moved` — hover belongs to the scrollbar alone — and
    /// horizontal wheel (`ScrollLeft` / `ScrollRight`), which is not a chat
    /// gesture at all.
    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> MouseOutcome {
        use crossterm::event::MouseButton;
        use crossterm::event::MouseEventKind;

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.chat.scroll_up(WHEEL_SCROLL_LINES);
                MouseOutcome::Immediate
            }
            MouseEventKind::ScrollDown => {
                self.chat
                    .scroll_down(WHEEL_SCROLL_LINES, self.visible_height);
                MouseOutcome::Immediate
            }
            // Hover belongs to the overlay scrollbar alone — it is the only
            // thing that reacts to motion today.
            MouseEventKind::Moved => self.hover_scrollbar(mouse.column, mouse.row),
            MouseEventKind::Down(MouseButton::Left) => self.mouse_press(mouse.column, mouse.row),
            MouseEventKind::Drag(MouseButton::Left) => {
                // An in-flight bar drag owns the motion; everything else is
                // the selection's flood — a drag, so coalesced by the gate.
                if self.scrollbar.dragging {
                    return self.drag_scrollbar(mouse.row);
                }
                if self.mouse_drag(mouse.column, mouse.row) {
                    MouseOutcome::Coalesced
                } else {
                    MouseOutcome::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.scrollbar.dragging {
                    return self.release_scrollbar(mouse.column, mouse.row);
                }
                self.mouse_release(mouse.column, mouse.row)
            }
            _ => MouseOutcome::Ignored,
        }
    }

    /// Grab the bar at a pressed row: the track jumps there, and the grip is
    /// taken at that same row so the drag that follows keeps it — the thumb
    /// stays under the pointer for the whole gesture (a press on the thumb
    /// itself is therefore a no-op: it keeps the position it already has).
    ///
    /// The grab also lights the bar up: "the user is manipulating the bar" is
    /// the visual contract, and without any-motion reporting (multiplexers)
    /// the press is the only event that can say so.
    fn grab_scrollbar(&mut self, geom: &scrollbar::ScrollbarGeometry, row: u16) -> MouseOutcome {
        let was_active = self.scrollbar.is_active();
        self.scrollbar.dragging = true;
        self.scrollbar.hovered = true;
        self.scrollbar.grip = scrollbar::grip_at(geom, row);
        if self.scroll_to_row(geom, row) || !was_active {
            MouseOutcome::Immediate
        } else {
            MouseOutcome::Ignored
        }
    }

    /// Drag with the grip held: re-target continuously so the thumb keeps
    /// following the pointer 1:1 (the mapping is proportional, see
    /// [`scrollbar::offset_for_row`]). The column is ignored on purpose — the
    /// row is what maps onto the track, so a drag that wanders off the bar
    /// still scrolls.
    fn drag_scrollbar(&mut self, row: u16) -> MouseOutcome {
        let Some(geom) = self.scrollbar_geometry() else {
            // The bar vanished mid-drag (the content stopped overflowing):
            // the interaction state cannot outlive it.
            return if self.clear_scrollbar_interaction() {
                MouseOutcome::Coalesced
            } else {
                MouseOutcome::Ignored
            };
        };
        if self.scroll_to_row(&geom, row) {
            // A drag is a flood of motion events — coalesced by the frame gate.
            MouseOutcome::Coalesced
        } else {
            MouseOutcome::Ignored
        }
    }

    /// Release ends the bar drag; the pointer stays where it was released, so
    /// the hover look follows it.
    fn release_scrollbar(&mut self, column: u16, row: u16) -> MouseOutcome {
        self.scrollbar.dragging = false;
        self.scrollbar.hovered = self.scrollbar_at(column, row).is_some();
        MouseOutcome::Immediate
    }

    /// Pointer motion: the overlay scrollbar is the only hover owner.
    ///
    /// A hover that stays put reports nothing at all — motion is a flood, and
    /// an event that changes nothing must not wake the frame gate.
    fn hover_scrollbar(&mut self, column: u16, row: u16) -> MouseOutcome {
        let Some(geom) = self.scrollbar_geometry() else {
            // No bar this frame (content fits, or nothing has been drawn yet):
            // drop any stale look now instead of waiting for the next draw.
            return if self.clear_scrollbar_interaction() {
                MouseOutcome::Coalesced
            } else {
                MouseOutcome::Ignored
            };
        };
        let hovered = scrollbar::hit(&geom, column, row);
        if hovered == self.scrollbar.hovered {
            return MouseOutcome::Ignored;
        }
        self.scrollbar.hovered = hovered;
        MouseOutcome::Coalesced
    }

    /// Left press: start a selection in the region the pointer landed in —
    /// unless the overlay scrollbar claims it first.
    ///
    /// The bar overprints the chat band's last column, so a press there is the
    /// bar's (it is also the only way to drag the bar). Everything else goes
    /// to the regions: the composer is checked first — it is the only region
    /// that reacts to a plain click (it places the cursor), and it is guarded
    /// by the modal panels (see [`Self::composer_pointer_blocked`]) — and the
    /// chat band owns the rest, ignoring presses outside its rect.
    ///
    /// A press off the bar also drops whatever hover / drag look the bar kept
    /// — the same cleanup a focus loss performs — so the outcome cannot be the
    /// region's alone: the repaint the bar asks for wins over the selection's
    /// "nothing to see" verdict.
    fn mouse_press(&mut self, column: u16, row: u16) -> MouseOutcome {
        if let Some(geom) = self.scrollbar_at(column, row) {
            return self.grab_scrollbar(&geom, row);
        }
        let bar_repaint = self.clear_scrollbar_interaction();
        let outcome = if self.composer_contains(column, row) {
            self.composer_press(column, row)
        } else {
            self.chat_selection_press(column, row)
        };
        match outcome {
            MouseOutcome::Ignored if bar_repaint => MouseOutcome::Immediate,
            outcome => outcome,
        }
    }

    /// Drag: extend the selection in the region it started in.
    ///
    /// The pointer is mapped into *that* region's space (clamped to its band,
    /// see the region handlers), so a drag that leaves the region keeps
    /// producing meaningful coordinates instead of switching spaces.
    fn mouse_drag(&mut self, column: u16, row: u16) -> bool {
        match self.selection.region() {
            Some(SelectionRegion::Composer) => self.composer_drag(column, row),
            // `None` means no press is active (a stray drag), and the chat
            // handler reports that the same way.
            Some(SelectionRegion::Chat) | None => self.chat_selection_drag(column, row),
        }
    }

    /// Release: finish the selection and copy whatever it covered.
    ///
    /// The fingerprint is re-checked here as well: a structural change can
    /// land in the same loop iteration (the intent runs right after the draw),
    /// so a release must never copy from content that no longer matches the
    /// coordinates the anchor was taken in.
    fn mouse_release(&mut self, column: u16, row: u16) -> MouseOutcome {
        if !self.selection.is_press_active() {
            return MouseOutcome::Ignored;
        }
        if self.selection_guard.as_ref() != Some(&self.selection_fingerprint()) {
            self.cancel_selection();
            return MouseOutcome::Immediate;
        }
        match self.selection.region() {
            Some(SelectionRegion::Composer) => self.composer_release(column, row),
            Some(SelectionRegion::Chat) => self.chat_selection_release(column, row),
            None => MouseOutcome::Ignored,
        }
    }

    /// Whether a screen position lies inside the composer's rect of the last
    /// frame.
    ///
    /// Like `ChatView::contains_screen`, the rect comes from the last render —
    /// mouse events arrive between frames, so hit testing has to describe the
    /// screen the user is pointing at. The widget records it (`InputArea` owns
    /// the geometry it drew into); a collapsed or not-yet-rendered rect
    /// (`Rect::default()`) accepts nothing.
    fn composer_contains(&self, column: u16, row: u16) -> bool {
        let area = self.input.rendered_area();
        area.width > 0
            && area.height > 0
            && column >= area.x
            && column < area.right()
            && row >= area.y
            && row < area.bottom()
    }

    /// Left press inside the composer: arm a drag selection.
    ///
    /// The cursor is **not** moved here: a press cannot know yet whether it
    /// becomes a click (→ place the cursor) or a drag (→ select and copy) —
    /// the reference behaviour, and the same reason the chat press only
    /// records an anchor.
    fn composer_press(&mut self, column: u16, row: u16) -> MouseOutcome {
        if self.composer_pointer_blocked() {
            return MouseOutcome::Ignored;
        }
        let Some(point) = pointer::point(&self.input, self.input.rendered_area(), column, row)
        else {
            return MouseOutcome::Ignored;
        };
        self.selection.begin(point);
        // No `chat.unfollow()` here: the composer selection has nothing to do
        // with the chat's follow contract, and no edge auto-scroll either — a
        // composer press even disarms a stale chat deadline.
        self.selection_guard = Some(self.selection_fingerprint());
        self.selection_autoscroll_at = None;
        MouseOutcome::Immediate
    }

    /// Drag inside the composer: extend the selection, including the character
    /// under the pointer (the mirror of `ChatView::snap_focus_right`).
    fn composer_drag(&mut self, column: u16, row: u16) -> bool {
        if !self.selection.is_press_active() {
            return false;
        }
        let Some(point) = pointer::focus(&self.input, self.input.rendered_area(), column, row)
        else {
            return false;
        };
        self.selection.drag_to(point);
        true
    }

    /// Release inside the composer: place the cursor for a click, copy for a
    /// drag.
    ///
    /// A click (press + release, no motion) never produced a selection
    /// (`bounds_in` is `None` for zero width), so it falls through to the
    /// cursor placement; a real drag copies the draft fragment it covered.
    fn composer_release(&mut self, column: u16, row: u16) -> MouseOutcome {
        let Some(hit) = pointer::hit(&self.input, self.input.rendered_area(), column, row) else {
            self.cancel_selection();
            return MouseOutcome::Immediate;
        };
        // The composer decides click vs. drag **positionally**: a motion event
        // that stayed on the same cell (trackpad jitter inside one character)
        // is still a click, and dragging away and back onto the anchor is a
        // zero-width selection, not a one-character copy. The chat band also
        // ORs in `is_dragged()` because its mapping can snap within a grapheme;
        // here the character under the pointer is the only thing that matters.
        let dragged = self.selection.anchor() != Some(hit.point);
        if !dragged {
            self.cancel_selection();
            let area = self.input.rendered_area();
            self.input
                .set_cursor_from_visual(area.width, hit.vis_row, hit.display_col);
            return MouseOutcome::Immediate;
        }
        self.selection_guard = None;
        let bounds = self.selection.release(hit.focus());
        let Some(text) = bounds.and_then(|bounds| pointer::selected_text(&self.input, bounds))
        else {
            return MouseOutcome::Immediate;
        };
        self.push_intent(AppIntent::CopyToClipboard(text));
        MouseOutcome::Immediate
    }

    /// Left press inside the chat band: start a drag selection.
    ///
    /// Returns `Ignored` (no redraw, no state) when the press is outside the
    /// chat band or before the first frame has established the geometry.
    fn chat_selection_press(&mut self, column: u16, row: u16) -> MouseOutcome {
        if !self.chat.contains_screen(column, row) {
            return MouseOutcome::Ignored;
        }
        let Some(point) = self.selectable_point_at(column, row) else {
            return MouseOutcome::Ignored;
        };
        // Link hit test first, from the frame the user is looking at. It only
        // *records* — a press still begins a selection so dragging across a
        // link selects its text.
        self.mouse_link = self.chat.link_at(column, row).map(str::to_owned);
        self.selection.begin(point);
        // Freeze follow for the duration of the drag: the render pins the
        // viewport to the bottom edge and re-arms `auto_scroll` whenever the
        // offset sits there, so streaming content would otherwise yank the view
        // (and the highlighted rows) away. Released again by
        // `chat_selection_release` → `scroll_down(0, …)`.
        self.chat.unfollow();
        self.selection_guard = Some(self.selection_fingerprint());
        self.selection_autoscroll_at = None;
        MouseOutcome::Immediate
    }

    /// Drag: extend the selection and arm / disarm the edge auto-scroll.
    ///
    /// The pointer is clamped into the visible band, so dragging past an edge
    /// keeps producing content coordinates — that is what makes the pointer
    /// resting on the top / bottom row scroll the view and extend the
    /// selection.
    fn chat_selection_drag(&mut self, column: u16, row: u16) -> bool {
        if !self.selection.is_press_active() {
            return false;
        }
        let Some(point) = self.selectable_point_at(column, row) else {
            return false;
        };
        // The pointer selects the character it rests on (reference behaviour):
        // snap the focus to the right edge of that grapheme, so the copy takes
        // it whole. A click never gets here, so "press and release without
        // moving = no selection" holds.
        self.selection.drag_to(self.chat.snap_focus_right(point));
        let area = self.chat.geometry().area;
        let direction = if row <= area.y {
            -1
        } else if row >= area.bottom() - 1 {
            1
        } else {
            0
        };
        self.selection.set_auto_scroll(direction);
        // Arm / re-arm the *absolute* deadline: a drag that keeps moving along
        // an edge restarts the 50 ms cadence from now.
        self.selection_autoscroll_at =
            (direction != 0).then(|| std::time::Instant::now() + SELECTION_AUTOSCROLL_DELAY);
        true
    }

    /// Release: end the selection and copy whatever it covered.
    ///
    /// The highlight disappears by construction (the selection state is gone
    /// after this call), the follow contract is restored from the current
    /// scroll position, and a non-empty selection is pushed as a clipboard
    /// intent. A zero-width selection (plain click) copies nothing.
    fn chat_selection_release(&mut self, column: u16, row: u16) -> MouseOutcome {
        self.selection_guard = None;
        self.selection_autoscroll_at = None;
        // A press that never moved is a click: open the link recorded at press
        // time instead of copying (a click copies nothing anyway — the
        // selection is zero-width — so this only decides *what* the click
        // does). A drag keeps the selection semantics. "Never moved" is
        // checked both ways — no `Drag` event *and* the pointer back on the
        // anchor — so a click that lost its motion events (tmux, a terminal
        // that drops `?1002`) cannot open a link the user dragged away from.
        let release_point = self.selectable_point_at(column, row);
        let clicked_link = self.mouse_link.take();
        if let Some(target) = clicked_link
            && !self.selection.is_dragged()
            && release_point.is_some()
            && release_point == self.selection.anchor()
        {
            // Restore the follow contract from the current position, exactly
            // like the copy path below.
            self.chat.scroll_down(0, self.visible_height);
            self.selection.cancel();
            self.push_intent(AppIntent::OpenLink(target));
            return MouseOutcome::Immediate;
        }
        // Re-arm the follow state iff the viewport is still at the bottom edge
        // (`n = 0` only judges — it never moves). This runs for clicks too, so
        // the `unfollow` from the press cannot leave the view stuck in reading
        // mode.
        self.chat.scroll_down(0, self.visible_height);
        let bounds = match release_point {
            Some(point) => {
                // Include the character under the pointer (reference
                // behaviour) — but only for a real drag: a plain click must
                // stay zero-width, and snapping it would select one character.
                let point = if self.selection.is_dragged() || self.selection.anchor() != Some(point)
                {
                    self.chat.snap_focus_right(point)
                } else {
                    point
                };
                self.selection.release(point)
            }
            None => {
                self.selection.cancel();
                None
            }
        };
        let Some(text) = bounds.and_then(|bounds| self.chat.selected_text(bounds)) else {
            return MouseOutcome::Immediate;
        };
        self.push_intent(AppIntent::CopyToClipboard(text));
        MouseOutcome::Immediate
    }

    /// Abort an in-flight selection and restore the follow state.
    ///
    /// Used when a drag can no longer be trusted: the release will never
    /// arrive (focus loss) or the content moved under the anchor (structural
    /// change). Lifting the freeze is part of the chat contract — otherwise the
    /// view would stay in reading mode forever — while a composer selection has
    /// no follow state to restore.
    fn cancel_selection(&mut self) {
        // A recorded link must not survive an aborted gesture either — a
        // release after a focus loss / structural change opens nothing.
        self.mouse_link = None;
        if !self.selection.is_press_active() {
            return;
        }
        let region = self.selection.region();
        self.selection.cancel();
        self.selection_guard = None;
        self.selection_autoscroll_at = None;
        if region == Some(SelectionRegion::Chat) {
            self.chat.scroll_down(0, self.visible_height);
        }
    }

    /// Fingerprint of the region the current selection is anchored in, see
    /// [`SelectionGuard`].
    fn selection_fingerprint(&self) -> SelectionGuard {
        match self.selection.region() {
            Some(SelectionRegion::Composer) => SelectionGuard::Composer {
                text: self.input.text(),
                width: self.terminal_width,
            },
            _ => SelectionGuard::Chat {
                cells: self.chat.len(),
                pending: self.chat.pending_len(),
                width: self.terminal_width,
                rebuilds: self.chat.structure_epoch(),
            },
        }
    }

    /// Map a pointer position onto chat content, for the selection.
    ///
    /// A name for [`ChatView::content_point_at`] on this side of the seam: the
    /// band the chat is rendered into already stops short of the scrollbar
    /// gutter (`ui::scrollbar`), so nothing here has to clamp a point off the
    /// bar's column — a pointer resting on the bar never reaches this path at
    /// all (the bar dispatches first, see [`Self::handle_mouse`]).
    fn selectable_point_at(&self, column: u16, row: u16) -> Option<SelectionPoint> {
        self.chat.content_point_at(column, row)
    }

    /// Step the drag edge auto-scroll by one content line.
    ///
    /// Returns `true` when the frame must be redrawn. The step stops (and
    /// disarms the deadline) the moment the viewport cannot move any further —
    /// no busy loop, no timer left behind. The focus travels with the rows
    /// that scrolled by, so the selection grows while the view moves.
    fn tick_selection_autoscroll(&mut self) -> bool {
        let direction = self.selection.auto_scroll();
        if direction == 0 {
            self.selection_autoscroll_at = None;
            return false;
        }
        let before = self.chat.scroll_position();
        if direction < 0 {
            self.chat.scroll_up(1);
        } else {
            self.chat.scroll_down(1, self.visible_height);
        }
        // The drag keeps the follow state frozen, even when this step landed
        // exactly on the bottom edge (that judgement happens on release).
        self.chat.unfollow();
        if self.chat.scroll_position() == before {
            self.selection.stop_auto_scroll();
            self.selection_autoscroll_at = None;
            return false;
        }
        // The pointer did not move, but the content under it did: the mapping
        // still describes the previous frame, so the focus shifts by the same
        // single line the view just scrolled. (Re-deriving it from the frame
        // would be stale by exactly one step.)
        if let Some(focus) = self.selection.focus() {
            let vrow = focus
                .row
                .saturating_add_signed(direction as isize)
                .min(self.chat.content_height().saturating_sub(1));
            self.selection
                .drag_to(SelectionPoint::chat(vrow, focus.col));
        }
        self.selection_autoscroll_at = Some(std::time::Instant::now() + SELECTION_AUTOSCROLL_DELAY);
        true
    }

    /// Geometry of the overlay scrollbar for the current frame state
    /// (`None` = content fits, or nothing has been drawn yet).
    fn scrollbar_geometry(&self) -> Option<scrollbar::ScrollbarGeometry> {
        scrollbar::geometry(
            self.chat_area,
            self.chat.content_height(),
            self.chat.scroll_position(),
        )
    }

    /// The bar as a claim on a pointer position: the geometry when the pointer
    /// is on the bar itself, `None` when there is no bar or the pointer missed
    /// it. Nothing outside the bar is ever the bar's.
    fn scrollbar_at(&self, column: u16, row: u16) -> Option<scrollbar::ScrollbarGeometry> {
        let geom = self.scrollbar_geometry()?;
        scrollbar::hit(&geom, column, row).then_some(geom)
    }

    /// Move the chat to the position under a pointer row, through the shared
    /// follow contract (see `ChatView::scroll_to`). Returns whether anything
    /// visible changed, so the 16ms frame gate can be bypassed only when the
    /// view really moved.
    fn scroll_to_row(&mut self, geom: &scrollbar::ScrollbarGeometry, row: u16) -> bool {
        let target = scrollbar::offset_for_row(geom, row, self.scrollbar.grip);
        let before = (self.chat.scroll_position(), self.chat.is_at_bottom());
        self.chat.scroll_to(target, geom.viewport_height);
        before != (self.chat.scroll_position(), self.chat.is_at_bottom())
    }

    /// Drop hover / drag state (content stopped overflowing, a press missed
    /// the bar, or the window lost focus). Returns whether it changed.
    fn clear_scrollbar_interaction(&mut self) -> bool {
        self.scrollbar.clear()
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
            self.terminal_width = area.width;

            // A selection is anchored to *content* coordinates, which only
            // survive while the content is stable: adding / removing /
            // promoting cells or changing the width shifts the virtual rows
            // under a chat anchor, editing the draft moves the text under a
            // composer anchor. Abort instead of pointing the highlight at
            // different text. Streamed chat text growth does not (it never
            // moves an existing row), so it keeps the selection alive.
            if self.selection.is_press_active()
                && self.selection_guard.as_ref() != Some(&self.selection_fingerprint())
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
            // works out. `self.chat_area` keeps the full band: that is what the
            // bar's geometry and its hit testing are derived from.
            chat_height = chunks[1].height;
            self.chat_area = chunks[1];
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
                            .paint_selection(frame.buffer_mut(), &self.selection);
                        self.chat.capture_visible_rows(frame.buffer_mut());
                    }
                    Some(SelectionRegion::Composer) => {
                        pointer::paint_selection(
                            frame.buffer_mut(),
                            &self.input,
                            input_rect,
                            &self.selection,
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
            _ = selection_autoscroll_tick(app.selection_autoscroll_at) => {
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
