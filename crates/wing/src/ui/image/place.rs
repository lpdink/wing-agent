//! Ready-to-draw images and the buffer placement primitive.
//!
//! [`paint`] is the only function in the crate that writes an image into a ratatui buffer,
//! and it owns the contract that makes graphics coexist with text:
//!
//! * it writes **only** the part of the image that falls inside `area` (signed vertical
//!   offset — an image scrolled half out of the viewport draws its visible rows only);
//! * it refuses to draw horizontally clipped images (see [`paint`]'s documentation);
//! * it returns the rect it covered, so the caller can mask selection hit-testing, skip link
//!   detection and keep its own geometry honest.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget;
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};

use super::probe::ImageProtocol;

/// An encoded image, ready to be painted as many times as the caller likes.
///
/// Cheap to clone (an `Arc` bump): the caller can pull several images out of
/// [`super::ImageStore`], collect them, and paint them later in the same frame.
#[derive(Clone)]
pub struct ReadyImage {
    inner: Arc<Inner>,
}

struct Inner {
    protocol: SlicedProtocol,
    kind: ImageProtocol,
    size: Size,
}

impl std::fmt::Debug for ReadyImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyImage")
            .field("protocol", &self.inner.kind)
            .field("size", &self.inner.size)
            .finish_non_exhaustive()
    }
}

impl ReadyImage {
    pub(crate) fn new(protocol: SlicedProtocol, kind: ImageProtocol) -> Self {
        let size = protocol.size();
        Self {
            inner: Arc::new(Inner {
                protocol,
                kind,
                size,
            }),
        }
    }

    /// The protocol this image was encoded for.
    pub fn protocol(&self) -> ImageProtocol {
        self.inner.kind
    }

    /// The image's footprint in cells.
    ///
    /// Never larger than the target the image was requested for: encoding uses
    /// `Resize::Fit`, which does not upscale, so a small image honestly reports a small
    /// footprint.
    pub fn size(&self) -> Size {
        self.inner.size
    }

    /// Whether two handles point at the very same encoding (identity, not equality).
    #[cfg(test)]
    pub(crate) fn same_encoding(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// Paint `image` into `buf`, clipped to `area`.
///
/// `offset` is the signed cell offset of the image's top-left corner **relative to the
/// top-left of `area`** — `(2, -3)` means "two columns in, starting three rows above the
/// viewport", i.e. the image is scrolled half out of view.
///
/// Returns the rect actually covered (the intersection of the image's footprint with
/// `area`), or `None` when nothing was drawn because the image is entirely outside the area.
///
/// # Contract
///
/// * **Placement is the last write of the frame.** Within the covered rect, the cells
///   carrying the graphics payload (the kitty placeholders, the escape-sequence anchors)
///   cannot be `CellDiffOption::Skip` — skipping them would suppress the sequence that
///   draws the image in the first place. Callers must therefore paint *after* all text has
///   been drawn into the buffer.
/// * **No horizontal clipping.** The image must fit: `0 <= offset.0` and
///   `offset.0 + size.width() <= area.width`. Otherwise `None` is returned and nothing is
///   drawn — the layout's job is to derive the encode target from the available width, so a
///   too-wide image is a caller bug rather than something to paper over with three
///   protocols' worth of inconsistent clipping.
/// * **Vertical clipping is real.** A partially visible image paints its visible rows, and
///   the returned rect is the visible part only.
pub fn paint(image: &ReadyImage, area: Rect, offset: (i16, i16), buf: &mut Buffer) -> Option<Rect> {
    let size = image.size();
    if area.width == 0 || area.height == 0 || size.width == 0 || size.height == 0 {
        return None;
    }
    if offset.0 < 0
        || u32::from(offset.0.unsigned_abs()) + u32::from(size.width) > u32::from(area.width)
    {
        return None;
    }

    let covered = covered_rect(size, area, offset)?;
    let position = SignedPosition::from(offset);
    SlicedImage::new(&image.inner.protocol, position).render(area, buf);
    Some(covered)
}

/// The part of the image's footprint that lands inside `area`, or `None` if none of it does.
fn covered_rect(size: Size, area: Rect, offset: (i16, i16)) -> Option<Rect> {
    let top = i32::from(offset.1);
    let bottom = top + i32::from(size.height);
    let visible_top = top.max(0);
    let visible_bottom = bottom.min(i32::from(area.height));
    if visible_bottom <= visible_top {
        return None;
    }
    Some(Rect::new(
        area.x + u16::try_from(offset.0).expect("checked non-negative"),
        area.y + u16::try_from(visible_top).expect("inside area"),
        size.width,
        u16::try_from(visible_bottom - visible_top).expect("inside area"),
    ))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use ratatui::buffer::CellDiffOption;
    use ratatui::style::Style;

    use super::*;
    use crate::ui::image::encode::{decode, encode};
    use crate::ui::image::meta::write_png_fixture;
    use crate::ui::image::probe::CellPixels;
    use crate::ui::image::test_support::TempDir;

    const CELL: CellPixels = CellPixels::new(10, 20);

    /// A 200x60 px image at a 10x20 cell = 20x3 cells: three rows to slice.
    fn image(tag: &str, protocol: ImageProtocol) -> (TempDir, ReadyImage) {
        let dir = TempDir::new(tag);
        let path = dir.path().join("plot.png");
        write_png_fixture(&path, 200, 60);
        let decoded = decode(&path).expect("decode");
        let encoded = encode(decoded, Size::new(20, 3), protocol, CELL, false).expect("encode");
        (dir, ReadyImage::new(encoded, protocol))
    }

    fn buffer(width: u16, height: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, width, height))
    }

    #[test]
    fn size_is_the_encoded_cell_footprint() {
        let (_dir, image) = image("place-size", ImageProtocol::Kitty);
        assert_eq!(image.size(), Size::new(20, 3));
        assert_eq!(image.protocol(), ImageProtocol::Kitty);
    }

    #[test]
    fn kitty_writes_placeholders_over_the_whole_footprint() {
        let (_dir, image) = image("place-kitty", ImageProtocol::Kitty);
        let area = Rect::new(0, 0, 24, 6);
        let mut buf = buffer(24, 6);
        let covered = paint(&image, area, (0, 0), &mut buf).expect("painted");
        assert_eq!(covered, Rect::new(0, 0, 20, 3));

        for y in 0..3 {
            for x in 0..20 {
                let symbol = buf[(x, y)].symbol();
                assert!(
                    symbol.contains('\u{10EEEE}'),
                    "cell ({x},{y}) is not a kitty placeholder: {symbol:?}"
                );
                // The placeholder is one cell wide even though the anchor cell also carries
                // the (much longer) transmit sequence.
                assert_eq!(
                    buf[(x, y)].diff_option,
                    CellDiffOption::ForcedWidth(NonZeroU16::new(1).expect("1 is not zero"))
                );
            }
        }
        // The transmit sequence rides in the anchor cell, once.
        assert!(buf[(0, 0)].symbol().contains("\u{1b}_G"));
        assert!(!buf[(1, 0)].symbol().contains("\u{1b}_G"));

        // Nothing outside the footprint was touched.
        assert_eq!(buf[(20, 0)].symbol(), " ");
        assert_eq!(buf[(0, 3)].symbol(), " ");
    }

    #[test]
    fn kitty_placeholders_are_not_skipped() {
        // A placeholder *is* the image: marking it `Skip` would drop the sequence that draws
        // it. This pins the reason the kitty path relies on paint-last instead.
        let (_dir, image) = image("place-kitty-skip", ImageProtocol::Kitty);
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = buffer(20, 3);
        paint(&image, area, (0, 0), &mut buf).expect("painted");
        for y in 0..3 {
            for x in 0..20 {
                assert_ne!(
                    buf[(x, y)].diff_option,
                    CellDiffOption::Skip,
                    "kitty placeholder at ({x},{y}) must stay emittable"
                );
            }
        }
    }

    #[test]
    fn paint_overwrites_text_that_was_drawn_before_it() {
        let (_dir, image) = image("place-kitty-overwrite", ImageProtocol::Kitty);
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = buffer(20, 3);
        buf.set_string(0, 0, "x".repeat(20), Style::default());
        paint(&image, area, (0, 0), &mut buf).expect("painted");
        assert!(buf[(4, 0)].symbol().contains('\u{10EEEE}'));
    }

    #[test]
    fn sixel_and_iterm2_reserve_the_area_against_later_text() {
        // The buffer diff is what reaches the terminal. Cells the image reserved are `Skip`,
        // so text written into them *after* the paint is changed in the buffer but never
        // emitted — the image survives.
        let empty = Buffer::empty(Rect::new(0, 0, 20, 3));
        for protocol in [ImageProtocol::Sixel, ImageProtocol::Iterm2] {
            let (_dir, image) = image("place-reserve", protocol);
            let area = Rect::new(0, 0, 20, 3);
            let mut buf = buffer(20, 3);
            let covered = paint(&image, area, (0, 0), &mut buf).expect("painted");
            assert_eq!(covered, Rect::new(0, 0, 20, 3));

            // Every covered cell except the row anchors (leftmost column, which carries the
            // escape sequence) is skipped by the buffer diff.
            for y in 0..3 {
                for x in 0..20 {
                    let symbol = buf[(x, y)].symbol();
                    if x == 0 {
                        assert!(
                            symbol.contains("\u{1b}P") || symbol.contains("\u{1b}]1337"),
                            "{protocol:?} anchor at ({x},{y}) carries no sequence: {symbol:?}"
                        );
                        continue;
                    }
                    assert_eq!(
                        buf[(x, y)].diff_option,
                        CellDiffOption::Skip,
                        "{protocol:?} cell ({x},{y}) is not protected"
                    );
                }
            }

            // Now some later code paints text all over the reserved rect.
            buf.set_string(1, 1, "CLOBBER", Style::default());
            assert_eq!(
                buf[(1, 1)].symbol(),
                "C",
                "the write did land in the buffer"
            );

            let emitted: Vec<(u16, u16)> = empty
                .diff(&buf)
                .into_iter()
                .map(|(x, y, _)| (x, y))
                .collect();
            for (x, y) in emitted {
                assert!(
                    x == 0,
                    "{protocol:?} let later text reach the terminal at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn negative_offset_paints_only_the_visible_rows() {
        let (_dir, image) = image("place-clip-top", ImageProtocol::Kitty);
        let area = Rect::new(0, 0, 20, 4);
        let mut full = buffer(20, 4);
        paint(&image, area, (0, 0), &mut full).expect("painted");
        let mut clipped = buffer(20, 4);
        let covered = paint(&image, area, (0, -2), &mut clipped).expect("painted");
        assert_eq!(covered, Rect::new(0, 0, 20, 1));
        // Row 0 of the clipped render is row 2 of the full render.
        assert_eq!(clipped[(5, 0)].symbol(), full[(5, 2)].symbol());
    }

    #[test]
    fn positive_offset_shifts_and_trims_the_bottom() {
        let (_dir, image) = image("place-clip-bottom", ImageProtocol::Kitty);
        let area = Rect::new(1, 2, 20, 3);
        let mut full = buffer(22, 8);
        paint(&image, area, (0, 0), &mut full).expect("painted");
        let mut shifted = buffer(22, 8);
        let covered = paint(&image, area, (0, 2), &mut shifted).expect("painted");
        assert_eq!(covered, Rect::new(1, 4, 20, 1));
        assert_eq!(shifted[(6, 4)].symbol(), full[(6, 2)].symbol());
    }

    #[test]
    fn image_entirely_above_or_below_the_area_draws_nothing() {
        let (_dir, image) = image("place-outside", ImageProtocol::Kitty);
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = buffer(20, 4);
        let before = buf.clone();
        assert_eq!(paint(&image, area, (0, -3), &mut buf), None);
        assert_eq!(paint(&image, area, (0, 4), &mut buf), None);
        assert_eq!(buf, before);
    }

    #[test]
    fn images_that_do_not_fit_horizontally_are_refused() {
        let (_dir, image) = image("place-wide", ImageProtocol::Kitty);
        // 20 cells of image into 19 cells of area.
        let area = Rect::new(0, 0, 19, 4);
        let mut buf = buffer(19, 4);
        let before = buf.clone();
        assert_eq!(paint(&image, area, (0, 0), &mut buf), None);
        // Negative column offsets are refused too (nothing supports left clipping).
        assert_eq!(paint(&image, area, (-1, 0), &mut buf), None);
        assert_eq!(buf, before);

        // Fits the area but starts too far right.
        let area = Rect::new(0, 0, 24, 4);
        let mut buf = buffer(24, 4);
        assert_eq!(paint(&image, area, (5, 0), &mut buf), None);
        assert_eq!(buf, buffer(24, 4));
    }

    #[test]
    fn zero_sized_area_draws_nothing() {
        let (_dir, image) = image("place-empty", ImageProtocol::Kitty);
        let mut buf = buffer(20, 4);
        let before = buf.clone();
        assert_eq!(paint(&image, Rect::new(0, 0, 0, 4), (0, 0), &mut buf), None);
        assert_eq!(
            paint(&image, Rect::new(0, 0, 20, 0), (0, 0), &mut buf),
            None
        );
        assert_eq!(buf, before);
    }
}
