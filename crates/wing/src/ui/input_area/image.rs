//! The image chip: what a pasted picture leaves in the draft.
//!
//! An image chip is a chip like any other — one atomic unit inserted at the
//! cursor, stepped over by the arrows, removed whole by Backspace/Delete,
//! never broken by the wrapper (see [`super::model`], which owns all of that
//! and knows nothing about pictures). What is new here is the *payload*: the
//! compressed JPEG the message will carry, plus the label the chip shows.
//!
//! Two contracts live in this file:
//!
//! * the **label text** — `[Image #N W×H SIZE]`, with the compressed picture's
//!   own dimensions and byte count (that is what really leaves the machine, and
//!   it is what tells the user how much of the budget the picture costs);
//! * the **number** — monotonic, never reused, and the chip's identity, exactly
//!   like a paste chip's (clearing the draft starts the count over).
//!
//! The label is computed once, at insertion, and cached next to the payload: it
//! is read on every frame, keystroke and hit test, while the picture behind it
//! never changes.

use std::collections::BTreeMap;

use crate::util::image_compress::CompressedImage;

use super::InputArea;
use super::model;

/// An image chip's number — the `#N` in `[Image #N …]`.
pub(crate) type ImageNumber = usize;

/// One image chip: the picture the message carries, and the label the chip
/// shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePayload {
    /// The compressed picture (mime is always `image/jpeg`) — the bytes the
    /// message is sent with.
    pub image: CompressedImage,
    /// The chip's visible text, computed once at insertion (see [`Images`]).
    /// Display only: it never rides the request.
    pub label: String,
}

impl ImagePayload {
    /// The picture alone — the one conversion point to the sender's
    /// `CompressedImage` (the label stays behind with the frontend's own
    /// pending/history rows).
    pub fn into_image(self) -> CompressedImage {
        self.image
    }
}

/// The images a draft still holds: chip number → its payload and label.
#[derive(Debug, Default)]
pub(crate) struct Images {
    next_number: ImageNumber,
    entries: BTreeMap<ImageNumber, ImagePayload>,
}

impl Images {
    /// Register `image` as a new image chip and return the number that stands
    /// for it.
    ///
    /// Numbers are allocated monotonically and **never reused** within a
    /// draft, and the label is built from the number just handed out — so the
    /// `#N` a user has already read keeps its number even after its neighbours
    /// are deleted. Clearing the draft starts the count over.
    pub fn add(&mut self, image: CompressedImage) -> ImageNumber {
        self.next_number += 1;
        let label = chip_label(self.next_number, &image);
        self.entries
            .insert(self.next_number, ImagePayload { image, label });
        self.next_number
    }

    /// Take the payload of a chip (submit keeps it, a deletion drops it) —
    /// `None` once it is gone.
    pub fn take(&mut self, number: ImageNumber) -> Option<ImagePayload> {
        self.entries.remove(&number)
    }

    /// A chip's visible text (cached — see [`ImagePayload::label`]).
    pub fn label(&self, number: ImageNumber) -> &str {
        self.entries
            .get(&number)
            .map_or(CHIP_WITHOUT_IMAGE, |image| image.label.as_str())
    }

    /// Forget every payload and restart the numbering.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next_number = 0;
    }
}

/// The fallback label of a chip whose payload is gone — a state the model
/// maintains against (a chip and its payload are inserted and dropped
/// together), so it only ever shows up in a defensive read.
const CHIP_WITHOUT_IMAGE: &str = "[Image #?]";

/// The label an image chip shows: `[Image #N W×H SIZE]` (`W×H` the size the
/// JPEG was encoded at, `SIZE` its byte count — see [`chip_size`]).
fn chip_label(number: ImageNumber, image: &CompressedImage) -> String {
    format!(
        "[Image #{number} {}×{} {}]",
        image.width,
        image.height,
        chip_size(image.bytes.len())
    )
}

/// The compact byte notation the label carries: `512B`, `384KB`, `1.0MB`.
///
/// Deliberately not the CLI's general formatter (`util::format`): the label is
/// a fixed-width chip, and the contract is this shape. KB is rounded to the
/// nearest whole one and never rounds *up* into `1024KB` — that reads as
/// `1.0MB`.
fn chip_size(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes < KIB {
        format!("{bytes}B")
    } else if bytes + KIB / 2 < MIB {
        format!("{}KB", (bytes + KIB / 2) / KIB)
    } else {
        format!("{:.1}MB", bytes as f64 / MIB as f64)
    }
}

impl InputArea {
    /// Insert an image chip at the cursor.
    ///
    /// Same contract as a paste chip ([`super::paste::InputArea::insert_paste`]):
    /// the chip is *inline* — whatever follows the cursor stays on the line and
    /// the cursor lands right after the chip — and it is atomic until the
    /// draft is submitted. The number comes back so the caller can talk about
    /// the chip it just added.
    pub fn insert_image(&mut self, image: CompressedImage) -> ImageNumber {
        let number = self.chips.add_image(image);
        let id = model::ChipId::Image(number);
        let width = self.chips.label(id).chars().count();
        model::insert_chip(
            &mut self.lines[self.cursor_row],
            &self.chips,
            self.cursor_col,
            id,
        );
        self.cursor_col += width;
        self.clear_desired_col();
        number
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture of `width × height` whose JPEG is `bytes` long (the bytes
    /// themselves are never read here — only their length).
    fn picture(width: u32, height: u32, bytes: usize) -> CompressedImage {
        CompressedImage {
            bytes: vec![0xFF; bytes],
            width,
            height,
        }
    }

    #[test]
    fn the_label_carries_the_number_the_size_and_the_bytes() {
        assert_eq!(
            chip_label(1, &picture(1024, 768, 393_216)),
            "[Image #1 1024×768 384KB]"
        );
        assert_eq!(
            chip_label(12, &picture(800, 600, 1_048_576)),
            "[Image #12 800×600 1.0MB]"
        );
        assert_eq!(chip_label(3, &picture(8, 8, 7)), "[Image #3 8×8 7B]");
    }

    #[test]
    fn the_byte_notation_covers_its_boundaries() {
        assert_eq!(chip_size(0), "0B");
        assert_eq!(chip_size(7), "7B");
        assert_eq!(chip_size(1023), "1023B");
        assert_eq!(chip_size(1024), "1KB");
        assert_eq!(chip_size(1536), "2KB", "rounded to the nearest KB");
        assert_eq!(chip_size(393_216), "384KB");
        // Just under a mebibyte rounds to 1024KB *only* if that were allowed —
        // the branch hands it to MB instead, so the label never reads `1024KB`.
        assert_eq!(chip_size(1_048_575), "1.0MB");
        assert_eq!(chip_size(1_048_576), "1.0MB");
        assert_eq!(chip_size(1_572_864), "1.5MB");
    }

    #[test]
    fn numbers_are_monotonic_and_never_reused() {
        let mut images = Images::default();
        assert_eq!(images.add(picture(1, 1, 1)), 1);
        assert_eq!(images.add(picture(1, 1, 1)), 2);
        images.take(1);
        assert_eq!(
            images.add(picture(1, 1, 1)),
            3,
            "a freed number is not reused"
        );
        assert_eq!(images.label(1), CHIP_WITHOUT_IMAGE, "a defensive read");
        assert_eq!(images.label(3), "[Image #3 1×1 1B]");
        images.clear();
        assert_eq!(images.add(picture(1, 1, 1)), 1, "clearing starts over");
    }

    #[test]
    fn taking_a_payload_hands_out_the_picture_and_its_label() {
        let mut images = Images::default();
        let number = images.add(picture(4, 2, 2048));
        let payload = images.take(number).expect("the payload");
        assert_eq!(payload.label, "[Image #1 4×2 2KB]");
        assert_eq!(payload.image.width, 4);
        assert_eq!(payload.image.height, 2);
        assert_eq!(payload.into_image().bytes.len(), 2048);
        assert!(images.take(number).is_none(), "taken once");
    }

    #[test]
    fn a_chip_is_inserted_inline_and_the_cursor_follows_it() {
        let mut input = InputArea::new("");
        input.set_text("see this");
        input.cursor_col = 4;
        let number = input.insert_image(picture(64, 64, 4096));
        assert_eq!(number, 1);
        assert_eq!(
            input.text(),
            "see [Image #1 64×64 4KB]this",
            "the chip lands in the middle of the run and the text closes it"
        );
        assert_eq!(
            input.cursor_col,
            4 + "[Image #1 64×64 4KB]".chars().count(),
            "the cursor sits right after the chip"
        );
    }

    #[test]
    fn images_and_text_share_a_draft() {
        let mut input = InputArea::new("");
        input.insert_str("1\n2\n3\n4"); // >2 lines: a paste chip
        input.insert_image(picture(8, 8, 10));
        input.insert_char('!');
        let paste = "[Pasted text #1 +3 lines]";
        assert_eq!(input.text(), format!("{paste}[Image #1 8×8 10B]!"));
    }
}
