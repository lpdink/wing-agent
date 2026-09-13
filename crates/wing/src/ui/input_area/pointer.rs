//! Composer pointer support — hit testing, drag selection and the highlight
//! patch for the input area.
//!
//! The composer's content space is **logical**: a position is a `(line, char)`
//! pair in [`InputArea`]'s `lines`, and it survives scrolling and re-wrapping
//! (only edits invalidate it — see `App::selection_guard`). The visual rows the
//! screen is made of are derived per frame with the very same [`wrap`]
//! functions the widget renders with, which is what keeps the two in sync.
//!
//! Why not the chat band's "snapshot the rendered buffer" approach: the chat's
//! wrapping happens inside ratatui's `Paragraph`, so the rendered buffer is the
//! only authority; the composer's wrapping is ours, so the model is. See the
//! change design (`tui-composer-pointer`) for the full rationale.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;

use super::InputArea;
use super::helpers::PREFIX_WIDTH;
use super::wrap;
use crate::ui::selection::Selection;
use crate::ui::selection::SelectionPoint;
use crate::ui::selection::SelectionRegion;

/// Where the pointer landed inside the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposerHit {
    /// Visual row under the pointer (index into the full visual-row list, i.e.
    /// the value [`super::InputArea::set_cursor_from_visual`] wants).
    pub vis_row: usize,
    /// Display column inside the text area: `0` is the first cell after the
    /// `> ` prefix. Values past the row's text point into its trailing blank.
    pub display_col: u16,
    /// Logical position the pointer resolves to: the insertion point at or
    /// before the character whose cells cover it.
    pub point: SelectionPoint,
    /// Whether the pointer actually rests on one of the cells of a character
    /// (as opposed to the blank past the row's text, or an empty line).
    pub on_char: bool,
}

impl ComposerHit {
    /// The drag focus for this hit: the character under the pointer is included
    /// — the composer mirror of `ChatView::snap_focus_right`.
    pub fn focus(&self) -> SelectionPoint {
        if self.on_char {
            SelectionPoint::composer(self.point.row, self.point.col + 1)
        } else {
            self.point
        }
    }
}

/// Resolve a screen position into the composer's visual *and* logical
/// coordinates.
///
/// The pointer is clamped (not rejected): the row is clamped into the visible
/// window and then into the visual-row list, and the column saturates at the
/// text area's left edge — so a drag that leaves the composer keeps producing
/// meaningful positions, exactly like the chat band's mapping. `None` before
/// the first frame (no rect yet) or when the rect is collapsed.
pub fn hit(input: &InputArea, area: Rect, column: u16, row: u16) -> Option<ComposerHit> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let text_width = area.width.saturating_sub(PREFIX_WIDTH) as usize;
    // Same expression the widget renders with, so the rows are identical.
    let vis_rows = wrap::build_visual_rows(&input.lines, text_width.max(1));
    let last = vis_rows.len().checked_sub(1)?;

    let visible = (row.clamp(area.y, area.bottom() - 1) - area.y) as usize;
    let vis_row = (input.vertical_scroll + visible).min(last);
    let display_col = column.saturating_sub(area.x).saturating_sub(PREFIX_WIDTH);

    let row = vis_rows[vis_row];
    let line = &input.lines[row.logical_line];
    let col = wrap::display_col_to_char(line, &row, display_col as usize);
    Some(ComposerHit {
        vis_row,
        display_col,
        point: SelectionPoint::composer(row.logical_line, col as u16),
        on_char: col < row.char_end,
    })
}

/// Logical position for a press / click (no character snapping).
pub fn point(input: &InputArea, area: Rect, column: u16, row: u16) -> Option<SelectionPoint> {
    hit(input, area, column, row).map(|hit| hit.point)
}

/// Logical position for a drag endpoint: includes the character under the
/// pointer (see [`ComposerHit::focus`]).
pub fn focus(input: &InputArea, area: Rect, column: u16, row: u16) -> Option<SelectionPoint> {
    hit(input, area, column, row).map(|hit| hit.focus())
}

/// Paint the composer's selection highlight into the frame's buffer.
///
/// Same channel as the chat band's highlight: a merge (`REVERSED` only, the
/// underlying fg/bg survive) applied after every widget has rendered, clipped
/// to *this* frame's composer rect — the `> ` prefix and everything outside the
/// composer are never touched. The selected char range is turned into display
/// columns per visual row with the model's own widths, so a wide character is
/// covered whole (both of its cells).
///
/// Does nothing unless the selection belongs to the composer
/// ([`Selection::bounds_in`]) — a chat selection can never paint here.
pub fn paint_selection(buf: &mut Buffer, input: &InputArea, area: Rect, selection: &Selection) {
    let Some((start, end)) = selection.bounds_in(SelectionRegion::Composer) else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text_width = area.width.saturating_sub(PREFIX_WIDTH) as usize;
    let vis_rows = wrap::build_visual_rows(&input.lines, text_width.max(1));
    for (visible, screen_row) in (area.y..area.bottom()).enumerate() {
        let Some(row) = vis_rows.get(input.vertical_scroll + visible) else {
            continue;
        };
        // Only the rows of the selected logical lines can carry the highlight.
        if row.logical_line < start.row || row.logical_line > end.row {
            continue;
        }
        let line = &input.lines[row.logical_line];
        // The selected char range, intersected with this visual row.
        let from = if row.logical_line == start.row {
            (start.col as usize).max(row.char_start)
        } else {
            row.char_start
        };
        let to = if row.logical_line == end.row {
            (end.col as usize).min(row.char_end)
        } else {
            row.char_end
        };
        if from >= to {
            continue;
        }
        let from_x = area
            .x
            .saturating_add(PREFIX_WIDTH)
            .saturating_add(wrap::char_display_offset(line, row, from) as u16);
        let to_x = area
            .x
            .saturating_add(PREFIX_WIDTH)
            .saturating_add(wrap::char_display_offset(line, row, to) as u16);
        for x in from_x..to_x.min(area.right()) {
            buf[(x, screen_row)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        }
    }
}

/// Text of a composer selection, taken from the draft.
///
/// Rows are joined with `\n` **only at logical line boundaries**: a soft wrap
/// is a display fold, not a newline in the draft, so a selection that spans one
/// copies a single line (unlike the chat band, whose copy is what the renderer
/// produced). Nothing is trimmed — the composer has no padding, so whitespace
/// is content; an empty range yields `None` (nothing to copy, no feedback).
pub fn selected_text(
    input: &InputArea,
    bounds: (SelectionPoint, SelectionPoint),
) -> Option<String> {
    let (start, end) = bounds;
    let mut lines: Vec<String> = Vec::new();
    for row in start.row..=end.row {
        let Some(line) = input.lines.get(row) else {
            continue;
        };
        let chars: Vec<char> = line.chars().collect();
        let from = if row == start.row {
            (start.col as usize).min(chars.len())
        } else {
            0
        };
        let to = if row == end.row {
            (end.col as usize).min(chars.len())
        } else {
            chars.len()
        };
        lines.push(chars[from.min(to)..to].iter().collect());
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemePalette;
    use crate::ui::input_area::InputAreaWidget;
    use ratatui::widgets::Widget;

    /// The rect the app hands the widget: two columns of prefix are inside it.
    fn composer_area() -> Rect {
        Rect::new(0, 10, 30, 3)
    }

    fn reversed_columns(buf: &Buffer, row: u16) -> Vec<u16> {
        (buf.area.x..buf.area.right())
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::REVERSED))
            .collect()
    }

    /// Render the input area into a full-screen buffer and return it.
    fn render(input: &mut InputArea, area: Rect) -> Buffer {
        let palette = ThemePalette::default();
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 16));
        InputAreaWidget::new(input, &palette).render(area, &mut buf);
        buf
    }

    #[test]
    fn test_hit_resolves_the_text_column() {
        let mut input = InputArea::new("");
        input.set_text("hello world");
        let area = composer_area();
        // Column 2 is the `> ` prefix end → the first character.
        let found = hit(&input, area, 2, 10).expect("inside the composer");
        assert_eq!(found.vis_row, 0);
        assert_eq!(found.display_col, 0);
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        assert!(found.on_char, "the pointer rests on 'h'");

        let found = hit(&input, area, 8, 10).expect("inside the composer");
        assert_eq!(found.display_col, 6);
        assert_eq!(found.point, SelectionPoint::composer(0, 6), "before 'w'");

        // Past the text: the row's end, not a character.
        let found = hit(&input, area, 25, 10).expect("inside the composer");
        assert_eq!(found.point, SelectionPoint::composer(0, 11));
        assert!(!found.on_char);
    }

    #[test]
    fn test_hit_clamps_the_prefix_and_the_row() {
        let mut input = InputArea::new("");
        input.set_text("hi");
        let area = composer_area();
        // On the prefix (or left of the area): the row start.
        for column in [0, 1] {
            let found = hit(&input, area, column, 10).expect("inside the composer");
            assert_eq!(found.display_col, 0);
            assert_eq!(found.point, SelectionPoint::composer(0, 0));
        }
        // Below the content: clamped to the last visible row.
        let found = hit(&input, area, 3, 15).expect("below the composer");
        assert_eq!(found.vis_row, 0, "only one visual row exists");
        assert_eq!(found.point, SelectionPoint::composer(0, 1));
    }

    #[test]
    fn test_hit_follows_soft_wraps_and_the_scroll_window() {
        // Width 12 → text width 10: "abcdefghij" | "klmno".
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno");
        let area = Rect::new(0, 10, 12, 2);
        let found = hit(&input, area, 2 + 2, 11).expect("second visual row");
        assert_eq!(found.vis_row, 1);
        assert_eq!(found.point, SelectionPoint::composer(0, 12));

        // With a scrolled window the screen row is an offset into it.
        let mut input = InputArea::with_max_lines(String::new(), 2);
        input.set_text("one\ntwo\nthree");
        input.update_vertical_scroll(2, 12);
        assert_eq!(input.vertical_scroll, 1);
        let found = hit(&input, area, 2 + 2, 11).expect("second window row");
        assert_eq!(found.vis_row, 2);
        assert_eq!(found.point, SelectionPoint::composer(2, 2));
    }

    #[test]
    fn test_hit_handles_wide_characters() {
        // Width 8 → text width 6: "你好世" | "界".
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        let area = Rect::new(0, 10, 8, 2);
        // Either cell of '你' is on that character.
        let found = hit(&input, area, 2, 10).expect("first cell");
        assert!(found.on_char);
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        assert_eq!(found.focus(), SelectionPoint::composer(0, 1), "includes 你");
        let found = hit(&input, area, 3, 10).expect("second cell");
        assert!(found.on_char);
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        // '世' occupies columns 4..6.
        let found = hit(&input, area, 2 + 5, 10).expect("wide character");
        assert_eq!(found.point, SelectionPoint::composer(0, 2));
        assert_eq!(found.focus(), SelectionPoint::composer(0, 3));
        // Past the row's text: no character to include.
        let found = hit(&input, area, 2 + 6, 10).expect("row end");
        assert!(!found.on_char);
        assert_eq!(found.focus(), SelectionPoint::composer(0, 3));
    }

    #[test]
    fn test_hit_rejects_a_collapsed_or_first_frame_area() {
        let input = InputArea::new("");
        assert!(hit(&input, Rect::default(), 0, 0).is_none());
        assert!(hit(&input, Rect::new(0, 10, 0, 3), 0, 10).is_none());
        assert!(hit(&input, Rect::new(0, 10, 30, 0), 0, 10).is_none());
    }

    #[test]
    fn test_empty_draft_has_nothing_to_hit() {
        let input = InputArea::new("今天构建什么？");
        let found = hit(&input, composer_area(), 6, 10).expect("inside the composer");
        assert!(!found.on_char, "the placeholder is not content");
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        assert_eq!(
            found.focus(),
            found.point,
            "nothing is dragged into the selection"
        );
    }

    #[test]
    fn test_paint_selection_covers_only_the_selected_cells() {
        let mut input = InputArea::new("");
        input.set_text("hello world");
        let area = composer_area();
        let mut buf = render(&mut input, area);
        let before = buf.clone();

        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 0));
        selection.drag_to(SelectionPoint::composer(0, 5));
        paint_selection(&mut buf, &input, area, &selection);

        // Columns 2..7 are "hello" (the prefix is at 0..2 and stays untouched).
        assert_eq!(reversed_columns(&buf, 10), (2..7).collect::<Vec<_>>());
        // Every other cell is bit-for-bit unchanged.
        for y in 0..16u16 {
            for x in 0..40u16 {
                if y == 10 && (2..7).contains(&x) {
                    continue;
                }
                assert_eq!(buf[(x, y)], before[(x, y)], "cell ({x},{y}) changed");
            }
        }
    }

    #[test]
    fn test_paint_selection_includes_wide_characters_whole() {
        // Width 8 → text width 6: "你好世" | "界".
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        let area = Rect::new(0, 10, 8, 2);
        let mut buf = render(&mut input, area);

        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 0));
        selection.drag_to(SelectionPoint::composer(0, 2));
        paint_selection(&mut buf, &input, area, &selection);
        // '你' (2..4) and '好' (4..6) — both cells of each.
        assert_eq!(reversed_columns(&buf, 10), vec![2, 3, 4, 5]);

        // A selection on the second visual row paints there, not above it.
        let mut buf = render(&mut input, area);
        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 3));
        selection.drag_to(SelectionPoint::composer(0, 4));
        paint_selection(&mut buf, &input, area, &selection);
        assert!(reversed_columns(&buf, 10).is_empty());
        assert_eq!(reversed_columns(&buf, 11), vec![2, 3]);
    }

    #[test]
    fn test_paint_selection_paints_every_row_of_a_multi_line_selection() {
        let mut input = InputArea::new("");
        input.set_text("ab\ncdef");
        let area = composer_area();
        let mut buf = render(&mut input, area);

        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 1));
        selection.drag_to(SelectionPoint::composer(1, 3));
        paint_selection(&mut buf, &input, area, &selection);

        assert_eq!(reversed_columns(&buf, 10), vec![3], "row 0: from 'b' on");
        assert_eq!(reversed_columns(&buf, 11), vec![2, 3, 4], "row 1: to 'd'");
        assert!(reversed_columns(&buf, 12).is_empty());
    }

    #[test]
    fn test_paint_selection_ignores_a_chat_selection() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let area = composer_area();
        let mut buf = render(&mut input, area);
        let before = buf.clone();

        let mut selection = Selection::default();
        selection.begin(SelectionPoint::chat(0, 0));
        selection.drag_to(SelectionPoint::chat(0, 20));
        paint_selection(&mut buf, &input, area, &selection);
        assert_eq!(
            buf, before,
            "another region's selection must not paint here"
        );
    }

    #[test]
    fn test_selected_text_joins_logical_lines_only() {
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno");
        // Spanning the soft wrap of a single logical line: no newline.
        let text = selected_text(
            &input,
            (
                SelectionPoint::composer(0, 8),
                SelectionPoint::composer(0, 12),
            ),
        );
        assert_eq!(text.as_deref(), Some("ijkl"));

        input.set_text("hello\nworld");
        let text = selected_text(
            &input,
            (
                SelectionPoint::composer(0, 3),
                SelectionPoint::composer(1, 3),
            ),
        );
        assert_eq!(text.as_deref(), Some("lo\nwor"));
    }

    #[test]
    fn test_selected_text_keeps_wide_characters_intact() {
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        let text = selected_text(
            &input,
            (
                SelectionPoint::composer(0, 1),
                SelectionPoint::composer(0, 3),
            ),
        );
        assert_eq!(text.as_deref(), Some("好世"));

        // All-blank selections are real content here (no padding to strip).
        input.set_text("a  b");
        let text = selected_text(
            &input,
            (
                SelectionPoint::composer(0, 1),
                SelectionPoint::composer(0, 3),
            ),
        );
        assert_eq!(text.as_deref(), Some("  "));
    }

    #[test]
    fn test_selected_text_clamps_and_rejects_empty_ranges() {
        let mut input = InputArea::new("");
        input.set_text("hi");
        let text = selected_text(
            &input,
            (
                SelectionPoint::composer(0, 0),
                SelectionPoint::composer(0, 99),
            ),
        );
        assert_eq!(text.as_deref(), Some("hi"), "clamped to the line's length");

        // A range that lands entirely beyond the draft copies nothing.
        let text = selected_text(
            &input,
            (
                SelectionPoint::composer(5, 0),
                SelectionPoint::composer(6, 0),
            ),
        );
        assert_eq!(text, None, "no row of the draft is covered");
    }
}
