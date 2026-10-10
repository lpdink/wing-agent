//! Pixels into cells: the one place where a picture meets a character box.
//!
//! Two layers need the same answer to "how many cells does this picture occupy?", and they must
//! never disagree:
//!
//! * the **layout** reserves an anchor's rows before anything is decoded —
//!   `render/markdown/images.rs::anchor_rows`, whose result is a cell's height cache entry and part
//!   of the streaming engine's "resting state == the reference render" invariant;
//! * the **encoder** (`ui/image/encode.rs`) computes the cell footprint it is about to paint from
//!   the whole decoded image.
//!
//! One row too many leaves a blank band under the picture; one too few and the picture covers the
//! text below it. So both call [`fit_cells`]: the layout passes the header's pixel dimensions, the
//! encoder passes the decoded image's.
//!
//! **The arithmetic is `ratatui-image`'s**: [`fit_cells`] ports `ratatui_image::Resize::Fit(None)`
//! `::size_for`, narrowed to plain pixel dimensions so the layout can use it without a decoded image
//! (the layout never touches the filesystem). The port is asserted against the upstream function in
//! `ui/image/encode.rs`'s tests, so the encoder's output size cannot drift from it:
//!
//! ```text
//! box (cells) × cell (pixels)      →  box (pixels)
//! min(box, native) per axis        →  never upscale the slot the picture may use
//! fit_area_proportionally          →  largest aspect-preserving size inside it, ≥1 px
//! ceil(px / cell)                  →  whole cells (the picture's cell footprint)
//! ```
//!
//! The result is never larger than the box on either axis and never larger than the picture's own
//! cell footprint — that is what makes it a *fit*, and why a small picture reserves few rows instead
//! of being letterboxed into a tall box. The exception is upstream's `u16::MAX` saturation branch
//! (see [`fit_area_proportionally`]), in two symmetric variants that each need two absurd inputs at
//! once and are unreachable from a real terminal: the **width** variant answers `u16::MAX` columns
//! for a picture wider than 65535 px inside a box past 65535 px wide (≈ 6554 columns at a 10 px
//! cell); the **height** variant answers `u16::MAX` rows for a picture taller than 65535 px in a box
//! past 65535 px tall (at the layout's 36-row cap that takes a cell at least 1821 px high). Real
//! character cells are 8–40 px tall.

use ratatui::layout::Size;

/// The pixel size of one character cell — the terminal's font size.
///
/// The layout's second input, next to the image's own pixel dimensions: both the row count and the
/// drawn footprint are computed from a box expressed in cells, so the cell size has to be known
/// *before* anything is laid out.
///
/// Deliberately a separate type from `ui::image::CellPixels` (which the capability probe owns and
/// the encoder reads): this layer must not depend on the `ui` module. The chat view builds one from
/// a probe result with `CellPixels::new(cell.width, cell.height)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CellPixels {
    /// Cell width in pixels.
    pub width: u16,
    /// Cell height in pixels.
    pub height: u16,
}

impl CellPixels {
    /// A cell size from the probe's two dimensions.
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    /// Whether both dimensions are non-zero — i.e. whether the fit below can
    /// divide by them at all. A degenerate cell is not a layout input: see
    /// [`ImageOpts::anchor`](crate::render::markdown::ImageOpts::anchor).
    pub const fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// The cell footprint of a `px_w × px_h` picture inside a `box_cells` box.
///
/// Total: a degenerate input (a zero box, a zero pixel dimension, a zero cell) answers
/// [`Size::ZERO`] — the caller decides what "no room" means (the encoder reports a failure, the
/// layout keeps its minimum). Never panics and never divides by zero.
///
/// The picture's own pixel size caps the answer on both axes, so this never upscales (a 16×16 icon
/// in a 10×20 cell is 2×1 cells however large the box is) — except for upstream's `u16::MAX`
/// saturation branch, whose two variants no real terminal can reach (see the module docs).
pub fn fit_cells(px_w: u32, px_h: u32, box_cells: Size, cell: CellPixels) -> Size {
    if px_w == 0 || px_h == 0 || box_cells.width == 0 || box_cells.height == 0 || !cell.is_valid() {
        return Size::ZERO;
    }
    let box_px_w = u32::from(box_cells.width) * u32::from(cell.width);
    let box_px_h = u32::from(box_cells.height) * u32::from(cell.height);
    // `Resize::Fit` clamps the *box* with the picture's own size (not the
    // fitted result with it): that is what keeps the natural size as the
    // ceiling on both axes.
    let (fitted_w, fitted_h) =
        fit_area_proportionally(px_w, px_h, box_px_w.min(px_w), box_px_h.min(px_h));
    Size::new(
        cells_ceil(fitted_w, cell.width),
        cells_ceil(fitted_h, cell.height),
    )
}

/// Round a pixel extent up to whole cells.
///
/// Integer arithmetic, and total: an extent past `u16::MAX` cells saturates
/// (what the `as u16` cast in `ratatui-image`'s float version does too). That
/// saturation is only reachable together with the branch in
/// [`fit_area_proportionally`] — the store's 16 Mpx pixel ceiling bounds the
/// *picture*, but the box's own pixel width can still be the trigger, so the
/// real bound is "no terminal is 6554 columns wide".
fn cells_ceil(px: u32, cell_px: u16) -> u16 {
    let cells = u64::from(px).div_ceil(u64::from(cell_px));
    u16::try_from(cells).unwrap_or(u16::MAX)
}

/// The largest `w × h` that fits `nwidth × nheight` with the aspect ratio kept, at least one pixel
/// on each axis.
///
/// A port of `ratatui_image`'s private `fit_area_proportionally` — itself the `image` crate's
/// `resize_dimensions` (`fill = false`) — including its `u16::MAX` saturation branch, kept so that
/// [`fit_cells`] agrees with the upstream function for *every* input (see `ui/image/encode.rs`'s
/// parity test).
///
/// The width branch triggers when the fitted width itself exceeds 65535 px, which needs the picture
/// **and** the box to be that wide at once (`nw` is bounded by `min(box_px_w, px_w)`): a guard
/// against arithmetic no real terminal can produce, not something the store's pixel budget rules
/// out on its own (a 100000×3 picture is only 300 000 px). It answers `u32::MAX` pixels of width,
/// i.e. `u16::MAX` cells, past the box on that axis. The height branch mirrors it exactly on `nh`.
/// Both are ports of upstream's behaviour, not choices made here.
fn fit_area_proportionally(width: u32, height: u32, nwidth: u32, nheight: u32) -> (u32, u32) {
    let ratio = f64::min(
        f64::from(nwidth) / f64::from(width),
        f64::from(nheight) / f64::from(height),
    );
    let nw = ((f64::from(width) * ratio).round() as u64).max(1);
    let nh = ((f64::from(height) * ratio).round() as u64).max(1);
    if nw > u64::from(u16::MAX) {
        let ratio = f64::from(u16::MAX) / f64::from(width);
        (
            u32::MAX,
            ((f64::from(height) * ratio).round() as u32).max(1),
        )
    } else if nh > u64::from(u16::MAX) {
        let ratio = f64::from(u16::MAX) / f64::from(height);
        (((f64::from(width) * ratio).round() as u32).max(1), u32::MAX)
    } else {
        (nw as u32, nh as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellPixels = CellPixels::new(10, 20);

    /// Hand-computed fits: what the encoder draws for each (picture, box).
    #[test]
    fn fit_is_the_largest_box_inside_both_limits() {
        let cases: &[(u32, u32, u16, u16, u16, u16)] = &[
            // (px_w, px_h, box cols, box rows, expected cols, expected rows)
            //
            // Smaller than the box: the picture's own size wins (no upscaling).
            (512, 512, 140, 36, 52, 26),
            (16, 16, 140, 36, 2, 1),
            (100, 100, 10, 10, 10, 5),
            // Larger than the box: the box wins, aspect kept.
            (1920, 1080, 140, 36, 128, 36),
            (800, 600, 80, 36, 80, 30),
            // Width-limited, with the height rounding up to a whole cell.
            (400, 100, 20, 20, 20, 3),
            // A one-pixel sliver still occupies one cell on the short axis.
            (1, 5000, 118, 36, 1, 36),
            (8000, 100, 118, 36, 118, 1),
        ];
        for &(px_w, px_h, cols, rows, want_cols, want_rows) in cases {
            assert_eq!(
                fit_cells(px_w, px_h, Size::new(cols, rows), CELL),
                Size::new(want_cols, want_rows),
                "{px_w}x{px_h} into {cols}x{rows} cells"
            );
        }
    }

    #[test]
    fn fit_never_exceeds_the_box_or_the_picture() {
        // The two invariants the layout and the encoder both rely on.
        for (px_w, px_h) in [
            (1u32, 1u32),
            (16, 16),
            (512, 512),
            (800, 600),
            (1920, 1080),
            (100, 10000),
            (10000, 100),
            (4000, 4000),
        ] {
            for cols in [1u16, 2, 40, 118, 500] {
                for rows in [1u16, 2, 36] {
                    let fitted = fit_cells(px_w, px_h, Size::new(cols, rows), CELL);
                    assert!(fitted.width <= cols, "{px_w}x{px_h} into {cols}x{rows}");
                    assert!(fitted.height <= rows, "{px_w}x{px_h} into {cols}x{rows}");
                    let natural = Size::new(
                        (px_w as f32 / 10.0).ceil() as u16,
                        (px_h as f32 / 20.0).ceil() as u16,
                    );
                    assert!(fitted.width <= natural.width, "upscaled width");
                    assert!(fitted.height <= natural.height, "upscaled height");
                    assert!(fitted.width >= 1 && fitted.height >= 1, "collapsed");
                }
            }
        }
    }

    #[test]
    fn fit_is_degenerate_for_degenerate_inputs() {
        // No panics, no division by zero, one defined answer: zero.
        for (px_w, px_h, cols, rows, cell) in [
            (0u32, 100u32, 10u16, 10u16, CELL),
            (100, 0, 10, 10, CELL),
            (0, 0, 10, 10, CELL),
            (100, 100, 0, 10, CELL),
            (100, 100, 10, 0, CELL),
            (100, 100, 10, 10, CellPixels::new(0, 20)),
            (100, 100, 10, 10, CellPixels::new(10, 0)),
            (100, 100, 10, 10, CellPixels::new(0, 0)),
            (100, 100, 0, 0, CellPixels::new(0, 0)),
        ] {
            assert_eq!(
                fit_cells(px_w, px_h, Size::new(cols, rows), cell),
                Size::ZERO,
                "{px_w}x{px_h} into {cols}x{rows} at {cell:?}"
            );
        }
        assert!(!CellPixels::new(0, 20).is_valid());
        assert!(!CellPixels::new(10, 0).is_valid());
        assert!(CellPixels::new(10, 20).is_valid());
        assert_eq!(CellPixels::default(), CellPixels::new(0, 0));
    }

    #[test]
    fn a_cell_of_one_pixel_is_the_natural_size() {
        // The degenerate-cell question, asked of the *valid* extreme: a 1x1
        // cell means one pixel per cell, i.e. rounding to whole pixels.
        let cell = CellPixels::new(1, 1);
        assert_eq!(
            fit_cells(100, 50, Size::new(200, 200), cell),
            Size::new(100, 50)
        );
        assert_eq!(
            fit_cells(100, 50, Size::new(40, 40), cell),
            Size::new(40, 20)
        );
        assert_eq!(fit_cells(1, 1, Size::new(4, 4), cell), Size::new(1, 1));
    }

    /// The `u16::MAX` saturation branch, kept for parity with upstream: a
    /// picture wider than 65535 pixels that barely shrinks.
    #[test]
    fn a_picture_wider_than_the_cell_grid_saturates_like_upstream() {
        // 70000x100 into a 7000x36 box at a 10x20 cell: the fit is a no-op on
        // both axes, and upstream's `nw > u16::MAX` branch collapses the width
        // to `u32::MAX` — i.e. to `u16::MAX` cells.
        let fitted = fit_cells(70000, 100, Size::new(7000, 36), CELL);
        assert_eq!(fitted, Size::new(u16::MAX, 5));
        // The row count is unaffected either way: that branch never moves the
        // height, which is the number the layout reads.
        assert_eq!(fitted.height, 5);

        // The trigger is "the picture *and* the box are past 65535 px wide",
        // not "the picture is big": the same 100000x3 picture in a 1000-column
        // box never reaches it (its fit is bounded by the box), which is the
        // other side of the documented exception.
        assert_eq!(
            fit_cells(100000, 3, Size::new(1000, 36), CELL),
            Size::new(1000, 1),
            "a narrow box keeps the width bound"
        );
        // Both sides past 65535 — here through an absurd 32768-px cell in a
        // 2-column box — and the width exceeds the box: the one input class
        // that breaks the column bound.
        let saturated = fit_cells(100000, 3, Size::new(2, 36), CellPixels::new(32768, 32768));
        assert_eq!(saturated, Size::new(u16::MAX, 1));
        assert!(
            saturated.width > 2,
            "the documented exception: the width may exceed the box here"
        );
        assert_eq!(saturated.height, 1, "the height never exceeds the box");
    }
}
