//! Decode + terminal-protocol encoding. **Worker-thread only.**
//!
//! # Why the protocols are built by hand
//!
//! `ratatui-image`'s own `Picker::new_protocol` is the natural entry point, but a `Picker`
//! can only get a font size from `from_query_stdio` (I/O) or the deprecated `from_fontsize`
//! — there is no public setter, and no public way to force its tmux flag. This layer must be
//! able to *inject* both (tests, demo, future DPI override), so it constructs the protocol
//! types directly — `Kitty::new` / `Sixel::new` / `Iterm2::new` are public — and reproduces
//! upstream's size policy with the public `Resize`:
//!
//! ```text
//! target (cells) --Resize::size_for--> actual (cells, ≤ target, aspect kept)
//!                --Resize::resize----> exact pixels = actual × cell_pixels (padded, never upscaled)
//! ```
//!
//! `Resize::Fit(None)` never grows the **cell footprint**: a 16×16 icon in a 10×20 cell is a
//! 2×1-cell image, however large the box it was given. (The pixels themselves are scaled to a
//! whole number of cells — that same icon is encoded as 20×20 pixels, at most one cell of
//! upscaling, with the remainder padded transparent.) That is deliberate: reporting a small
//! image as a big block would lie about its resolution and waste the reserved rows.
//!
//! # Why sixel is row-sliced
//!
//! Vertically clipped rendering is only reachable through
//! [`ratatui_image::sliced::SlicedImage`], whose `Sixel` variant wraps `SlicedSixel` — a type
//! that lives in a **private** module and therefore cannot be built from outside the crate.
//! The remaining public shape is upstream's own `Sliced(Vec<Protocol>)`: one single-cell-tall
//! protocol per row. So every protocol except kitty (which has real placeholders) is emitted
//! as a stack of one-row protocols. Cost: one escape sequence per row instead of one for the
//! whole image; benefit: scrolling works, and the extra sequences are only sent when the row
//! actually changes.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use image::{DynamicImage, ImageReader};
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::iterm2::Iterm2;
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::protocol::sixel::Sixel;
use ratatui_image::sliced::SlicedProtocol;

use super::meta::Unavailable;
use super::probe::{CellPixels, ImageProtocol};

/// Kitty image ids. Unique-within-session is all the protocol needs; a counter is
/// reproducible where `rand::random()` is not, and `rand` is not a dependency this step may
/// add. Wrapping after 4 billion images is not a scenario.
static NEXT_KITTY_ID: AtomicU32 = AtomicU32::new(1);

fn next_kitty_id() -> u32 {
    NEXT_KITTY_ID.fetch_add(1, Ordering::Relaxed)
}

/// Read and fully decode `path`. Worker-thread only.
pub(crate) fn decode(path: &Path) -> Result<DynamicImage, Unavailable> {
    let file = File::open(path).map_err(|_| Unavailable::Unreadable)?;
    let reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|_| Unavailable::Unreadable)?;
    reader.decode().map_err(|_| Unavailable::NotAnImage)
}

/// Encode an already-decoded image for `protocol`, targeting `target` cells.
///
/// The result's cell `size()` is `≤ target` on both axes, aspect ratio preserved: the cell
/// footprint never grows, while the pixels are scaled to whole cells (see the module docs).
/// Worker-thread only (the encoders are CPU-bound).
pub(crate) fn encode(
    image: DynamicImage,
    target: Size,
    protocol: ImageProtocol,
    cell: CellPixels,
    is_tmux: bool,
) -> Result<SlicedProtocol, Unavailable> {
    let font = ratatui_image::FontSize::new(cell.width, cell.height);
    let resize = Resize::Fit(None);
    let actual = resize.size_for(&image, font, target);
    let image = resize.resize(&image, font, actual, None);

    if let ImageProtocol::Kitty = protocol {
        let kitty = Kitty::new(image, actual, next_kitty_id(), is_tmux, false)
            .map_err(|_| Unavailable::EncodeFailed)?;
        return Ok(SlicedProtocol::Kitty(kitty));
    }

    let cell_height = u32::from(font.height);
    let mut rows = Vec::with_capacity(usize::from(actual.height));
    for index in 0..actual.height {
        let y = u32::from(index) * cell_height;
        let height = cell_height.min(image.height().saturating_sub(y));
        if height == 0 {
            break;
        }
        let row = image.crop_imm(0, y, image.width(), height);
        let row_size = Size::new(actual.width, 1);
        let encoded = match protocol {
            ImageProtocol::Kitty => unreachable!("handled above"),
            ImageProtocol::Sixel => Protocol::Sixel(
                Sixel::new(row, row_size, is_tmux).map_err(|_| Unavailable::EncodeFailed)?,
            ),
            ImageProtocol::Iterm2 => Protocol::ITerm2(
                Iterm2::new(row, row_size, is_tmux).map_err(|_| Unavailable::EncodeFailed)?,
            ),
        };
        rows.push(encoded);
    }
    if rows.is_empty() {
        return Err(Unavailable::EncodeFailed);
    }
    Ok(SlicedProtocol::Sliced(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::image::meta::write_png_fixture;
    use crate::ui::image::test_support::TempDir;

    const CELL: CellPixels = CellPixels::new(10, 20);

    fn fixture(tag: &str, px_w: u32, px_h: u32) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new(tag);
        let path = dir.path().join("fixture.png");
        write_png_fixture(&path, px_w, px_h);
        (dir, path)
    }

    #[test]
    fn decodes_a_real_png() {
        let (_dir, path) = fixture("encode-decode", 40, 30);
        let image = decode(&path).expect("decode");
        assert_eq!((image.width(), image.height()), (40, 30));
    }

    #[test]
    fn decode_rejects_a_file_that_is_not_an_image() {
        let dir = TempDir::new("encode-liar");
        let path = dir.path().join("liar.png");
        std::fs::write(&path, b"not a png at all").expect("write");
        assert_eq!(decode(&path).unwrap_err(), Unavailable::NotAnImage);
    }

    #[test]
    fn decode_reports_an_unreadable_file() {
        let dir = TempDir::new("encode-missing");
        assert_eq!(
            decode(&dir.path().join("nope.png")).unwrap_err(),
            Unavailable::Unreadable
        );
    }

    #[test]
    fn every_protocol_encodes_within_the_target() {
        let (_dir, path) = fixture("encode-protocols", 400, 300);
        for protocol in [
            ImageProtocol::Kitty,
            ImageProtocol::Sixel,
            ImageProtocol::Iterm2,
        ] {
            let image = decode(&path).expect("decode");
            let encoded = encode(image, Size::new(20, 10), protocol, CELL, false)
                .unwrap_or_else(|err| panic!("encode {protocol:?}: {err}"));
            let size = encoded.size();
            assert!(
                size.width <= 20 && size.height <= 10,
                "{protocol:?} produced {size:?} for a 20x10 target"
            );
            assert!(size.width > 0 && size.height > 0, "{protocol:?} is empty");
            let shape_matches = match protocol {
                ImageProtocol::Kitty => matches!(encoded, SlicedProtocol::Kitty(_)),
                // Sixel and iTerm2 both ride the row-stack variant.
                ImageProtocol::Sixel | ImageProtocol::Iterm2 => {
                    matches!(encoded, SlicedProtocol::Sliced(_))
                }
            };
            assert!(shape_matches, "{protocol:?} produced the wrong variant");
        }
    }

    #[test]
    fn row_stack_holds_one_protocol_per_visible_row() {
        // 100x100 pixels at a 10x20 cell = 10x5 cells; the target keeps it at natural size.
        let (_dir, path) = fixture("encode-rows", 100, 100);
        let image = decode(&path).expect("decode");
        let encoded =
            encode(image, Size::new(40, 20), ImageProtocol::Iterm2, CELL, false).expect("encode");
        assert_eq!(encoded.size(), Size::new(10, 5));
        let SlicedProtocol::Sliced(rows) = encoded else {
            panic!("expected a row stack");
        };
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn fit_never_grows_the_cell_footprint() {
        // 16x16 pixels at a 10x20 cell is 2x1 cells, well inside a 40x20 target.
        let (_dir, path) = fixture("encode-small", 16, 16);
        let image = decode(&path).expect("decode");
        let encoded =
            encode(image, Size::new(40, 20), ImageProtocol::Kitty, CELL, false).expect("encode");
        assert_eq!(encoded.size(), Size::new(2, 1));

        // The pixels, though, are scaled to fill those cells exactly: the 16x16 icon is
        // transmitted as 20x20 pixels (2 cells x 10px wide, 1 cell x 20px tall). That is
        // padding to whole cells, not upscaling the image — at most one cell of slack.
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        ratatui_image::sliced::SlicedImage::new(
            &encoded,
            ratatui_image::sliced::SignedPosition::from((0, 0)),
        )
        .render(area, &mut buf);
        let anchor = buf[(0, 0)].symbol();
        assert!(
            anchor.contains("s=20,v=20"),
            "expected the image to be scaled to whole cells, got {anchor:?}"
        );
    }

    #[test]
    fn fit_keeps_the_aspect_ratio_inside_the_target() {
        let (_dir, path) = fixture("encode-aspect", 400, 100);
        let image = decode(&path).expect("decode");
        let encoded =
            encode(image, Size::new(20, 20), ImageProtocol::Kitty, CELL, false).expect("encode");
        // 400x100 pixels is 40x5 cells at natural size. Into a 20-cell-wide box the pixels
        // halve to 200x50, and 50 pixels is 2.5 cells — rounded up to 3, never distorted.
        assert_eq!(encoded.size(), Size::new(20, 3));
    }

    #[test]
    fn kitty_ids_are_unique_per_encoding() {
        let (_dir, path) = fixture("encode-kitty-id", 40, 40);
        let render_anchor = |image: DynamicImage| {
            let encoded =
                encode(image, Size::new(4, 4), ImageProtocol::Kitty, CELL, false).expect("encode");
            let area = ratatui::layout::Rect::new(0, 0, 4, 4);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            ratatui::widgets::Widget::render(
                ratatui_image::sliced::SlicedImage::new(
                    &encoded,
                    ratatui_image::sliced::SignedPosition::from((0, 0)),
                ),
                area,
                &mut buf,
            );
            // The kitty anchor carries the transmit sequence, which embeds the image id.
            buf[(0, 0)].symbol().to_string()
        };
        let first = render_anchor(decode(&path).expect("decode"));
        let second = render_anchor(decode(&path).expect("decode"));
        assert!(first.contains("\u{1b}_G"), "anchor is not a kitty transmit");
        assert_ne!(first, second, "two encodings reused the same image id");
    }
}
