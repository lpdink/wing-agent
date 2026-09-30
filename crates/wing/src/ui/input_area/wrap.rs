//! Word-wrap computation for the input area.
//!
//! Splits logical lines into visual rows based on available display width.
//! Provides bidirectional mapping between logical (row, col) and visual coordinates.
//!
//! A paste chip never wraps: the row breaks *before* it when it no longer
//! fits, so its label is always shown in one piece (a chip wider than the whole
//! row overflows it and is clipped by the widget, like any other over-wide
//! unit — the alternative, breaking inside the label, would tear apart the one
//! thing the chip has to say).

use unicode_width::UnicodeWidthChar;

use super::model;
use super::model::Line;
use super::model::Pastes;
use super::model::Segment;

/// A visual row — one screen line produced by wrapping a logical line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    /// Index of the logical line this visual row belongs to.
    pub logical_line: usize,
    /// Start char index within the logical line (inclusive).
    pub char_start: usize,
    /// End char index within the logical line (exclusive).
    pub char_end: usize,
    /// Display width of this visual row.
    pub display_width: usize,
}

/// Build the visual row list from logical lines and available text width.
///
/// `available_width` is the text area width (total width minus prefix).
pub fn build_visual_rows(
    lines: &[Line],
    pastes: &Pastes,
    available_width: usize,
) -> Vec<VisualRow> {
    let mut result = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if available_width == 0 {
            // Zero-width: emit as single visual row.
            let flat = model::flat(line, pastes);
            let w: usize = flat.chars().map(|c| c.width().unwrap_or(0)).sum();
            result.push(VisualRow {
                logical_line: i,
                char_start: 0,
                char_end: flat.chars().count(),
                display_width: w,
            });
            continue;
        }
        wrap_line(line, pastes, i, available_width, &mut result);
    }
    result
}

/// Wrap a single logical line into visual rows.
fn wrap_line(
    line: &[Segment],
    pastes: &Pastes,
    logical_idx: usize,
    max_width: usize,
    out: &mut Vec<VisualRow>,
) {
    let flat = model::flat(line, pastes);
    let chars: Vec<char> = flat.chars().collect();
    if chars.is_empty() {
        out.push(VisualRow {
            logical_line: logical_idx,
            char_start: 0,
            char_end: 0,
            display_width: 0,
        });
        return;
    }

    let chips = model::chip_spans(line, pastes);
    let mut next_chip = 0;
    let mut char_start = 0;
    let mut row_width = 0;
    let mut i = 0;

    while i < chars.len() {
        // A chip is atomic: it never breaks, the row breaks before it.
        if let Some((span, _)) = chips.get(next_chip).filter(|(span, _)| span.start == i) {
            let width: usize = chars[span.start..span.end]
                .iter()
                .map(|c| c.width().unwrap_or(0))
                .sum();
            if row_width > 0 && row_width + width > max_width {
                out.push(VisualRow {
                    logical_line: logical_idx,
                    char_start,
                    char_end: i,
                    display_width: row_width,
                });
                char_start = i;
                row_width = 0;
                continue;
            }
            row_width += width;
            i = span.end;
            next_chip += 1;
            continue;
        }

        let cw = chars[i].width().unwrap_or(0);

        // If this character would exceed the width, close the current visual row.
        if row_width + cw > max_width && i > char_start {
            out.push(VisualRow {
                logical_line: logical_idx,
                char_start,
                char_end: i,
                display_width: row_width,
            });
            char_start = i;
            row_width = 0;
        }

        row_width += cw;
        i += 1;
    }

    out.push(VisualRow {
        logical_line: logical_idx,
        char_start,
        char_end: chars.len(),
        display_width: row_width,
    });
}

/// Map logical (row, col) to visual (row, col).
///
/// Returns `(visual_row_index, visual_col)`.
pub fn logical_to_visual(vis_rows: &[VisualRow], log_row: usize, log_col: usize) -> (usize, usize) {
    // Find the visual row that contains log_col within log_row.
    let mut best_vis_row = 0;
    let mut best_vis_col = 0;
    for (vi, vr) in vis_rows.iter().enumerate() {
        if vr.logical_line != log_row {
            continue;
        }
        if log_col >= vr.char_start && (log_col < vr.char_end || vr.char_end == vr.char_start) {
            return (vi, log_col - vr.char_start);
        }
        // Fallback: `log_col == char_end` means the cursor sits exactly at the
        // boundary between two visual rows (end of this row = start of next).
        // We record this position and keep scanning — if a next visual row exists
        // for the same logical line, the loop will match it above (log_col == its
        // char_start) and return early.  If no next row exists (cursor at end of
        // the logical line), this recorded "row end" position is the correct answer.
        if log_col == vr.char_end && vr.char_end > vr.char_start {
            best_vis_row = vi;
            best_vis_col = vr.char_end - vr.char_start;
        }
    }
    (best_vis_row, best_vis_col)
}

/// Char index (absolute within `line`) for a *display column* inside `vr`.
///
/// This is the pointer-facing inverse of [`char_display_offset`]: the column is
/// measured from the row's first cell (`0` is the row's first character), which
/// is exactly what a mouse column gives after subtracting the area origin and
/// the text area's inset.
///
/// The result is the insertion point **at or before** the character whose cells
/// cover `display_col` — a char index has no half-cell precision, so landing on
/// a wide character (either cell) resolves to before it. A column past the
/// row's text resolves to the row's end (`char_end`), which is also the next
/// visual row's start when the logical line soft-wrapped there.
///
/// This is the *character* rule; the composer adds the chip rule on top of it
/// (`model::column_point`), which turns a column on a chip into the whole chip.
pub fn display_col_to_char(line: &str, vr: &VisualRow, display_col: usize) -> usize {
    let mut acc = 0usize;
    for (index, ch) in line
        .chars()
        .enumerate()
        .skip(vr.char_start)
        .take(vr.char_end.saturating_sub(vr.char_start))
    {
        let width = ch.width().unwrap_or(0);
        if display_col < acc + width {
            return index;
        }
        acc += width;
    }
    vr.char_end
}

/// Display column of `char_col` (absolute char index within `line`) inside
/// `vr`, measured from the row's first cell.
///
/// The inverse of [`display_col_to_char`], used to turn a selected char range
/// back into the cell span the highlight has to paint. Values outside the row's
/// char range clamp to its ends.
pub fn char_display_offset(line: &str, vr: &VisualRow, char_col: usize) -> usize {
    let char_end = char_col.min(vr.char_end);
    let char_start = vr.char_start.min(char_end);
    line.chars()
        .skip(char_start)
        .take(char_end - char_start)
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::input_area::model::Pastes;

    fn lines(texts: &[&str]) -> Vec<Line> {
        texts.iter().map(|s| model::text_line(s)).collect()
    }

    /// A registry with `count` payloads of `extra + 1` lines each; chip `n`
    /// (1-based) reads `[Pasted text #n +{extra} lines]`.
    fn pastes(extra: usize, count: usize) -> Pastes {
        let mut pastes = Pastes::default();
        for _ in 0..count {
            let payload = (0..=extra)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n");
            pastes.add(payload);
        }
        pastes
    }

    #[test]
    fn test_short_line_no_wrap() {
        let ls = lines(&["hello"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 80);
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].logical_line, 0);
        assert_eq!(vis[0].char_start, 0);
        assert_eq!(vis[0].char_end, 5);
        assert_eq!(vis[0].display_width, 5);
    }

    #[test]
    fn test_long_line_wraps() {
        let ls = lines(&["abcdefghij"]); // 10 chars
        let vis = build_visual_rows(&ls, &Pastes::default(), 4); // width 4
        assert_eq!(vis.len(), 3); // 4+4+2
        assert_eq!(vis[0].char_end, 4);
        assert_eq!(vis[1].char_start, 4);
        assert_eq!(vis[1].char_end, 8);
        assert_eq!(vis[2].char_start, 8);
        assert_eq!(vis[2].char_end, 10);
    }

    #[test]
    fn test_empty_line() {
        let ls = lines(&[""]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 80);
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].char_start, 0);
        assert_eq!(vis[0].char_end, 0);
        assert_eq!(vis[0].display_width, 0);
    }

    #[test]
    fn test_cjk_boundary() {
        // "你好世界" = 4 chars × 2 width = 8 display cols
        let ls = lines(&["你好世界"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 5); // width 5: fits 2 CJK chars (4 cols), 3rd would be 6
        assert_eq!(vis.len(), 2);
        assert_eq!(vis[0].char_start, 0);
        assert_eq!(vis[0].char_end, 2); // "你好" = 4 display width
        assert_eq!(vis[0].display_width, 4);
        assert_eq!(vis[1].char_start, 2);
        assert_eq!(vis[1].char_end, 4); // "世界" = 4 display width
    }

    #[test]
    fn test_cjk_exact_fit() {
        let ls = lines(&["你好"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 4); // exactly fits "你好"
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].char_end, 2);
    }

    #[test]
    fn test_multiple_lines_mixed() {
        let ls = lines(&["short", "abcdefghij", ""]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 4);
        // "short" = 5 chars → 2 vis rows (4+1)
        // "abcdefghij" = 10 chars → 3 vis rows (4+4+2)
        // "" = 1 vis row
        assert_eq!(vis.len(), 6);
        assert_eq!(vis[0].logical_line, 0);
        assert_eq!(vis[1].logical_line, 0);
        assert_eq!(vis[2].logical_line, 1);
        assert_eq!(vis[3].logical_line, 1);
        assert_eq!(vis[4].logical_line, 1);
        assert_eq!(vis[5].logical_line, 2);
    }

    // ── Chips ──────────────────────────────────────────

    #[test]
    fn test_a_chip_is_never_split() {
        let pastes = pastes(4, 1); // chip 1 = "[Pasted text #1 +4 lines]" (25 chars)
        let chip = pastes.chip(1);
        let mut ls = lines(&["see "]);
        ls[0].push(model::Segment::Paste(1));
        // Width 20: "see " + chip (25 chars) does not fit on one row.
        let vis = build_visual_rows(&ls, &pastes, 20);
        assert_eq!(vis.len(), 2, "the chip opens a row of its own");
        assert_eq!(vis[0].char_end, 4, "the text before it keeps the first row");
        assert_eq!(vis[1].char_start, 4);
        assert_eq!(
            vis[1].char_end,
            4 + chip.chars().count(),
            "the row holds the chip whole"
        );
    }

    #[test]
    fn test_a_chip_that_fits_stays_inline() {
        let pastes = pastes(4, 1);
        let mut ls = lines(&["see "]);
        ls[0].push(model::Segment::Paste(1));
        let vis = build_visual_rows(&ls, &pastes, 80);
        assert_eq!(vis.len(), 1, "plenty of room: one row");
        assert_eq!(vis[0].char_end, model::flat_len(&ls[0], &pastes));
    }

    #[test]
    fn test_text_after_a_chip_keeps_its_place() {
        let pastes = pastes(4, 1);
        let mut ls = lines(&["ab"]);
        ls[0].push(model::Segment::Paste(1));
        ls[0].push(model::Segment::Text("cd".into()));
        let chip_len = pastes.chip(1).chars().count();
        // Width 8: "ab" | chip (clipped) | "cd".
        let vis = build_visual_rows(&ls, &pastes, 8);
        assert_eq!(vis.len(), 3);
        assert_eq!((vis[0].char_start, vis[0].char_end), (0, 2));
        assert_eq!((vis[1].char_start, vis[1].char_end), (2, 2 + chip_len));
        assert_eq!(
            (vis[2].char_start, vis[2].char_end),
            (2 + chip_len, 4 + chip_len)
        );
    }

    #[test]
    fn test_an_over_wide_chip_overflows_its_row_and_keeps_going() {
        let pastes = pastes(9, 1); // "[Pasted text #1 +9 lines]" = 26 chars
        let chip_len = pastes.chip(1).chars().count();
        let mut ls = lines(&["ab"]);
        ls[0].push(model::Segment::Paste(1));
        ls[0].push(model::Segment::Text("cd".into()));
        // Width 10: the chip cannot fit any row — it gets one of its own (the
        // widget clips it) and the text after it continues below.
        let vis = build_visual_rows(&ls, &pastes, 10);
        assert_eq!(vis.len(), 3);
        assert_eq!((vis[1].char_start, vis[1].char_end), (2, 2 + chip_len));
        assert_eq!(vis[1].display_width, chip_len);
        assert_eq!(
            (vis[2].char_start, vis[2].char_end),
            (2 + chip_len, 4 + chip_len)
        );
    }

    #[test]
    fn test_two_chips_do_not_share_a_row_that_does_not_fit_them() {
        let pastes = pastes(0, 2); // two "[Pasted text #n]" chips (18 chars each)
        let mut ls = lines(&[""]);
        ls[0].push(model::Segment::Paste(1));
        ls[0].push(model::Segment::Text(" ".into()));
        ls[0].push(model::Segment::Paste(2));
        let one = pastes.chip(1).chars().count();
        // Width 40: 18 + 1 + 18 = 37 fits.
        assert_eq!(build_visual_rows(&ls, &pastes, 40).len(), 1);
        // Width 30: the second chip moves to a row of its own.
        let vis = build_visual_rows(&ls, &pastes, 30);
        assert_eq!(vis.len(), 2);
        assert_eq!((vis[0].char_start, vis[0].char_end), (0, one + 1));
        assert_eq!((vis[1].char_start, vis[1].char_end), (one + 1, 2 * one + 1));
    }

    // ── logical_to_visual ──────────────────────────────

    #[test]
    fn test_logical_to_visual_basic() {
        let ls = lines(&["abcdefghij"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 4);
        // col 0 → vis row 0, col 0
        assert_eq!(logical_to_visual(&vis, 0, 0), (0, 0));
        // col 3 → vis row 0, col 3
        assert_eq!(logical_to_visual(&vis, 0, 3), (0, 3));
        // col 4 → vis row 1, col 0
        assert_eq!(logical_to_visual(&vis, 0, 4), (1, 0));
        // col 7 → vis row 1, col 3
        assert_eq!(logical_to_visual(&vis, 0, 7), (1, 3));
        // col 9 → vis row 2, col 1
        assert_eq!(logical_to_visual(&vis, 0, 9), (2, 1));
    }

    #[test]
    fn test_logical_to_visual_multi_line() {
        let ls = lines(&["ab", "cdefgh"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 3);
        // "ab" → 1 vis row
        // "cdefgh" → 2 vis rows (3+3)
        assert_eq!(vis.len(), 3);
        // line 1, col 0 → vis row 1, col 0
        assert_eq!(logical_to_visual(&vis, 1, 0), (1, 0));
        // line 1, col 4 → vis row 2, col 1
        assert_eq!(logical_to_visual(&vis, 1, 4), (2, 1));
    }

    // ── Roundtrip ──────────────────────────────────────

    #[test]
    fn test_roundtrip() {
        let ls = lines(&["hello world this is a test"]);
        let flat = model::flat(&ls[0], &Pastes::default());
        let vis = build_visual_rows(&ls, &Pastes::default(), 10);
        for log_col in 0..flat.chars().count() {
            let (vr, vc) = logical_to_visual(&vis, 0, log_col);
            assert_eq!(
                vis[vr].char_start + vc,
                log_col,
                "roundtrip failed for col {log_col}"
            );
        }
    }

    // ── Display column ↔ char index (pointer mapping) ─────

    #[test]
    fn test_display_col_to_char_ascii_rows() {
        let ls = lines(&["abcdefghij"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 4); // "abcd" | "efgh" | "ij"
        assert_eq!(vis.len(), 3);
        assert_eq!(display_col_to_char("abcdefghij", &vis[0], 0), 0);
        assert_eq!(display_col_to_char("abcdefghij", &vis[0], 3), 3);
        // Past the row's text → the row's end (== the next row's start).
        assert_eq!(display_col_to_char("abcdefghij", &vis[0], 4), 4);
        assert_eq!(display_col_to_char("abcdefghij", &vis[0], 99), 4);
        assert_eq!(display_col_to_char("abcdefghij", &vis[1], 0), 4);
        assert_eq!(display_col_to_char("abcdefghij", &vis[2], 1), 9);
        assert_eq!(display_col_to_char("abcdefghij", &vis[2], 9), 10);
    }

    #[test]
    fn test_display_col_to_char_wide_glyphs() {
        // "你好世界" at width 5 → "你好" (4 cols) | "世界".
        let ls = lines(&["你好世界"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 5);
        assert_eq!(vis.len(), 2);
        // Either cell of a wide character resolves to *before* that character
        // (a char index has no half-cell precision).
        assert_eq!(display_col_to_char("你好世界", &vis[0], 0), 0);
        assert_eq!(display_col_to_char("你好世界", &vis[0], 1), 0);
        assert_eq!(display_col_to_char("你好世界", &vis[0], 2), 1);
        assert_eq!(display_col_to_char("你好世界", &vis[0], 3), 1);
        // Past the row's text → the row's end.
        assert_eq!(display_col_to_char("你好世界", &vis[0], 4), 2);
        // The second visual row starts at char 2.
        assert_eq!(display_col_to_char("你好世界", &vis[1], 2), 3);
    }

    #[test]
    fn test_display_col_to_char_empty_and_chip_rows() {
        let ls = lines(&[""]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 10);
        assert_eq!(display_col_to_char("", &vis[0], 0), 0);
        assert_eq!(display_col_to_char("", &vis[0], 5), 0);

        // A chip row maps its cells to the chip's characters (the composer
        // turns that into the whole chip — see `model::column_point`).
        let pastes = pastes(4, 1);
        let chip = pastes.chip(1);
        let ls: Vec<Line> = vec![vec![model::Segment::Paste(1)]];
        let vis = build_visual_rows(&ls, &pastes, 40);
        assert_eq!(vis.len(), 1);
        assert_eq!(display_col_to_char(&chip, &vis[0], 3), 3);
    }

    #[test]
    fn test_char_display_offset_maps_char_ranges_to_cells() {
        // "你好a好你" at width 6 → "你好a" (5 cols) | "好你".
        let ls = lines(&["你好a好你"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 6);
        assert_eq!(vis.len(), 2);
        assert_eq!(vis[0].char_end, 3);
        assert_eq!(char_display_offset("你好a好你", &vis[0], 0), 0);
        assert_eq!(char_display_offset("你好a好你", &vis[0], 1), 2);
        assert_eq!(char_display_offset("你好a好你", &vis[0], 2), 4);
        assert_eq!(char_display_offset("你好a好你", &vis[0], 3), 5, "row end");
        assert_eq!(char_display_offset("你好a好你", &vis[0], 99), 5, "clamped");
        assert_eq!(char_display_offset("你好a好你", &vis[1], 3), 0);
        assert_eq!(char_display_offset("你好a好你", &vis[1], 4), 2);
        assert_eq!(char_display_offset("你好a好你", &vis[1], 5), 4);
    }

    #[test]
    fn test_display_col_round_trips_through_char_offsets() {
        let ls = lines(&["你好a好你"]);
        let vis = build_visual_rows(&ls, &Pastes::default(), 6);
        for (index, vr) in vis.iter().enumerate() {
            for char_col in vr.char_start..vr.char_end {
                let display = char_display_offset("你好a好你", vr, char_col);
                assert_eq!(
                    display_col_to_char("你好a好你", &vis[index], display),
                    char_col,
                    "visual row {index}, char {char_col} must round-trip"
                );
            }
        }
    }
}
