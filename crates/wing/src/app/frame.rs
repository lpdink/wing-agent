//! The last frame's geometry — the contract every input path reads.
//!
//! `draw` **records** what it laid out (the whole terminal area, the chat band
//! and the composer block); the mouse, the selection, the keyboard's page keys
//! and the composer's editing width **consume** that record. Nothing else may
//! rely on "which field `draw` happened to fill in" — hit testing a pointer
//! between frames is a query about the frame the user is pointing at, and that
//! frame is this value.
//!
//! Two kinds of facts, deliberately kept apart:
//!
//! * **recorded** — the rects above. They are the frame's layout, so they only
//!   change when a new frame is drawn;
//! * **derived** — anything that follows the *content* (the scrollbar's thumb
//!   position, which tracks the chat's content height and scroll offset). Those
//!   are computed from the recorded band plus live state at query time, so a
//!   drag that scrolls the view between two frames still hit-tests against the
//!   position the bar has right now (see [`FrameGeometry::scrollbar`]).
//!
//! The chat's *own* rect (the band minus the scrollbar gutter, plus the scroll
//! offset it was rendered with) is recorded by [`crate::ui::chat_view`] itself
//! and read back through `ChatView::geometry` / `contains_screen`: one fact,
//! one writer.

use ratatui::layout::Rect;

use crate::ui::scrollbar;
use crate::ui::scrollbar::ScrollbarGeometry;

/// Terminal width assumed before the first frame is drawn.
///
/// The run loop draws before it can deliver a key or a mouse event, so nothing
/// in production reads the geometry of a frame that was never drawn. The
/// composer's editor is driven without one in the tests, though, and it needs a
/// width to wrap against; 80 is the value the field this type replaced started
/// with, kept so those tests keep describing the same editor.
const UNFRAMED_WIDTH: u16 = 80;

/// Geometry of the last drawn frame (`Default` = nothing drawn yet).
///
/// [`App::draw`](super::App::draw) is the only writer; the input paths are the
/// readers. See the module docs for what is recorded and what is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FrameGeometry {
    /// The whole terminal area of the last frame.
    area: Rect,
    /// The chat band of the last frame — the viewport **including** the
    /// scrollbar gutter (the bar paints and hit-tests over this rect; the chat
    /// widget itself renders into the band minus the gutter).
    chat_band: Rect,
    /// The composer block (input area) of the last frame.
    composer: Rect,
}

impl Default for FrameGeometry {
    /// The pre-first-frame state: no band, no composer — and a terminal width
    /// of [`UNFRAMED_WIDTH`], see there.
    fn default() -> Self {
        Self {
            area: Rect::new(0, 0, UNFRAMED_WIDTH, 0),
            chat_band: Rect::default(),
            composer: Rect::default(),
        }
    }
}

impl FrameGeometry {
    /// Record the whole terminal area of the frame being drawn.
    ///
    /// Called at the top of the draw pass, before anything reads the width —
    /// the selection-invalidation check compares the anchor's width against
    /// **this** frame's, not the previous one's.
    pub(super) fn record_area(&mut self, area: Rect) {
        self.area = area;
    }

    /// Record the chat band this frame laid out.
    pub(super) fn record_chat_band(&mut self, band: Rect) {
        self.chat_band = band;
    }

    /// Record the composer block this frame laid out.
    pub(super) fn record_composer(&mut self, area: Rect) {
        self.composer = area;
    }

    /// Terminal width of the last frame.
    pub(super) fn width(&self) -> u16 {
        self.area.width
    }

    /// The chat band of the last frame (for the bar's geometry and hit tests).
    pub(super) fn chat_band(&self) -> Rect {
        self.chat_band
    }

    /// Height of the chat band in rows — the unit the page keys and the wheel
    /// clamp their steps against.
    pub(super) fn chat_height(&self) -> usize {
        self.chat_band.height as usize
    }

    /// Whether a screen position lies inside the composer block of the last
    /// frame.
    ///
    /// A zero-sized rect (nothing drawn yet, or a collapsed composer) accepts
    /// nothing — the same "no geometry, no hit" rule the chat band follows.
    pub(super) fn composer_contains(&self, column: u16, row: u16) -> bool {
        let area = self.composer;
        area.width > 0
            && area.height > 0
            && column >= area.x
            && column < area.right()
            && row >= area.y
            && row < area.bottom()
    }

    /// Geometry of the overlay scrollbar over the recorded band (`None` =
    /// content fits, or nothing has been drawn).
    ///
    /// **Derived, not recorded**: the band, track and column are frame facts,
    /// but the thumb's position follows the chat's *content height and scroll
    /// offset* — quantities a drag moves between two frames. Caching the thumb
    /// would let a hit test disagree with the view it is mapping (the very
    /// "which frame am I pointing at?" bug this contract exists to kill), so
    /// the caller passes the live pair in and gets the geometry of the moment.
    pub(super) fn scrollbar(
        &self,
        content_height: usize,
        scroll_offset: usize,
    ) -> Option<ScrollbarGeometry> {
        scrollbar::geometry(self.chat_band(), content_height, scroll_offset)
    }

    /// The bar as a claim on a pointer position: the geometry when the pointer
    /// is on the bar itself, `None` when there is no bar or the pointer missed
    /// it. Nothing outside the bar is ever the bar's.
    pub(super) fn scrollbar_at(
        &self,
        content_height: usize,
        scroll_offset: usize,
        column: u16,
        row: u16,
    ) -> Option<ScrollbarGeometry> {
        let geom = self.scrollbar(content_height, scroll_offset)?;
        scrollbar::hit(&geom, column, row).then_some(geom)
    }
}
