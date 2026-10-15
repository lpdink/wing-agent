//! The image chip as a **consumer** outside the crate sees it (07's hand-off).
//!
//! Everything here goes through the public API only, in the shape the next step
//! will use it: the chip's label (the pending/history rows carry it), the
//! picture itself, and the two halves of the submit product. An item that
//! should have been `pub` but was left `pub(crate)` fails this build instead of
//! the next step's.
//!
//! Headless by construction: no terminal, no TUI loop — the composer is a state
//! machine, and drawing it is the app's business.

use wing::ui::input_area::ImagePayload;
use wing::ui::input_area::InputArea;
use wing::ui::input_area::SubmitPayload;
use wing::util::image_compress::CompressedImage;

/// A picture of `bytes` bytes at 4 × 2 (the label's numbers are what matter).
fn picture(bytes: usize) -> CompressedImage {
    CompressedImage {
        bytes: vec![0xFF; bytes],
        width: 4,
        height: 2,
    }
}

/// Every item step 07 may name through `wing::ui::input_area::…` / `util`.
#[test]
fn the_image_chip_surface_is_reachable_through_the_public_api() {
    let mut input = InputArea::new("");

    // Insertion hands the chip's number back (the caller's bookkeeping).
    let number: usize = input.insert_image(picture(4096));
    assert_eq!(number, 1);
    assert_eq!(
        input.text(),
        "[Image #1 4×2 4KB]",
        "the draft reads as the label's contract text"
    );
    assert_eq!(input.lines(), vec!["[Image #1 4×2 4KB]"]);

    // A second chip accumulates, and text still splices around them.
    assert_eq!(input.insert_image(picture(10)), 2);
    input.insert_str("note");
    assert_eq!(input.text(), "[Image #1 4×2 4KB][Image #2 4×2 10B]note");

    // The submit product is nameable and its two halves are reachable — the
    // label is what a pending/history row needs, the picture what the sender
    // needs (and the one conversion point between them).
    let payload = SubmitPayload {
        text: "note".to_string(),
        images: vec![ImagePayload {
            image: picture(2048),
            label: "[Image #1 4×2 2KB]".to_string(),
        }],
    };
    assert_eq!(payload.images[0].label, "[Image #1 4×2 2KB]");
    let image: CompressedImage = payload.images[0].clone().into_image();
    assert_eq!(image.bytes.len(), 2048);
    assert_eq!((image.width, image.height), (4, 2));
}

/// The label's numbers come from the **compressed** picture (that is what
/// leaves the machine), and the notation covers its three ranges — 07's
/// pending/history rows must render the very same strings.
#[test]
fn the_label_reports_the_compressed_size_and_shape() {
    let mut input = InputArea::new("");
    for bytes in [7usize, 1024, 393_216, 1_048_576] {
        input.insert_image(picture(bytes));
    }
    assert_eq!(
        input.text(),
        "[Image #1 4×2 7B][Image #2 4×2 1KB][Image #3 4×2 384KB][Image #4 4×2 1.0MB]"
    );
}
