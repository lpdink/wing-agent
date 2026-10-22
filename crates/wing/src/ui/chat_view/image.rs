//! The picture face: where this frame's anchors go (markdown images and the pictures a
//! user message carries), and which local files the view references.
//!
//! Two facts, both produced **while the band renders** and both consumed by the
//! app — the same shape as the link table next door:
//!
//! * [`FrameImage`] — one paint request per visible anchor (box, signed offset,
//!   encode target, source). Recorded by [`ChatViewWidget`](super::ChatViewWidget)
//!   and installed in one assignment at the end of the frame, so a half-rendered
//!   frame is never painted. The app resolves them through the image store and
//!   paints *after* every overlay (see `app::images`), which is what makes a
//!   picture the last write over its rect.
//! * [`ChatView::image_candidates`] — every local image path the cells
//!   reference, harvested off the rendered lines (`CachedCell::image_candidates`)
//!   so the app can probe their headers. Discovery only: a path that stops being
//!   listed keeps whatever the table already knows.
//!
//! Nothing here draws, reads a file, fetches bytes or decides a policy: geometry is the
//! anchor's (`ImageSpan`), the identity is the anchor's [`PictureSource`], and the pixels are
//! [`crate::ui::image`]'s (files) or `app::images::media`'s (gateway objects) to produce.

use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::layout::Size;

use crate::render::markdown::ImageSpan;
use crate::render::picture::PictureSource;

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
    /// What the box holds: a local file (the metadata table's key and the store's)
    /// or a gateway media object (its bytes are fetched by `app::images`).
    pub source: PictureSource,
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
    /// Read off the cells' memoised candidate lists (rebuilt with their lines —
    /// see `CachedCell::image_candidates`), so this is a concatenation, not a
    /// re-render. Duplicates across cells are kept: the caller looks every path
    /// up in its own table anyway, which is both the dedupe and the "already
    /// probed" check.
    ///
    /// The concatenation is buffered and only rebuilt when the cell set or some
    /// cell's list changed — the frame asks for it twice, and a re-collection
    /// copies every path of the whole view. The signature folds both halves
    /// (cell count and the epochs' sum): the sum alone is not monotone (removing
    /// a projected cell lowers it), so counting the cells with it keeps a
    /// removal from cancelling a rebuild.
    ///
    /// User messages are not walked for *media* either: their pictures are gateway objects,
    /// not files, so there is no header to probe (`media::render_body` gets the shape from
    /// the ref itself). This walk is about local files only.
    pub fn image_candidates(&mut self) -> &[PathBuf] {
        let signature = self.candidates_signature_now();
        if self.candidates_signature != Some(signature) {
            self.candidates.clear();
            for cell in &self.cells {
                self.candidates.extend_from_slice(cell.image_candidates());
            }
            self.candidates_signature = Some(signature);
        }
        &self.candidates
    }

    /// Drop the candidate buffer (the cells are gone).
    pub(super) fn forget_candidates(&mut self) {
        self.candidates.clear();
        self.candidates_signature = None;
    }

    /// What the candidate buffer was built from: the cell count and the sum of
    /// their `candidates_epoch`s.
    fn candidates_signature_now(&self) -> (usize, u64) {
        let epochs = self
            .cells
            .iter()
            .fold(0u64, |acc, cell| acc.wrapping_add(cell.candidates_epoch()));
        (self.cells.len(), epochs)
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
    // The window a picture may touch is the cell's own visible slice, bounded by the band:
    // a box whose tail runs past the rows its cell actually rendered (a user bubble's bottom
    // padding is one such row) is clipped rather than painted over someone else's row.
    let window_top = i32::from(band.y).max(i32::from(render_y));
    let window_bottom = i32::from(band.bottom()).min(i32::from(render_y) + visible);
    let clipped_top = top.max(window_top);
    let clipped_bottom = (top + rows).min(window_bottom);
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
        source: span.source.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::markdown::CellPixels;
    use crate::render::markdown::ImageShape;
    use crate::render::markdown::anchor_rows;

    /// The terminal cell every box here is laid out for.
    const CELL: CellPixels = CellPixels::new(10, 20);

    /// An 800×600 anchor at markdown width 38 (`cols`), line 4, column 2.
    fn anchor(line: usize, cols: u16) -> ImageSpan {
        ImageSpan {
            line,
            column: 2,
            cols,
            rows: anchor_rows(cols, ImageShape::new(800, 600), CELL),
            source: PictureSource::File(PathBuf::from("/ws/plot.png")),
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
        assert_eq!(
            frame.source,
            PictureSource::File(PathBuf::from("/ws/plot.png"))
        );
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
