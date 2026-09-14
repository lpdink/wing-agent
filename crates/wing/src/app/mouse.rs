//! Mouse routing — one declaration of who owns a pointer gesture.
//!
//! The pointer has four channels, and each of them is declared here:
//!
//! * [`App::pointer_owner`] walks [`POINTER_PRIORITY`] — the **one** place
//!   where the order (scrollbar → composer → chat band) is written down. A
//!   press is offered to each owner in turn and the first claim wins;
//! * the wheel is a channel of its own: it always scrolls the chat view and is
//!   never claimed by a panel or a popup, so the history stays reachable while
//!   an ask panel / the model picker / the command popup is up;
//! * hover belongs to the overlay scrollbar alone (it is the only thing that
//!   reacts to motion);
//! * a drag / release does **not** re-pick an owner: it goes back to the region
//!   the press started in (see [`super::selection_session`]), so a pointer that
//!   wanders out of the region keeps producing coordinates in the space the
//!   gesture began in.
//!
//! Call directions: the run loop calls [`App::handle_mouse`] and turns the
//! [`MouseOutcome`] into a frame request; this module calls the composer /
//! chat selection handlers of the selection lane and the modal lane's
//! [`App::composer_pointer_blocked`] guard (it does not re-derive "is a modal
//! up?" on its own).

use crate::ui::scrollbar;
use crate::ui::scrollbar::ScrollbarGeometry;
use crate::ui::selection::SelectionRegion;

use super::App;

/// How a handled mouse event wants the next frame to happen.
///
/// The wheel, a press and a release are one-shot user actions: they draw
/// immediately (like keys do, bypassing the frame gate). A drag is a *flood* —
/// a touchpad emits motion far above the frame rate, and each drag frame
/// re-snapshots the visible rows — so it is coalesced by the frame gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MouseOutcome {
    /// Nothing visible changed; no redraw.
    Ignored,
    /// The view changed, but the redraw can wait for the frame gate.
    Coalesced,
    /// Direct user action — draw right away.
    Immediate,
}

/// An owner a pointer gesture can belong to.
///
/// The owner list of a gesture is [`POINTER_PRIORITY`]; the enum carries no
/// state of its own — the claim (and the geometry it claims through) is asked
/// of the live frame each time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerOwner {
    /// The overlay scrollbar: it overprints the chat band's last column, so it
    /// has to see the event before the band does.
    Scrollbar,
    /// The composer block: the only region that reacts to a plain click (it
    /// places the cursor), guarded by the modal lane's pointer guard.
    Composer,
    /// The chat band: it keeps its drag-to-copy contract and the link click.
    Chat,
}

/// The order a pointer gesture is offered to the owners.
///
/// **The declaration order is the priority order** — this array is the single
/// place where it is written down, and [`App::pointer_owner`] is its only
/// reader. Moving a line here moves the priority for presses; nothing else in
/// the app encodes it.
pub(super) const POINTER_PRIORITY: [PointerOwner; 3] = [
    PointerOwner::Scrollbar,
    PointerOwner::Composer,
    PointerOwner::Chat,
];

impl PointerOwner {
    /// Whether this owner claims the position in the current frame state.
    ///
    /// Every claim reads the last frame's geometry ([`super::frame`]) and the
    /// live widget state; none of them mutates anything — the press handler
    /// does that once, after the chain has picked a winner.
    fn claims(self, app: &App, column: u16, row: u16) -> bool {
        match self {
            PointerOwner::Scrollbar => app.scrollbar_at(column, row).is_some(),
            // The composer sits below the chat band, and it is claimed only
            // while no modal owns the keyboard: the ask panel / model picker /
            // visible popup are typing into the draft, and placing a cursor or
            // copying a fragment under them would race that flow.
            PointerOwner::Composer => {
                app.composer_contains(column, row) && !app.composer_pointer_blocked()
            }
            PointerOwner::Chat => app.chat.contains_screen(column, row),
        }
    }
}

impl App {
    /// The owner of a pointer gesture at this position (`None` = nobody claims
    /// it, e.g. the status bar).
    pub(super) fn pointer_owner(&self, column: u16, row: u16) -> Option<PointerOwner> {
        POINTER_PRIORITY
            .into_iter()
            .find(|owner| owner.claims(self, column, row))
    }

    /// Handle a mouse event, reporting how the next frame should happen.
    ///
    /// See the module docs for the four channels; the press path is the only
    /// one that consults the priority chain (a drag / release belongs to the
    /// region the press claimed).
    pub(super) fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> MouseOutcome {
        use crossterm::event::MouseButton;
        use crossterm::event::MouseEventKind;

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.chat.scroll_up(super::WHEEL_SCROLL_LINES);
                MouseOutcome::Immediate
            }
            MouseEventKind::ScrollDown => {
                self.chat
                    .scroll_down(super::WHEEL_SCROLL_LINES, self.geometry.chat_height());
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

    /// Left press: hand the position to the first owner in
    /// [`POINTER_PRIORITY`] that claims it.
    ///
    /// A press off the bar also drops whatever hover / drag look the bar kept
    /// — the same cleanup a focus loss performs — so the outcome cannot be the
    /// region's alone: the repaint the bar asks for wins over the selection's
    /// "nothing to see" verdict.
    fn mouse_press(&mut self, column: u16, row: u16) -> MouseOutcome {
        let owner = self.pointer_owner(column, row);
        // The bar first: its column overprints the chat band, so grabbing the
        // grip under the pointer is the only way to drag it.
        if owner == Some(PointerOwner::Scrollbar) {
            let geom = self
                .scrollbar_at(column, row)
                .expect("the bar claims the position it was found at");
            return self.grab_scrollbar(&geom, row);
        }
        let bar_repaint = self.clear_scrollbar_interaction();
        let outcome = match owner {
            Some(PointerOwner::Composer) => self.composer_press(column, row),
            // A position outside both regions (status bar, popups) belongs to
            // nobody: like hover, it changes nothing.
            Some(PointerOwner::Chat) => self.chat_selection_press(column, row),
            Some(PointerOwner::Scrollbar) | None => MouseOutcome::Ignored,
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
    pub(super) fn mouse_drag(&mut self, column: u16, row: u16) -> bool {
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
    /// frame (the claim [`PointerOwner::Composer`] is built on).
    pub(super) fn composer_contains(&self, column: u16, row: u16) -> bool {
        self.geometry.composer_contains(column, row)
    }

    /// Grab the bar at a pressed row: the track jumps there, and the grip is
    /// taken at that same row so the drag that follows keeps it — the thumb
    /// stays under the pointer for the whole gesture (a press on the thumb
    /// itself is therefore a no-op: it keeps the position it already has).
    ///
    /// The grab also lights the bar up: "the user is manipulating the bar" is
    /// the visual contract, and without any-motion reporting (multiplexers)
    /// the press is the only event that can say so.
    fn grab_scrollbar(&mut self, geom: &ScrollbarGeometry, row: u16) -> MouseOutcome {
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

    /// Geometry of the overlay scrollbar for the current frame state
    /// (`None` = content fits, or nothing has been drawn yet).
    pub(super) fn scrollbar_geometry(&self) -> Option<ScrollbarGeometry> {
        self.geometry
            .scrollbar(self.chat.content_height(), self.chat.scroll_position())
    }

    /// The bar as a claim on a pointer position, read through the frame's
    /// geometry (see [`super::frame::FrameGeometry::scrollbar_at`]).
    pub(super) fn scrollbar_at(&self, column: u16, row: u16) -> Option<ScrollbarGeometry> {
        self.geometry.scrollbar_at(
            self.chat.content_height(),
            self.chat.scroll_position(),
            column,
            row,
        )
    }

    /// Move the chat to the position under a pointer row, through the shared
    /// follow contract (see `ChatView::scroll_to`). Returns whether anything
    /// visible changed, so the 16ms frame gate can be bypassed only when the
    /// view really moved.
    fn scroll_to_row(&mut self, geom: &ScrollbarGeometry, row: u16) -> bool {
        let target = scrollbar::offset_for_row(geom, row, self.scrollbar.grip);
        let before = (self.chat.scroll_position(), self.chat.is_at_bottom());
        self.chat.scroll_to(target, geom.viewport_height);
        before != (self.chat.scroll_position(), self.chat.is_at_bottom())
    }

    /// Drop hover / drag state (content stopped overflowing, a press missed
    /// the bar, or the window lost focus). Returns whether it changed.
    pub(super) fn clear_scrollbar_interaction(&mut self) -> bool {
        self.scrollbar.clear()
    }
}
