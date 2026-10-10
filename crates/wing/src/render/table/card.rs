//! Info-card renderer: a framed block of single-line rows.
//!
//! Third consumer of the shared table machinery, next to the TUI markdown
//! tables ([`crate::render::markdown::tables`]) and the CLI plain-text tables
//! ([`super::plain`]). A card is deliberately *not* a data table — it has no
//! header band and no rules between its rows: the heavy frame closes the
//! block, every row is one line of information (the welcome nameplate's
//! version / session facts / keys / tip / entry column).
//!
//! What it shares is everything that makes the family read as one product:
//! the skin ([`TableSkin::framed`] — heavy frame, one quiet ink) and the width
//! accounting ([`frame_overhead`]).
//!
//! Rows **elide** (marked with `…`) instead of wrapping, like the CLI tables:
//! a card's row count is a layout contract (the welcome header keeps a
//! constant height per width band — a wrapped row would move everything below
//! it), so it degrades by losing the tail of a line, never by growing.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::TableSkin;
use super::frame_overhead;
use super::split_str_by_width;

/// Render `rows` as a framed card.
///
/// The card hugs its content: as wide as the widest row needs, clamped to
/// `[min_width, max_width]` (when the two conflict the minimum wins — an
/// aligned card beats a squeezed one). Rows wider than the final width elide
/// with `…`; every returned line is exactly as wide as the frame.
///
/// Degenerate case: a maximum below the frame's own width (4 columns) returns
/// **no lines** — the caller loses the rows entirely rather than getting a
/// broken frame. No caller reaches it (the welcome's narrowest column is 34),
/// so it is a guard, not a mode.
pub fn render(
    rows: &[Line<'static>],
    min_width: usize,
    max_width: usize,
    frame: Style,
) -> Vec<Line<'static>> {
    let width = content_width(rows).clamp(min_width, max_width.max(min_width));
    if width < frame_overhead(1) {
        return Vec::new();
    }
    let inner = width - frame_overhead(1);
    let skin = TableSkin::framed();

    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(rule_line(
        skin.top.fill,
        skin.top.left,
        skin.top.right,
        inner,
        frame,
    ));
    for row in rows {
        let mut spans = vec![
            Span::styled(skin.outer_v.to_string(), frame),
            Span::raw(" "),
        ];
        let content = fit_row(row, inner);
        let used: usize = content.iter().map(|span| span.content.width()).sum();
        spans.extend(content.spans);
        spans.push(Span::raw(" ".repeat(inner - used)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(skin.outer_v.to_string(), frame));
        out.push(Line::from(spans));
    }
    out.push(rule_line(
        skin.bottom.fill,
        skin.bottom.left,
        skin.bottom.right,
        inner,
        frame,
    ));
    out
}

/// The width the card wants for `rows`: the widest row plus the frame.
fn content_width(rows: &[Line<'static>]) -> usize {
    rows.iter()
        .map(|row| row.width())
        .max()
        .unwrap_or(0)
        .saturating_add(frame_overhead(1))
}

/// One border line: `left` + `fill` × (inner + 2) + `right`.
fn rule_line(fill: char, left: char, right: char, inner: usize, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(left.to_string(), style),
        Span::styled(fill.to_string().repeat(inner + 2), style),
        Span::styled(right.to_string(), style),
    ])
}

/// Fit a styled row into `max` display columns, marking a cut with `…`.
///
/// Whole spans survive while they fit; the first span that does not is cut on
/// a character boundary (CJK-aware, via the shared [`split_str_by_width`]).
///
/// Shared with the welcome's un-carded text column: the card and the plain
/// column degrade the same way (the card calls this for every row it draws).
pub fn fit_row(row: &Line<'static>, max: usize) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut budget = max;
    for span in &row.spans {
        let width = span.content.width();
        if width <= budget {
            budget -= width;
            out.push(span.clone());
            continue;
        }
        if budget == 0 {
            break;
        }
        // The ellipsis takes one of the remaining columns.
        let (head, _) = split_str_by_width(&span.content, budget - 1);
        out.push(Span::styled(format!("{head}…"), span.style));
        break;
    }
    Line::from(out)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Line<'static> {
        Line::from(text.to_string())
    }

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn width_of(line: &Line<'static>) -> usize {
        line.spans.iter().map(|s| s.content.width()).sum()
    }

    #[test]
    fn draws_the_frame_around_the_rows() {
        let lines = render(&[row("a"), row("bb")], 0, 80, Style::new());
        assert_eq!(text_of(&lines), ["┏━━━━┓", "┃ a  ┃", "┃ bb ┃", "┗━━━━┛",]);
    }

    #[test]
    fn the_card_hugs_its_content_between_the_bounds() {
        // Content 2 + frame 4 = 6 requested; a wider maximum is not stretched.
        assert_eq!(render(&[row("bb")], 0, 40, Style::new())[0].width(), 6);
        // …but the minimum is honoured (the welcome aligns it with the wordmark).
        assert_eq!(render(&[row("bb")], 20, 40, Style::new())[0].width(), 20);
        // …and the maximum caps it.
        assert_eq!(
            render(&[row("x".repeat(60).as_str())], 0, 30, Style::new())[0].width(),
            30
        );
    }

    #[test]
    fn every_line_is_the_same_width() {
        for rows in [
            vec![row("short")],
            vec![row("a much longer row that must elide"), row("x")],
            vec![row("")],
        ] {
            for (min, max) in [(0usize, 40usize), (24, 24), (10, 30)] {
                let lines = render(&rows, min, max, Style::new());
                let target = width_of(&lines[0]);
                assert!(target <= max, "min={min} max={max}: {target}");
                for line in &lines {
                    assert_eq!(width_of(line), target, "min={min} max={max}");
                }
            }
        }
    }

    #[test]
    fn long_rows_elide_instead_of_wrapping() {
        let lines = render(&[row("abcdefghij")], 10, 10, Style::new());
        assert_eq!(
            lines.len(),
            3,
            "one row in, one row out: {:?}",
            text_of(&lines)
        );
        assert_eq!(text_of(&lines)[1], "┃ abcde… ┃");
    }

    #[test]
    fn cjk_elision_counts_columns_not_chars() {
        // Inner width 6 columns: three CJK glyphs + `…` is 7 — only two fit.
        let lines = render(&[row("中文字符")], 10, 10, Style::new());
        assert_eq!(text_of(&lines)[1], "┃ 中文…  ┃");
        assert_eq!(width_of(&lines[1]), 10);
    }

    #[test]
    fn styled_rows_keep_their_spans_when_they_fit() {
        let styled = Line::from(vec![
            Span::styled("Tip  ", Style::new().bold()),
            Span::raw("body"),
        ]);
        let lines = render(&[styled], 0, 40, Style::new());
        let row_spans = &lines[1].spans;
        // border, padding, the two content spans, the trailing fill, border.
        assert_eq!(row_spans[2].content.as_ref(), "Tip  ");
        assert!(
            row_spans[2]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(row_spans[3].content.as_ref(), "body");
    }

    #[test]
    fn keeps_the_whole_row_until_the_budget_runs_out() {
        let styled = Line::from(vec![Span::raw("abcdefgh"), Span::raw("ijklmnop")]);
        let lines = render(&[styled], 0, 4 + 12, Style::new());
        // 12 inner columns: the first span (8) survives whole, the second is cut.
        let row_spans = &lines[1].spans;
        assert_eq!(row_spans[2].content.as_ref(), "abcdefgh");
        assert_eq!(row_spans[3].content.as_ref(), "ijk…");
    }

    #[test]
    fn degenerate_width_renders_nothing() {
        // Narrower than the frame itself: the caller never asks for this (the
        // welcome's bands are all wider), but the renderer must not panic or
        // emit a broken line.
        assert!(render(&[row("x")], 0, 3, Style::new()).is_empty());
    }
}
