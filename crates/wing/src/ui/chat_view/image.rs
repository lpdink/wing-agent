//! The picture face: where this frame's markdown anchors go, and which files
//! the view references.
//!
//! Two facts, both produced **while the band renders** and both consumed by the
//! app — the same shape as the link table next door:
//!
//! * [`FrameImage`] — one paint request per visible anchor (box, signed offset,
//!   encode target, path). Recorded by [`ChatViewWidget`](super::ChatViewWidget)
//!   and installed in one assignment at the end of the frame, so a half-rendered
//!   frame is never painted. The app resolves them through the image store and
//!   paints *after* every overlay (see `app::images`), which is what makes a
//!   picture the last write over its rect.
//! * [`ChatView::image_candidates`] — every local image path the cells
//!   reference, harvested off the rendered lines (`CachedCell::image_candidates`)
//!   so the app can probe their headers. Discovery only: a path that stops being
//!   listed keeps whatever the table already knows.
//!
//! Nothing here draws, reads a file or decides a policy: geometry is the
//! anchor's (`ImageSpan`), the path is [`resolve_image_path`](crate::render::markdown::resolve_image_path)'s
//! output (keyed by the markdown layer), and the picture itself is
//! [`crate::ui::image`]'s.

use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::layout::Size;

use crate::render::markdown::ImageSpan;

use super::ChatView;

/// One picture a frame wants drawn.
///
/// Coordinates are **absolute screen cells** of the frame that recorded it; the
/// picture is painted into `area`'s box at [`frame.area`](Self::area)'s
/// top-left, clipped to the band by [`crate::ui::image::paint`] (via
/// [`offset`](Self::offset)) — so a box scrolled half out of view draws its
/// visible rows only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameImage {
    /// The anchor's box **clipped to the band**: full width (the anchor's
    /// `cols` never exceed the content width), and only the rows that are on
    /// screen. Empty-area entries are never recorded.
    pub area: Rect,
    /// The box's top-left corner relative to the band's top-left, as cells —
    /// `(column, negative)` when the box starts above the viewport. This is
    /// `ui::image::paint`'s signed offset; `area.y` is its clamped form.
    pub offset: (i16, i16),
    /// The encode target in cells: the **whole** box (`cols` × `rows`), not the
    /// visible part — scrolling must not change the target, or every wheel
    /// notch would re-encode.
    pub target: Size,
    /// Resolved image path: the metadata table's key and the store's.
    pub path: PathBuf,
}

impl ChatView {
    /// The pictures the last rendered frame asked for (empty when images are
    /// off, nothing is anchored, or nothing was drawn yet).
    ///
    /// The app resolves and paints them *after* the overlays — see
    /// [`FrameImage`] and `app::images`.
    pub fn frame_images(&self) -> &[FrameImage] {
        &self.frame_images
    }

    /// Every local image path the view's cells reference, for header probing.
    ///
    /// Collected from the cells' memoised candidate lists (rebuilt with their
    /// lines — see `CachedCell::image_candidates`), so this is a walk over the
    /// cells and their (short) path lists, not a re-render. Duplicates across
    /// cells are kept: the caller looks every path up in its own table anyway,
    /// which is both the dedupe and the "already probed" check.
    ///
    /// Pending user messages are not walked: they render as plain text, so they
    /// can carry no anchors and no image destinations.
    pub fn image_candidates(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for cell in &self.cells {
            out.extend_from_slice(cell.image_candidates());
        }
        out
    }
}

/// The frame's paint request for one anchor, or `None` when the box misses the
/// visible window entirely (or cannot be painted at all).
///
/// `row` is the anchor's first screen row **relative to the cell's first
/// rendered row** and `skip` how many of the cell's rows are scrolled off the
/// top of the band; `render_y` is the screen row the first visible row renders
/// at — the same pair the widget draws the cell with. `row` is the anchor's line
/// index unless an over-wide line above it wrapped into extra rows
/// (`CellFrame::image_row`). `band` is the area the widget was rendered into
/// (the band minus the scrollbar gutter).
///
/// The vertical extent is clipped to the band here (so the caller's overlay
/// tests see the rect the user sees), while the horizontal extent is left whole:
/// an anchor's `column + cols` is exactly the content width by construction, and
/// `paint` refuses — rather than clips — a picture that does not fit.
pub(super) fn frame_image_for(
    span: &ImageSpan,
    row: usize,
    band: Rect,
    skip: usize,
    render_y: u16,
    cell_visible: usize,
) -> Option<FrameImage> {
    if span.cols == 0 || span.rows == 0 || band.width == 0 || band.height == 0 {
        return None;
    }
    let rows = i32::from(span.rows);
    let line = i32::try_from(row).unwrap_or(i32::MAX);
    let skip = i32::try_from(skip).unwrap_or(i32::MAX);
    let visible = i32::try_from(cell_visible).unwrap_or(i32::MAX);
    // The box's own row range [line, line + rows) must meet the cell's visible
    // rows [skip, skip + visible).
    if line + rows <= skip || line >= skip + visible {
        return None;
    }
    let top = i32::from(render_y) + line - skip;
    let clipped_top = top.max(i32::from(band.y));
    let clipped_bottom = (top + rows).min(i32::from(band.bottom()));
    if clipped_bottom <= clipped_top {
        return None;
    }
    let offset_y = top - i32::from(band.y);
    Some(FrameImage {
        area: Rect::new(
            band.x.saturating_add(span.column),
            u16::try_from(clipped_top).ok()?,
            span.cols,
            u16::try_from(clipped_bottom - clipped_top).ok()?,
        ),
        offset: (
            i16::try_from(span.column).ok()?,
            i16::try_from(offset_y.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).ok()?,
        ),
        target: Size::new(span.cols, span.rows),
        path: span.path.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::markdown::ImageShape;
    use crate::render::markdown::anchor_rows;

    /// An 800×600 anchor at markdown width 38 (`cols`), line 4, column 2.
    fn anchor(line: usize, cols: u16) -> ImageSpan {
        ImageSpan {
            line,
            column: 2,
            cols,
            rows: anchor_rows(cols, ImageShape::new(800, 600)),
            path: PathBuf::from("/ws/plot.png"),
            alt: "plot".into(),
            px_w: 800,
            px_h: 600,
        }
    }

    #[test]
    fn a_fully_visible_anchor_keeps_its_box() {
        let band = Rect::new(0, 1, 40, 20);
        let span = anchor(4, 38);
        let frame = frame_image_for(&span, span.line, band, 0, band.y, 20).expect("visible");
        assert_eq!(frame.area, Rect::new(2, 5, 38, span.rows));
        assert_eq!(frame.offset, (2, 4));
        assert_eq!(frame.target, Size::new(38, span.rows));
        assert_eq!(frame.path, PathBuf::from("/ws/plot.png"));
    }

    #[test]
    fn a_box_scrolled_above_the_band_is_clipped_and_keeps_a_negative_offset() {
        let band = Rect::new(0, 1, 40, 20);
        let span = anchor(6, 38);
        // The cell's first visible line is line 8 (skip 8 rows), rendered at band.y.
        let frame = frame_image_for(&span, span.line, band, 8, band.y, 20).expect("partly visible");
        assert_eq!(frame.offset.1, -2, "the box starts two rows above the band");
        assert_eq!(frame.area.y, band.y);
        assert_eq!(frame.area.height, span.rows - 2);
    }

    #[test]
    fn a_box_running_past_the_bottom_is_clipped_to_the_band() {
        let band = Rect::new(0, 1, 40, 6);
        let span = anchor(0, 38);
        let frame = frame_image_for(&span, span.line, band, 0, band.y, 6).expect("visible");
        assert_eq!(frame.area.bottom(), band.bottom());
        assert_eq!(frame.area.height, 6);
        // The target is the whole box: scrolling must not re-encode.
        assert_eq!(frame.target.height, span.rows);
    }

    #[test]
    fn a_box_outside_the_window_is_not_recorded() {
        let band = Rect::new(0, 1, 40, 10);
        let span = anchor(0, 38);
        // Wholly above the window.
        assert!(frame_image_for(&span, span.line, band, 1, band.y, 9).is_some());
        assert!(frame_image_for(&span, span.line, band, 40, band.y, 9).is_none());
        // Wholly below it.
        assert!(frame_image_for(&span, span.line, band, 0, band.y, 0).is_none());
    }

    /// The wrapped row (not the line index) is what the picture is placed at:
    /// an anchor pushed down by an over-wide line above it keeps its columns and
    /// target, and moves by exactly the extra rows.
    #[test]
    fn the_anchor_lands_on_its_wrapped_row() {
        let band = Rect::new(0, 1, 40, 20);
        let span = anchor(4, 38);
        let on_line = frame_image_for(&span, span.line, band, 0, band.y, 20).expect("visible");
        let wrapped = frame_image_for(&span, span.line + 3, band, 0, band.y, 20).expect("visible");
        assert_eq!(wrapped.offset, (2, 7));
        assert_eq!(wrapped.offset.1 - on_line.offset.1, 3);
        assert_eq!(wrapped.area.y - on_line.area.y, 3);
        assert_eq!(wrapped.area.x, on_line.area.x);
        assert_eq!(wrapped.target, on_line.target);
    }

    #[test]
    fn a_collapsed_band_or_box_records_nothing() {
        let span = anchor(0, 38);
        assert!(frame_image_for(&span, span.line, Rect::new(0, 0, 0, 10), 0, 0, 10).is_none());
        assert!(frame_image_for(&span, span.line, Rect::new(0, 0, 40, 0), 0, 0, 10).is_none());
        let zero = anchor(0, 0);
        assert!(frame_image_for(&zero, zero.line, Rect::new(0, 0, 40, 10), 0, 0, 10).is_none());
    }
}
