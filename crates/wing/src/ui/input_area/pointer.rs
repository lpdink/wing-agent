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
use super::chrome::Chrome;
use super::model;
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
    /// prompt glyph, in the card's own columns. Values past the row's text
    /// point into its trailing blank.
    pub display_col: u16,
    /// Logical position the pointer resolves to: the insertion point at or
    /// before the character whose cells cover it — and for the cells of a
    /// paste chip, the point before the whole chip (see
    /// [`super::model::column_point`]).
    pub point: SelectionPoint,
    /// The drag endpoint for this hit: the unit under the pointer is included
    /// whole (a chip in its entirety) — the composer mirror of
    /// `ChatView::snap_focus_right`.
    pub focus: SelectionPoint,
    /// Whether the pointer actually rests on a character or chip (as opposed
    /// to the blank past the row's text, or an empty line).
    pub on_char: bool,
}

/// Resolve a screen position into the composer's visual *and* logical
/// coordinates.
///
/// The pointer is **clamped, never rejected** (except by the frame itself —
/// see [`press_hit`]): a point on the card's rails and a point outside the
/// composer alike resolve to the nearest text row, the row is clamped into the
/// visible window and then into the visual-row list, and the column saturates
/// at the text area's left edge — so a drag that leaves the region keeps
/// producing meaningful positions, exactly like the chat band's mapping.
/// `None` only before the first frame (no rect yet) or when the rect is
/// collapsed.
pub fn hit(input: &InputArea, area: Rect, column: u16, row: u16) -> Option<ComposerHit> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let chrome = Chrome::of(area);
    let text_width = chrome.text_width as usize;
    // Same expression the widget renders with, so the rows are identical.
    let vis_rows = wrap::build_visual_rows(&input.lines, &input.pastes, text_width.max(1));
    let last = vis_rows.len().checked_sub(1)?;

    // Rows on the rails and rows off the card both clamp into the text band:
    // the frame is not text, but a pointer that crosses it still *means* a
    // position in the draft (a release on the border must not throw the
    // selection away).
    let row_in_area = if row < area.y + chrome.top_row() {
        chrome.top_row()
    } else if row >= area.bottom() - u16::from(chrome.card) {
        area.height - 1 - u16::from(chrome.card)
    } else {
        row - area.y
    };
    let visible = row_in_area.saturating_sub(chrome.top_row()) as usize;
    let vis_row = (input.vertical_scroll + visible).min(last);
    let display_col = column.saturating_sub(area.x).saturating_sub(chrome.text_x);

    let row = vis_rows[vis_row];
    let point = model::column_point(
        &input.lines[row.logical_line],
        &input.pastes,
        &row,
        display_col as usize,
        text_width,
    );
    Some(ComposerHit {
        vis_row,
        display_col,
        point: SelectionPoint::composer(row.logical_line, point.point as u16),
        focus: SelectionPoint::composer(row.logical_line, point.focus as u16),
        on_char: point.on_char,
    })
}

/// Resolve a **press** into the composer, rejecting the card's frame.
///
/// The composer is claimed by its whole block (see `PointerOwner::Composer`),
/// but the frame is not text: a press that lands on a rail does nothing — no
/// cursor, no selection — instead of dropping a cursor under a border the user
/// aimed at. Drags and releases go through [`hit`], which clamps: a gesture
/// that *started* in the draft keeps meaning the draft even when the pointer
/// overshoots onto the border.
pub fn press_hit(input: &InputArea, area: Rect, column: u16, row: u16) -> Option<ComposerHit> {
    let chrome = Chrome::of(area);
    if chrome.card && (row < area.y + chrome.top_row() || row >= area.bottom() - 1) {
        return None;
    }
    hit(input, area, column, row)
}

/// Logical position for a press / click (no character snapping).
pub fn point(input: &InputArea, area: Rect, column: u16, row: u16) -> Option<SelectionPoint> {
    hit(input, area, column, row).map(|hit| hit.point)
}

/// Logical position for a drag endpoint: includes the unit under the pointer
/// (see [`ComposerHit::focus`]).
pub fn focus(input: &InputArea, area: Rect, column: u16, row: u16) -> Option<SelectionPoint> {
    hit(input, area, column, row).map(|hit| hit.focus)
}

/// Paint the composer's selection highlight into the frame's buffer.
///
/// Same channel as the chat band's highlight: a merge (`REVERSED` only, the
/// underlying fg/bg survive) applied after every widget has rendered, clipped
/// to *this* frame's composer rect — the card's frame, the prompt glyph and
/// everything outside the composer are never touched. The selected char range
/// is turned into display columns per visual row with the model's own widths,
/// so a wide character is covered whole (both of its cells).
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
    let chrome = Chrome::of(area);
    let text_width = chrome.text_width as usize;
    let vis_rows = wrap::build_visual_rows(&input.lines, &input.pastes, text_width.max(1));
    let first_row = area.y + chrome.top_row();
    let last_row = first_row + chrome.text_rows;
    for (visible, screen_row) in (first_row..last_row).enumerate() {
        let Some(row) = vis_rows.get(input.vertical_scroll + visible) else {
            continue;
        };
        // Only the rows of the selected logical lines can carry the highlight.
        if row.logical_line < start.row || row.logical_line > end.row {
            continue;
        }
        let line = model::flat(&input.lines[row.logical_line], &input.pastes);
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
            .saturating_add(chrome.text_x)
            .saturating_add(wrap::char_display_offset(&line, row, from) as u16);
        let to_x = area
            .x
            .saturating_add(chrome.text_x)
            .saturating_add(wrap::char_display_offset(&line, row, to) as u16);
        for x in from_x..to_x.min(area.right().saturating_sub(u16::from(chrome.card))) {
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
/// is content.
///
/// A selection that contributes no characters at all yields `None`: it may
/// still span several rows (dragging from the end of a line to the start of the
/// next one), but everything it covers is line breaks — copying a bare `\n`
/// and claiming `Copied!` would contradict "an empty selection copies nothing".
pub fn selected_text(
    input: &InputArea,
    bounds: (SelectionPoint, SelectionPoint),
) -> Option<String> {
    let (start, end) = bounds;
    debug_assert_eq!(start.region, SelectionRegion::Composer);
    debug_assert_eq!(end.region, SelectionRegion::Composer);
    let mut lines: Vec<String> = Vec::new();
    for row in start.row..=end.row {
        let Some(line) = input.lines.get(row) else {
            continue;
        };
        // The draft's own projection: a chip copies as the visible chip text
        // (`[Pasted text #1 …]`), which is exactly what is on screen.
        let chars: Vec<char> = model::flat(line, &input.pastes).chars().collect();
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
    if lines.is_empty() || lines.iter().all(String::is_empty) {
        return None;
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemePalette;
    use crate::ui::input_area::ComposerWidget;
    use crate::ui::input_area::MetaRail;
    use crate::ui::status_bar::TurnUsage;
    use ratatui::widgets::Widget;

    /// Column of the first draft cell: the frame's border, its padding and the
    /// prompt glyph are left of it.
    const TEXT_X: u16 = 4;
    /// Row of the first draft row: the frame's top border is above it.
    const TEXT_Y: u16 = 11;

    /// The rect the app hands the widget for a composer `text_width` columns
    /// and `rows` text rows wide (the card's frame included).
    fn composer_area(text_width: u16, rows: u16) -> Rect {
        Rect::new(0, 10, text_width + TEXT_X + 2, rows + 2)
    }

    fn reversed_columns(buf: &Buffer, row: u16) -> Vec<u16> {
        (buf.area.x..buf.area.right())
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::REVERSED))
            .collect()
    }

    /// Render the composer into a full-screen buffer and return it.
    fn render(input: &mut InputArea, area: Rect) -> Buffer {
        let palette = ThemePalette::default();
        let usage = TurnUsage::default();
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 16));
        ComposerWidget::new(input, &palette, None, MetaRail::bare(&usage)).render(area, &mut buf);
        buf
    }

    #[test]
    fn test_hit_resolves_the_text_column() {
        let mut input = InputArea::new("");
        input.set_text("hello world");
        let area = composer_area(24, 1);
        // The first text column is the draft's first character.
        let found = hit(&input, area, TEXT_X, TEXT_Y).expect("inside the composer");
        assert_eq!(found.vis_row, 0);
        assert_eq!(found.display_col, 0);
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        assert!(found.on_char, "the pointer rests on 'h'");

        let found = hit(&input, area, TEXT_X + 6, TEXT_Y).expect("inside the composer");
        assert_eq!(found.display_col, 6);
        assert_eq!(found.point, SelectionPoint::composer(0, 6), "before 'w'");

        // Past the text: the row's end, not a character.
        let found = hit(&input, area, 25, TEXT_Y).expect("inside the composer");
        assert_eq!(found.point, SelectionPoint::composer(0, 11));
        assert!(!found.on_char);
    }

    #[test]
    fn test_hit_clamps_the_left_edge() {
        let mut input = InputArea::new("");
        input.set_text("hi");
        let area = composer_area(24, 1);
        // On the frame, the padding or the prompt glyph (or left of the area):
        // the row start.
        for column in [0, 1, 2, 3] {
            let found = hit(&input, area, column, TEXT_Y).expect("inside the composer");
            assert_eq!(found.display_col, 0);
            assert_eq!(found.point, SelectionPoint::composer(0, 0));
        }
    }

    #[test]
    fn test_hit_clamps_the_rails_but_a_press_does_not() {
        let mut input = InputArea::new("");
        input.set_text("hi");
        let area = composer_area(24, 1);
        let (top, bottom) = (area.y, area.bottom() - 1);

        // The mapping never rejects: a gesture that crosses the frame still
        // means a position in the draft (a release on the border must not
        // throw the selection away) — the top rail resolves to the first text
        // row, the meta rail to the last one.
        let found = hit(&input, area, TEXT_X, top).expect("clamped onto the top rail");
        assert_eq!(found.vis_row, 0);
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        let found = hit(&input, area, 25, bottom).expect("clamped onto the meta rail");
        assert_eq!(
            found.point,
            SelectionPoint::composer(0, 2),
            "the row is kept, the column past the text lands on the row end"
        );

        // A *press* on the rails is nobody's: the frame is not text, so it
        // places no cursor and starts no selection.
        assert!(press_hit(&input, area, TEXT_X, top).is_none(), "top rail");
        assert!(
            press_hit(&input, area, TEXT_X, bottom).is_none(),
            "meta rail"
        );
        assert!(press_hit(&input, area, TEXT_X, TEXT_Y).is_some());
    }

    #[test]
    fn test_hit_follows_soft_wraps_and_the_scroll_window() {
        // Text width 10: "abcdefghij" | "klmno".
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno");
        let area = composer_area(10, 2);
        let found = hit(&input, area, TEXT_X + 2, TEXT_Y + 1).expect("second visual row");
        assert_eq!(found.vis_row, 1);
        assert_eq!(found.point, SelectionPoint::composer(0, 12));

        // With a scrolled window the screen row is an offset into it.
        let mut input = InputArea::with_max_lines(String::new(), 2);
        input.set_text("one\ntwo\nthree");
        input.update_vertical_scroll(Chrome::of(area));
        assert_eq!(input.vertical_scroll, 1);
        let found = hit(&input, area, TEXT_X + 2, TEXT_Y + 1).expect("second window row");
        assert_eq!(found.vis_row, 2);
        assert_eq!(found.point, SelectionPoint::composer(2, 2));
    }

    #[test]
    fn test_hit_handles_wide_characters() {
        // Text width 6: "你好世" | "界".
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        let area = composer_area(6, 2);
        // Either cell of '你' is on that character.
        let found = hit(&input, area, TEXT_X, TEXT_Y).expect("first cell");
        assert!(found.on_char);
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        assert_eq!(found.focus, SelectionPoint::composer(0, 1), "includes 你");
        let found = hit(&input, area, TEXT_X + 1, TEXT_Y).expect("second cell");
        assert!(found.on_char);
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        // '世' occupies display columns 4..6 of the text area.
        let found = hit(&input, area, TEXT_X + 5, TEXT_Y).expect("wide character");
        assert_eq!(found.point, SelectionPoint::composer(0, 2));
        assert_eq!(found.focus, SelectionPoint::composer(0, 3));
        // Past the row's text: no character to include.
        let found = hit(&input, area, TEXT_X + 6, TEXT_Y).expect("row end");
        assert!(!found.on_char);
        assert_eq!(found.focus, SelectionPoint::composer(0, 3));
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
        let found = hit(&input, composer_area(24, 1), TEXT_X + 2, TEXT_Y).expect("inside it");
        assert!(!found.on_char, "the placeholder is not content");
        assert_eq!(found.point, SelectionPoint::composer(0, 0));
        assert_eq!(
            found.focus, found.point,
            "nothing is dragged into the selection"
        );
    }

    #[test]
    fn test_paint_selection_covers_only_the_selected_cells() {
        let mut input = InputArea::new("");
        input.set_text("hello world");
        let area = composer_area(24, 1);
        let mut buf = render(&mut input, area);
        let before = buf.clone();

        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 0));
        selection.drag_to(SelectionPoint::composer(0, 5));
        paint_selection(&mut buf, &input, area, &selection);

        // Columns 4..9 are "hello" — the frame and the prompt glyph stay
        // untouched.
        assert_eq!(
            reversed_columns(&buf, TEXT_Y),
            (TEXT_X..TEXT_X + 5).collect::<Vec<_>>()
        );
        // Every other cell is bit-for-bit unchanged.
        for y in 0..16u16 {
            for x in 0..40u16 {
                if y == TEXT_Y && (TEXT_X..TEXT_X + 5).contains(&x) {
                    continue;
                }
                assert_eq!(buf[(x, y)], before[(x, y)], "cell ({x},{y}) changed");
            }
        }
    }

    #[test]
    fn test_paint_selection_includes_wide_characters_whole() {
        // Text width 6: "你好世" | "界".
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        let area = composer_area(6, 2);
        let mut buf = render(&mut input, area);

        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 0));
        selection.drag_to(SelectionPoint::composer(0, 2));
        paint_selection(&mut buf, &input, area, &selection);
        // '你' (4..6) and '好' (6..8) — both cells of each.
        assert_eq!(reversed_columns(&buf, TEXT_Y), vec![4, 5, 6, 7]);

        // A selection on the second visual row paints there, not above it.
        let mut buf = render(&mut input, area);
        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 3));
        selection.drag_to(SelectionPoint::composer(0, 4));
        paint_selection(&mut buf, &input, area, &selection);
        assert!(reversed_columns(&buf, TEXT_Y).is_empty());
        assert_eq!(reversed_columns(&buf, TEXT_Y + 1), vec![4, 5]);
    }

    #[test]
    fn test_paint_selection_paints_every_row_of_a_multi_line_selection() {
        let mut input = InputArea::new("");
        input.set_text("ab\ncdef");
        let area = composer_area(24, 2);
        let mut buf = render(&mut input, area);

        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 1));
        selection.drag_to(SelectionPoint::composer(1, 3));
        paint_selection(&mut buf, &input, area, &selection);

        assert_eq!(
            reversed_columns(&buf, TEXT_Y),
            vec![TEXT_X + 1],
            "row 0: from 'b' on"
        );
        assert_eq!(
            reversed_columns(&buf, TEXT_Y + 1),
            vec![TEXT_X, TEXT_X + 1, TEXT_X + 2],
            "row 1: to 'd'"
        );
        assert!(
            reversed_columns(&buf, TEXT_Y + 2).is_empty(),
            "the meta rail"
        );
    }

    #[test]
    fn test_paint_selection_ignores_a_chat_selection() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let area = composer_area(24, 1);
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
    fn test_selected_text_rejects_line_break_only_ranges() {
        let mut input = InputArea::new("");
        input.set_text("ab\n");
        // From the end of the first line to the start of the empty second one:
        // the range is non-empty but contributes no characters at all.
        assert_eq!(
            selected_text(
                &input,
                (
                    SelectionPoint::composer(0, 2),
                    SelectionPoint::composer(1, 0)
                )
            ),
            None
        );

        input.set_text("ab\n\ncd");
        assert_eq!(
            selected_text(
                &input,
                (
                    SelectionPoint::composer(0, 2),
                    SelectionPoint::composer(2, 0)
                )
            ),
            None,
            "a span of nothing but line breaks is not a copyable selection"
        );
        // A range that does carry characters keeps its empty middle line.
        assert_eq!(
            selected_text(
                &input,
                (
                    SelectionPoint::composer(0, 1),
                    SelectionPoint::composer(2, 1)
                )
            )
            .as_deref(),
            Some("b\n\nc")
        );
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
