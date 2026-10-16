//! The image chip: what a pasted picture leaves in the draft.
//!
//! An image chip is a chip like any other — one atomic unit inserted at the
//! cursor, stepped over by the arrows, removed whole by Backspace/Delete,
//! never broken by the wrapper (see [`super::model`], which owns all of that
//! and knows nothing about pictures). What is new here is the *payload*: the
//! compressed JPEG the message will carry, plus the label the chip shows.
//!
//! A chip is born as a **placeholder** and settles in place. The paste starts
//! long before a picture exists (the bytes crawl in over the terminal channel,
//! then the compressor runs), so the chip appears at once — at the cursor, with
//! the number it will keep — and walks through its stages there:
//!
//! ```text
//! 粘贴      [Image #1 ⠋]             Receiving（字节还没到）
//! 传输中    [Image #1 ⠋ ↓12.4MB]      Receiving（节流后的累计字节数）
//! 压缩中    [Image #1 ⠋ 处理中…]      Compressing
//! 落定      [Image #1 2048×1324 419KB] Ready
//! 失败      [Image #1 ✗]              Pending(Failed) + toast
//! ```
//!
//! Three contracts live in this file:
//!
//! * the **label text** — `[Image #N W×H SIZE]` once settled, with the
//!   compressed picture's own dimensions and byte count (that is what really
//!   leaves the machine, and it is what tells the user how much of the budget
//!   the picture costs); the stage spellings above while in flight. The
//!   settled notation is shared with the message rows ([`crate::util::format`]):
//!   one spelling, two callers;
//! * the **number** — monotonic, never reused, and the chip's identity, exactly
//!   like a paste chip's (clearing the draft starts the count over). It is the
//!   *draft's* number; a message numbers its pictures by position instead
//!   (`crate::ui::chat_view::media`);
//! * the **draft epoch** — bumped whenever the draft is cleared or rewritten.
//!   A paste in flight is settled through an [`ImageChipTarget`] (number +
//!   epoch): the number alone would find a *stranger's* chip if the draft was
//!   cleared in between (the count starts over), and a late result must never
//!   land in a chip the user built after it.
//!
//! The label is cached next to the stage: it is read on every frame, keystroke
//! and hit test, while the picture behind it never changes. A stage change (or
//! a spinner frame) rewrites it — and shifts the cursor when the chip grew to
//! the left of it, see [`InputArea::relabel_image`].

use std::collections::BTreeMap;
use std::time::Duration;

use crate::ui::spinner::SpinnerState;
use crate::util::format;
use crate::util::image_compress::CompressedImage;

use super::InputArea;
use super::model;
use super::model::Segment;

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
    /// `CompressedImage`. The label stays behind with the **composer chip**
    /// (nothing else reads it): a message rebuilds its own label rows from the
    /// pictures it carries (`crate::ui::chat_view::media`).
    pub fn into_image(self) -> CompressedImage {
        self.image
    }
}

/// How far a chip that has no picture yet has come.
///
/// `Failed` is terminal: the bytes never arrived (transfer failed) or they
/// could not be turned into a picture (compression failed) — the chip stays as
/// the user's visible mark of what was lost, and it does not block the send
/// (`Failed` is not "unsettled": there is nothing left to wait for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageStage {
    /// The bytes are still coming in over the terminal channel; `bytes` is the
    /// cumulative count the protocol machine last reported.
    Receiving { bytes: u64 },
    /// The bytes are in hand, the compressor is running.
    Compressing,
    /// The paste failed; there is no picture.
    Failed,
}

/// A handle to a chip that has not settled: its number, and the draft epoch it
/// belongs to (see the module docs).
///
/// The pipeline carries this from the moment the placeholder is inserted to
/// the moment the result lands, so a result that outlived its draft settles
/// nothing (`InputArea` checks the epoch and drops it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageChipTarget {
    /// The draft the placeholder was inserted into (see [`Images::epoch`]).
    pub epoch: u64,
    /// The chip's number within that draft.
    pub number: ImageNumber,
}

/// The images a draft still holds: chip number → its entry.
#[derive(Debug, Default)]
pub(crate) struct Images {
    next_number: ImageNumber,
    /// Bumped by [`Images::clear`] — the identity of "this draft" for pastes
    /// in flight (see [`ImageChipTarget`]).
    epoch: u64,
    /// Drives the braille glyph every in-flight chip shows. One state for the
    /// whole draft: the chips are all "the paste you just did", spinning them
    /// in step is the point.
    spinner: SpinnerState,
    entries: BTreeMap<ImageNumber, Entry>,
}

/// One chip's entry: a picture that is there, or the stage of one that is not
/// yet.
#[derive(Debug)]
enum Entry {
    /// The picture landed — the chip is what the message will carry.
    Ready(ImagePayload),
    /// Still in flight (or failed): a placeholder.
    Pending(PendingImage),
}

#[derive(Debug)]
struct PendingImage {
    stage: ImageStage,
    /// The visible label, recomputed when the stage or the spinner frame
    /// changes (see the module docs).
    label: String,
}

impl Images {
    /// Register `image` as a settled image chip and return the number that
    /// stands for it.
    ///
    /// Numbers are allocated monotonically and **never reused** within a
    /// draft, and the label is built from the number just handed out — so the
    /// `#N` a user has already read keeps its number even after its neighbours
    /// are deleted. Clearing the draft starts the count over.
    pub fn add(&mut self, image: CompressedImage) -> ImageNumber {
        self.next_number += 1;
        let label = chip_label(self.next_number, &image);
        self.entries.insert(
            self.next_number,
            Entry::Ready(ImagePayload { image, label }),
        );
        self.next_number
    }

    /// Register a chip with no picture yet (the placeholder) and return the
    /// number that stands for it — same numbering contract as [`Images::add`].
    pub fn add_pending(&mut self, stage: ImageStage) -> ImageNumber {
        self.next_number += 1;
        let label = pending_label(self.next_number, stage, self.spinner.frame_str());
        self.entries.insert(
            self.next_number,
            Entry::Pending(PendingImage { stage, label }),
        );
        self.next_number
    }

    /// Take the payload of a chip (submit keeps it, a deletion drops it) —
    /// `None` once it is gone or while it is still a placeholder.
    ///
    /// A placeholder's entry goes with it (there is nothing to hand out, but
    /// the chip is off the draft): a result that lands afterwards finds no
    /// chip and is dropped instead of resurrecting a deleted paste.
    pub fn take(&mut self, number: ImageNumber) -> Option<ImagePayload> {
        match self.entries.remove(&number) {
            Some(Entry::Ready(payload)) => Some(payload),
            _ => None,
        }
    }

    /// A chip's visible text (cached — see the module docs).
    pub fn label(&self, number: ImageNumber) -> &str {
        match self.entries.get(&number) {
            Some(Entry::Ready(payload)) => payload.label.as_str(),
            Some(Entry::Pending(pending)) => pending.label.as_str(),
            None => CHIP_WITHOUT_IMAGE,
        }
    }

    /// Move a placeholder to `stage`; `None` if there is no such chip, it
    /// already settled, or the label would not change. The new label comes
    /// back so the caller can keep the cursor where the user left it.
    pub fn set_stage(&mut self, number: ImageNumber, stage: ImageStage) -> Option<String> {
        let spinner = self.spinner.frame_str();
        let Some(Entry::Pending(pending)) = self.entries.get_mut(&number) else {
            return None;
        };
        if pending.stage == stage {
            return None;
        }
        pending.stage = stage;
        pending.label = pending_label(number, stage, spinner);
        Some(pending.label.clone())
    }

    /// Settle a placeholder into a real chip (the picture arrived); `None` if
    /// there is no such pending chip. The new label comes back for the
    /// caller's cursor fix-up.
    pub fn settle(&mut self, number: ImageNumber, image: CompressedImage) -> Option<String> {
        if !matches!(self.entries.get(&number), Some(Entry::Pending(_))) {
            return None;
        }
        let label = chip_label(number, &image);
        self.entries.insert(
            number,
            Entry::Ready(ImagePayload {
                image,
                label: label.clone(),
            }),
        );
        Some(label)
    }

    /// How many chips are waiting for something (transfer or compression).
    ///
    /// A `Failed` chip does not count: it is terminal, nothing is coming.
    pub fn unsettled(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| {
                matches!(
                    entry,
                    Entry::Pending(pending)
                        if !matches!(pending.stage, ImageStage::Failed)
                )
            })
            .count()
    }

    /// Advance the spinner by `dt` and rewrite the in-flight chips' labels;
    /// `true` when a visible glyph changed (the caller repaints).
    ///
    /// A no-op — not even a tick — when nothing is in flight: an idle draft
    /// must not pay a per-tick label rewrite (the system tick runs 10×/s
    /// forever).
    pub fn tick_spinner(&mut self, dt: Duration) -> bool {
        if self.unsettled() == 0 {
            return false;
        }
        let before = self.spinner.frame_str();
        self.spinner.tick(dt);
        if self.spinner.frame_str() == before {
            return false;
        }
        let pending: Vec<(ImageNumber, ImageStage)> = self
            .entries
            .iter()
            .filter_map(|(number, entry)| match entry {
                Entry::Pending(pending) if !matches!(pending.stage, ImageStage::Failed) => {
                    Some((*number, pending.stage))
                }
                _ => None,
            })
            .collect();
        let spinner = self.spinner.frame_str();
        for (number, stage) in pending {
            if let Some(Entry::Pending(pending)) = self.entries.get_mut(&number) {
                pending.label = pending_label(number, stage, spinner);
            }
        }
        true
    }

    /// The draft's epoch (see [`ImageChipTarget`]).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Forget every payload and restart the numbering — a new draft, so every
    /// paste in flight loses its target (the epoch goes with it).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next_number = 0;
        self.epoch += 1;
        self.spinner.reset();
    }
}

/// The fallback label of a chip whose payload is gone — a state the model
/// maintains against (a chip and its payload are inserted and dropped
/// together), so it only ever shows up in a defensive read.
const CHIP_WITHOUT_IMAGE: &str = "[Image #?]";

/// The label a settled image chip shows — the shared notation
/// ([`format::image_label`]) under the draft's own numbers (see [`Images`]).
fn chip_label(number: ImageNumber, image: &CompressedImage) -> String {
    format::image_label(number, image.width, image.height, image.bytes.len() as u64)
}

/// The label an in-flight chip shows: `[Image #N ⠋]` / `[Image #N ⠋ ↓12.4MB]` /
/// `[Image #N ⠋ 处理中…]` / `[Image #N ✗]`.
///
/// The glyph rides the label because that is what the screen shows; the
/// progress figure is what has really arrived (never a guess at a total — the
/// terminal protocol does not announce sizes).
fn pending_label(number: ImageNumber, stage: ImageStage, spinner: &str) -> String {
    match stage {
        ImageStage::Receiving { bytes: 0 } => format!("[Image #{number} {spinner}]"),
        ImageStage::Receiving { bytes } => {
            format!(
                "[Image #{number} {spinner} ↓{}]",
                format::image_bytes(bytes)
            )
        }
        ImageStage::Compressing => format!("[Image #{number} {spinner} 处理中…]"),
        ImageStage::Failed => format!("[Image #{number} ✗]"),
    }
}

impl InputArea {
    /// Insert a settled image chip at the cursor.
    ///
    /// Same contract as a paste chip ([`super::paste::InputArea::insert_paste`]):
    /// the chip is *inline* — whatever follows the cursor stays on the line and
    /// the cursor lands right after the chip — and it is atomic until the
    /// draft is submitted. The number comes back so the caller can talk about
    /// the chip it just added.
    pub fn insert_image(&mut self, image: CompressedImage) -> ImageNumber {
        let number = self.chips.add_image(image);
        self.splice_image_chip(number)
    }

    /// Insert a placeholder at the cursor: the chip a paste shows while its
    /// picture is still on its way.
    ///
    /// The returned handle is what settles it later ([`Self::set_image_stage`],
    /// [`Self::complete_image`], [`Self::fail_image`]) — and the same as
    /// [`Self::insert_image`]'s contract, the chip lands where the cursor is
    /// **now**, which for a paste is the moment the user pasted.
    pub fn insert_pending_image(&mut self, stage: ImageStage) -> ImageChipTarget {
        let number = self.chips.images.add_pending(stage);
        let epoch = self.chips.images.epoch();
        self.splice_image_chip(number);
        ImageChipTarget { epoch, number }
    }

    /// Put the chip on the line and step the cursor past it (the shared tail
    /// of both insert paths — the atom's landing rules are written once).
    fn splice_image_chip(&mut self, number: ImageNumber) -> ImageNumber {
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

    /// Move a placeholder to `stage` (transfer progress, compression start) —
    /// the label is rewritten in place and the cursor kept on the same
    /// character.
    ///
    /// Returns `false` when the handle is stale: the draft was cleared or
    /// rewritten since ([`ImageChipTarget`]'s epoch), or the chip was deleted.
    /// A stale update is a no-op, never an error: the paste simply lost its
    /// audience.
    pub fn set_image_stage(&mut self, target: ImageChipTarget, stage: ImageStage) -> bool {
        if target.epoch != self.chips.images.epoch() {
            return false;
        }
        let before = self.chips.label(model::ChipId::Image(target.number));
        let old_chars = before.chars().count();
        match self.chips.images.set_stage(target.number, stage) {
            Some(label) => {
                self.relabel_image(target.number, old_chars, &label);
                true
            }
            None => false,
        }
    }

    /// Settle a placeholder into the real chip: the picture replaces the stage
    /// text **where the placeholder is** (the number and the position do not
    /// move).
    ///
    /// `false` for a stale handle (see [`Self::set_image_stage`]) — the result
    /// is dropped, not inserted somewhere else.
    pub fn complete_image(&mut self, target: ImageChipTarget, image: CompressedImage) -> bool {
        if target.epoch != self.chips.images.epoch() {
            return false;
        }
        let before = self.chips.label(model::ChipId::Image(target.number));
        let old_chars = before.chars().count();
        match self.chips.images.settle(target.number, image) {
            Some(label) => {
                self.relabel_image(target.number, old_chars, &label);
                true
            }
            None => false,
        }
    }

    /// Flip a placeholder into its failed state (`[Image #N ✗]`) — the chip
    /// stays where it is, as the visible mark of what was lost.
    ///
    /// `false` for a stale handle (see [`Self::set_image_stage`]).
    pub fn fail_image(&mut self, target: ImageChipTarget) -> bool {
        self.set_image_stage(target, ImageStage::Failed)
    }

    /// Advance every in-flight chip's spinner by `dt`; `true` when something
    /// visible changed (the caller schedules a repaint).
    ///
    /// The glyph is one char wide in every frame, so the labels keep their
    /// length — the cursor needs no shifting here (a stage change does, see
    /// [`Self::relabel_image`]).
    pub fn tick_pending_images(&mut self, dt: Duration) -> bool {
        self.chips.images.tick_spinner(dt)
    }

    /// Whether the draft holds an image chip that has not settled yet — the
    /// submit gate (see [`InputAction`](super::InputAction)).
    ///
    /// A *failed* chip does not block: nothing is coming for it, and the toast
    /// that announced the failure is the user's story of what happened.
    pub fn has_unsettled_images(&self) -> bool {
        self.chips.images.unsettled() > 0
    }

    /// Rewrite chip `number`'s label and keep the cursor on the same character.
    ///
    /// `old_chars` is the label's char length **before** the registry was
    /// updated — the caller reads it first, because the registry overwrites the
    /// label in place and no longer remembers the old one.
    ///
    /// The cursor column is a **char index into the flat line**, so a chip
    /// that grows to its left shifts every position behind it — without this
    /// adjustment the user's typing point would crawl with the progress figure
    /// (a rewrite every ~100 ms during a transfer), and a shrinking chip would
    /// leave the cursor *inside* the new label. Only the chip's own width
    /// changed, hence only positions *behind it on its line* move; the sticky
    /// column is dropped rather than re-derived (it is measured in display
    /// cells of a visual row the re-wrap may have just redrawn — the cheap
    /// correct answer is to let the next Up/Down re-measure).
    fn relabel_image(&mut self, number: ImageNumber, old_chars: usize, label: &str) {
        let Some(row) = self.image_row(number) else {
            return;
        };
        let new_chars = label.chars().count() as isize;
        let start: usize = self.lines[row]
            .iter()
            .take_while(|segment| !matches!(segment, Segment::Image(n) if *n == number))
            .map(|segment| self.chips.segment_label(segment).chars().count())
            .sum();
        if self.cursor_row == row && self.cursor_col > start {
            let shifted = self.cursor_col as isize + (new_chars - old_chars as isize);
            self.cursor_col = shifted.max(start as isize) as usize;
            self.clear_desired_col();
        }
    }

    /// The line an image chip lives on (`None` once the chip is gone — a
    /// placeholder can be deleted while its paste is still in flight).
    fn image_row(&self, number: ImageNumber) -> Option<usize> {
        self.lines.iter().position(|line| {
            line.iter()
                .any(|segment| matches!(segment, Segment::Image(n) if *n == number))
        })
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

    /// 标签的**形状**契约（单测钉住用户看到的文案；字节记法的边界在
    /// `util::format`，那是两处共用的那一份）。
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

    /// 在途 chip 的文案（阶段 → 标签，spinner 帧进标签）。
    #[test]
    fn the_placeholder_label_spells_out_the_stage() {
        let label = |stage| pending_label(1, stage, "⠋");
        assert_eq!(label(ImageStage::Receiving { bytes: 0 }), "[Image #1 ⠋]");
        assert_eq!(
            label(ImageStage::Receiving { bytes: 256 * 1024 }),
            "[Image #1 ⠋ ↓256KB]"
        );
        assert_eq!(
            label(ImageStage::Receiving { bytes: 12_582_912 }),
            "[Image #1 ⠋ ↓12.0MB]"
        );
        assert_eq!(label(ImageStage::Compressing), "[Image #1 ⠋ 处理中…]");
        assert_eq!(label(ImageStage::Failed), "[Image #1 ✗]");
        // 每个 spinner 帧都只占一格宽：标签长度不随动画变（光标不用挪）。
        for frame in ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] {
            assert_eq!(
                pending_label(1, ImageStage::Compressing, frame)
                    .chars()
                    .count(),
                "[Image #1 ⠋ 处理中…]".chars().count()
            );
        }
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

    /// 纪元只被 clear 推进：号码可以重来，**在途句柄不会**因此指到新草稿的同号 chip 上。
    #[test]
    fn the_epoch_moves_only_when_the_draft_does() {
        let mut images = Images::default();
        assert_eq!(images.epoch(), 0);
        let first = images.add_pending(ImageStage::Compressing);
        assert_eq!(images.epoch(), 0, "插入不是一次草稿更替");
        assert_eq!(first, 1);
        images.clear();
        assert_eq!(images.epoch(), 1);
        assert_eq!(images.add_pending(ImageStage::Compressing), 1, "号码重来");
    }

    /// 占位符的就地翻面：settle 前后号码、位置、光标都不动，标签换成正式文案。
    #[test]
    fn a_placeholder_settles_in_place() {
        let mut input = InputArea::new("");
        input.set_text("see ");
        input.cursor_col = 4;
        let target = input.insert_pending_image(ImageStage::Receiving { bytes: 0 });
        assert_eq!(target.number, 1);
        assert_eq!(input.text(), "see [Image #1 ⠋]");
        input.insert_str("after");

        assert!(input.set_image_stage(target, ImageStage::Receiving { bytes: 12_582_912 }));
        assert_eq!(input.text(), "see [Image #1 ⠋ ↓12.0MB]after");
        assert_eq!(
            input.cursor_col,
            "see [Image #1 ⠋ ↓12.0MB]after".chars().count(),
            "光标还在行尾（chip 变长，位置被平移）"
        );

        assert!(input.complete_image(target, picture(2048, 1324, 428_032)));
        assert_eq!(input.text(), "see [Image #1 2048×1324 418KB]after");
        assert_eq!(
            input.cursor_col,
            "see [Image #1 2048×1324 418KB]after".chars().count(),
            "落定也不移动光标"
        );
        assert!(!input.has_unsettled_images(), "落定不再是门禁");
    }

    /// 失败态：标签就位、门禁放开（图已经没了，没什么可等的）。
    #[test]
    fn a_failed_placeholder_reads_as_lost_and_does_not_gate() {
        let mut input = InputArea::new("");
        let target = input.insert_pending_image(ImageStage::Compressing);
        assert!(input.has_unsettled_images());
        assert!(input.fail_image(target));
        assert_eq!(input.text(), "[Image #1 ✗]");
        assert!(!input.has_unsettled_images(), "失败不拦发送");
        assert!(!input.fail_image(target), "失败态不重复改写");
    }

    /// 陈旧句柄（草稿被清空 / 重写）不落地：号码重来之后同号 chip 是**别人**的。
    #[test]
    fn a_stale_handle_settles_nothing() {
        let mut input = InputArea::new("");
        let stale = input.insert_pending_image(ImageStage::Compressing);
        input.clear();
        let fresh = input.insert_pending_image(ImageStage::Compressing);
        assert_eq!(fresh.number, stale.number, "号码确实重来了");
        assert_ne!(fresh.epoch, stale.epoch);
        assert!(
            !input.complete_image(stale, picture(4, 4, 10)),
            "老回执不许碰新 chip"
        );
        assert!(!input.fail_image(stale));
        assert!(!input.set_image_stage(stale, ImageStage::Failed));
        assert_eq!(
            input.text(),
            "[Image #1 ⠋ 处理中…]",
            "新 chip 还是它自己的样子"
        );
        assert!(input.complete_image(fresh, picture(4, 4, 10)));
        assert_eq!(input.text(), "[Image #1 4×4 10B]");
    }

    /// chip 被用户删掉之后回执落地：什么都不发生（不重新插入、不 panic）。
    #[test]
    fn a_result_for_a_deleted_chip_is_dropped() {
        let mut input = InputArea::new("");
        let target = input.insert_pending_image(ImageStage::Compressing);
        input.backspace();
        assert_eq!(input.text(), "");
        assert!(!input.complete_image(target, picture(4, 4, 10)));
        assert!(!input.fail_image(target));
        assert_eq!(input.text(), "");
    }

    /// spinner 只推进在途的 chip，且没有在途时一次活都不干。
    #[test]
    fn the_spinner_advances_only_while_something_is_in_flight() {
        let mut input = InputArea::new("");
        assert!(!input.tick_pending_images(Duration::from_millis(100)));
        let target = input.insert_pending_image(ImageStage::Compressing);
        assert!(input.tick_pending_images(Duration::from_millis(100)));
        let shown = input.text();
        assert!(shown.starts_with("[Image #1 ⠙"), "{shown}");
        input.complete_image(target, picture(4, 4, 10));
        assert!(!input.tick_pending_images(Duration::from_millis(100)));
        assert_eq!(input.text(), "[Image #1 4×4 10B]");
        // 失败态也不转。
        let failed = input.insert_pending_image(ImageStage::Compressing);
        input.fail_image(failed);
        assert!(!input.tick_pending_images(Duration::from_millis(500)));
        assert!(input.text().ends_with("[Image #2 ✗]"), "{}", input.text());
    }

    /// 光标在 chip **之前**时不受标签变化影响（只平移它后面的位置）。
    #[test]
    fn a_growing_chip_moves_only_the_positions_behind_it() {
        let mut input = InputArea::new("");
        input.set_text("ab");
        let target = input.insert_pending_image(ImageStage::Receiving { bytes: 0 });
        input.insert_str("cd");
        input.move_home();
        input.cursor_col = 1;
        assert!(input.set_image_stage(target, ImageStage::Receiving { bytes: 1_048_576 }));
        assert_eq!(input.cursor_col, 1, "chip 之前的列不动");
        assert_eq!(input.text(), "ab[Image #1 ⠋ ↓1.0MB]cd");
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

    /// 占位符**没有**载荷可取；它的条目随 chip 一起走（删掉之后回执找不到落点）。
    #[test]
    fn a_placeholder_has_no_payload_to_take() {
        let mut images = Images::default();
        let number = images.add_pending(ImageStage::Compressing);
        assert!(images.take(number).is_none());
        assert_eq!(images.label(number), CHIP_WITHOUT_IMAGE, "条目也走了");
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
