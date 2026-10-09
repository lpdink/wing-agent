//! Table rendering — framed grid with content-aware width allocation.
//!
//! Visual style (**重框三档**, see [`TableSkin::framed`]): a heavy outer frame
//! wraps the table, the header band is heavy (its separator and its own column
//! dividers), and the body grid is light — three tiers carried by stroke
//! weight alone, all strokes sharing one quiet ink (the theme `border` colour).
//! Cell padding honours column alignment (left / center / right); cell content
//! word-wraps instead of truncating (zero information loss).
//!
//! ## Width allocation
//!
//! Columns are classified as [`ColumnKind::Narrative`] (long prose),
//! [`ColumnKind::TokenHeavy`] (paths, URLs, hashes), or [`ColumnKind::Compact`]
//! (short values such as counts or status labels). When the table overflows the
//! available width, token-heavy columns surrender excess width before narrative
//! prose, and compact columns are preserved last — so an oversized path does not
//! collapse readable prose into an unreadable narrow strip. The maths itself
//! lives in [`crate::render::table`], shared with the CLI table renderer.

use pulldown_cmark::Alignment;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::types::MarkdownLine;
use super::types::MarkdownSegment;
use super::types::SegmentKind;
use crate::render::table::ColumnMetrics;
use crate::render::table::LONG_TOKEN_WIDTH;
use crate::render::table::RuleGlyphs;
use crate::render::table::TableSkin;
use crate::render::table::classify_column;
use crate::render::table::compute_column_widths;
use crate::render::table::frame_overhead;
use crate::render::table::longest_token_width;
use crate::render::table::split_str_by_width;

/// Spaces of padding on each side of a cell's content.
const CELL_PADDING: usize = 1;

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
/// `frame_style` inks every glyph of the frame and the grid (callers pass the
/// theme's `border` style); `available_width` is the full content width the
/// table may occupy (caller has already subtracted any line prefix). When
/// `Some`, column widths are shrunk to fit; the rendered lines are guaranteed
/// not to exceed this width so downstream wrapping never breaks the layout
/// mid-row.
pub(crate) fn render_table(
    table: &TableBuffer,
    frame_style: Style,
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

    // Content budget = available width minus the frame (outer verticals, cell
    // padding, inner dividers). This is the space the column *content* widths
    // may collectively use.
    let content_budget =
        available_width.map(|avail| (avail as usize).saturating_sub(frame_overhead(col_count)));

    let col_widths = compute_column_widths(&metrics, content_budget);
    let skin = TableSkin::framed();

    lines.push(rule_line(&skin.top, &col_widths, frame_style));

    // Header band: the heavy row and its heavy separator.
    if !table.headers.is_empty() {
        lines.extend(render_row(
            &table.headers,
            &RowEnv {
                col_widths: &col_widths,
                alignments: &alignments,
                base_style,
                frame_style,
                outer_v: skin.outer_v,
                divider: skin.header_v,
                bold: true,
            },
        ));
        lines.push(rule_line(&skin.header_sep, &col_widths, frame_style));
    }

    // Body rows with a light rule between each pair.
    for (row_idx, row) in table.rows.iter().enumerate() {
        lines.extend(render_row(
            row,
            &RowEnv {
                col_widths: &col_widths,
                alignments: &alignments,
                base_style,
                frame_style,
                outer_v: skin.outer_v,
                divider: skin.inner_v,
                bold: false,
            },
        ));
        if row_idx + 1 < table.rows.len() {
            lines.push(rule_line(&skin.body_sep, &col_widths, frame_style));
        }
    }

    lines.push(rule_line(&skin.bottom, &col_widths, frame_style));

    lines
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

        let kind = classify_column(
            avg_words_per_cell,
            avg_cell_width,
            long_body_token_count,
            body_token_count,
        );

        metrics.push(ColumnMetrics {
            max_width,
            header_token_width,
            body_token_width,
            kind,
        });
    }
    metrics
}

/// Render a horizontal rule spanning all columns.
///
/// Each column contributes `width + 2*CELL_PADDING` fill characters, joined by
/// the rule's junction glyph — matching the row layout exactly, so every line
/// of the table has the same display width.
fn rule_line(glyphs: &RuleGlyphs, col_widths: &[usize], style: Style) -> MarkdownLine {
    let mut line = MarkdownLine::default();
    let left = glyphs.left.to_string();
    let junction = glyphs.junction.to_string();
    let fill = glyphs.fill.to_string();
    let right = glyphs.right.to_string();

    line.push_segment(SegmentKind::Border, style, &left);
    for (i, &w) in col_widths.iter().enumerate() {
        line.push_segment(
            SegmentKind::Border,
            style,
            &fill.repeat(w + CELL_PADDING * 2),
        );
        if i + 1 < col_widths.len() {
            line.push_segment(SegmentKind::Border, style, &junction);
        }
    }
    line.push_segment(SegmentKind::Border, style, &right);
    line
}

/// What one row needs from its table: the column geometry plus the ink and
/// glyph choices for the row's tier (heavy header band vs light body grid).
struct RowEnv<'a> {
    col_widths: &'a [usize],
    alignments: &'a [Alignment],
    base_style: Style,
    frame_style: Style,
    outer_v: char,
    divider: char,
    bold: bool,
}

/// Render a single table row (possibly multi-line after wrapping) as one or
/// more full-width grid lines.
///
/// Every line draws the complete row — all columns (empty cells included) and
/// both frame verticals — because a missing column would leave a hole in the
/// grid. Cells are alignment-aware padded; `env.divider` is the column
/// separator for this row's tier (heavy inside the header band, light in the
/// body).
fn render_row(row: &[MarkdownLine], env: &RowEnv<'_>) -> Vec<MarkdownLine> {
    let col_widths = env.col_widths;
    let wrapped_cells: Vec<Vec<Vec<MarkdownSegment>>> = col_widths
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let cell = row.get(i).cloned().unwrap_or_default();
            wrap_cell(&cell, w)
        })
        .collect();
    let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1);

    let outer = env.outer_v.to_string();
    let divider = env.divider.to_string();
    let mut out = Vec::with_capacity(row_height);
    for line_idx in 0..row_height {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Border, env.frame_style, &outer);
        for col in 0..col_widths.len() {
            let width = col_widths[col];
            let segments = wrapped_cells[col]
                .get(line_idx)
                .cloned()
                .unwrap_or_default();
            let content_width: usize = segments.iter().map(|s| s.width()).sum();
            let remaining = width.saturating_sub(content_width);
            let (left_pad, right_pad) = match env.alignments[col] {
                Alignment::Left | Alignment::None => (0, remaining),
                Alignment::Center => (remaining / 2, remaining - remaining / 2),
                Alignment::Right => (remaining, 0),
            };

            // Padding is layout whitespace within the prose area.
            line.push_segment(SegmentKind::Text, env.base_style, &" ".repeat(CELL_PADDING));
            if left_pad > 0 {
                line.push_segment(SegmentKind::Text, env.base_style, &" ".repeat(left_pad));
            }
            for seg in &segments {
                let style = if env.bold {
                    seg.style.bold()
                } else {
                    seg.style
                };
                line.push_segment(seg.kind, style, &seg.text);
            }
            if right_pad > 0 {
                line.push_segment(SegmentKind::Text, env.base_style, &" ".repeat(right_pad));
            }
            line.push_segment(SegmentKind::Text, env.base_style, &" ".repeat(CELL_PADDING));
            if col + 1 < col_widths.len() {
                line.push_segment(SegmentKind::Border, env.frame_style, &divider);
            }
        }
        line.push_segment(SegmentKind::Border, env.frame_style, &outer);
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
                    // Safety valve (same as `wrap::hard_break`): a width-2
                    // glyph cannot fit a degenerate 1-column budget, so the
                    // split hands back an empty head. Advance one char
                    // anyway — the row overflows by the glyph's width, but
                    // the loop must consume its input (it used to spin and
                    // grow `lines` without bound on a narrow CJK table).
                    let (head, tail) = if head.is_empty() {
                        let end = remaining
                            .char_indices()
                            .nth(1)
                            .map_or(remaining.len(), |(i, _)| i);
                        (&remaining[..end], &remaining[end..])
                    } else {
                        (head, tail)
                    };
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

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::table::ColumnKind;

    fn make_line(text: &str) -> MarkdownLine {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new(), text);
        line
    }

    fn plain_lines(lines: &[MarkdownLine]) -> Vec<String> {
        lines.iter().map(|l| l.to_plain()).collect()
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

    /// Regression: a 1-column budget with a width-2 glyph used to hand back an
    /// empty head forever — the loop spun and grew `lines` without bound (a
    /// narrow CJK table hung the renderer; `theme_preview --width 11` and the
    /// baseline `render_probe` both reproduced it). The safety valve must
    /// consume the input, overflowing the row by the glyph's width.
    #[test]
    fn test_wrap_cell_degenerate_width_consumes_input() {
        let cell = make_line("中文字");
        let wrapped = wrap_cell(&cell, 1);
        let text: String = wrapped
            .iter()
            .flat_map(|line| line.iter().map(|s| s.text.clone()))
            .collect();
        assert_eq!(text, "中文字", "每列 1 格时也必须消费完输入");
        assert!(
            wrapped.iter().all(|line| line.len() <= 1),
            "每个宽字形独占一行：{wrapped:?}"
        );
    }

    /// The same degenerate budget at the table level: `share = 1` columns must
    /// terminate and still produce a bordered table (rows may overflow).
    #[test]
    fn test_render_table_degenerate_budget_terminates_with_cjk() {
        let table = TableBuffer {
            headers: vec![make_line("槽位"), make_line("岗位")],
            rows: vec![vec![make_line("text"), make_line("正文")]],
            ..Default::default()
        };
        // 2 columns à 1 content column + chrome ≈ the degenerate share.
        let lines = render_table(&table, Style::new(), Style::new(), Some(12));
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_wrap_cell_empty() {
        let cell = MarkdownLine::default();
        let wrapped = wrap_cell(&cell, 10);
        assert_eq!(wrapped.len(), 1);
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

    /// Exact shape of the framed grid: heavy frame, heavy header band, light
    /// body grid — one line per visual line, every line the same width.
    #[test]
    fn test_render_table_framed_grid_shape() {
        let table = TableBuffer {
            headers: vec![make_line("A"), make_line("B")],
            rows: vec![vec![make_line("1"), make_line("2")]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Style::new(), Some(80));
        let expected = [
            "┏━━━━━┳━━━━━┓",
            "┃ A   ┃ B   ┃",
            "┣━━━━━╇━━━━━┫",
            "┃ 1   │ 2   ┃",
            "┗━━━━━┷━━━━━┛",
        ];
        assert_eq!(plain_lines(&lines), expected);
    }

    /// Every line of a table is exactly as wide as every other one — the frame
    /// closes on both sides and cells are padded to their column width — so
    /// downstream layout (and the equal-width CLI grids) can trust the
    /// geometry even when a cell wraps.
    #[test]
    fn test_render_table_lines_are_equal_width() {
        let table = TableBuffer {
            headers: vec![make_line("Name"), make_line("Description")],
            rows: vec![
                vec![make_line("one"), make_line("short")],
                vec![
                    make_line("two"),
                    make_line("this description wraps onto several lines at width"),
                ],
            ],
            ..Default::default()
        };
        for avail in [24u16, 40, 80] {
            let lines = render_table(&table, Style::new(), Style::new(), Some(avail));
            let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
            assert!(
                widths.iter().all(|w| *w == widths[0]),
                "avail={avail}: {widths:?}"
            );
            assert!(widths[0] <= avail as usize, "avail={avail}: {}", widths[0]);
        }
    }

    #[test]
    fn test_render_table_body_separator_between_rows() {
        let table = TableBuffer {
            headers: vec![make_line("H")],
            rows: vec![vec![make_line("a")], vec![make_line("b")]],
            ..Default::default()
        };
        let lines = render_table(&table, Style::new(), Style::new(), Some(40));
        let text = plain_lines(&lines).join("\n");
        // The heavy header separator and the light body rule between the two
        // body rows, each with its own junction glyphs.
        assert!(text.contains("┣━━━━━┫"), "header separator missing: {text}");
        assert!(text.contains("┠─────┨"), "body separator missing: {text}");
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
            let lines = render_table(&table, Style::new(), Style::new(), Some(avail));
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
        let lines = render_table(&table, Style::new(), Style::new(), Some(30));
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
        let lines = render_table(&table, Style::new(), Style::new(), Some(20));
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
        let lines = render_table(&table, Style::new(), Style::new(), Some(40));
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
        let lines = render_table(&table, Style::new(), Style::new(), Some(40));
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
        let lines = render_table(&table, Style::new(), Style::new(), Some(40));
        assert!(!lines.is_empty());
    }
}
