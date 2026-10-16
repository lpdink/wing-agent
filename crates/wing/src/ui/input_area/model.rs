//! The composer draft's content model.
//!
//! A line is a sequence of [`Segment`]s: literal text the user typed (or a
//! small paste spliced in place), and *chips* — the `[Pasted text #N +M lines]`
//! placeholder a large paste leaves behind, or the `[Image #N W×H SIZE]` one a
//! pasted picture leaves. A chip's payload lives in the registry that owns it
//! ([`Pastes`] for text, [`super::image::Images`] for pictures); the draft's
//! [`Chips`] holds both.
//!
//! A chip is **one unit**, not text that happens to look special: the cursor
//! never lands inside it (←/→ step over it, Backspace/Delete remove it whole),
//! the renderer never breaks a line inside it, and it is expanded once — on
//! submit — so the draft keeps showing the chip while the agent receives the
//! text. Nothing is ever re-parsed from the chip's own text: the segments are
//! the authority, so a draft restored from plain text (no registry entry) can
//! never grow a live chip out of a look-alike string.
//!
//! **Both kinds are the same unit to this module**: the operations below walk a
//! line's segments and ask nothing about what a chip stands for, except for its
//! label ([`Chips::label`]) and its payload (on submit: [`Chips::take_images`]).
//! An image chip therefore steps, wraps, deletes and hit-tests exactly like a
//! paste chip — there is no second code path to drift.
//!
//! Everything else in the composer speaks *flat char indices* — cursor
//! columns, visual rows, pointer columns: [`flat`] projects a line the way the
//! screen shows it, and a chip's span is exactly its text's range in it.

use std::collections::BTreeMap;
use std::ops::Range;

use super::helpers::char_to_byte;
use super::image::ImageNumber;
use super::image::ImagePayload;
use super::image::Images;
use super::wrap;
use crate::util::image_compress::CompressedImage;

/// A paste chip's number — the `#N` in `[Pasted text #N …]`.
pub(crate) type PasteNumber = usize;

/// One piece of a draft line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Literal text: typed, or a small paste spliced in place.
    Text(String),
    /// A chip standing for the paste registered under this number.
    Paste(PasteNumber),
    /// A chip standing for the image registered under this number.
    Image(ImageNumber),
}

/// A chip's identity: which registry its payload lives in, and its number.
///
/// The atomic operations below carry this and nothing else, so "a chip is one
/// unit" is stated once for both kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChipId {
    Paste(PasteNumber),
    Image(ImageNumber),
}

impl ChipId {
    /// The segment that stands for this chip.
    fn segment(self) -> Segment {
        match self {
            Self::Paste(number) => Segment::Paste(number),
            Self::Image(number) => Segment::Image(number),
        }
    }
}

impl Segment {
    /// The chip this segment is, if any — the one place a segment becomes a
    /// chip identity.
    fn chip_id(&self) -> Option<ChipId> {
        match self {
            Self::Text(_) => None,
            Self::Paste(number) => Some(ChipId::Paste(*number)),
            Self::Image(number) => Some(ChipId::Image(*number)),
        }
    }
}

/// One draft line.
pub(crate) type Line = Vec<Segment>;

/// One pending paste: the text behind a chip, and the label the chip shows.
#[derive(Debug)]
struct Paste {
    /// The text the chip expands to on submit.
    text: String,
    /// The chip's visible text, computed once at insertion.
    ///
    /// The payload never changes, while the label is read on every frame,
    /// keystroke and hit test (`flat` / `segment_len` / `runs`) — scanning a
    /// multi-megabyte paste per read would make the composer slower the more
    /// there is to hide, which is the one thing a chip is for.
    label: String,
}

/// The pastes a draft still holds: chip number → its text and label.
#[derive(Debug, Default)]
pub(crate) struct Pastes {
    next_number: PasteNumber,
    entries: BTreeMap<PasteNumber, Paste>,
}

impl Pastes {
    /// Register `text` as a new paste and return the chip number that stands
    /// for it.
    ///
    /// Numbers are allocated monotonically and **never reused** within a
    /// draft: the number is the chip's identity, and one that the user has
    /// already read keeps it even after its neighbours are deleted. Clearing
    /// the draft starts the count over.
    pub fn add(&mut self, text: String) -> PasteNumber {
        self.next_number += 1;
        let label = chip_label(self.next_number, &text);
        self.entries.insert(self.next_number, Paste { text, label });
        self.next_number
    }

    /// Drop the payload of a chip that is gone from the draft.
    pub fn remove(&mut self, number: PasteNumber) {
        self.entries.remove(&number);
    }

    /// The text a chip expands to on submit.
    pub fn payload(&self, number: PasteNumber) -> Option<&str> {
        self.entries.get(&number).map(|paste| paste.text.as_str())
    }

    /// Forget every payload and restart the numbering.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next_number = 0;
    }

    /// A chip's visible text (cached — see [`Paste::label`]).
    pub fn chip(&self, number: PasteNumber) -> &str {
        self.entries
            .get(&number)
            .map_or(CHIP_WITHOUT_PASTE, |paste| paste.label.as_str())
    }
}

/// The fallback label of a chip whose payload is gone — a state the model
/// maintains against (a chip and its payload are inserted and dropped
/// together), so it only ever shows up in a defensive read.
const CHIP_WITHOUT_PASTE: &str = "[Pasted text #?]";

/// The label a chip shows: `[Pasted text #N +M lines]`, `M` being the lines
/// the paste adds.
///
/// Single-line pastes carry no count: `+0 lines` would be a count of nothing —
/// the chip still stands for a large paste (that is why it is one), it just
/// does not span lines.
fn chip_label(number: PasteNumber, text: &str) -> String {
    let extra = text.split('\n').count().saturating_sub(1);
    if extra == 0 {
        format!("[Pasted text #{number}]")
    } else {
        format!("[Pasted text #{number} +{extra} lines]")
    }
}

/// A line holding one run of literal text (`""` for an empty line).
pub fn text_line(text: &str) -> Line {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Segment::Text(text.to_string())]
    }
}

/// The chips a draft holds: one registry per kind, side by side.
///
/// Two independent registries rather than one keyed by payload type — a paste
/// carries text and a picture carries bytes, and neither has business in the
/// other's map. This type is only the *lookup* both kinds share (a label, a
/// payload) plus the two counters; the model's atomic operations go through it
/// and never care which side answers.
///
/// The registries stay reachable one level down (`pub(super)`) because a chip
/// kind's own operations — an image's stages, its spinner, its draft epoch —
/// belong to [`super::image`], which is also where they are implemented: this
/// module stays the kind-agnostic core.
#[derive(Debug, Default)]
pub(crate) struct Chips {
    pastes: Pastes,
    pub(super) images: Images,
}

impl Chips {
    /// Register `text` as a new paste chip and return the number that stands
    /// for it (monotonic, never reused — see [`Pastes::add`]).
    pub fn add_paste(&mut self, text: String) -> PasteNumber {
        self.pastes.add(text)
    }

    /// Register `image` as a new image chip and return its number (same
    /// contract — see [`Images::add`]).
    pub fn add_image(&mut self, image: CompressedImage) -> ImageNumber {
        self.images.add(image)
    }

    /// A chip's visible text.
    pub fn label(&self, id: ChipId) -> &str {
        match id {
            ChipId::Paste(number) => self.pastes.chip(number),
            ChipId::Image(number) => self.images.label(number),
        }
    }

    /// What one line segment reads as: its text, or its chip's label.
    pub fn segment_label<'a>(&'a self, segment: &'a Segment) -> &'a str {
        match segment {
            Segment::Text(text) => text,
            Segment::Paste(number) => self.pastes.chip(*number),
            Segment::Image(number) => self.images.label(*number),
        }
    }

    /// The text a paste chip expands to on submit.
    pub fn paste_payload(&self, number: PasteNumber) -> Option<&str> {
        self.pastes.payload(number)
    }

    /// Drop the payload of a chip that is gone from the draft.
    pub fn remove(&mut self, id: ChipId) {
        match id {
            ChipId::Paste(number) => self.pastes.remove(number),
            ChipId::Image(number) => {
                self.images.take(number);
            }
        }
    }

    /// Take the image payloads of a draft, in appearance order (line by line).
    ///
    /// Submit consumes them: the draft is cleared right after, so the bytes
    /// move out instead of being cloned. A number appearing twice (it cannot —
    /// a chip is inserted once — but the take makes it moot) yields one item.
    pub fn take_images(&mut self, lines: &[Line]) -> Vec<ImagePayload> {
        let mut images = Vec::new();
        for segment in lines.iter().flat_map(|line| line.iter()) {
            if let Some(ChipId::Image(number)) = segment.chip_id()
                && let Some(payload) = self.images.take(number)
            {
                images.push(payload);
            }
        }
        images
    }

    /// Forget every payload and restart both counts.
    pub fn clear(&mut self) {
        self.pastes.clear();
        self.images.clear();
    }
}

/// The line as the screen shows it — chips as their visible chip text.
pub fn flat(line: &[Segment], chips: &Chips) -> String {
    let mut out = String::new();
    for segment in line {
        out.push_str(chips.segment_label(segment));
    }
    out
}

/// The char length of [`flat`].
pub fn flat_len(line: &[Segment], chips: &Chips) -> usize {
    line.iter().map(|segment| segment_len(segment, chips)).sum()
}

/// The line as it is submitted — paste chips expanded to their payloads, image
/// chips contributing nothing.
///
/// A picture has no text form: it rides the message as its own field (see
/// `SubmitPayload`), so the expanded text is exactly what the user typed plus
/// what the pastes stood for.
pub fn expanded(line: &[Segment], chips: &Chips) -> String {
    let mut out = String::new();
    for segment in line {
        match segment {
            Segment::Text(text) => out.push_str(text),
            Segment::Paste(number) => match chips.paste_payload(*number) {
                Some(text) => out.push_str(text),
                // A payload-less chip cannot happen; the chip itself is a
                // better answer than dropping the segment silently.
                None => out.push_str(chips.label(ChipId::Paste(*number))),
            },
            // A picture has no text form (see the function's doc).
            Segment::Image(_) => {}
        }
    }
    out
}

/// Char ranges of the line's chips in the flat projection, with their
/// identities.
///
/// A chip is never empty (its label has a fixed prefix), so a span is always
/// a real range — the wrap loop below steps over it by `span.end > start`.
pub fn chip_spans(line: &[Segment], chips: &Chips) -> Vec<(Range<usize>, ChipId)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for segment in line {
        let len = segment_len(segment, chips);
        if let Some(id) = segment.chip_id()
            && len > 0
        {
            spans.push((start..start + len, id));
        }
        start += len;
    }
    spans
}

/// Whether `col` falls strictly inside a chip — a position the model never
/// lets the cursor reach (the callers snap out of it; the edits below treat it
/// as the chip's leading edge so a stray one cannot corrupt the line).
pub fn inside_chip(line: &[Segment], chips: &Chips, col: usize) -> bool {
    chip_spans(line, chips)
        .iter()
        .any(|(span, _)| span.start < col && col < span.end)
}

/// Where a display column inside one visual row resolves to in the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnPoint {
    /// The insertion point — never inside a chip: a column landing on a chip
    /// resolves to the chip's leading edge. A chip is one unit and a char
    /// index has no sub-unit precision, the same rule a wide character
    /// already follows.
    pub point: usize,
    /// The drag endpoint: the unit under the column is included whole, so a
    /// chip's trailing edge.
    pub focus: usize,
    /// Whether the column rests on a character or a chip at all, as opposed
    /// to the blank past the row's text.
    pub on_char: bool,
}

/// Resolve a display column inside `row` (of `line`) to the draft.
///
/// `visible_cols` is how many display columns the row was really drawn into
/// (the text area's width). A chip wider than that is clipped, and the columns
/// past the clip are the blank past the row's text — exactly as on any other
/// row: they resolve to the row's end (never inside the over-wide chip, whose
/// hidden half is not a place the cursor may go), and they do not count as
/// being on the chip.
pub fn column_point(
    line: &[Segment],
    chips: &Chips,
    row: &wrap::VisualRow,
    display_col: usize,
    visible_cols: usize,
) -> ColumnPoint {
    let flat = flat(line, chips);
    let col = if display_col >= visible_cols {
        row.char_end
    } else {
        wrap::display_col_to_char(&flat, row, display_col)
    };
    for (span, _) in chip_spans(line, chips) {
        let start = wrap::char_display_offset(&flat, row, span.start);
        let end = wrap::char_display_offset(&flat, row, span.end).min(visible_cols);
        if (start..end).contains(&display_col) {
            // The whole chip is the unit under the pointer — a chip never
            // wraps, so both edges live in this very row.
            return ColumnPoint {
                point: span.start,
                focus: span.end,
                on_char: true,
            };
        }
    }
    let on_char = col < row.char_end;
    ColumnPoint {
        point: col,
        focus: if on_char { col + 1 } else { col },
        on_char,
    }
}

/// Split a flat char range of a line into runs — `(text, is_chip)` — so a
/// renderer can style the chips without styling the text around them.
pub fn runs(line: &[Segment], chips: &Chips, range: Range<usize>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    for segment in line {
        let end = start + segment_len(segment, chips);
        let from = range.start.max(start);
        let to = range.end.min(end);
        if from < to {
            let slice = |text: &str| -> String {
                text.chars().skip(from - start).take(to - from).collect()
            };
            let run = match segment {
                Segment::Text(text) => (slice(text), false),
                chip => (slice(chips.segment_label(chip)), true),
            };
            out.push(run);
        }
        start = end;
    }
    out
}

/// Splice plain text into the line at flat char column `col`.
///
/// A text run is split open; a chip (or the line's end) gets a segment of its
/// own, *before* the chip — a chip is never entered from either side.
pub fn insert_text(line: &mut Line, chips: &Chips, col: usize, text: &str) {
    let spot = spot_at(line, chips, col);
    match line.get_mut(spot.segment) {
        Some(Segment::Text(existing)) => {
            let byte = char_to_byte(existing, spot.offset);
            existing.insert_str(byte, text);
        }
        _ => line.insert(spot.segment, Segment::Text(text.to_string())),
    }
    coalesce(line);
}

/// Splice the chip `id` into the line at flat char column `col`.
pub fn insert_chip(line: &mut Line, chips: &Chips, col: usize, id: ChipId) {
    let spot = spot_at(line, chips, col);
    match line.get_mut(spot.segment) {
        // Inside a text run: open it so the chip lands in the middle of it.
        Some(Segment::Text(existing)) if spot.offset > 0 => {
            let byte = char_to_byte(existing, spot.offset);
            let tail = existing.split_off(byte);
            line.insert(spot.segment + 1, id.segment());
            line.insert(spot.segment + 2, Segment::Text(tail));
        }
        // Before a chip, or at the line's end: a segment of its own. (A
        // column *inside* a chip also lands here — the chip keeps its place
        // and the new one goes before it.)
        _ => line.insert(spot.segment, id.segment()),
    }
    coalesce(line);
}

/// Split the line at flat char column `col`; `col` starts the returned tail.
pub fn split_off(line: &mut Line, chips: &Chips, col: usize) -> Line {
    let spot = spot_at(line, chips, col);
    let mut tail = line.split_off(spot.segment);
    if spot.offset > 0 {
        // Only a text run has an interior to split — a chip stays whole.
        if let Some(Segment::Text(text)) = tail.first_mut() {
            let byte = char_to_byte(text, spot.offset);
            let head = text[..byte].to_string();
            *text = text[byte..].to_string();
            line.push(Segment::Text(head));
        }
    }
    coalesce(line);
    coalesce(&mut tail);
    tail
}

/// Delete the unit before `col` — one character, or a whole chip (a chip is
/// atomic). Returns how many chars the line lost, so the caller can move the
/// cursor; nothing happens at column 0 (that is the line's start, where the
/// caller merges lines instead).
pub fn remove_unit_before(line: &mut Line, chips: &mut Chips, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    let spot = spot_at(line, chips, col);
    if spot.offset == 0 {
        // A segment boundary: the unit before it is the previous segment's
        // last one.
        let removed = remove_last_unit(line, chips, spot.segment);
        coalesce(line);
        return removed;
    }
    match line.get(spot.segment).and_then(Segment::chip_id) {
        Some(id) => take_chip(line, chips, spot.segment, id),
        None => {
            let removed = match line.get_mut(spot.segment) {
                Some(Segment::Text(text)) => remove_char_before(text, spot.offset),
                _ => 0,
            };
            coalesce(line);
            removed
        }
    }
}

/// Delete the unit at `col` — one character, or a whole chip. Returns how many
/// chars the line lost; nothing happens past the line's end (the caller merges
/// lines there).
pub fn remove_unit_at(line: &mut Line, chips: &mut Chips, col: usize) -> usize {
    let spot = spot_at(line, chips, col);
    match line.get(spot.segment).and_then(Segment::chip_id) {
        Some(id) => take_chip(line, chips, spot.segment, id),
        None => {
            let removed = match line.get_mut(spot.segment) {
                Some(Segment::Text(text)) => remove_char_at(text, spot.offset),
                _ => 0,
            };
            coalesce(line);
            removed
        }
    }
}

/// The column ← lands on from `col`: the start of the chip that ends there
/// (a chip is one unit to step over), or one char left.
pub fn unit_start_before(line: &[Segment], chips: &Chips, col: usize) -> usize {
    chip_spans(line, chips)
        .into_iter()
        .find(|(span, _)| span.end == col)
        .map_or_else(|| col.saturating_sub(1), |(span, _)| span.start)
}

/// The column → lands on from `col`: the end of the chip that starts there, or
/// one char right.
pub fn unit_end_at(line: &[Segment], chips: &Chips, col: usize) -> usize {
    chip_spans(line, chips)
        .into_iter()
        .find(|(span, _)| span.start == col)
        .map_or(col + 1, |(span, _)| span.end)
}

/// Merge neighbouring text runs, dropping the empty ones — the invariant that
/// keeps a line's segments readable after an edit.
pub fn coalesce(line: &mut Line) {
    let mut merged: Line = Vec::with_capacity(line.len());
    for segment in line.drain(..) {
        match (&segment, merged.last_mut()) {
            (Segment::Text(text), Some(Segment::Text(previous))) => previous.push_str(text),
            (Segment::Text(text), _) if text.is_empty() => {}
            _ => merged.push(segment),
        }
    }
    *line = merged;
}

// ── Internals ───────────────────────────────────────────────────

fn segment_len(segment: &Segment, chips: &Chips) -> usize {
    chips.segment_label(segment).chars().count()
}

/// A flat char column as a segment + a char offset into it.
///
/// A boundary resolves to the **later** segment, so a position at a chip's
/// trailing edge is *after* the chip, and one at its leading edge is *before*
/// it — never inside.
struct Spot {
    segment: usize,
    offset: usize,
}

fn spot_at(line: &[Segment], chips: &Chips, col: usize) -> Spot {
    let mut remaining = col;
    for (index, segment) in line.iter().enumerate() {
        let len = segment_len(segment, chips);
        if remaining < len {
            return Spot {
                segment: index,
                offset: remaining,
            };
        }
        remaining -= len;
    }
    Spot {
        segment: line.len(),
        offset: 0,
    }
}

/// Remove the chip at `index` (and its payload) — however many chars it took.
fn take_chip(line: &mut Line, chips: &mut Chips, index: usize, id: ChipId) -> usize {
    let len = chips.label(id).chars().count();
    line.remove(index);
    chips.remove(id);
    coalesce(line);
    len
}

/// Remove the last unit of the segment before `boundary` (a chip whole).
fn remove_last_unit(line: &mut Line, chips: &mut Chips, boundary: usize) -> usize {
    let Some(index) = boundary.checked_sub(1) else {
        return 0;
    };
    match line.get(index).and_then(Segment::chip_id) {
        Some(id) => take_chip(line, chips, index, id),
        None => match line.get_mut(index) {
            Some(Segment::Text(text)) => pop_char(text),
            _ => 0,
        },
    }
}

/// Remove the char at char `index`; returns 1, or 0 if there is none.
fn remove_char_at(text: &mut String, index: usize) -> usize {
    let from = char_to_byte(text, index);
    let to = char_to_byte(text, index + 1);
    if from == to {
        return 0;
    }
    text.replace_range(from..to, "");
    1
}

/// Remove the char before char `index`; returns 1, or 0 if there is none.
fn remove_char_before(text: &mut String, index: usize) -> usize {
    if index == 0 {
        return 0;
    }
    remove_char_at(text, index - 1)
}

/// Remove the last char; returns 1, or 0 if the text was empty.
fn pop_char(text: &mut String) -> usize {
    match text.char_indices().next_back() {
        Some((index, _)) => {
            text.truncate(index);
            1
        }
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bundle holding one paste per payload — the numbers the tests read
    /// back are the ones `add` hands out.
    fn registry(payloads: &[&str]) -> Chips {
        let mut chips = Chips::default();
        for payload in payloads {
            chips.add_paste((*payload).to_string());
        }
        chips
    }

    /// The label paste chip `number` shows (most tests only care how long it
    /// is).
    fn chip(chips: &Chips, number: PasteNumber) -> &str {
        chips.label(ChipId::Paste(number))
    }

    /// A picture of `bytes` bytes, 4 × 2 — the label's numbers are what these
    /// tests read.
    fn picture(bytes: usize) -> CompressedImage {
        CompressedImage {
            bytes: vec![0; bytes],
            width: 4,
            height: 2,
        }
    }

    fn draft_line(text: &str) -> Line {
        text_line(text)
    }

    #[test]
    fn numbers_are_monotonic_and_never_reused() {
        let mut pastes = Pastes::default();
        assert_eq!(pastes.add("a".into()), 1);
        assert_eq!(pastes.add("b".into()), 2);
        pastes.remove(1);
        assert_eq!(pastes.add("c".into()), 3, "a freed number is not reused");
        assert_eq!(pastes.payload(1), None);
        assert_eq!(pastes.payload(3), Some("c"));
        pastes.clear();
        assert_eq!(pastes.add("d".into()), 1, "clearing starts the count over");
    }

    #[test]
    fn a_chip_without_a_payload_reads_as_unknown() {
        let mut chips = registry(&["a\nb"]);
        assert_eq!(chip(&chips, 1), "[Pasted text #1 +1 lines]");
        chips.remove(ChipId::Paste(1));
        assert_eq!(chip(&chips, 1), "[Pasted text #?]", "a defensive read");
    }

    #[test]
    fn chip_text_carries_the_line_count_it_adds() {
        let chips = registry(&["one line", "a\nb\nc"]);
        assert_eq!(chip(&chips, 1), "[Pasted text #1]");
        assert_eq!(chip(&chips, 2), "[Pasted text #2 +2 lines]");
    }

    #[test]
    fn flat_projects_the_chip_and_len_agrees() {
        let chips = registry(&["a\nb"]);
        let line: Line = vec![Segment::Text("see ".into()), Segment::Paste(1)];
        assert_eq!(flat(&line, &chips), "see [Pasted text #1 +1 lines]");
        assert_eq!(flat_len(&line, &chips), flat(&line, &chips).chars().count());
    }

    #[test]
    fn expanded_swaps_the_chip_for_its_payload() {
        let chips = registry(&["a\nb"]);
        let line: Line = vec![
            Segment::Text("see ".into()),
            Segment::Paste(1),
            Segment::Text(" end".into()),
        ];
        assert_eq!(expanded(&line, &chips), "see a\nb end");
    }

    #[test]
    fn chip_spans_report_flat_ranges() {
        let chips = registry(&["a\nb"]);
        let line: Line = vec![
            Segment::Text("ab".into()),
            Segment::Paste(1),
            Segment::Text("cd".into()),
        ];
        let spans = chip_spans(&line, &chips);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1, ChipId::Paste(1));
        assert_eq!(spans[0].0.start, 2);
        assert_eq!(spans[0].0.end, 2 + chip(&chips, 1).chars().count());
    }

    #[test]
    fn insert_text_splits_a_run_and_lands_before_a_chip() {
        let chips = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Text("abcd".into()), Segment::Paste(1)];
        insert_text(&mut line, &chips, 2, "X");
        assert_eq!(line, vec![Segment::Text("abXcd".into()), Segment::Paste(1)]);
        // At the chip's leading edge the text opens a run of its own — the
        // chip keeps its place, the cursor cannot enter it — and the splice
        // coalesces with the run before it.
        insert_text(&mut line, &chips, 5, "Y");
        assert_eq!(
            line,
            vec![Segment::Text("abXcdY".into()), Segment::Paste(1)]
        );
    }

    #[test]
    fn insert_chip_opens_a_run_in_the_middle() {
        let chips = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Text("abcd".into())];
        insert_chip(&mut line, &chips, 2, ChipId::Paste(1));
        assert_eq!(
            line,
            vec![
                Segment::Text("ab".into()),
                Segment::Paste(1),
                Segment::Text("cd".into())
            ]
        );
        // At either edge it simply joins the neighbours, and a column inside
        // an existing chip never splits it.
        insert_chip(&mut line, &chips, 0, ChipId::Paste(1));
        insert_chip(&mut line, &chips, 0, ChipId::Paste(1));
        assert_eq!(
            line,
            vec![
                Segment::Paste(1),
                Segment::Paste(1),
                Segment::Text("ab".into()),
                Segment::Paste(1),
                Segment::Text("cd".into())
            ]
        );
    }

    #[test]
    fn insert_text_appends_at_the_end() {
        let chips = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Paste(1)];
        insert_text(&mut line, &chips, chip(&chips, 1).chars().count(), " tail");
        assert_eq!(line, vec![Segment::Paste(1), Segment::Text(" tail".into())]);
        // An empty line takes the text directly.
        let mut empty = draft_line("");
        insert_text(&mut empty, &chips, 0, "hi");
        assert_eq!(flat(&empty, &chips), "hi");
    }

    #[test]
    fn split_off_keeps_a_chip_whole() {
        let chips = registry(&["a\nb"]);
        let chip_len = chip(&chips, 1).chars().count();
        let mut line: Line = vec![
            Segment::Text("ab".into()),
            Segment::Paste(1),
            Segment::Text("cd".into()),
        ];
        // Inside the text run before the chip.
        let tail = split_off(&mut line, &chips, 1);
        assert_eq!(flat(&line, &chips), "a");
        assert_eq!(flat(&tail, &chips), format!("b{}cd", chip(&chips, 1)));
        // At the chip's leading edge: the chip opens the tail.
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        let tail = split_off(&mut line, &chips, 2);
        assert_eq!(flat(&line, &chips), "ab");
        assert_eq!(tail, vec![Segment::Paste(1)]);
        // At the chip's trailing edge: the chip stays behind.
        let mut line: Line = vec![Segment::Paste(1), Segment::Text("cd".into())];
        let tail = split_off(&mut line, &chips, chip_len);
        assert_eq!(line, vec![Segment::Paste(1)]);
        assert_eq!(flat(&tail, &chips), "cd");
        // At the end: nothing moves.
        let mut line: Line = vec![Segment::Text("ab".into())];
        assert!(split_off(&mut line, &chips, 2).is_empty());
    }

    #[test]
    fn remove_unit_before_takes_a_whole_chip() {
        let mut chips = registry(&["a\nb"]);
        let chip_len = chip(&chips, 1).chars().count();
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        // Right after the chip: the chip goes, not one of its characters.
        assert_eq!(
            remove_unit_before(&mut line, &mut chips, 2 + chip_len),
            chip_len
        );
        assert_eq!(flat(&line, &chips), "ab");
        assert_eq!(
            chips.paste_payload(1),
            None,
            "the payload is dropped with the chip"
        );
        // Inside the text run.
        assert_eq!(remove_unit_before(&mut line, &mut chips, 2), 1);
        assert_eq!(flat(&line, &chips), "a");
        // Column 0 is the caller's business (line merge).
        assert_eq!(remove_unit_before(&mut line, &mut chips, 0), 0);
    }

    #[test]
    fn remove_unit_before_crosses_a_segment_boundary() {
        let mut chips = registry(&["a\nb"]);
        let chip_len = chip(&chips, 1).chars().count();
        // A chip at the start of a line: the position after it is a boundary.
        let mut line: Line = vec![Segment::Paste(1), Segment::Text("cd".into())];
        assert_eq!(
            remove_unit_before(&mut line, &mut chips, chip_len),
            chip_len
        );
        assert_eq!(flat(&line, &chips), "cd");
        // A run boundary: the unit before is the previous run's last char.
        let mut chips = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Text("cd".into())];
        assert_eq!(remove_unit_before(&mut line, &mut chips, 2), 1);
        assert_eq!(flat(&line, &chips), "acd");
    }

    #[test]
    fn remove_unit_at_takes_a_whole_chip() {
        let mut chips = registry(&["a\nb"]);
        let chip_len = chip(&chips, 1).chars().count();
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        assert_eq!(remove_unit_at(&mut line, &mut chips, 2), chip_len);
        assert_eq!(flat(&line, &chips), "ab");
        assert_eq!(chips.paste_payload(1), None);
        // One character, and nothing past the end.
        let mut line: Line = vec![Segment::Text("ab".into())];
        assert_eq!(remove_unit_at(&mut line, &mut chips, 0), 1);
        assert_eq!(remove_unit_at(&mut line, &mut chips, 1), 0);
    }

    #[test]
    fn stepping_units_walks_over_a_chip() {
        let chips = registry(&["a\nb"]);
        let chip_len = chip(&chips, 1).chars().count();
        let line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        // Left from the chip's trailing edge → its leading edge.
        assert_eq!(unit_start_before(&line, &chips, 2 + chip_len), 2);
        // Left from inside the text → one char.
        assert_eq!(unit_start_before(&line, &chips, 2), 1);
        // Right from the chip's leading edge → its trailing edge.
        assert_eq!(unit_end_at(&line, &chips, 2), 2 + chip_len);
        assert_eq!(unit_end_at(&line, &chips, 0), 1);
        // Column 0 has nothing before it.
        assert_eq!(unit_start_before(&line, &chips, 0), 0);
    }

    #[test]
    fn column_point_resolves_chips_to_their_edges() {
        let chips = registry(&["a\nb"]);
        let line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        let flat = flat(&line, &chips);
        let row = wrap::VisualRow {
            logical_line: 0,
            char_start: 0,
            char_end: flat.chars().count(),
            display_width: flat.chars().count(),
        };
        // On the text: the char at or before the column, focus one further.
        let point = column_point(&line, &chips, &row, 1, row.display_width);
        assert_eq!((point.point, point.focus, point.on_char), (1, 2, true));
        // On the chip: the whole chip is the unit — point before it, focus
        // after it.
        let chip_len = chip(&chips, 1).chars().count();
        let point = column_point(&line, &chips, &row, 2 + chip_len / 2, row.display_width);
        assert_eq!(
            (point.point, point.focus, point.on_char),
            (2, 2 + chip_len, true)
        );
        let point = column_point(&line, &chips, &row, 2 + chip_len - 1, row.display_width);
        assert_eq!((point.point, point.focus), (2, 2 + chip_len));
        // Past the row's text: nothing to include.
        let point = column_point(&line, &chips, &row, 99, row.display_width);
        assert!(!point.on_char);
        assert_eq!(point.point, point.focus);
    }

    #[test]
    fn column_point_ignores_the_hidden_half_of_a_clipped_chip() {
        let chips = registry(&["a\nb"]);
        let line: Line = vec![Segment::Paste(1)];
        let chip_len = chip(&chips, 1).chars().count();
        let row = wrap::VisualRow {
            logical_line: 0,
            char_start: 0,
            char_end: chip_len,
            display_width: chip_len,
        };
        // The row was drawn into 10 columns: the chip is clipped at 10.
        let visible = 10;
        // Inside what was drawn: the whole chip is the unit under the pointer.
        let point = column_point(&line, &chips, &row, 0, visible);
        assert_eq!(
            (point.point, point.focus, point.on_char),
            (0, chip_len, true)
        );
        let point = column_point(&line, &chips, &row, visible - 1, visible);
        assert_eq!(point.point, 0, "the last drawn cell is still the chip");

        // Past the clip (the card's padding): the blank past the row's text,
        // like on any other row — the cursor lands at the row's end, after
        // the chip, never *inside* its hidden half.
        let point = column_point(&line, &chips, &row, visible, visible);
        assert!(!point.on_char);
        assert_eq!(point.point, chip_len, "the row's end");
        assert_eq!(point.focus, point.point);
    }

    #[test]
    fn runs_split_a_row_into_text_and_chips() {
        let chips = registry(&["a\nb"]);
        let line: Line = vec![
            Segment::Text("ab".into()),
            Segment::Paste(1),
            Segment::Text("cd".into()),
        ];
        let len = flat_len(&line, &chips);
        let pieces = runs(&line, &chips, 0..len);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0], ("ab".to_string(), false));
        assert_eq!(pieces[1], (chip(&chips, 1).to_string(), true));
        assert!(pieces[1].1, "the run is a chip");
        assert_eq!(pieces[2], ("cd".to_string(), false));
        // A range inside the text takes only that slice.
        assert_eq!(runs(&line, &chips, 1..2), vec![("b".to_string(), false)]);
    }

    #[test]
    fn spot_at_prefers_the_later_segment_on_a_boundary() {
        let chips = registry(&["a\nb"]);
        let chip_len = chip(&chips, 1).chars().count();
        let line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        assert_eq!(
            (
                spot_at(&line, &chips, 1).segment,
                spot_at(&line, &chips, 1).offset
            ),
            (0, 1)
        );
        // The chip's leading edge is segment 1, offset 0 (before the chip)…
        assert_eq!(
            (
                spot_at(&line, &chips, 2).segment,
                spot_at(&line, &chips, 2).offset
            ),
            (1, 0)
        );
        // …and its trailing edge is past it, not inside.
        let spot = spot_at(&line, &chips, 2 + chip_len);
        assert_eq!((spot.segment, spot.offset), (2, 0));
    }

    // ── Image chips are the same unit ───────────────────────────
    //
    // The assertions below mirror the paste-chip ones one for one: they are
    // the "zero difference" contract stated in a form that fails loudly if the
    // two kinds ever take different code paths.

    /// A bundle holding one image of `bytes` bytes per entry.
    fn image_registry(sizes: &[usize]) -> Chips {
        let mut chips = Chips::default();
        for size in sizes {
            chips.add_image(picture(*size));
        }
        chips
    }

    fn image_label(chips: &Chips, number: ImageNumber) -> &str {
        chips.label(ChipId::Image(number))
    }

    #[test]
    fn an_image_chip_projects_wraps_and_steps_like_a_paste_chip() {
        let mut chips = image_registry(&[4096]);
        let line: Line = vec![Segment::Text("see ".into()), Segment::Image(1)];
        let label = image_label(&chips, 1);
        assert_eq!(label, "[Image #1 4×2 4KB]");
        assert_eq!(flat(&line, &chips), format!("see {label}"));
        assert_eq!(flat_len(&line, &chips), flat(&line, &chips).chars().count());

        // One span, exactly the label's range, carrying the chip's identity.
        let spans = chip_spans(&line, &chips);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1, ChipId::Image(1));
        assert_eq!(spans[0].0, 4..4 + label.chars().count());
        assert!(inside_chip(&line, &chips, 4 + label.chars().count() - 1));
        assert!(!inside_chip(&line, &chips, 4));
        assert!(!inside_chip(&line, &chips, 4 + label.chars().count()));

        // ←/→ step over the whole chip.
        let end = 4 + label.chars().count();
        assert_eq!(unit_start_before(&line, &chips, end), 4);
        assert_eq!(unit_end_at(&line, &chips, 4), end);

        // A click on any of its cells resolves to its leading edge, the
        // trailing one included in the drag.
        let shown = flat(&line, &chips);
        let row = wrap::VisualRow {
            logical_line: 0,
            char_start: 0,
            char_end: shown.chars().count(),
            display_width: shown.chars().count(),
        };
        let point = column_point(&line, &chips, &row, 6, row.display_width);
        assert_eq!((point.point, point.focus, point.on_char), (4, end, true));

        // It never breaks: the run is the chip, whole.
        let len = flat_len(&line, &chips);
        let pieces = runs(&line, &chips, 0..len);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[1], (label.to_string(), true));

        // Deleting it takes the whole chip — and its payload.
        let label_len = label.chars().count();
        let mut line = line;
        assert_eq!(remove_unit_before(&mut line, &mut chips, end), label_len);
        assert_eq!(flat(&line, &chips), "see ");
        assert_eq!(
            image_label(&chips, 1),
            "[Image #?]",
            "the payload went with it"
        );
    }

    #[test]
    fn expanded_leaves_image_chips_out_of_the_text() {
        let mut chips = image_registry(&[1000]);
        chips.add_paste("a\nb".to_string());
        let line: Line = vec![
            Segment::Text("see ".into()),
            Segment::Paste(1),
            Segment::Text(" and ".into()),
            Segment::Image(1),
            Segment::Text(" end".into()),
        ];
        assert_eq!(
            flat(&line, &chips),
            "see [Pasted text #1 +1 lines] and [Image #1 4×2 1000B] end",
            "the screen shows both chips"
        );
        assert_eq!(
            expanded(&line, &chips),
            "see a\nb and  end",
            "submit expands the paste and drops the picture (it rides its own field)"
        );
    }

    #[test]
    fn take_images_returns_the_payloads_in_appearance_order() {
        let mut chips = Chips::default();
        // Registered 1 then 2, but the draft shows 2 before 1: appearance
        // order (what the submit product promises) is the *draft's* order.
        chips.add_image(picture(111));
        chips.add_image(picture(222));
        let lines: Vec<Line> = vec![
            vec![Segment::Image(2), Segment::Text(" mid".into())],
            Vec::new(),
            vec![Segment::Image(1)],
        ];
        let images = chips.take_images(&lines);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].image.bytes.len(), 222, "the first row's chip");
        assert_eq!(images[1].image.bytes.len(), 111);
        assert_eq!(
            images[0].label, "[Image #2 4×2 222B]",
            "labels travel whole"
        );
        assert!(chips.take_images(&lines).is_empty(), "taken once");
        assert_eq!(chips.label(ChipId::Image(1)), "[Image #?]");
    }

    #[test]
    fn an_image_chip_survives_a_line_split_whole() {
        let chips = image_registry(&[10]);
        let label_len = image_label(&chips, 1).chars().count();
        let mut line: Line = vec![
            Segment::Text("ab".into()),
            Segment::Image(1),
            Segment::Text("cd".into()),
        ];
        let tail = split_off(&mut line, &chips, 2 + label_len);
        assert_eq!(flat(&line, &chips), format!("ab{}", image_label(&chips, 1)));
        assert_eq!(flat(&tail, &chips), "cd");
        // A split *inside* the chip's span keeps it whole on the same side it
        // started (the leading edge is where the split lands).
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Image(1)];
        let tail = split_off(&mut line, &chips, 2 + label_len / 2);
        assert_eq!(flat(&line, &chips), "ab");
        assert_eq!(tail, vec![Segment::Image(1)]);
    }
}
