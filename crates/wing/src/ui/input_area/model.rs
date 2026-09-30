//! The composer draft's content model.
//!
//! A line is a sequence of [`Segment`]s: literal text the user typed (or a
//! small paste spliced in place), and *chips* — the `[Pasted text #N +M lines]`
//! placeholder a large paste leaves behind, whose payload lives in the draft's
//! [`Pastes`] registry.
//!
//! A chip is **one unit**, not text that happens to look special: the cursor
//! never lands inside it (←/→ step over it, Backspace/Delete remove it whole),
//! the renderer never breaks a line inside it, and it is expanded once — on
//! submit — so the draft keeps showing the chip while the agent receives the
//! text. Nothing is ever re-parsed from the chip's own text: the segments are
//! the authority, so a draft restored from plain text (no registry entry) can
//! never grow a live chip out of a look-alike string.
//!
//! Everything else in the composer speaks *flat char indices* — cursor
//! columns, visual rows, pointer columns: [`flat`] projects a line the way the
//! screen shows it, and a chip's span is exactly its text's range in it.

use std::collections::BTreeMap;
use std::ops::Range;

use super::helpers::char_to_byte;
use super::wrap;

/// A chip's number — the `#N` in `[Pasted text #N …]`.
pub(crate) type PasteNumber = usize;

/// One piece of a draft line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Literal text: typed, or a small paste spliced in place.
    Text(String),
    /// A chip standing for the paste registered under this number.
    Paste(PasteNumber),
}

/// One draft line.
pub(crate) type Line = Vec<Segment>;

/// The pastes a draft still holds: chip number → payload text.
#[derive(Debug, Default)]
pub(crate) struct Pastes {
    next_number: PasteNumber,
    payloads: BTreeMap<PasteNumber, String>,
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
        self.payloads.insert(self.next_number, text);
        self.next_number
    }

    /// Drop the payload of a chip that is gone from the draft.
    pub fn remove(&mut self, number: PasteNumber) {
        self.payloads.remove(&number);
    }

    /// The text a chip expands to on submit.
    pub fn payload(&self, number: PasteNumber) -> Option<&str> {
        self.payloads.get(&number).map(String::as_str)
    }

    /// Forget every payload and restart the numbering.
    pub fn clear(&mut self) {
        self.payloads.clear();
        self.next_number = 0;
    }

    /// A chip's visible text.
    ///
    /// Single-line pastes carry no line count: `+0 lines` would be a count of
    /// nothing — the chip still stands for a large paste (that is why it is
    /// one), it just does not span lines.
    pub fn chip(&self, number: PasteNumber) -> String {
        let extra = self
            .payload(number)
            .map(|text| text.split('\n').count().saturating_sub(1))
            .unwrap_or(0);
        if extra == 0 {
            format!("[Pasted text #{number}]")
        } else {
            format!("[Pasted text #{number} +{extra} lines]")
        }
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

/// The line as the screen shows it — chips as their visible chip text.
pub fn flat(line: &[Segment], pastes: &Pastes) -> String {
    let mut out = String::new();
    for segment in line {
        match segment {
            Segment::Text(text) => out.push_str(text),
            Segment::Paste(number) => out.push_str(&pastes.chip(*number)),
        }
    }
    out
}

/// The char length of [`flat`].
pub fn flat_len(line: &[Segment], pastes: &Pastes) -> usize {
    line.iter()
        .map(|segment| segment_len(segment, pastes))
        .sum()
}

/// The line as it is submitted — chips expanded to their payloads.
pub fn expanded(line: &[Segment], pastes: &Pastes) -> String {
    let mut out = String::new();
    for segment in line {
        match segment {
            Segment::Text(text) => out.push_str(text),
            Segment::Paste(number) => match pastes.payload(*number) {
                Some(text) => out.push_str(text),
                // A payload-less chip cannot happen; the chip itself is a
                // better answer than dropping the segment silently.
                None => out.push_str(&pastes.chip(*number)),
            },
        }
    }
    out
}

/// Char ranges of the line's chips in the flat projection, with their numbers.
///
/// A chip is never empty (its label has a fixed prefix), so a span is always
/// a real range — the wrap loop below steps over it by `span.end > start`.
pub fn chip_spans(line: &[Segment], pastes: &Pastes) -> Vec<(Range<usize>, PasteNumber)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for segment in line {
        let len = segment_len(segment, pastes);
        if let Segment::Paste(number) = segment
            && len > 0
        {
            spans.push((start..start + len, *number));
        }
        start += len;
    }
    spans
}

/// Whether `col` falls strictly inside a chip — a position the model never
/// lets the cursor reach (the callers snap out of it; the edits below treat it
/// as the chip's leading edge so a stray one cannot corrupt the line).
pub fn inside_chip(line: &[Segment], pastes: &Pastes, col: usize) -> bool {
    chip_spans(line, pastes)
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
pub fn column_point(
    line: &[Segment],
    pastes: &Pastes,
    row: &wrap::VisualRow,
    display_col: usize,
) -> ColumnPoint {
    let flat = flat(line, pastes);
    let col = wrap::display_col_to_char(&flat, row, display_col);
    for (span, _) in chip_spans(line, pastes) {
        let start = wrap::char_display_offset(&flat, row, span.start);
        let end = wrap::char_display_offset(&flat, row, span.end);
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
pub fn runs(line: &[Segment], pastes: &Pastes, range: Range<usize>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    for segment in line {
        let end = start + segment_len(segment, pastes);
        let from = range.start.max(start);
        let to = range.end.min(end);
        if from < to {
            let slice = |text: &str| -> String {
                text.chars().skip(from - start).take(to - from).collect()
            };
            let run = match segment {
                Segment::Text(text) => (slice(text), false),
                Segment::Paste(number) => (slice(&pastes.chip(*number)), true),
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
pub fn insert_text(line: &mut Line, pastes: &Pastes, col: usize, text: &str) {
    let spot = spot_at(line, pastes, col);
    match line.get_mut(spot.segment) {
        Some(Segment::Text(existing)) => {
            let byte = char_to_byte(existing, spot.offset);
            existing.insert_str(byte, text);
        }
        _ => line.insert(spot.segment, Segment::Text(text.to_string())),
    }
    coalesce(line);
}

/// Splice a chip for `number` into the line at flat char column `col`.
pub fn insert_chip(line: &mut Line, pastes: &Pastes, col: usize, number: PasteNumber) {
    let spot = spot_at(line, pastes, col);
    match line.get_mut(spot.segment) {
        // Inside a text run: open it so the chip lands in the middle of it.
        Some(Segment::Text(existing)) if spot.offset > 0 => {
            let byte = char_to_byte(existing, spot.offset);
            let tail = existing.split_off(byte);
            line.insert(spot.segment + 1, Segment::Paste(number));
            line.insert(spot.segment + 2, Segment::Text(tail));
        }
        // Before a chip, or at the line's end: a segment of its own. (A
        // column *inside* a chip also lands here — the chip keeps its place
        // and the new one goes before it.)
        _ => line.insert(spot.segment, Segment::Paste(number)),
    }
    coalesce(line);
}

/// Split the line at flat char column `col`; `col` starts the returned tail.
pub fn split_off(line: &mut Line, pastes: &Pastes, col: usize) -> Line {
    let spot = spot_at(line, pastes, col);
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
pub fn remove_unit_before(line: &mut Line, pastes: &mut Pastes, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    let spot = spot_at(line, pastes, col);
    if spot.offset == 0 {
        // A segment boundary: the unit before it is the previous segment's
        // last one.
        let removed = remove_last_unit(line, pastes, spot.segment);
        coalesce(line);
        return removed;
    }
    match line.get(spot.segment) {
        Some(Segment::Paste(number)) => take_chip(line, pastes, spot.segment, *number),
        Some(Segment::Text(_)) => {
            let removed = match line.get_mut(spot.segment) {
                Some(Segment::Text(text)) => remove_char_before(text, spot.offset),
                _ => 0,
            };
            coalesce(line);
            removed
        }
        None => 0,
    }
}

/// Delete the unit at `col` — one character, or a whole chip. Returns how many
/// chars the line lost; nothing happens past the line's end (the caller merges
/// lines there).
pub fn remove_unit_at(line: &mut Line, pastes: &mut Pastes, col: usize) -> usize {
    let spot = spot_at(line, pastes, col);
    match line.get(spot.segment) {
        Some(Segment::Paste(number)) => take_chip(line, pastes, spot.segment, *number),
        Some(Segment::Text(_)) => {
            let removed = match line.get_mut(spot.segment) {
                Some(Segment::Text(text)) => remove_char_at(text, spot.offset),
                _ => 0,
            };
            coalesce(line);
            removed
        }
        None => 0,
    }
}

/// The column ← lands on from `col`: the start of the chip that ends there
/// (a chip is one unit to step over), or one char left.
pub fn unit_start_before(line: &[Segment], pastes: &Pastes, col: usize) -> usize {
    chip_spans(line, pastes)
        .into_iter()
        .find(|(span, _)| span.end == col)
        .map_or_else(|| col.saturating_sub(1), |(span, _)| span.start)
}

/// The column → lands on from `col`: the end of the chip that starts there, or
/// one char right.
pub fn unit_end_at(line: &[Segment], pastes: &Pastes, col: usize) -> usize {
    chip_spans(line, pastes)
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

fn segment_len(segment: &Segment, pastes: &Pastes) -> usize {
    match segment {
        Segment::Text(text) => text.chars().count(),
        Segment::Paste(number) => pastes.chip(*number).chars().count(),
    }
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

fn spot_at(line: &[Segment], pastes: &Pastes, col: usize) -> Spot {
    let mut remaining = col;
    for (index, segment) in line.iter().enumerate() {
        let len = segment_len(segment, pastes);
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
fn take_chip(line: &mut Line, pastes: &mut Pastes, index: usize, number: PasteNumber) -> usize {
    let len = pastes.chip(number).chars().count();
    line.remove(index);
    pastes.remove(number);
    coalesce(line);
    len
}

/// Remove the last unit of the segment before `boundary` (a chip whole).
fn remove_last_unit(line: &mut Line, pastes: &mut Pastes, boundary: usize) -> usize {
    let Some(index) = boundary.checked_sub(1) else {
        return 0;
    };
    match line.get(index) {
        Some(Segment::Paste(number)) => {
            let number = *number;
            take_chip(line, pastes, index, number)
        }
        _ => match line.get_mut(index) {
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

    /// A registry holding `(number, payload)` pairs — the numbers the tests
    /// read back are the ones `add` hands out.
    fn registry(payloads: &[&str]) -> Pastes {
        let mut pastes = Pastes::default();
        for payload in payloads {
            pastes.add((*payload).to_string());
        }
        pastes
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
    fn chip_text_carries_the_line_count_it_adds() {
        let pastes = registry(&["one line", "a\nb\nc"]);
        assert_eq!(pastes.chip(1), "[Pasted text #1]");
        assert_eq!(pastes.chip(2), "[Pasted text #2 +2 lines]");
    }

    #[test]
    fn flat_projects_the_chip_and_len_agrees() {
        let pastes = registry(&["a\nb"]);
        let line: Line = vec![Segment::Text("see ".into()), Segment::Paste(1)];
        assert_eq!(flat(&line, &pastes), "see [Pasted text #1 +1 lines]");
        assert_eq!(
            flat_len(&line, &pastes),
            flat(&line, &pastes).chars().count()
        );
    }

    #[test]
    fn expanded_swaps_the_chip_for_its_payload() {
        let pastes = registry(&["a\nb"]);
        let line: Line = vec![
            Segment::Text("see ".into()),
            Segment::Paste(1),
            Segment::Text(" end".into()),
        ];
        assert_eq!(expanded(&line, &pastes), "see a\nb end");
    }

    #[test]
    fn chip_spans_report_flat_ranges() {
        let pastes = registry(&["a\nb"]);
        let line: Line = vec![
            Segment::Text("ab".into()),
            Segment::Paste(1),
            Segment::Text("cd".into()),
        ];
        let spans = chip_spans(&line, &pastes);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1, 1);
        assert_eq!(spans[0].0.start, 2);
        assert_eq!(spans[0].0.end, 2 + pastes.chip(1).chars().count());
    }

    #[test]
    fn insert_text_splits_a_run_and_lands_before_a_chip() {
        let pastes = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Text("abcd".into()), Segment::Paste(1)];
        insert_text(&mut line, &pastes, 2, "X");
        assert_eq!(line, vec![Segment::Text("abXcd".into()), Segment::Paste(1)]);
        // At the chip's leading edge the text opens a run of its own — the
        // chip keeps its place, the cursor cannot enter it — and the splice
        // coalesces with the run before it.
        insert_text(&mut line, &pastes, 5, "Y");
        assert_eq!(
            line,
            vec![Segment::Text("abXcdY".into()), Segment::Paste(1)]
        );
    }

    #[test]
    fn insert_chip_opens_a_run_in_the_middle() {
        let pastes = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Text("abcd".into())];
        insert_chip(&mut line, &pastes, 2, 1);
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
        insert_chip(&mut line, &pastes, 0, 1);
        insert_chip(&mut line, &pastes, 0, 1);
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
        let pastes = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Paste(1)];
        insert_text(&mut line, &pastes, pastes.chip(1).chars().count(), " tail");
        assert_eq!(line, vec![Segment::Paste(1), Segment::Text(" tail".into())]);
        // An empty line takes the text directly.
        let mut empty = draft_line("");
        insert_text(&mut empty, &pastes, 0, "hi");
        assert_eq!(flat(&empty, &pastes), "hi");
    }

    #[test]
    fn split_off_keeps_a_chip_whole() {
        let pastes = registry(&["a\nb"]);
        let chip_len = pastes.chip(1).chars().count();
        let mut line: Line = vec![
            Segment::Text("ab".into()),
            Segment::Paste(1),
            Segment::Text("cd".into()),
        ];
        // Inside the text run before the chip.
        let tail = split_off(&mut line, &pastes, 1);
        assert_eq!(flat(&line, &pastes), "a");
        assert_eq!(flat(&tail, &pastes), format!("b{}cd", pastes.chip(1)));
        // At the chip's leading edge: the chip opens the tail.
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        let tail = split_off(&mut line, &pastes, 2);
        assert_eq!(flat(&line, &pastes), "ab");
        assert_eq!(tail, vec![Segment::Paste(1)]);
        // At the chip's trailing edge: the chip stays behind.
        let mut line: Line = vec![Segment::Paste(1), Segment::Text("cd".into())];
        let tail = split_off(&mut line, &pastes, chip_len);
        assert_eq!(line, vec![Segment::Paste(1)]);
        assert_eq!(flat(&tail, &pastes), "cd");
        // At the end: nothing moves.
        let mut line: Line = vec![Segment::Text("ab".into())];
        assert!(split_off(&mut line, &pastes, 2).is_empty());
    }

    #[test]
    fn remove_unit_before_takes_a_whole_chip() {
        let mut pastes = registry(&["a\nb"]);
        let chip_len = pastes.chip(1).chars().count();
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        // Right after the chip: the chip goes, not one of its characters.
        assert_eq!(
            remove_unit_before(&mut line, &mut pastes, 2 + chip_len),
            chip_len
        );
        assert_eq!(flat(&line, &pastes), "ab");
        assert_eq!(
            pastes.payload(1),
            None,
            "the payload is dropped with the chip"
        );
        // Inside the text run.
        assert_eq!(remove_unit_before(&mut line, &mut pastes, 2), 1);
        assert_eq!(flat(&line, &pastes), "a");
        // Column 0 is the caller's business (line merge).
        assert_eq!(remove_unit_before(&mut line, &mut pastes, 0), 0);
    }

    #[test]
    fn remove_unit_before_crosses_a_segment_boundary() {
        let mut pastes = registry(&["a\nb"]);
        let chip_len = pastes.chip(1).chars().count();
        // A chip at the start of a line: the position after it is a boundary.
        let mut line: Line = vec![Segment::Paste(1), Segment::Text("cd".into())];
        assert_eq!(
            remove_unit_before(&mut line, &mut pastes, chip_len),
            chip_len
        );
        assert_eq!(flat(&line, &pastes), "cd");
        // A run boundary: the unit before is the previous run's last char.
        let mut pastes = registry(&["a\nb"]);
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Text("cd".into())];
        assert_eq!(remove_unit_before(&mut line, &mut pastes, 2), 1);
        assert_eq!(flat(&line, &pastes), "acd");
    }

    #[test]
    fn remove_unit_at_takes_a_whole_chip() {
        let mut pastes = registry(&["a\nb"]);
        let chip_len = pastes.chip(1).chars().count();
        let mut line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        assert_eq!(remove_unit_at(&mut line, &mut pastes, 2), chip_len);
        assert_eq!(flat(&line, &pastes), "ab");
        assert_eq!(pastes.payload(1), None);
        // One character, and nothing past the end.
        let mut line: Line = vec![Segment::Text("ab".into())];
        assert_eq!(remove_unit_at(&mut line, &mut pastes, 0), 1);
        assert_eq!(remove_unit_at(&mut line, &mut pastes, 1), 0);
    }

    #[test]
    fn stepping_units_walks_over_a_chip() {
        let pastes = registry(&["a\nb"]);
        let chip_len = pastes.chip(1).chars().count();
        let line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        // Left from the chip's trailing edge → its leading edge.
        assert_eq!(unit_start_before(&line, &pastes, 2 + chip_len), 2);
        // Left from inside the text → one char.
        assert_eq!(unit_start_before(&line, &pastes, 2), 1);
        // Right from the chip's leading edge → its trailing edge.
        assert_eq!(unit_end_at(&line, &pastes, 2), 2 + chip_len);
        assert_eq!(unit_end_at(&line, &pastes, 0), 1);
        // Column 0 has nothing before it.
        assert_eq!(unit_start_before(&line, &pastes, 0), 0);
    }

    #[test]
    fn column_point_resolves_chips_to_their_edges() {
        let pastes = registry(&["a\nb"]);
        let line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        let flat = flat(&line, &pastes);
        let row = wrap::VisualRow {
            logical_line: 0,
            char_start: 0,
            char_end: flat.chars().count(),
            display_width: flat.chars().count(),
        };
        // On the text: the char at or before the column, focus one further.
        let point = column_point(&line, &pastes, &row, 1);
        assert_eq!((point.point, point.focus, point.on_char), (1, 2, true));
        // On the chip: the whole chip is the unit — point before it, focus
        // after it.
        let chip_len = pastes.chip(1).chars().count();
        let point = column_point(&line, &pastes, &row, 2 + chip_len / 2);
        assert_eq!(
            (point.point, point.focus, point.on_char),
            (2, 2 + chip_len, true)
        );
        let point = column_point(&line, &pastes, &row, 2 + chip_len - 1);
        assert_eq!((point.point, point.focus), (2, 2 + chip_len));
        // Past the row's text: nothing to include.
        let point = column_point(&line, &pastes, &row, 99);
        assert!(!point.on_char);
        assert_eq!(point.point, point.focus);
    }

    #[test]
    fn runs_split_a_row_into_text_and_chips() {
        let pastes = registry(&["a\nb"]);
        let line: Line = vec![
            Segment::Text("ab".into()),
            Segment::Paste(1),
            Segment::Text("cd".into()),
        ];
        let len = flat_len(&line, &pastes);
        let pieces = runs(&line, &pastes, 0..len);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0], ("ab".to_string(), false));
        assert_eq!(pieces[1], (pastes.chip(1), true));
        assert_eq!(pieces[2], ("cd".to_string(), false));
        // A range inside the text takes only that slice.
        assert_eq!(runs(&line, &pastes, 1..2), vec![("b".to_string(), false)]);
    }

    #[test]
    fn spot_at_prefers_the_later_segment_on_a_boundary() {
        let pastes = registry(&["a\nb"]);
        let chip_len = pastes.chip(1).chars().count();
        let line: Line = vec![Segment::Text("ab".into()), Segment::Paste(1)];
        assert_eq!(
            (
                spot_at(&line, &pastes, 1).segment,
                spot_at(&line, &pastes, 1).offset
            ),
            (0, 1)
        );
        // The chip's leading edge is segment 1, offset 0 (before the chip)…
        assert_eq!(
            (
                spot_at(&line, &pastes, 2).segment,
                spot_at(&line, &pastes, 2).offset
            ),
            (1, 0)
        );
        // …and its trailing edge is past it, not inside.
        let spot = spot_at(&line, &pastes, 2 + chip_len);
        assert_eq!((spot.segment, spot.offset), (2, 0));
    }
}
