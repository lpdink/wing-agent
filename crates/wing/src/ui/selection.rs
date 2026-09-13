//! Text selection state machine — pure logic, no IO.
//!
//! The selection is anchored in **content coordinates**: `vrow` is a virtual
//! row of the chat's continuous content space (header + cells + pending
//! messages) and `col` is a column inside the chat band. The screen mapping
//! lives in [`crate::ui::chat_view`] (it needs the frame geometry), the
//! highlight patch and the clipboard copy live in the `app` layer.
//!
//! Why content coordinates: appending content (the streaming case) never
//! moves an existing row, so an anchored selection does not drift while the
//! turn streams on. Anything that *does* move rows (resize, compaction,
//! rewind, session switch) is handled by aborting the selection — see
//! `App::selection_fingerprint`.
//!
//! Granularity is characters only: word / line selection (double / triple
//! click) is explicitly out of scope, so there is no click-count or
//! word-pivot state here.

use unicode_width::UnicodeWidthStr;

/// A point in chat content coordinates.
///
/// Ordering is row-major (`vrow` first, then `col`), which is what
/// [`Selection::bounds`] uses to sort anchor and focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContentPoint {
    /// Virtual row in the chat content space.
    pub vrow: usize,
    /// Column inside the chat band (0-based, content-relative).
    pub col: u16,
}

/// One rendered grapheme as it appears in a chat row.
///
/// `col` is the content column of the grapheme's **first** cell; a wide
/// grapheme (`width == 2`) owns the following cell too, whose symbol is a
/// filler space — walking by `width` is what keeps CJK / emoji rows from
/// gaining phantom spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grapheme {
    pub col: u16,
    pub width: u16,
    pub symbol: String,
}

/// One rendered chat row in a copy snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedRow {
    /// Graphemes as they were drawn, in content columns.
    pub graphemes: Vec<Grapheme>,
    /// Columns at the row's left edge that are the *cell's* padding (the
    /// background inset a user message draws before its text), not content.
    ///
    /// Extraction skips them, so dragging from the chat band's left edge does
    /// not paste two phantom spaces per line — while indentation that is part
    /// of the text stays.
    pub inset: u16,
}

/// Drag selection over the chat content.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    anchor: Option<ContentPoint>,
    focus: Option<ContentPoint>,
    /// A left button is down and this selection owns the drag.
    press_active: bool,
    /// The pointer moved since the press (a `Drag` event arrived).
    ///
    /// Distinguishes "dragged back onto the anchor" from a plain click: both
    /// look like `anchor == focus` in coordinates, but only the click must stay
    /// zero-width (the release path also snaps the focus onto the character
    /// under the pointer when the user really dragged).
    dragged: bool,
    /// Edge auto-scroll direction: -1 up, 0 stopped, 1 down.
    auto_scroll: i8,
}

impl Selection {
    /// Start a selection at `at`. Any previous state is dropped (a new press
    /// always starts from scratch).
    pub fn begin(&mut self, at: ContentPoint) {
        self.anchor = Some(at);
        self.focus = Some(at);
        self.press_active = true;
        self.dragged = false;
        self.auto_scroll = 0;
    }

    /// Extend the selection to `at`. Ignored unless a press is active.
    pub fn drag_to(&mut self, at: ContentPoint) {
        if !self.press_active {
            return;
        }
        self.focus = Some(at);
        self.dragged = true;
    }

    /// Finish the drag: returns the ordered bounds when the selection is
    /// non-empty, then clears all state.
    ///
    /// Clearing here is what makes "release = no highlight, no residue" true
    /// by construction — nothing can outlive the release, so there is no Esc
    /// interaction and no cross-frame state to maintain.
    pub fn release(&mut self, at: ContentPoint) -> Option<(ContentPoint, ContentPoint)> {
        if !self.press_active {
            self.reset();
            return None;
        }
        self.drag_to(at);
        let bounds = self.bounds();
        self.reset();
        bounds
    }

    /// Abort the selection (structural change, focus loss, …).
    pub fn cancel(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.press_active = false;
        self.dragged = false;
        self.auto_scroll = 0;
    }

    /// Whether a drag is in flight (highlight / freeze-follow / snapshotting
    /// all key off this).
    pub fn is_press_active(&self) -> bool {
        self.press_active
    }

    /// Whether the pointer moved since the press.
    pub fn is_dragged(&self) -> bool {
        self.dragged
    }

    /// The press position (the fixed end of the selection), if any.
    pub fn anchor(&self) -> Option<ContentPoint> {
        self.anchor
    }

    /// Ordered selection bounds, or `None` when the selection is empty.
    ///
    /// A zero-width selection (anchor == focus, i.e. a plain click) yields
    /// `None`: nothing is highlighted and nothing is copied.
    pub fn bounds(&self) -> Option<(ContentPoint, ContentPoint)> {
        let anchor = self.anchor?;
        let focus = self.focus?;
        if anchor == focus {
            return None;
        }
        Some(if anchor < focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        })
    }

    /// Column span selected on content row `vrow` (half-open, content
    /// columns), `width` being the chat band width.
    pub fn row_span(&self, vrow: usize, width: u16) -> Option<(u16, u16)> {
        row_span(self.bounds()?, vrow, width)
    }

    /// Arm / re-arm the edge auto-scroll for `dir` (-1 up, 1 down); `0`
    /// stops it. The step itself lives in the app layer (it needs the frame
    /// geometry and the viewport height).
    pub fn set_auto_scroll(&mut self, dir: i8) {
        self.auto_scroll = dir;
    }

    /// Stop the edge auto-scroll (keeps the selection itself).
    pub fn stop_auto_scroll(&mut self) {
        self.auto_scroll = 0;
    }

    /// Current auto-scroll direction (-1 / 0 / 1).
    pub fn auto_scroll(&self) -> i8 {
        self.auto_scroll
    }

    /// Current focus (the moving end of the selection), if any.
    ///
    /// The edge auto-scroll shifts it by the line the view just scrolled: the
    /// pointer never moved, so the content under it is the previous focus
    /// shifted along.
    pub fn focus(&self) -> Option<ContentPoint> {
        self.focus
    }
}

/// Column span of ordered `bounds` on row `vrow` (half-open, content
/// columns), clamped to `width`.
///
/// Rows outside the bounds yield `None`; the first / last row contribute
/// their own columns, rows in between are fully covered.
pub fn row_span(
    bounds: (ContentPoint, ContentPoint),
    vrow: usize,
    width: u16,
) -> Option<(u16, u16)> {
    let (start, end) = bounds;
    if vrow < start.vrow || vrow > end.vrow {
        return None;
    }
    let from = if vrow == start.vrow { start.col } else { 0 };
    let to = if vrow == end.vrow { end.col } else { width };
    let from = from.min(width);
    let to = to.min(width);
    if from >= to {
        return None;
    }
    Some((from, to))
}

/// Extract the selected text from a snapshot of rendered rows.
///
/// `rows[i]` holds the graphemes of the screen row that showed content row
/// `scroll_offset + i` when the snapshot was taken. Rows missing from the
/// snapshot (the selection may reach past the captured band, or the view may
/// have scrolled since) are skipped — the copy stays "what you saw".
///
/// Each row starts at its own `inset` ([`RenderedRow::inset`]): the columns a
/// cell fills with background padding are not text, so dragging from the chat
/// band's left edge does not paste the inset — while indentation that is part
/// of the content is kept.
///
/// A grapheme is taken as a whole when it *intersects* the column span, so a
/// partially selected wide character is not split in two. Each row is
/// trimmed at the end, rows are joined with `\n`, and leading / trailing
/// blank lines are dropped. An all-blank selection yields `None` (nothing to
/// copy, no feedback).
pub fn extract_text(
    rows: &[RenderedRow],
    scroll_offset: usize,
    bounds: (ContentPoint, ContentPoint),
) -> Option<String> {
    let (start, end) = bounds;
    if rows.is_empty() {
        return None;
    }
    // Only the rows the snapshot actually holds can contribute text (an edge
    // drag may span far more content rows than were ever on screen).
    let first = start.vrow.max(scroll_offset);
    let last = end.vrow.min(scroll_offset + rows.len() - 1);
    let mut lines: Vec<String> = Vec::new();
    for vrow in first..=last {
        let row = &rows[vrow - scroll_offset];
        let from = if vrow == start.vrow {
            start.col.max(row.inset)
        } else {
            row.inset
        };
        let to = if vrow == end.vrow { end.col } else { u16::MAX };
        let mut line = String::new();
        for grapheme in &row.graphemes {
            let grapheme_end = grapheme.col.saturating_add(grapheme.width);
            if grapheme_end > from && grapheme.col < to {
                line.push_str(&grapheme.symbol);
            }
        }
        lines.push(line.trim_end().to_string());
    }
    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

/// Whether `symbol` is a width-2 (or wider) grapheme filler cell.
///
/// Used by the buffer walkers in [`crate::ui::chat_view`]: cells that hold a
/// wide grapheme are followed by `width - 1` cells whose symbol is a space.
pub fn grapheme_width(symbol: &str) -> u16 {
    (UnicodeWidthStr::width(symbol) as u16).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(vrow: usize, col: u16) -> ContentPoint {
        ContentPoint { vrow, col }
    }

    fn grapheme(col: u16, width: u16, symbol: &str) -> Grapheme {
        Grapheme {
            col,
            width,
            symbol: symbol.to_string(),
        }
    }

    /// A snapshot row with no cell padding.
    fn row(graphemes: Vec<Grapheme>) -> RenderedRow {
        RenderedRow {
            graphemes,
            inset: 0,
        }
    }

    /// A snapshot row of a cell that insets its text (user messages).
    fn padded_row(graphemes: Vec<Grapheme>, inset: u16) -> RenderedRow {
        RenderedRow { graphemes, inset }
    }

    // ── 状态机 ──────────────────────────────────────────────

    #[test]
    fn test_begin_without_drag_has_no_bounds() {
        let mut sel = Selection::default();
        sel.begin(p(3, 4));
        assert!(sel.is_press_active());
        assert!(sel.bounds().is_none(), "a press alone selects nothing");
    }

    #[test]
    fn test_drag_bounds_are_ordered() {
        let mut sel = Selection::default();
        sel.begin(p(3, 4));
        sel.drag_to(p(3, 9));
        assert_eq!(sel.bounds(), Some((p(3, 4), p(3, 9))));

        // Reverse drag (right-to-left) normalizes to the same interval.
        let mut sel = Selection::default();
        sel.begin(p(3, 9));
        sel.drag_to(p(3, 4));
        assert_eq!(sel.bounds(), Some((p(3, 4), p(3, 9))));
    }

    #[test]
    fn test_drag_across_rows_is_ordered_row_major() {
        let mut sel = Selection::default();
        sel.begin(p(7, 12));
        sel.drag_to(p(4, 3));
        assert_eq!(sel.bounds(), Some((p(4, 3), p(7, 12))));
    }

    #[test]
    fn test_zero_width_selection_yields_none() {
        let mut sel = Selection::default();
        sel.begin(p(2, 5));
        sel.drag_to(p(2, 9));
        sel.drag_to(p(2, 5)); // dragged back onto the anchor
        assert!(sel.bounds().is_none(), "zero width is not a selection");
    }

    #[test]
    fn test_drag_without_press_is_ignored() {
        let mut sel = Selection::default();
        sel.drag_to(p(1, 1));
        assert!(!sel.is_press_active());
        assert!(sel.bounds().is_none());
    }

    #[test]
    fn test_release_returns_bounds_and_clears_state() {
        let mut sel = Selection::default();
        sel.begin(p(1, 2));
        sel.drag_to(p(1, 6));
        assert_eq!(sel.release(p(1, 7)), Some((p(1, 2), p(1, 7))));
        assert!(!sel.is_press_active(), "release ends the press");
        assert!(sel.bounds().is_none(), "release leaves no residue");
    }

    #[test]
    fn test_release_uses_the_release_position_as_focus() {
        // A fast drag may only deliver press + release (no Drag events).
        let mut sel = Selection::default();
        sel.begin(p(1, 2));
        assert_eq!(sel.release(p(1, 9)), Some((p(1, 2), p(1, 9))));
    }

    #[test]
    fn test_release_without_press_is_none() {
        let mut sel = Selection::default();
        assert_eq!(sel.release(p(0, 0)), None);
    }

    #[test]
    fn test_cancel_clears_selection_and_auto_scroll() {
        let mut sel = Selection::default();
        sel.begin(p(1, 2));
        sel.drag_to(p(1, 6));
        sel.set_auto_scroll(1);
        sel.cancel();
        assert!(!sel.is_press_active());
        assert!(sel.bounds().is_none());
        assert_eq!(sel.auto_scroll(), 0);
    }

    #[test]
    fn test_release_stops_auto_scroll() {
        let mut sel = Selection::default();
        sel.begin(p(1, 2));
        sel.drag_to(p(1, 6));
        sel.set_auto_scroll(-1);
        sel.release(p(1, 6));
        assert_eq!(sel.auto_scroll(), 0);
    }

    #[test]
    fn test_auto_scroll_round_trip() {
        let mut sel = Selection::default();
        sel.begin(p(1, 2));
        sel.set_auto_scroll(1);
        assert_eq!(sel.auto_scroll(), 1);
        sel.set_auto_scroll(0);
        assert_eq!(sel.auto_scroll(), 0);
        sel.set_auto_scroll(-1);
        assert_eq!(sel.auto_scroll(), -1);
        sel.stop_auto_scroll();
        assert_eq!(sel.auto_scroll(), 0);
    }

    // ── 行内列区间 ──────────────────────────────────────────

    #[test]
    fn test_row_span_single_row() {
        let mut sel = Selection::default();
        sel.begin(p(5, 3));
        sel.drag_to(p(5, 8));
        assert_eq!(sel.row_span(5, 20), Some((3, 8)));
        assert_eq!(sel.row_span(4, 20), None);
        assert_eq!(sel.row_span(6, 20), None);
    }

    #[test]
    fn test_row_span_cross_row() {
        let mut sel = Selection::default();
        sel.begin(p(5, 3));
        sel.drag_to(p(8, 4));
        assert_eq!(sel.row_span(5, 20), Some((3, 20)), "first row: to the edge");
        assert_eq!(sel.row_span(6, 20), Some((0, 20)), "middle row: full width");
        assert_eq!(sel.row_span(7, 20), Some((0, 20)));
        assert_eq!(sel.row_span(8, 20), Some((0, 4)), "last row: from the left");
    }

    #[test]
    fn test_row_span_clamps_to_width() {
        let mut sel = Selection::default();
        sel.begin(p(0, 30));
        sel.drag_to(p(1, 40));
        assert_eq!(sel.row_span(0, 10), None, "start past the row width");
        assert_eq!(sel.row_span(1, 10), Some((0, 10)), "end clamps to width");
    }

    #[test]
    fn test_row_span_zero_width_is_none() {
        let mut sel = Selection::default();
        sel.begin(p(0, 5));
        assert_eq!(sel.row_span(0, 10), None);
    }

    // ── 文本抽取 ────────────────────────────────────────────

    #[test]
    fn test_extract_single_row() {
        let rows = vec![row(vec![
            grapheme(0, 1, "h"),
            grapheme(1, 1, "i"),
            grapheme(2, 1, " "),
            grapheme(3, 1, "x"),
        ])];
        let text = extract_text(&rows, 0, (p(0, 0), p(0, 2)));
        assert_eq!(text.as_deref(), Some("hi"));
    }

    #[test]
    fn test_extract_wide_grapheme_does_not_add_padding() {
        // A CJK row: one 2-cell grapheme per character, no filler cells.
        let rows = vec![row(vec![
            grapheme(0, 2, "你"),
            grapheme(2, 2, "好"),
            grapheme(4, 2, "🌍"),
        ])];
        let text = extract_text(&rows, 0, (p(0, 0), p(0, 6)));
        assert_eq!(text.as_deref(), Some("你好🌍"));
    }

    #[test]
    fn test_extract_partially_selected_wide_grapheme_takes_it_whole() {
        let rows = vec![row(vec![grapheme(0, 2, "你"), grapheme(2, 2, "好")])];
        // Selecting only the first cell of the second grapheme still yields it.
        let text = extract_text(&rows, 0, (p(0, 2), p(0, 3)));
        assert_eq!(text.as_deref(), Some("好"));
    }

    #[test]
    fn test_extract_trims_row_ends_and_joins_lines() {
        let rows = vec![
            row(vec![
                grapheme(0, 1, "a"),
                grapheme(1, 1, " "),
                grapheme(2, 1, " "),
            ]),
            row(vec![grapheme(0, 1, "b"), grapheme(1, 1, " ")]),
        ];
        let text = extract_text(&rows, 10, (p(10, 0), p(12, 1)));
        // Row 12 is outside the snapshot — it is skipped, not padded.
        assert_eq!(text.as_deref(), Some("a\nb"));
    }

    #[test]
    fn test_extract_drops_leading_and_trailing_blank_lines() {
        let rows = vec![
            row(vec![grapheme(0, 1, " ")]),
            row(vec![grapheme(0, 1, "x")]),
            row(vec![grapheme(0, 1, " ")]),
        ];
        let text = extract_text(&rows, 0, (p(0, 0), p(2, 1)));
        assert_eq!(text.as_deref(), Some("x"));
    }

    #[test]
    fn test_extract_all_blank_is_none() {
        let rows = vec![
            row(vec![grapheme(0, 1, " ")]),
            row(vec![grapheme(0, 1, " ")]),
        ];
        assert_eq!(extract_text(&rows, 0, (p(0, 0), p(1, 1))), None);
    }

    #[test]
    fn test_extract_skips_rows_below_the_snapshot() {
        let rows = vec![row(vec![grapheme(0, 1, "x")])];
        // Selection starts above the captured band: the first line is empty
        // and therefore dropped, the visible row is still copied.
        let text = extract_text(&rows, 5, (p(4, 0), p(5, 1)));
        assert_eq!(text.as_deref(), Some("x"));
    }

    #[test]
    fn test_extract_before_snapshot_is_none() {
        let rows = vec![row(vec![grapheme(0, 1, "x")])];
        assert_eq!(extract_text(&rows, 9, (p(0, 0), p(1, 1))), None);
    }

    #[test]
    fn test_extract_ignores_rows_outside_the_snapshot() {
        let rows = vec![
            row(vec![grapheme(0, 1, "x")]),
            row(vec![grapheme(0, 1, "y")]),
        ];
        // Snapshot covers content rows 5..=6.
        assert_eq!(
            extract_text(&rows, 5, (p(10, 0), p(12, 3))),
            None,
            "a selection entirely below the snapshot copies nothing"
        );
        assert_eq!(
            extract_text(&rows, 5, (p(0, 0), p(3, 3))),
            None,
            "a selection entirely above the snapshot copies nothing"
        );
        // A selection overlapping the snapshot keeps exactly the covered rows.
        assert_eq!(
            extract_text(&rows, 5, (p(4, 0), p(6, 1))).as_deref(),
            Some("x\ny")
        );
    }

    #[test]
    fn test_extract_skips_cell_padding_but_keeps_content_indent() {
        // A user message cell fills two columns of background before its text:
        // dragging from the chat band's left edge must not paste them.
        let rows = vec![padded_row(
            vec![
                grapheme(0, 1, " "),
                grapheme(1, 1, " "),
                grapheme(2, 1, "h"),
                grapheme(3, 1, "i"),
            ],
            2,
        )];
        assert_eq!(
            extract_text(&rows, 0, (p(0, 0), p(0, 4))).as_deref(),
            Some("hi")
        );
        // Selecting *only* the padding is not a copyable selection.
        assert_eq!(extract_text(&rows, 0, (p(0, 0), p(0, 2))), None);

        // Indentation that is part of the content stays (inset 0 row).
        let rows = vec![row(vec![
            grapheme(0, 1, " "),
            grapheme(1, 1, " "),
            grapheme(2, 1, "x"),
        ])];
        assert_eq!(
            extract_text(&rows, 0, (p(0, 0), p(0, 3))).as_deref(),
            Some("  x")
        );
    }

    #[test]
    fn test_extract_inset_applies_to_every_row_of_a_multi_row_selection() {
        let rows = vec![
            padded_row(vec![grapheme(0, 1, " "), grapheme(1, 1, "a")], 1),
            padded_row(vec![grapheme(0, 1, " "), grapheme(1, 1, "b")], 1),
        ];
        assert_eq!(
            extract_text(&rows, 0, (p(0, 0), p(1, 2))).as_deref(),
            Some("a\nb")
        );
    }

    #[test]
    fn test_grapheme_width_uses_display_width() {
        assert_eq!(grapheme_width("a"), 1);
        assert_eq!(grapheme_width("你"), 2);
        assert_eq!(grapheme_width(" "), 1);
    }
}
