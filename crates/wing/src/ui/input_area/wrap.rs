//! Word-wrap computation for the input area.
//!
//! Splits logical lines into visual rows based on available display width.
//! Provides bidirectional mapping between logical (row, col) and visual coordinates.

use unicode_width::UnicodeWidthChar;

use super::helpers::is_placeholder_line;

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
/// Placeholder lines are never wrapped.
pub fn build_visual_rows(lines: &[String], available_width: usize) -> Vec<VisualRow> {
    let mut result = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if is_placeholder_line(line) || available_width == 0 {
            // Placeholder or zero-width: emit as single visual row.
            let w: usize = line.chars().map(|c| c.width().unwrap_or(0)).sum();
            result.push(VisualRow {
                logical_line: i,
                char_start: 0,
                char_end: line.chars().count(),
                display_width: w,
            });
            continue;
        }
        wrap_line(line, i, available_width, &mut result);
    }
    result
}

/// Wrap a single logical line into visual rows.
fn wrap_line(line: &str, logical_idx: usize, max_width: usize, out: &mut Vec<VisualRow>) {
    if line.is_empty() {
        out.push(VisualRow {
            logical_line: logical_idx,
            char_start: 0,
            char_end: 0,
            display_width: 0,
        });
        return;
    }

    let mut char_start = 0;
    let mut row_width = 0;

    for (char_idx, ch) in line.char_indices() {
        let cw = ch.width().unwrap_or(0);

        // If this character would exceed the width, close the current visual row.
        if row_width + cw > max_width && char_idx > 0 {
            let char_count = line[..char_idx].chars().count();
            out.push(VisualRow {
                logical_line: logical_idx,
                char_start,
                char_end: char_count,
                display_width: row_width,
            });
            char_start = char_count;
            row_width = 0;
        }

        row_width += cw;
    }

    // Emit the last (or only) visual row.
    let total_chars = line.chars().count();
    out.push(VisualRow {
        logical_line: logical_idx,
        char_start,
        char_end: total_chars,
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

/// Map visual (row, col) to logical (row, col).
///
/// `vis_col` is a char offset within the visual row.
/// Returns `(logical_row, logical_col)`.
pub fn visual_to_logical(vis_rows: &[VisualRow], vis_row: usize, vis_col: usize) -> (usize, usize) {
    if vis_row >= vis_rows.len() {
        return (0, 0);
    }
    let vr = &vis_rows[vis_row];
    let log_col = vr.char_start + vis_col.min(vr.char_end - vr.char_start);
    (vr.logical_line, log_col)
}

/// Char index (absolute within `line`) for a *display column* inside `vr`.
///
/// This is the pointer-facing inverse of [`char_display_offset`]: the column is
/// measured from the row's first cell (`0` is the row's first character), which
/// is exactly what a mouse column gives after subtracting the area origin and
/// the `> ` prefix.
///
/// The result is the insertion point **at or before** the character whose cells
/// cover `display_col` — a char index has no half-cell precision, so landing on
/// a wide character (either cell) resolves to before it. A column past the
/// row's text resolves to the row's end (`char_end`), which is also the next
/// visual row's start when the logical line soft-wrapped there.
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

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    // ── build_visual_rows ──────────────────────────────

    #[test]
    fn test_short_line_no_wrap() {
        let ls = lines(&["hello"]);
        let vis = build_visual_rows(&ls, 80);
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].logical_line, 0);
        assert_eq!(vis[0].char_start, 0);
        assert_eq!(vis[0].char_end, 5);
        assert_eq!(vis[0].display_width, 5);
    }

    #[test]
    fn test_long_line_wraps() {
        let ls = lines(&["abcdefghij"]); // 10 chars
        let vis = build_visual_rows(&ls, 4); // width 4
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
        let vis = build_visual_rows(&ls, 80);
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].char_start, 0);
        assert_eq!(vis[0].char_end, 0);
        assert_eq!(vis[0].display_width, 0);
    }

    #[test]
    fn test_cjk_boundary() {
        // "你好世界" = 4 chars × 2 width = 8 display cols
        let ls = lines(&["你好世界"]);
        let vis = build_visual_rows(&ls, 5); // width 5: fits 2 CJK chars (4 cols), 3rd would be 6
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
        let vis = build_visual_rows(&ls, 4); // exactly fits "你好"
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].char_end, 2);
    }

    #[test]
    fn test_placeholder_no_wrap() {
        let ls = lines(&["[Pasted text #1 +5 lines]"]);
        let vis = build_visual_rows(&ls, 10);
        assert_eq!(vis.len(), 1); // not wrapped even though wider than 10
    }

    #[test]
    fn test_multiple_lines_mixed() {
        let ls = lines(&["short", "abcdefghij", ""]);
        let vis = build_visual_rows(&ls, 4);
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

    // ── logical_to_visual ──────────────────────────────

    #[test]
    fn test_logical_to_visual_basic() {
        let ls = lines(&["abcdefghij"]);
        let vis = build_visual_rows(&ls, 4);
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
        let vis = build_visual_rows(&ls, 3);
        // "ab" → 1 vis row
        // "cdefgh" → 2 vis rows (3+3)
        assert_eq!(vis.len(), 3);
        // line 1, col 0 → vis row 1, col 0
        assert_eq!(logical_to_visual(&vis, 1, 0), (1, 0));
        // line 1, col 4 → vis row 2, col 1
        assert_eq!(logical_to_visual(&vis, 1, 4), (2, 1));
    }

    // ── visual_to_logical ──────────────────────────────

    #[test]
    fn test_visual_to_logical_basic() {
        let ls = lines(&["abcdefghij"]);
        let vis = build_visual_rows(&ls, 4);
        assert_eq!(visual_to_logical(&vis, 0, 0), (0, 0));
        assert_eq!(visual_to_logical(&vis, 0, 3), (0, 3));
        assert_eq!(visual_to_logical(&vis, 1, 0), (0, 4));
        assert_eq!(visual_to_logical(&vis, 1, 3), (0, 7));
        assert_eq!(visual_to_logical(&vis, 2, 1), (0, 9));
    }

    #[test]
    fn test_visual_to_logical_clamp() {
        let ls = lines(&["abc"]);
        let vis = build_visual_rows(&ls, 80);
        // vis_col beyond end → clamp
        assert_eq!(visual_to_logical(&vis, 0, 100), (0, 3));
    }

    // ── Roundtrip ──────────────────────────────────────

    #[test]
    fn test_roundtrip() {
        let ls = lines(&["hello world this is a test"]);
        let vis = build_visual_rows(&ls, 10);
        for log_col in 0..ls[0].chars().count() {
            let (vr, vc) = logical_to_visual(&vis, 0, log_col);
            let (lr, lc) = visual_to_logical(&vis, vr, vc);
            assert_eq!((lr, lc), (0, log_col), "roundtrip failed for col {log_col}");
        }
    }

    // ── Display column ↔ char index (pointer mapping) ─────

    #[test]
    fn test_display_col_to_char_ascii_rows() {
        let ls = lines(&["abcdefghij"]);
        let vis = build_visual_rows(&ls, 4); // "abcd" | "efgh" | "ij"
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 0), 0);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 3), 3);
        // Past the row's text → the row's end (== the next row's start).
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 4), 4);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 99), 4);
        assert_eq!(display_col_to_char(&ls[0], &vis[1], 0), 4);
        assert_eq!(display_col_to_char(&ls[0], &vis[2], 1), 9);
        assert_eq!(display_col_to_char(&ls[0], &vis[2], 9), 10);
    }

    #[test]
    fn test_display_col_to_char_wide_glyphs() {
        // "你好世界" at width 5 → "你好" (4 cols) | "世界".
        let ls = lines(&["你好世界"]);
        let vis = build_visual_rows(&ls, 5);
        assert_eq!(vis.len(), 2);
        // Either cell of a wide character resolves to *before* that character
        // (a char index has no half-cell precision).
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 0), 0);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 1), 0);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 2), 1);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 3), 1);
        // Past the row's text → the row's end.
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 4), 2);
        // The second visual row starts at char 2.
        assert_eq!(display_col_to_char(&ls[0], &vis[1], 2), 3);
    }

    #[test]
    fn test_display_col_to_char_empty_and_placeholder_rows() {
        let ls = lines(&[""]);
        let vis = build_visual_rows(&ls, 10);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 0), 0);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 5), 0);

        // Placeholder lines never wrap: the whole line is one visual row.
        let ls = lines(&["[Pasted text #1 +5 lines]"]);
        let vis = build_visual_rows(&ls, 4);
        assert_eq!(vis.len(), 1);
        assert_eq!(display_col_to_char(&ls[0], &vis[0], 3), 3);
    }

    #[test]
    fn test_char_display_offset_maps_char_ranges_to_cells() {
        // "你好a好你" at width 6 → "你好a" (5 cols) | "好你".
        let ls = lines(&["你好a好你"]);
        let vis = build_visual_rows(&ls, 6);
        assert_eq!(vis.len(), 2);
        assert_eq!(vis[0].char_end, 3);
        assert_eq!(char_display_offset(&ls[0], &vis[0], 0), 0);
        assert_eq!(char_display_offset(&ls[0], &vis[0], 1), 2);
        assert_eq!(char_display_offset(&ls[0], &vis[0], 2), 4);
        assert_eq!(char_display_offset(&ls[0], &vis[0], 3), 5, "row end");
        assert_eq!(char_display_offset(&ls[0], &vis[0], 99), 5, "clamped");
        assert_eq!(char_display_offset(&ls[0], &vis[1], 3), 0);
        assert_eq!(char_display_offset(&ls[0], &vis[1], 4), 2);
        assert_eq!(char_display_offset(&ls[0], &vis[1], 5), 4);
    }

    #[test]
    fn test_display_col_round_trips_through_char_offsets() {
        let ls = lines(&["你好a好你"]);
        let vis = build_visual_rows(&ls, 6);
        for (index, vr) in vis.iter().enumerate() {
            for char_col in vr.char_start..vr.char_end {
                let display = char_display_offset(&ls[0], vr, char_col);
                assert_eq!(
                    display_col_to_char(&ls[0], &vis[index], display),
                    char_col,
                    "visual row {index}, char {char_col} must round-trip"
                );
            }
        }
    }
}
