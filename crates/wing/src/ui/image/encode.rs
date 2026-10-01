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
//! target (cells) --fit_cells--------> actual (cells, ≤ target, aspect kept, never upscaled)
//!                --Resize::resize--> exact pixels = actual × cell_pixels (padded, never upscaled)
//! ```
//!
//! The first step is [`crate::render::fit::fit_cells`], **not** upstream's
//! `Resize::size_for` — the very same function the markdown layout computes an anchor's row
//! count with (see `render/markdown/images.rs`). One shared function, one set of inputs, so
//! "the box reserves `rows`" and "the picture is `size().height` cells tall" cannot drift
//! apart. `tests` below assert the port still agrees with upstream's `size_for` for every
//! input, which is what keeps the encoded bytes unchanged by the swap.
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

use image::{DynamicImage, ImageDecoder, ImageReader};
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::iterm2::Iterm2;
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::protocol::sixel::Sixel;
use ratatui_image::sliced::SlicedProtocol;

use super::meta::Unavailable;
use super::probe::{CellPixels, ImageProtocol};
use super::store::Limits;
use crate::render::fit::CellPixels as LayoutCellPixels;
use crate::render::fit::fit_cells;

/// Kitty image ids. Unique-within-session is all the protocol needs; a counter is
/// reproducible where `rand::random()` is not, and `rand` is not a dependency this step may
/// add. Wrapping after 4 billion images is not a scenario.
static NEXT_KITTY_ID: AtomicU32 = AtomicU32::new(1);

fn next_kitty_id() -> u32 {
    NEXT_KITTY_ID.fetch_add(1, Ordering::Relaxed)
}

/// Read and fully decode `path`, re-checking `limits` at the point of the read.
/// Worker-thread only.
///
/// The probe that filled the metadata table ran a frame or more earlier, and the file on
/// disk can have been replaced in between — rewriting the same path is exactly how a model
/// publishes a new picture. The budgets are therefore enforced *here* as well, on the very
/// descriptor that gets decoded: a swapped-in oversized file is never read, a swapped-in
/// giant header never gets an allocation.
///
/// The reasons are the probe's, so the caller cannot tell which side of the window it hit —
/// with one caveat: a file this path cannot even *construct* a decoder for is `NotAnImage`,
/// exactly as it would be from the probe. That includes a header truncated so early that the
/// codec rejects it while reading (an IHDR-only PNG declaring 30000×30000 lands here), which
/// is still refused before a single pixel is allocated — the gate is the *header read*, not
/// the verdict's spelling.
///
/// The header gate also covers what `ImageReader::decode` used to check for us: that call
/// ran `image`'s own `max_alloc` guard (512 MiB) over the decoded size, and `into_decoder`
/// does not. It is not needed here — `Limits::pixels` (16 M px by default) times the fattest
/// format this build decodes (`Rgba16`, 8 bytes per pixel) is ≤128 MiB, i.e. the stricter of
/// the two bounds.
pub(crate) fn decode(path: &Path, limits: &Limits) -> Result<DynamicImage, Unavailable> {
    let file = File::open(path).map_err(|_| Unavailable::Unreadable)?;
    // The size comes off the open descriptor: one `stat`, and no race between
    // the check and the `open` it is checking.
    let bytes = file.metadata().map_err(|_| Unavailable::Unreadable)?.len();
    if bytes > limits.file_bytes {
        return Err(Unavailable::TooLarge { bytes });
    }
    let reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|_| Unavailable::Unreadable)?;
    // `into_decoder` constructs the decoder from the **header only** — the same
    // call `into_dimensions` is built on — so the pixel budget can be checked
    // before anything is allocated, on the same reader the pixels come from.
    let decoder = reader.into_decoder().map_err(|_| Unavailable::NotAnImage)?;
    let (px_w, px_h) = decoder.dimensions();
    if u64::from(px_w) * u64::from(px_h) > limits.pixels {
        return Err(Unavailable::TooManyPixels { px_w, px_h });
    }
    DynamicImage::from_decoder(decoder).map_err(|_| Unavailable::NotAnImage)
}

/// Encode an already-decoded image for `protocol`, targeting `target` cells.
///
/// The result's cell `size()` is `≤ target` on both axes, aspect ratio preserved, and it is
/// exactly what [`crate::render::fit::fit_cells`] computes for the same numbers — the layout
/// reserved that many rows for the picture, so they agree by construction. Worker-thread only
/// (the encoders are CPU-bound).
pub(crate) fn encode(
    image: DynamicImage,
    target: Size,
    protocol: ImageProtocol,
    cell: CellPixels,
    is_tmux: bool,
) -> Result<SlicedProtocol, Unavailable> {
    let font = ratatui_image::FontSize::new(cell.width, cell.height);
    let resize = Resize::Fit(None);
    let cell_size = LayoutCellPixels::new(cell.width, cell.height);
    let actual = fit_cells(image.width(), image.height(), target, cell_size);
    if actual.width == 0 || actual.height == 0 {
        // Degenerate target or image (the store refuses a zero-sized target
        // before queueing) — never hand a zero size to `Resize::resize`.
        return Err(Unavailable::EncodeFailed);
    }
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
        let image = decode(&path, &Limits::default()).expect("decode");
        assert_eq!((image.width(), image.height()), (40, 30));
    }

    #[test]
    fn decode_rejects_a_file_that_is_not_an_image() {
        let dir = TempDir::new("encode-liar");
        let path = dir.path().join("liar.png");
        std::fs::write(&path, b"not a png at all").expect("write");
        assert_eq!(
            decode(&path, &Limits::default()).unwrap_err(),
            Unavailable::NotAnImage
        );
    }

    #[test]
    fn decode_reports_an_unreadable_file() {
        let dir = TempDir::new("encode-missing");
        assert_eq!(
            decode(&dir.path().join("nope.png"), &Limits::default()).unwrap_err(),
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
            let image = decode(&path, &Limits::default()).expect("decode");
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
        let image = decode(&path, &Limits::default()).expect("decode");
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
        let image = decode(&path, &Limits::default()).expect("decode");
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
        let image = decode(&path, &Limits::default()).expect("decode");
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
        let first = render_anchor(decode(&path, &Limits::default()).expect("decode"));
        let second = render_anchor(decode(&path, &Limits::default()).expect("decode"));
        assert!(first.contains("\u{1b}_G"), "anchor is not a kitty transmit");
        assert_ne!(first, second, "two encodings reused the same image id");
    }

    // ── The limits are re-checked at the read (PR #135 review, S1) ──────
    //
    // `decode` is the only place that reads a file for drawing, and it is
    // reached a frame or more after the probe that admitted the path. The
    // budgets therefore have to hold at the read itself: the file on disk may
    // have been swapped for another one in between (rewriting the same path is
    // how a model publishes a new picture).

    #[test]
    fn decode_refuses_a_file_that_grew_past_the_byte_budget() {
        let dir = TempDir::new("encode-swap-large");
        let path = dir.path().join("swapped.png");
        // Small enough to have been probed and admitted…
        write_png_fixture(&path, 40, 30);
        let limits = Limits {
            file_bytes: 512,
            ..Limits::default()
        };
        assert!(std::fs::metadata(&path).expect("stat").len() <= limits.file_bytes);
        assert!(decode(&path, &limits).is_ok(), "the small file decodes");
        // …then replaced by something far past the budget.
        std::fs::write(&path, vec![0u8; 64 * 1024]).expect("write");
        assert_eq!(
            decode(&path, &limits).unwrap_err(),
            Unavailable::TooLarge { bytes: 64 * 1024 },
            "the byte budget must hold at the read, not only at the probe"
        );
    }

    #[test]
    fn decode_refuses_a_header_past_the_pixel_budget() {
        let dir = TempDir::new("encode-swap-pixels");
        let path = dir.path().join("swapped.png");
        write_png_fixture(&path, 40, 30);
        let limits = Limits {
            pixels: 100,
            ..Limits::default()
        };
        // The header is rejected before a single pixel is allocated: the
        // reason carries the dimensions the file declares, which is only
        // knowable from the header.
        assert_eq!(
            decode(&path, &limits).unwrap_err(),
            Unavailable::TooManyPixels { px_w: 40, px_h: 30 }
        );
        // The same file under the default budget decodes, so the refusal above
        // is the budget and not a broken fixture.
        assert!(decode(&path, &Limits::default()).is_ok());
    }

    #[test]
    fn decode_still_reports_a_non_image_as_such() {
        // The new gates must not swallow the existing verdicts.
        let dir = TempDir::new("encode-swap-junk");
        let path = dir.path().join("junk.png");
        std::fs::write(&path, vec![0u8; 4096]).expect("write");
        assert_eq!(
            decode(&path, &Limits::default()).unwrap_err(),
            Unavailable::NotAnImage
        );
    }

    // ── The layout's rows are the encoder's rows ────────────────────
    //
    // The contract this step installs: `render::markdown::anchor_rows` reserves
    // exactly the rows `encode` draws. Both call `fit_cells`, so the two can
    // only agree — these tests are what makes "only" checkable, and what would
    // go red if either side went back to its own formula.

    /// The acceptance table of the step, end to end: for every (picture, cell,
    /// width) the frame hands the encoder `target = (cols, anchor_rows(…))` and
    /// the encoding comes back that many rows tall — no blank band under the
    /// picture, no overdraw below the box.
    #[test]
    fn the_encoded_height_is_the_reserved_anchor_rows() {
        // (px_w, px_h, markdown width) at a 10x20 cell, plus a second cell size
        // for the same shapes (the cell is an input, not a constant).
        let cases: &[(u32, u32, u16)] = &[
            (512, 512, 140),   // 52x26 — the user's fixture
            (512, 512, 40),    // 40x20 — width-limited
            (768, 768, 140),   // 72x36 — the cap, exactly filled
            (1920, 1080, 140), // 128x36
            (1024, 576, 140),  // 103x29
            (16, 16, 140),     // 2x1 — an icon
            (100, 10000, 140), // 1x36 — a tall sliver
            (2000, 100, 140),  // 1 row — a wide banner
        ];
        for cell in [CELL, CellPixels::new(8, 16), CellPixels::new(12, 24)] {
            for &(px_w, px_h, width) in cases {
                let (_dir, path) = fixture(&format!("fit-{px_w}x{px_h}-{width}"), px_w, px_h);
                let image = decode(&path, &Limits::default()).expect("decode");
                assert_eq!((image.width(), image.height()), (px_w, px_h));
                let rows = crate::render::markdown::anchor_rows(
                    width,
                    crate::render::markdown::ImageShape::new(px_w, px_h),
                    LayoutCellPixels::new(cell.width, cell.height),
                );
                let encoded = encode(
                    image,
                    Size::new(width, rows),
                    ImageProtocol::Kitty,
                    cell,
                    false,
                )
                .unwrap_or_else(|err| panic!("{px_w}x{px_h} into {width}x{rows}: {err}"));
                // The whole footprint, not just its height: a stretched picture
                // (the box filled by force) has the right row count and the
                // wrong size, and that is exactly the bug this contract bans.
                let expected = fit_cells(
                    px_w,
                    px_h,
                    Size::new(width, rows),
                    LayoutCellPixels::new(cell.width, cell.height),
                );
                assert_eq!(
                    encoded.size(),
                    expected,
                    "{px_w}x{px_h} at width {width} with cell {cell:?}: the layout \
                     reserved {rows} rows for a {expected:?} footprint"
                );
                assert_eq!(
                    encoded.size().height,
                    rows,
                    "{px_w}x{px_h} at width {width} with cell {cell:?}: the layout \
                     reserved {rows} rows, the encoder drew {}",
                    encoded.size().height
                );
                assert!(
                    encoded.size().width <= width,
                    "{px_w}x{px_h} at width {width}: {} columns is wider than the box",
                    encoded.size().width
                );
            }
        }
    }

    /// The same contract through the row-sliced protocols (sixel / iTerm2),
    /// whose `size()` comes from the emitted stack rather than a placeholder.
    #[test]
    fn the_row_stack_is_as_tall_as_the_reserved_rows() {
        let (_dir, path) = fixture("fit-row-stack", 512, 512);
        for protocol in [ImageProtocol::Sixel, ImageProtocol::Iterm2] {
            let image = decode(&path, &Limits::default()).expect("decode");
            let width = 140;
            let rows = crate::render::markdown::anchor_rows(
                width,
                crate::render::markdown::ImageShape::new(512, 512),
                LayoutCellPixels::new(CELL.width, CELL.height),
            );
            let encoded = encode(image, Size::new(width, rows), protocol, CELL, false)
                .unwrap_or_else(|err| panic!("{protocol:?}: {err}"));
            assert_eq!(encoded.size().height, rows, "{protocol:?}");
            let SlicedProtocol::Sliced(row_protocols) = &encoded else {
                panic!("{protocol:?} is not a row stack");
            };
            assert_eq!(row_protocols.len(), usize::from(rows), "{protocol:?}");
        }
    }

    /// A degenerate target or cell never reaches `Resize::resize` with a zero
    /// size (which would panic inside `image`): it is `EncodeFailed`.
    #[test]
    fn a_degenerate_target_is_a_failure_not_a_panic() {
        fn expect_failure(result: Result<SlicedProtocol, Unavailable>, what: &str) {
            match result {
                Err(err) => assert_eq!(err, Unavailable::EncodeFailed, "{what}"),
                Ok(_) => panic!("{what}: expected a failure, got an encoding"),
            }
        }
        let (_dir, path) = fixture("fit-degenerate", 64, 64);
        let image = decode(&path, &Limits::default()).expect("decode");
        expect_failure(
            encode(image, Size::new(0, 4), ImageProtocol::Kitty, CELL, false),
            "a zero-width target",
        );
        let image = decode(&path, &Limits::default()).expect("decode");
        expect_failure(
            encode(image, Size::new(4, 0), ImageProtocol::Kitty, CELL, false),
            "a zero-height target",
        );
        // A degenerate cell cannot be constructed through `ImageSupport`
        // (`from_parts` refuses it), but the encoder type is public enough for a
        // direct call — it must fail rather than divide by zero.
        let image = decode(&path, &Limits::default()).expect("decode");
        expect_failure(
            encode(
                image,
                Size::new(4, 4),
                ImageProtocol::Kitty,
                CellPixels::new(0, 20),
                false,
            ),
            "a zero-width cell",
        );
    }

    /// The port is upstream's arithmetic: `fit_cells` must agree with
    /// `Resize::Fit(None)::size_for` for every input the store can admit, so
    /// swapping the call inside `encode` cannot change an encoded size.
    ///
    /// The two `ceil`s are exact integers here and `f32` there; every pixel
    /// dimension the store can admit is below `2^24` (`Limits::pixels` is
    /// 16 Mpx), which is precisely the range where the `f32` quotient cannot
    /// round across an integer boundary — so agreement is by construction, and
    /// this test is the witness.
    #[test]
    fn fit_cells_agrees_with_upstream_size_for() {
        let resize = Resize::Fit(None);
        let cases: &[(u32, u32, u16, u16, u16, u16)] = &[
            // (px_w, px_h, box cols, box rows, cell w, cell h)
            (512, 512, 140, 36, 10, 20),
            (512, 512, 40, 20, 10, 20),
            (16, 16, 140, 36, 10, 20),
            (800, 600, 80, 30, 10, 20),
            (400, 100, 20, 20, 10, 20),
            (1920, 1080, 128, 36, 10, 20),
            (4000, 4000, 400, 200, 10, 20),
            (1, 5000, 118, 36, 10, 20),
            (8000, 100, 118, 36, 10, 20),
            (100, 100, 10, 10, 1, 1),
            (3000, 2000, 79, 33, 9, 18),
            // The store's pixel ceiling, asked at a 1-pixel cell: the widest
            // quotients the encoder can ever see.
            (16_000_000, 1, 100, 36, 1, 1),
            (8000, 2000, 60, 40, 3, 7),
            // Past `u16::MAX` pixels: the saturation branch, kept for parity.
            (70000, 100, 7000, 36, 10, 20),
            (100, 70000, 7000, 36, 10, 20),
        ];
        for &(px_w, px_h, cols, rows, cell_w, cell_h) in cases {
            let image = DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
                px_w,
                px_h,
                image::Rgb([7, 8, 9]),
            ));
            let font = ratatui_image::FontSize::new(cell_w, cell_h);
            let upstream = resize.size_for(&image, font, Size::new(cols, rows));
            let ours = fit_cells(
                px_w,
                px_h,
                Size::new(cols, rows),
                LayoutCellPixels::new(cell_w, cell_h),
            );
            assert_eq!(
                ours, upstream,
                "{px_w}x{px_h} into {cols}x{rows} at cell {cell_w}x{cell_h}"
            );
        }
    }
}
