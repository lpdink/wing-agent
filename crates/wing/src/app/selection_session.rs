//! The text-selection gesture — its lifecycle, its rules and its timers.
//!
//! A drag is one session with four pieces of state, all owned by
//! [`SelectionSession`]:
//!
//! * the anchor / focus state machine ([`Selection`], `ui::selection`) — the
//!   coordinates themselves;
//! * the **fingerprint** of the region the press landed in ([`SelectionGuard`])
//!   — see [`SelectionSession::needs_abort`] for the one invalidation rule;
//! * the markdown link recorded under the pointer at press time — a click opens
//!   *that* link, never whatever scrolled under the pointer since;
//! * the edge auto-scroll **deadline** (absolute, see
//!   [`SELECTION_AUTOSCROLL_DELAY`]).
//!
//! Two rules that used to be written twice in the app root live here once:
//!
//! * **invalidation** — `needs_abort` is read by the frame (a structural change
//!   can land between two frames) and by the release (it can land in the same
//!   loop iteration); both get the same verdict from the same predicate;
//! * **the timer invariant** — there is no deadline without a drag: every way a
//!   gesture ends (release, cancel, invalidation, reaching the content edge, a
//!   composer press) goes through [`SelectionSession::cancel`] /
//!   [`SelectionSession::release`] / [`SelectionSession::stop_edge_scroll`].
//!
//! The session owns *coordinates and rules*; the effects (placing the cursor,
//! pushing a clipboard / open-link intent) stay with [`App`], which is also
//! where the fingerprints are read from (chat structure, draft, frame width).
//!
//! Call directions: [`super::mouse`] dispatches presses, drags and releases in
//! here; the frame calls `cancel_selection` when a fingerprint expired and
//! paints through [`SelectionSession::state`]; the run loop parks its timer arm
//! on [`selection_autoscroll_tick`].

use std::time::Duration;
use std::time::Instant;

use crate::ui::input_area::pointer;
use crate::ui::selection::Selection;
use crate::ui::selection::SelectionPoint;
use crate::ui::selection::SelectionRegion;

use super::App;
use super::AppIntent;
use super::MouseOutcome;

/// Step interval of the drag edge auto-scroll: one content line per 50 ms
/// (20 lines/s). Matches the reference implementation's cadence, and is fast
/// enough to feel continuous without skipping rows. The timer only runs while
/// the pointer rests on the chat band's top / bottom row — reaching the
/// content edge stops it (see [`App::tick_selection_autoscroll`]).
pub(super) const SELECTION_AUTOSCROLL_DELAY: Duration = Duration::from_millis(50);

/// Timer arm of the run loop's `select!` for the drag edge auto-scroll.
///
/// The deadline is **absolute** (`Instant`, set when the drag arms a direction
/// and pushed forward after every step) because `select!` rebuilds this future
/// on every loop iteration: a relative `sleep` would restart on each incoming
/// event and, during streaming (events arrive far faster than the 50 ms tick),
/// would never complete at all. With `None` the arm parks in `pending()`, so
/// nothing wakes the loop while no drag sits on an edge.
pub(super) async fn selection_autoscroll_tick(deadline: Option<Instant>) {
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
pub(super) enum SelectionGuard {
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

/// One text-selection gesture, from press to release (or abort).
///
/// Reads mirror the state machine's own names (`is_press_active`, `region`,
/// `anchor`, …), so a caller cannot tell where the coordinates live — only the
/// three pieces of gesture state around them (`link`, `deadline`, the
/// fingerprint) have names of their own.
#[derive(Debug, Default)]
pub(super) struct SelectionSession {
    /// The anchor / focus coordinates of the drag.
    selection: Selection,
    /// Fingerprint of the region as it was when the press landed.
    guard: Option<SelectionGuard>,
    /// Markdown link under the pointer at press time.
    ///
    /// Recorded from the *frame the user pressed on* (never recomputed at
    /// release, which would hit whatever scrolled under the pointer in the
    /// meantime) and opened only when the gesture turns out to be a click —
    /// a drag is a selection, not an open.
    link: Option<String>,
    /// Absolute deadline of the next drag edge auto-scroll step.
    ///
    /// Absolute (not "sleep 50 ms from now") because the run loop's `select!`
    /// rebuilds its timer arm on every iteration — a relative sleep would be
    /// starved by streaming events. `None` while no drag rests on an edge.
    autoscroll_at: Option<Instant>,
}

impl SelectionSession {
    /// Start a gesture at `at`, recording the fingerprint of its region.
    ///
    /// A new press always starts from scratch: the previous deadline and any
    /// recorded link are dropped here, so the invariants hold by construction.
    fn begin(&mut self, at: SelectionPoint, guard: SelectionGuard) {
        self.selection.begin(at);
        self.guard = Some(guard);
        self.link = None;
        self.autoscroll_at = None;
    }

    /// Record the link under the pointer at press time (chat presses only).
    fn note_link(&mut self, target: Option<String>) {
        self.link = target;
    }

    /// Extend the gesture to `at`.
    fn drag_to(&mut self, at: SelectionPoint) {
        self.selection.drag_to(at);
    }

    /// Finish the drag: returns the ordered bounds when the selection is
    /// non-empty, then clears every piece of the session's state.
    fn release(&mut self, at: SelectionPoint) -> Option<(SelectionPoint, SelectionPoint)> {
        self.guard = None;
        self.link = None;
        self.autoscroll_at = None;
        self.selection.release(at)
    }

    /// Abort the gesture, dropping everything it recorded.
    fn cancel(&mut self) {
        // A recorded link must not survive an aborted gesture either — a
        // release after a focus loss / structural change opens nothing.
        self.guard = None;
        self.link = None;
        self.autoscroll_at = None;
        self.selection.cancel();
    }

    /// The one invalidation rule: **an in-flight gesture whose fingerprint no
    /// longer describes the region it is anchored in**.
    ///
    /// Read by the frame (a structural change can land between two frames) and
    /// by the release (it can land in the same loop iteration, the intent runs
    /// right after the draw) — one predicate, one verdict.
    ///
    /// The current fingerprint is produced lazily **for the region the gesture
    /// is anchored in**: building one costs an allocation (the composer's
    /// draft) and is only needed while a press is actually in flight, and the
    /// region is what selects the rule — the app never guesses it from
    /// anything else.
    pub(super) fn needs_abort(
        &self,
        current: impl FnOnce(SelectionRegion) -> SelectionGuard,
    ) -> bool {
        match self.selection.region() {
            None => false,
            Some(region) => self.guard.as_ref() != Some(&current(region)),
        }
    }

    /// The anchor / focus state machine, for the painters (`ui::selection` owns
    /// the highlight pass; this type owns the gesture's lifetime).
    pub(super) fn state(&self) -> &Selection {
        &self.selection
    }

    /// The link recorded at press time, taken out (a click consumes it once).
    fn take_link(&mut self) -> Option<String> {
        self.link.take()
    }

    /// The recorded link. Test seam: production consumes it through
    /// `take_link` at release, and the diagnostics assert that a bar press
    /// never records one.
    #[cfg(test)]
    pub(super) fn link(&self) -> Option<&str> {
        self.link.as_deref()
    }

    /// Arm / re-arm the edge auto-scroll for `direction` (-1 up, 1 down; `0`
    /// stops it) and push the next step's deadline one [`SELECTION_AUTOSCROLL_DELAY`] out.
    ///
    /// Re-arming on every motion event is what makes a drag that keeps moving
    /// along an edge restart the cadence; the deadline stays absolute, so a
    /// flood of events cannot postpone a step the way a relative sleep would.
    fn set_edge_scroll(&mut self, direction: i8) {
        self.selection.set_auto_scroll(direction);
        self.autoscroll_at = (direction != 0).then(|| Instant::now() + SELECTION_AUTOSCROLL_DELAY);
    }

    /// Stop the edge auto-scroll: no direction, no deadline.
    fn stop_edge_scroll(&mut self) {
        self.selection.stop_auto_scroll();
        self.autoscroll_at = None;
    }

    /// Re-arm the same direction for one more step (after a step has landed).
    fn arm_next_step(&mut self) {
        self.autoscroll_at = Some(Instant::now() + SELECTION_AUTOSCROLL_DELAY);
    }

    /// The next edge auto-scroll step's deadline (`None` = no edge is held).
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.autoscroll_at
    }

    /// Whether a drag is in flight (highlight / follow freeze / snapshots all
    /// key off this).
    pub(super) fn is_press_active(&self) -> bool {
        self.selection.is_press_active()
    }

    /// Whether the pointer moved since the press.
    pub(super) fn is_dragged(&self) -> bool {
        self.selection.is_dragged()
    }

    /// The press position (the fixed end of the selection), if any.
    pub(super) fn anchor(&self) -> Option<SelectionPoint> {
        self.selection.anchor()
    }

    /// The region this gesture is anchored in (`None` = no press).
    pub(super) fn region(&self) -> Option<SelectionRegion> {
        self.selection.region()
    }

    /// Ordered selection bounds, or `None` when the selection is empty. Test
    /// seam: `release` hands the same value back to the copy path.
    #[cfg(test)]
    pub(super) fn bounds(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        self.selection.bounds()
    }

    /// The moving end of the selection, if any.
    pub(super) fn focus(&self) -> Option<SelectionPoint> {
        self.selection.focus()
    }

    /// Current auto-scroll direction (-1 / 0 / 1).
    pub(super) fn auto_scroll(&self) -> i8 {
        self.selection.auto_scroll()
    }
}

impl App {
    /// Left press inside the composer: arm a drag selection.
    ///
    /// The cursor is **not** moved here: a press cannot know yet whether it
    /// becomes a click (→ place the cursor) or a drag (→ select and copy) —
    /// the reference behaviour, and the same reason the chat press only
    /// records an anchor.
    pub(super) fn composer_press(&mut self, column: u16, row: u16) -> MouseOutcome {
        let Some(point) = pointer::point(&self.input, self.input.rendered_area(), column, row)
        else {
            return MouseOutcome::Ignored;
        };
        // No `chat.unfollow()` here: the composer selection has nothing to do
        // with the chat's follow contract, and no edge auto-scroll either — a
        // composer press even disarms a stale chat deadline (`begin`).
        let guard = self.selection_fingerprint(SelectionRegion::Composer);
        self.selection.begin(point, guard);
        MouseOutcome::Immediate
    }

    /// Drag inside the composer: extend the selection, including the character
    /// under the pointer (the mirror of `ChatView::snap_focus_right`).
    pub(super) fn composer_drag(&mut self, column: u16, row: u16) -> bool {
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
    pub(super) fn composer_release(&mut self, column: u16, row: u16) -> MouseOutcome {
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
    pub(super) fn chat_selection_press(&mut self, column: u16, row: u16) -> MouseOutcome {
        if !self.chat.contains_screen(column, row) {
            return MouseOutcome::Ignored;
        }
        let Some(point) = self.selectable_point_at(column, row) else {
            return MouseOutcome::Ignored;
        };
        // Link hit test first, from the frame the user is looking at. It only
        // *records* — a press still begins a selection so dragging across a
        // link selects its text.
        let link = self.chat.link_at(column, row).map(str::to_owned);
        let guard = self.selection_fingerprint(SelectionRegion::Chat);
        self.selection.begin(point, guard);
        self.selection.note_link(link);
        // Freeze follow for the duration of the drag: the render pins the
        // viewport to the bottom edge and re-arms `auto_scroll` whenever the
        // offset sits there, so streaming content would otherwise yank the view
        // (and the highlighted rows) away. Released again by
        // `chat_selection_release` → `scroll_down(0, …)`.
        self.chat.unfollow();
        MouseOutcome::Immediate
    }

    /// Drag: extend the selection and arm / disarm the edge auto-scroll.
    ///
    /// The pointer is clamped into the visible band, so dragging past an edge
    /// keeps producing content coordinates — that is what makes the pointer
    /// resting on the top / bottom row scroll the view and extend the
    /// selection.
    pub(super) fn chat_selection_drag(&mut self, column: u16, row: u16) -> bool {
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
        // Arm / re-arm the *absolute* deadline: a drag that keeps moving along
        // an edge restarts the 50 ms cadence from now.
        self.selection.set_edge_scroll(direction);
        true
    }

    /// Release: end the selection and copy whatever it covered.
    ///
    /// The highlight disappears by construction (the selection state is gone
    /// after this call), the follow contract is restored from the current
    /// scroll position, and a non-empty selection is pushed as a clipboard
    /// intent. A zero-width selection (plain click) copies nothing.
    pub(super) fn chat_selection_release(&mut self, column: u16, row: u16) -> MouseOutcome {
        // A press that never moved is a click: open the link recorded at press
        // time instead of copying (a click copies nothing anyway — the
        // selection is zero-width — so this only decides *what* the click
        // does). A drag keeps the selection semantics. "Never moved" is
        // checked both ways — no `Drag` event *and* the pointer back on the
        // anchor — so a click that lost its motion events (tmux, a terminal
        // that drops `?1002`) cannot open a link the user dragged away from.
        let release_point = self.selectable_point_at(column, row);
        let clicked_link = self.selection.take_link();
        if let Some(target) = clicked_link
            && !self.selection.is_dragged()
            && release_point.is_some()
            && release_point == self.selection.anchor()
        {
            // Restore the follow contract from the current position, exactly
            // like the copy path below.
            self.chat.scroll_down(0, self.geometry.chat_height());
            self.selection.cancel();
            self.push_intent(AppIntent::OpenLink(target));
            return MouseOutcome::Immediate;
        }
        // Re-arm the follow state iff the viewport is still at the bottom edge
        // (`n = 0` only judges — it never moves). This runs for clicks too, so
        // the `unfollow` from the press cannot leave the view stuck in reading
        // mode.
        self.chat.scroll_down(0, self.geometry.chat_height());
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
    pub(super) fn cancel_selection(&mut self) {
        let region = self.selection.region();
        let was_active = self.selection.is_press_active();
        self.selection.cancel();
        if was_active && region == Some(SelectionRegion::Chat) {
            self.chat.scroll_down(0, self.geometry.chat_height());
        }
    }

    /// Fingerprint of `region` as it is right now — the value a press records
    /// and the frame / release compare against, see [`SelectionGuard`].
    ///
    /// The region is an argument, not a read of the live gesture: a press
    /// records the fingerprint of the region it is *starting in*, and a check
    /// asks for the one the gesture is *anchored in*. Deriving it from the
    /// session's own state would make the two calls disagree before and after
    /// `begin` — the swap that this signature exists to prevent.
    pub(super) fn selection_fingerprint(&self, region: SelectionRegion) -> SelectionGuard {
        match region {
            SelectionRegion::Composer => SelectionGuard::Composer {
                text: self.input.text(),
                width: self.geometry.width(),
            },
            SelectionRegion::Chat => SelectionGuard::Chat {
                cells: self.chat.len(),
                pending: self.chat.pending_len(),
                width: self.geometry.width(),
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
    /// all (the bar dispatches first, see [`App::pointer_owner`]).
    fn selectable_point_at(&self, column: u16, row: u16) -> Option<SelectionPoint> {
        self.chat.content_point_at(column, row)
    }

    /// Step the drag edge auto-scroll by one content line.
    ///
    /// Returns `true` when the frame must be redrawn. The step stops (and
    /// disarms the deadline) the moment the viewport cannot move any further —
    /// no busy loop, no timer left behind. The focus travels with the rows
    /// that scrolled by, so the selection grows while the view moves.
    pub(super) fn tick_selection_autoscroll(&mut self) -> bool {
        let direction = self.selection.auto_scroll();
        if direction == 0 {
            self.selection.stop_edge_scroll();
            return false;
        }
        let before = self.chat.scroll_position();
        if direction < 0 {
            self.chat.scroll_up(1);
        } else {
            self.chat.scroll_down(1, self.geometry.chat_height());
        }
        // The drag keeps the follow state frozen, even when this step landed
        // exactly on the bottom edge (that judgement happens on release).
        self.chat.unfollow();
        if self.chat.scroll_position() == before {
            self.selection.stop_edge_scroll();
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
        self.selection.arm_next_step();
        true
    }
}
