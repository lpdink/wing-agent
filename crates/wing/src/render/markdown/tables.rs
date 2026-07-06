//! Table rendering with word-wrap and column width balancing.
//!
//! Adapted from VTCode (MIT license) for the table framework.
//! Word-wrap algorithm inspired by md-tui's approach (AGPL, algorithm only).
//!
//! Key features:
//! - Cell content word-wraps instead of truncating (zero information loss)
//! - Column widths are balanced proportionally when the table overflows
//! - CJK-aware display width calculation
//! - Multi-line rows with automatic height adjustment

use std::cmp::max;

use ratatui::style::Style;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::types::MarkdownLine;
use super::types::MarkdownSegment;

/// Accumulates table rows during markdown parsing.
#[derive(Debug, Default)]
pub(crate) struct TableBuffer {
    pub(crate) headers: Vec<MarkdownLine>,
    pub(crate) rows: Vec<Vec<MarkdownLine>>,
    pub(crate) current_row: Vec<MarkdownLine>,
    pub(crate) in_head: bool,
}

/// Render a table buffer with word-wrap and column balancing.
pub(crate) fn render_table(
    table: &TableBuffer,
    base_style: Style,
    available_width: Option<u16>,
) -> Vec<MarkdownLine> {
    let mut lines = Vec::new();
    if table.headers.is_empty() && table.rows.is_empty() {
        return lines;
    }

    let max_cols = table
        .headers
        .len()
        .max(table.rows.iter().map(|r| r.len()).max().unwrap_or(0));

    if max_cols == 0 {
        return lines;
    }

    // Calculate natural column widths.
    let mut col_widths: Vec<usize> = vec![0; max_cols];
    for (i, header) in table.headers.iter().enumerate() {
        col_widths[i] = max(col_widths[i], header.width());
    }
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate() {
            if i < max_cols {
                col_widths[i] = max(col_widths[i], cell.width());
            }
        }
    }

    // Calculate styling overhead: `│ ` + content + ` │ ` per column + trailing `│`
    // = 1 (leading │) + cols * (1 space + content + 1 space + 1 │ + 1 space)
    // Simplified: overhead = 1 + cols * 4  (│ space ... space │ space)
    let styling_width = 1 + max_cols * 4;

    // Balance column widths if table overflows available width.
    if let Some(avail) = available_width {
        let avail = avail as usize;
        let total_content: usize = col_widths.iter().sum();
        if total_content + styling_width > avail && total_content > 0 {
            col_widths = balance_column_widths(&col_widths, avail.saturating_sub(styling_width));
        }
    }

    let border_style = base_style.dim();

    // Render header.
    if !table.headers.is_empty() {
        // Wrap header cells.
        let wrapped_headers: Vec<Vec<Vec<MarkdownSegment>>> = table
            .headers
            .iter()
            .zip(col_widths.iter())
            .map(|(cell, &w)| wrap_cell(cell, w))
            .collect();
        let header_height = wrapped_headers.iter().map(|c| c.len()).max().unwrap_or(1);

        for row_line in 0..header_height {
            lines.push(render_table_row_line(
                &wrapped_headers,
                &col_widths,
                border_style,
                base_style,
                row_line,
                true,
            ));
        }

        // Separator.
        let mut sep = MarkdownLine::default();
        sep.push_segment(border_style, "├");
        for (i, &w) in col_widths.iter().enumerate() {
            sep.push_segment(border_style, &"─".repeat(w + 2));
            sep.push_segment(
                border_style,
                if i < col_widths.len() - 1 {
                    "┼"
                } else {
                    "┤"
                },
            );
        }
        lines.push(sep);
    }

    // Render body rows with word-wrap.
    for row in &table.rows {
        let wrapped_cells: Vec<Vec<Vec<MarkdownSegment>>> = row
            .iter()
            .zip(col_widths.iter())
            .map(|(cell, &w)| wrap_cell(cell, w))
            .collect();
        let row_height = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);

        for row_line in 0..row_height {
            lines.push(render_table_row_line(
                &wrapped_cells,
                &col_widths,
                border_style,
                base_style,
                row_line,
                false,
            ));
        }
    }

    lines
}

/// Balance column widths proportionally when the table overflows.
///
/// Strategy: columns wider than the average threshold are compressed
/// proportionally, while narrower columns keep their natural width.
fn balance_column_widths(natural_widths: &[usize], available: usize) -> Vec<usize> {
    let col_count = natural_widths.len();
    if col_count == 0 || available == 0 {
        return vec![1; col_count];
    }

    let threshold = available / col_count;
    let mut balanced = natural_widths.to_vec();

    // Identify overflowing and non-overflowing columns.
    let mut overflowing: Vec<(usize, usize)> = Vec::new(); // (index, width)
    let mut non_overflowing_total = 0usize;

    for (i, &w) in natural_widths.iter().enumerate() {
        if w > threshold {
            overflowing.push((i, w));
        } else {
            non_overflowing_total += w;
        }
    }

    if overflowing.is_empty() {
        // All columns fit — no balancing needed, but check total.
        let total: usize = balanced.iter().sum();
        if total <= available {
            return balanced;
        }
        // Proportional shrink as fallback.
        for w in &mut balanced {
            *w = (*w * available) / total.max(1);
            *w = (*w).max(1);
        }
        return balanced;
    }

    // Sort by width ascending so narrower overflowing columns get minimum
    // allocation first, leaving more space for wider columns.
    overflowing.sort_by_key(|&(_, w)| w);

    let overflowing_total: usize = overflowing.iter().map(|(_, w)| *w).sum();
    let available_for_overflow = available.saturating_sub(non_overflowing_total);
    let min_col_width = (available_for_overflow / (2 * overflowing.len())).max(1);

    for &(i, old_width) in &overflowing {
        let ratio = old_width as f64 / overflowing_total as f64;
        let mut new_width = (ratio * available_for_overflow as f64).floor() as usize;
        new_width = new_width.max(min_col_width);
        balanced[i] = new_width;
    }

    // Ensure minimum width of 1 for all columns.
    for w in &mut balanced {
        *w = (*w).max(1);
    }

    balanced
}

/// Wrap a cell's segments into multiple lines, each fitting within `width`.
///
/// Returns a `Vec<Vec<MarkdownSegment>>` where each inner vec is one line.
fn wrap_cell(cell: &MarkdownLine, width: usize) -> Vec<Vec<MarkdownSegment>> {
    if width == 0 {
        return vec![Vec::new()];
    }

    let mut lines: Vec<Vec<MarkdownSegment>> = Vec::new();
    let mut current_line: Vec<MarkdownSegment> = Vec::new();
    let mut current_width = 0usize;

    for seg in &cell.segments {
        let text = &seg.text;
        if text.is_empty() {
            continue;
        }

        // Try to fit words from this segment.
        let words = split_into_words(text);
        for word in words {
            let word_width = UnicodeWidthStr::width(word.as_str());

            if current_width + word_width <= width {
                // Fits on current line.
                current_line.push(MarkdownSegment::new(seg.style, word));
                current_width += word_width;
            } else if word_width <= width {
                // Word fits on a new line.
                lines.push(std::mem::take(&mut current_line));
                let trimmed = word.trim_start();
                current_line.push(MarkdownSegment::new(seg.style, trimmed.to_string()));
                current_width = UnicodeWidthStr::width(trimmed);
            } else {
                // Word is wider than column — hard break it.
                let mut remaining = word.as_str();
                while !remaining.is_empty() {
                    let space_left = width.saturating_sub(current_width);
                    if space_left == 0 {
                        lines.push(std::mem::take(&mut current_line));
                        current_width = 0;
                        continue;
                    }
                    let (head, tail) = split_str_by_width(remaining, space_left);
                    if !head.is_empty() {
                        current_line.push(MarkdownSegment::new(seg.style, head.to_string()));
                        current_width += UnicodeWidthStr::width(head);
                    }
                    if !tail.is_empty() {
                        lines.push(std::mem::take(&mut current_line));
                        current_width = 0;
                    }
                    remaining = tail;
                }
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        lines.push(Vec::new());
    }

    lines
}

/// Split text into word-boundary tokens (preserving spaces as separate tokens).
fn split_into_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_space = false;

    for ch in text.chars() {
        if ch == ' ' {
            if !in_space && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.push(ch);
            in_space = true;
        } else {
            if in_space && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.push(ch);
            in_space = false;
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Split a string at a display width boundary (CJK-safe).
///
/// Returns `(head, tail)` where `head` fits within `max_width` display columns.
fn split_str_by_width(text: &str, max_width: usize) -> (&str, &str) {
    if max_width == 0 {
        return ("", text);
    }
    let mut width = 0;
    for (i, ch) in text.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            return (&text[..i], &text[i..]);
        }
        width += cw;
    }
    (text, "")
}

/// Render a single line of a multi-line table row.
fn render_table_row_line(
    wrapped_cells: &[Vec<Vec<MarkdownSegment>>],
    col_widths: &[usize],
    border_style: Style,
    base_style: Style,
    line_idx: usize,
    bold: bool,
) -> MarkdownLine {
    let mut line = MarkdownLine::default();
    line.push_segment(border_style, "│");

    for (i, &width) in col_widths.iter().enumerate() {
        line.push_segment(base_style, " ");

        if let Some(cell_lines) = wrapped_cells.get(i) {
            if let Some(segments) = cell_lines.get(line_idx) {
                for seg in segments {
                    let style = if bold { seg.style.bold() } else { seg.style };
                    line.push_segment(style, &seg.text);
                }
                // Pad remaining width.
                let content_width: usize = segments.iter().map(|s| s.width()).sum();
                let padding = width.saturating_sub(content_width);
                if padding > 0 {
                    line.push_segment(base_style, &" ".repeat(padding));
                }
            } else {
                // Empty line for this cell — fill with spaces.
                line.push_segment(base_style, &" ".repeat(width));
            }
        } else {
            line.push_segment(base_style, &" ".repeat(width));
        }

        line.push_segment(base_style, " ");
        line.push_segment(border_style, "│");
    }

    line
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_line(text: &str) -> MarkdownLine {
        let mut line = MarkdownLine::default();
        line.push_segment(Style::new(), text);
        line
    }

    #[test]
    fn test_split_str_by_width_ascii() {
        assert_eq!(split_str_by_width("hello world", 5), ("hello", " world"));
        assert_eq!(split_str_by_width("hello", 10), ("hello", ""));
        assert_eq!(split_str_by_width("abc", 0), ("", "abc"));
    }

    #[test]
    fn test_split_str_by_width_cjk() {
        // Each CJK char = 2 columns.
        assert_eq!(split_str_by_width("你好世界", 4), ("你好", "世界"));
        assert_eq!(split_str_by_width("你好世界", 3), ("你", "好世界"));
        assert_eq!(split_str_by_width("a你b", 3), ("a你", "b"));
    }

    #[test]
    fn test_wrap_cell_short() {
        let cell = make_line("short");
        let wrapped = wrap_cell(&cell, 20);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn test_wrap_cell_long() {
        let cell = make_line("This is a very long description that needs wrapping");
        let wrapped = wrap_cell(&cell, 15);
        assert!(wrapped.len() > 1, "should wrap into multiple lines");
        // Verify no line exceeds width.
        for line_segments in &wrapped {
            let w: usize = line_segments.iter().map(|s| s.width()).sum();
            assert!(w <= 15, "line width {w} exceeds 15: {:?}", line_segments);
        }
    }

    #[test]
    fn test_wrap_cell_oversized_word() {
        let cell = make_line("supercalifragilistic");
        let wrapped = wrap_cell(&cell, 8);
        assert!(wrapped.len() >= 2, "should break long word");
        for line_segments in &wrapped {
            let w: usize = line_segments.iter().map(|s| s.width()).sum();
            assert!(w <= 8, "line width {w} exceeds 8");
        }
    }

    #[test]
    fn test_wrap_cell_empty() {
        let cell = MarkdownLine::default();
        let wrapped = wrap_cell(&cell, 10);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn test_balance_columns_fits() {
        let widths = vec![10, 15, 5];
        let balanced = balance_column_widths(&widths, 100);
        assert_eq!(balanced, widths);
    }

    #[test]
    fn test_balance_columns_overflow() {
        let widths = vec![10, 80, 10];
        let balanced = balance_column_widths(&widths, 50);
        let total: usize = balanced.iter().sum();
        assert!(
            total <= 50,
            "balanced total {total} should be <= 50: {balanced:?}"
        );
        // Each column should have at least width 1.
        assert!(balanced.iter().all(|&w| w >= 1));
    }

    #[test]
    fn test_balance_columns_single_overflow() {
        let widths = vec![10, 10, 200];
        let balanced = balance_column_widths(&widths, 60);
        // The first two columns should keep their natural width.
        assert_eq!(balanced[0], 10);
        assert_eq!(balanced[1], 10);
        // The third should get the remaining space.
        assert!(balanced[2] <= 40);
        assert!(balanced[2] >= 1);
    }

    #[test]
    fn test_render_table_basic() {
        let table = TableBuffer {
            headers: vec![make_line("A"), make_line("B")],
            rows: vec![vec![make_line("1"), make_line("2")]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Some(80));
        let text: String = lines
            .iter()
            .map(|l| l.to_plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("A"), "header missing: {text}");
        assert!(text.contains("1"), "data missing: {text}");
        assert!(text.contains("│"), "border missing: {text}");
    }

    #[test]
    fn test_render_table_wrapped_cell() {
        let table = TableBuffer {
            headers: vec![make_line("Name"), make_line("Description")],
            rows: vec![vec![
                make_line("item"),
                make_line("This is a very long description that should wrap"),
            ]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Some(30));
        // The wrapped row should produce multiple lines.
        assert!(
            lines.len() >= 4,
            "expected header + separator + multi-line row, got {} lines",
            lines.len()
        );
        let text: String = lines
            .iter()
            .map(|l| l.to_plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("should wrap"),
            "wrapped text should be visible: {text}"
        );
    }

    #[test]
    fn test_render_table_no_truncation() {
        let mut table = TableBuffer::default();
        let long_text = "This very long text must not be truncated under any circumstances";
        table.headers = vec![make_line("Col")];
        table.rows = vec![vec![make_line(long_text)]];
        let lines = render_table(&table, Style::new(), Some(20));
        let text: String = lines
            .iter()
            .map(|l| l.to_plain())
            .collect::<Vec<_>>()
            .join("\n");
        // All words must be preserved (they'll be on separate lines due to wrapping).
        for word in long_text.split_whitespace() {
            assert!(
                text.contains(word),
                "word '{word}' must be preserved, got: {text}"
            );
        }
    }

    #[test]
    fn test_render_table_cjk_width() {
        let table = TableBuffer {
            headers: vec![make_line("名称"), make_line("值")],
            rows: vec![vec![make_line("你好"), make_line("1")]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Some(40));
        let text: String = lines
            .iter()
            .map(|l| l.to_plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("你好"), "CJK content missing: {text}");
    }

    #[test]
    fn test_render_table_empty_cell() {
        let table = TableBuffer {
            headers: vec![make_line("A"), make_line("B")],
            rows: vec![vec![make_line(""), make_line("x")]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Some(40));
        assert!(!lines.is_empty());
    }
}
