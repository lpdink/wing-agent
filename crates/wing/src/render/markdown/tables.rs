//! Table rendering — borderless layout with content-aware width allocation.
//!
//! Visual style (adapted from codex, MIT license):
//! - Columns are separated by gaps + cell padding instead of `│` borders
//! - A heavy `━` rule sits under the header; light `─` rules separate body rows
//! - Cell padding honors column alignment (left / center / right)
//! - Cell content word-wraps instead of truncating (zero information loss)
//!
//! ## Width allocation
//!
//! Columns are classified as [`ColumnKind::Narrative`] (long prose),
//! [`ColumnKind::TokenHeavy`] (paths, URLs, hashes), or [`ColumnKind::Compact`]
//! (short values such as counts or status labels). When the table overflows the
//! available width, token-heavy columns surrender excess width before narrative
//! prose, and compact columns are preserved last — so an oversized path does not
//! collapse readable prose into an unreadable narrow strip.

use pulldown_cmark::Alignment;
use ratatui::style::Style;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::types::MarkdownLine;
use super::types::MarkdownSegment;
use super::types::SegmentKind;

/// Spaces between adjacent columns.
const COLUMN_GAP: usize = 2;
/// Spaces of padding on each side of a cell's content.
const CELL_PADDING: usize = 1;
/// Rule character drawn under the header row.
const HEADER_SEPARATOR_CHAR: char = '━';
/// Rule character drawn between body rows.
const BODY_SEPARATOR_CHAR: char = '─';
/// Hard minimum column width.
const MIN_COLUMN_WIDTH: usize = 3;
/// Soft readable floor for narrative / token-heavy columns.
const PREFERRED_FLOOR: usize = 16;
/// A whitespace token at least this wide marks its column as token-heavy.
const LONG_TOKEN_WIDTH: usize = 20;

/// Accumulates table rows during markdown parsing.
#[derive(Debug, Default)]
pub(crate) struct TableBuffer {
    pub(crate) headers: Vec<MarkdownLine>,
    pub(crate) rows: Vec<Vec<MarkdownLine>>,
    pub(crate) current_row: Vec<MarkdownLine>,
    pub(crate) in_head: bool,
    pub(crate) alignments: Vec<Alignment>,
}

/// Render a table buffer with word-wrap and content-aware column balancing.
///
/// `available_width` is the full content width the table may occupy (caller has
/// already subtracted any line prefix). When `Some`, column widths are shrunk to
/// fit; the rendered lines are guaranteed not to exceed this width so downstream
/// wrapping never breaks the layout mid-row.
pub(crate) fn render_table(
    table: &TableBuffer,
    base_style: Style,
    available_width: Option<u16>,
) -> Vec<MarkdownLine> {
    let mut lines = Vec::new();
    if table.headers.is_empty() && table.rows.is_empty() {
        return lines;
    }

    let col_count = table
        .headers
        .len()
        .max(table.rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if col_count == 0 {
        return lines;
    }

    // Normalize alignments to the column count.
    let mut alignments = table.alignments.clone();
    alignments.resize(col_count, Alignment::None);

    let metrics = collect_column_metrics(&table.headers, &table.rows, col_count);

    // Content budget = available width minus per-column padding and inter-column
    // gaps. This is the space the column *content* widths may collectively use.
    let content_budget = available_width.map(|avail| {
        let overhead = col_count * CELL_PADDING * 2 + col_count.saturating_sub(1) * COLUMN_GAP;
        (avail as usize).saturating_sub(overhead)
    });

    let col_widths = compute_column_widths(&metrics, content_budget);

    let separator_style = base_style.dim();

    // Header row + heavy rule.
    if !table.headers.is_empty() {
        lines.extend(render_row(
            &table.headers,
            &col_widths,
            &alignments,
            base_style,
            true,
        ));
        lines.push(render_separator(
            &col_widths,
            HEADER_SEPARATOR_CHAR,
            separator_style,
        ));
    }

    // Body rows with a light rule between each pair.
    for (row_idx, row) in table.rows.iter().enumerate() {
        lines.extend(render_row(row, &col_widths, &alignments, base_style, false));
        if row_idx + 1 < table.rows.len() {
            lines.push(render_separator(
                &col_widths,
                BODY_SEPARATOR_CHAR,
                separator_style,
            ));
        }
    }

    lines
}

/// Classification of a table column for width-allocation priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnKind {
    /// Long-form prose content.
    Narrative,
    /// Paths, URLs, hashes — long unbroken tokens.
    TokenHeavy,
    /// Short values such as counts or status labels.
    Compact,
}

/// Per-column measurements used for width allocation.
struct ColumnMetrics {
    /// Widest cell content (display width) in this column.
    max_width: usize,
    /// Widest whitespace token in the header cell.
    header_token_width: usize,
    /// Widest whitespace token across body cells.
    body_token_width: usize,
    kind: ColumnKind,
}

/// Gather width/token statistics for each column and classify it.
fn collect_column_metrics(
    headers: &[MarkdownLine],
    rows: &[Vec<MarkdownLine>],
    col_count: usize,
) -> Vec<ColumnMetrics> {
    let mut metrics = Vec::with_capacity(col_count);
    for col in 0..col_count {
        let header_plain = headers.get(col).map(|h| h.to_plain()).unwrap_or_default();
        let header_token_width = longest_token_width(&header_plain);
        let mut max_width = headers.get(col).map(|h| h.width()).unwrap_or(0);
        let mut body_token_width = 0usize;
        let mut body_token_count = 0usize;
        let mut long_body_token_count = 0usize;
        let mut total_words = 0usize;
        let mut total_cells = 0usize;
        let mut total_cell_width = 0usize;

        for row in rows {
            let plain = row.get(col).map(|c| c.to_plain()).unwrap_or_default();
            let cell_width = row.get(col).map(|c| c.width()).unwrap_or(0);
            max_width = max_width.max(cell_width);
            body_token_width = body_token_width.max(longest_token_width(&plain));
            let word_count = plain.split_whitespace().count();
            if word_count > 0 {
                body_token_count += word_count;
                long_body_token_count += plain
                    .split_whitespace()
                    .filter(|token| token.width() >= LONG_TOKEN_WIDTH)
                    .count();
                total_words += word_count;
                total_cells += 1;
                total_cell_width += UnicodeWidthStr::width(plain.as_str());
            }
        }

        let avg_words_per_cell = if total_cells == 0 {
            header_plain.split_whitespace().count() as f64
        } else {
            total_words as f64 / total_cells as f64
        };
        let avg_cell_width = if total_cells == 0 {
            UnicodeWidthStr::width(header_plain.as_str()) as f64
        } else {
            total_cell_width as f64 / total_cells as f64
        };

        let kind = if long_body_token_count > 0
            && long_body_token_count >= body_token_count.saturating_sub(long_body_token_count)
        {
            ColumnKind::TokenHeavy
        } else if avg_words_per_cell >= 4.0 || avg_cell_width >= 28.0 {
            ColumnKind::Narrative
        } else {
            ColumnKind::Compact
        };

        metrics.push(ColumnMetrics {
            max_width,
            header_token_width,
            body_token_width,
            kind,
        });
    }
    metrics
}

/// Allocate column content widths so the table fits within `content_budget`.
///
/// Each column starts at its natural (max cell content) width, then columns are
/// shrunk one character at a time until the total fits. Token-heavy columns
/// shrink before narrative prose; compact columns are preserved last. Always
/// returns widths whose sum is `<= content_budget` (when a budget is given).
fn compute_column_widths(metrics: &[ColumnMetrics], content_budget: Option<usize>) -> Vec<usize> {
    let col_count = metrics.len();
    let mut widths: Vec<usize> = metrics
        .iter()
        .map(|m| m.max_width.max(MIN_COLUMN_WIDTH))
        .collect();

    let Some(budget) = content_budget else {
        return widths;
    };
    if col_count == 0 {
        return widths;
    }

    // Degenerate budget: cannot even hold minimum-width columns. Split evenly.
    let min_total = col_count * MIN_COLUMN_WIDTH;
    if budget < min_total {
        let share = (budget / col_count).max(1);
        return vec![share; col_count];
    }

    // Preferred floors, relaxed in shrink-priority order until they fit.
    let mut floors: Vec<usize> = metrics
        .iter()
        .map(|m| preferred_column_floor(m, MIN_COLUMN_WIDTH))
        .collect();
    let mut floor_total: usize = floors.iter().sum();
    while floor_total > budget {
        let Some((idx, _)) = floors
            .iter()
            .enumerate()
            .filter(|(_, floor)| **floor > MIN_COLUMN_WIDTH)
            .min_by_key(|(idx, floor)| {
                (
                    shrink_priority(metrics[*idx].kind),
                    usize::MAX.saturating_sub(**floor),
                )
            })
        else {
            break;
        };
        floors[idx] -= 1;
        floor_total -= 1;
    }

    // Shrink columns one char at a time until the total fits the budget.
    let mut total: usize = widths.iter().sum();
    while total > budget {
        let Some(idx) = next_column_to_shrink(&widths, &floors, metrics) else {
            break;
        };
        widths[idx] -= 1;
        total -= 1;
    }

    widths
}

/// Preferred minimum width for a column before the shrink loop runs.
///
/// Narrative and token-heavy columns keep a readable 16-cell soft floor; compact
/// columns floor at the wider of their header/body token widths (body capped at
/// 16). Clamped to `[min, max_width]`.
fn preferred_column_floor(metrics: &ColumnMetrics, min: usize) -> usize {
    let target = match metrics.kind {
        ColumnKind::Narrative | ColumnKind::TokenHeavy => PREFERRED_FLOOR,
        ColumnKind::Compact => metrics
            .header_token_width
            .max(metrics.body_token_width.min(PREFERRED_FLOOR)),
    };
    target.max(min).min(metrics.max_width.max(min))
}

/// Pick the next column to shrink by one character.
///
/// Priority: TokenHeavy before Narrative before Compact. Within the same kind,
/// the column with the most slack above its floor shrinks first so similarly
/// shaped columns stay balanced.
fn next_column_to_shrink(
    widths: &[usize],
    floors: &[usize],
    metrics: &[ColumnMetrics],
) -> Option<usize> {
    widths
        .iter()
        .enumerate()
        .filter(|(idx, width)| **width > floors[*idx])
        .min_by_key(|(idx, width)| {
            let slack = width.saturating_sub(floors[*idx]);
            (
                shrink_priority(metrics[*idx].kind),
                usize::MAX.saturating_sub(slack),
            )
        })
        .map(|(idx, _)| idx)
}

fn shrink_priority(kind: ColumnKind) -> usize {
    match kind {
        ColumnKind::TokenHeavy => 0,
        ColumnKind::Narrative => 1,
        ColumnKind::Compact => 2,
    }
}

/// Longest whitespace-delimited token width in `text` (CJK-aware).
fn longest_token_width(text: &str) -> usize {
    text.split_whitespace()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

/// Render a horizontal rule spanning all columns.
///
/// Each column contributes `width + 2*CELL_PADDING` rule characters, joined by
/// `COLUMN_GAP` spaces — matching the row layout exactly.
fn render_separator(col_widths: &[usize], ch: char, style: Style) -> MarkdownLine {
    let mut line = MarkdownLine::default();
    let segment = ch.to_string();
    for (i, &w) in col_widths.iter().enumerate() {
        line.push_segment(
            SegmentKind::Border,
            style,
            &segment.repeat(w + CELL_PADDING * 2),
        );
        if i + 1 < col_widths.len() {
            line.push_segment(SegmentKind::Border, style, &" ".repeat(COLUMN_GAP));
        }
    }
    line
}

/// Render a single table row (possibly multi-line after wrapping).
///
/// Columns are gap-separated with alignment-aware padding. Trailing columns that
/// are empty on a given line are trimmed so rows do not carry useless padding.
fn render_row(
    row: &[MarkdownLine],
    col_widths: &[usize],
    alignments: &[Alignment],
    base_style: Style,
    bold: bool,
) -> Vec<MarkdownLine> {
    let wrapped_cells: Vec<Vec<Vec<MarkdownSegment>>> = col_widths
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let cell = row.get(i).cloned().unwrap_or_default();
            wrap_cell(&cell, w)
        })
        .collect();
    let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1);

    let mut out = Vec::with_capacity(row_height);
    for line_idx in 0..row_height {
        // Rightmost column with visible content on this line.
        let last_visible = wrapped_cells.iter().rposition(|cell_lines| {
            cell_lines
                .get(line_idx)
                .is_some_and(|segs| segs.iter().any(|s| !s.text.is_empty()))
        });
        let Some(last) = last_visible else {
            out.push(MarkdownLine::default());
            continue;
        };

        let mut line = MarkdownLine::default();
        for col in 0..=last {
            let width = col_widths[col];
            let segments = wrapped_cells[col]
                .get(line_idx)
                .cloned()
                .unwrap_or_default();
            let content_width: usize = segments.iter().map(|s| s.width()).sum();
            let remaining = width.saturating_sub(content_width);
            let (left_pad, right_pad) = match alignments[col] {
                Alignment::Left | Alignment::None => (0, remaining),
                Alignment::Center => (remaining / 2, remaining - remaining / 2),
                Alignment::Right => (remaining, 0),
            };
            let is_last = col == last;

            // Padding is layout whitespace within the prose area.
            line.push_segment(SegmentKind::Text, base_style, &" ".repeat(CELL_PADDING));
            if left_pad > 0 {
                line.push_segment(SegmentKind::Text, base_style, &" ".repeat(left_pad));
            }
            for seg in &segments {
                let style = if bold { seg.style.bold() } else { seg.style };
                line.push_segment(seg.kind, style, &seg.text);
            }
            if !is_last {
                if right_pad > 0 {
                    line.push_segment(SegmentKind::Text, base_style, &" ".repeat(right_pad));
                }
                line.push_segment(SegmentKind::Text, base_style, &" ".repeat(CELL_PADDING));
                line.push_segment(SegmentKind::Text, base_style, &" ".repeat(COLUMN_GAP));
            }
        }
        out.push(line);
    }
    out
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
                current_line.push(MarkdownSegment::new(seg.kind, seg.style, word));
                current_width += word_width;
            } else if word_width <= width {
                // Word fits on a new line.
                lines.push(std::mem::take(&mut current_line));
                let trimmed = word.trim_start();
                current_line.push(MarkdownSegment::new(
                    seg.kind,
                    seg.style,
                    trimmed.to_string(),
                ));
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
                        current_line.push(MarkdownSegment::new(
                            seg.kind,
                            seg.style,
                            head.to_string(),
                        ));
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
pub(crate) fn split_str_by_width(text: &str, max_width: usize) -> (&str, &str) {
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

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_line(text: &str) -> MarkdownLine {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new(), text);
        line
    }

    fn plain_lines(lines: &[MarkdownLine]) -> Vec<String> {
        lines.iter().map(|l| l.to_plain()).collect()
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
    fn test_longest_token_width() {
        assert_eq!(longest_token_width("a bb ccc"), 3);
        assert_eq!(longest_token_width(""), 0);
        assert_eq!(longest_token_width("你好 ab"), 4); // 你好 = 4 cols
    }

    #[test]
    fn test_classify_token_heavy() {
        // A column of long paths classifies as TokenHeavy.
        let headers = vec![make_line("Path")];
        let rows = vec![
            vec![make_line("libs/core/wing/config.py")],
            vec![make_line("libs/core/wing/session_manager.py")],
        ];
        let metrics = collect_column_metrics(&headers, &rows, 1);
        assert_eq!(metrics[0].kind, ColumnKind::TokenHeavy);
    }

    #[test]
    fn test_classify_compact() {
        let headers = vec![make_line("Count")];
        let rows = vec![vec![make_line("1")], vec![make_line("2")]];
        let metrics = collect_column_metrics(&headers, &rows, 1);
        assert_eq!(metrics[0].kind, ColumnKind::Compact);
    }

    #[test]
    fn test_classify_narrative() {
        let headers = vec![make_line("Description")];
        let rows = vec![vec![make_line(
            "This is a fairly long prose description that reads like narrative text",
        )]];
        let metrics = collect_column_metrics(&headers, &rows, 1);
        assert_eq!(metrics[0].kind, ColumnKind::Narrative);
    }

    #[test]
    fn test_compute_widths_no_budget_keeps_natural() {
        let metrics = vec![
            ColumnMetrics {
                max_width: 10,
                header_token_width: 3,
                body_token_width: 5,
                kind: ColumnKind::Compact,
            },
            ColumnMetrics {
                max_width: 20,
                header_token_width: 3,
                body_token_width: 5,
                kind: ColumnKind::Compact,
            },
        ];
        let widths = compute_column_widths(&metrics, None);
        assert_eq!(widths, vec![10, 20]);
    }

    #[test]
    fn test_compute_widths_fits_budget() {
        let metrics = vec![
            ColumnMetrics {
                max_width: 10,
                header_token_width: 3,
                body_token_width: 5,
                kind: ColumnKind::Compact,
            },
            ColumnMetrics {
                max_width: 80,
                header_token_width: 3,
                body_token_width: 60,
                kind: ColumnKind::TokenHeavy,
            },
            ColumnMetrics {
                max_width: 40,
                header_token_width: 3,
                body_token_width: 30,
                kind: ColumnKind::Narrative,
            },
        ];
        let widths = compute_column_widths(&metrics, Some(60));
        let total: usize = widths.iter().sum();
        assert!(total <= 60, "total {total} exceeds budget 60: {widths:?}");
        assert!(widths.iter().all(|&w| w >= 1));
    }

    #[test]
    fn test_compute_widths_token_heavy_shrinks_first() {
        // TokenHeavy column should give up width before the Narrative column.
        let metrics = vec![
            ColumnMetrics {
                max_width: 60,
                header_token_width: 3,
                body_token_width: 50,
                kind: ColumnKind::TokenHeavy,
            },
            ColumnMetrics {
                max_width: 60,
                header_token_width: 3,
                body_token_width: 30,
                kind: ColumnKind::Narrative,
            },
        ];
        let widths = compute_column_widths(&metrics, Some(60));
        assert!(
            widths[1] >= widths[0],
            "narrative ({}) should retain at least as much width as token-heavy ({}): {widths:?}",
            widths[1],
            widths[0]
        );
    }

    #[test]
    fn test_compute_widths_degenerate_budget() {
        let metrics = vec![
            ColumnMetrics {
                max_width: 10,
                header_token_width: 3,
                body_token_width: 5,
                kind: ColumnKind::Compact,
            },
            ColumnMetrics {
                max_width: 10,
                header_token_width: 3,
                body_token_width: 5,
                kind: ColumnKind::Compact,
            },
            ColumnMetrics {
                max_width: 10,
                header_token_width: 3,
                body_token_width: 5,
                kind: ColumnKind::Compact,
            },
        ];
        // Budget too small for 3 * MIN_COLUMN_WIDTH.
        let widths = compute_column_widths(&metrics, Some(6));
        assert_eq!(widths.len(), 3);
        assert!(widths.iter().all(|&w| w >= 1));
    }

    #[test]
    fn test_render_table_basic_borderless() {
        let table = TableBuffer {
            headers: vec![make_line("A"), make_line("B")],
            rows: vec![vec![make_line("1"), make_line("2")]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Some(80));
        let text = plain_lines(&lines).join("\n");
        assert!(text.contains("A"), "header missing: {text}");
        assert!(text.contains("1"), "data missing: {text}");
        // Borderless style: no vertical bars, heavy header rule present.
        assert!(!text.contains("│"), "should be borderless: {text}");
        assert!(text.contains("━"), "header rule missing: {text}");
    }

    #[test]
    fn test_render_table_body_separator_between_rows() {
        let table = TableBuffer {
            headers: vec![make_line("H")],
            rows: vec![vec![make_line("a")], vec![make_line("b")]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Some(40));
        let text = plain_lines(&lines).join("\n");
        // Light rule between the two body rows.
        assert!(text.contains("─"), "body separator missing: {text}");
    }

    #[test]
    fn test_render_table_lines_fit_available_width() {
        // The critical invariant: no rendered line exceeds the available width,
        // so downstream Paragraph wrapping never breaks a row mid-line.
        let table = TableBuffer {
            headers: vec![
                make_line("Implementation"),
                make_line("Path"),
                make_line("Reuse"),
            ],
            rows: vec![
                vec![
                    make_line("_ensure_tmp_dir / _save_full_result pattern"),
                    make_line("libs/wing_hooks/wing_hooks/truncate_tool_result.py"),
                    make_line(
                        "Reference its temp file writing approach and write equivalent private methods in agent.py",
                    ),
                ],
                vec![
                    make_line("get_wing_home()"),
                    make_line("libs/core/wing/config.py:L107"),
                    make_line("Call directly to determine the tmp directory"),
                ],
            ],
            ..Default::default()
        };
        for avail in [40u16, 60, 80, 120] {
            let lines = render_table(&table, Style::new(), Some(avail));
            for line in &lines {
                assert!(
                    line.width() <= avail as usize,
                    "line width {} exceeds available {} (avail={avail}): {:?}",
                    line.width(),
                    avail,
                    line.to_plain()
                );
            }
        }
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
        let text = plain_lines(&lines).join("\n");
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
        let text = plain_lines(&lines).join("\n");
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
        let text = plain_lines(&lines).join("\n");
        assert!(text.contains("你好"), "CJK content missing: {text}");
    }

    #[test]
    fn test_render_table_right_alignment() {
        let table = TableBuffer {
            headers: vec![make_line("Name"), make_line("Count")],
            rows: vec![vec![make_line("x"), make_line("5")]],
            alignments: vec![Alignment::Left, Alignment::Right],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Some(40));
        // The right-aligned "5" should be preceded by padding spaces within
        // its column (i.e. appear with leading spaces before the column gap/end).
        let data_line = plain_lines(&lines)
            .into_iter()
            .find(|l| l.contains('5'))
            .expect("data line with 5");
        assert!(
            data_line.contains("5"),
            "right-aligned value missing: {data_line}"
        );
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
