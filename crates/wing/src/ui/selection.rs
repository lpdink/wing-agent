//! Text selection state machine — pure logic, no IO.
//!
//! A selection lives in **one region** at a time ([`SelectionRegion`]), and its
//! points are anchored in that region's *content* coordinates:
//!
//! * `Chat` — `row` is a virtual row of the chat's continuous content space
//!   (header + cells + pending messages) and `col` a column inside the band.
//!   The screen mapping lives in [`crate::ui::chat_view`] (it needs the frame
//!   geometry).
//! * `Composer` — `row` is a logical line of the draft and `col` a char index
//!   inside it; the visual-row mapping lives in
//!   [`crate::ui::input_area::pointer`].
//!
//! Why content coordinates: appending content (the streaming case) never
//! moves an existing chat row, and scrolling / re-wrapping never moves an
//! existing composer position — so an anchored selection does not drift while
//! the turn streams on or the view scrolls. Anything that *does* move the
//! coordinates (resize, compaction, rewind, session switch, an edit of the
//! draft) is handled by aborting the selection — see
//! `App::selection_fingerprint`.
//!
//! Granularity is characters only: word / line selection (double / triple
//! click) is explicitly out of scope, so there is no click-count or
//! word-pivot state here.

use unicode_width::UnicodeWidthStr;

/// Which interactive region a selection belongs to.
///
/// A selection is anchored in the region its press landed in and never mixes
/// the two coordinate spaces: a drag that leaves the region is clamped into
/// that region's visible band (see the app's pointer handlers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SelectionRegion {
    /// The scrollable chat band (content coordinates, see [`SelectionPoint`]).
    Chat,
    /// The composer — the multi-line draft input (logical coordinates).
    Composer,
}

/// A point in the owning region's content coordinates.
///
/// Within one region the derived ordering is row-major (`row` first, then
/// `col`), which is what [`Selection::bounds`] sorts anchor and focus by.
/// Points of *different* regions are never ordered against each other — that
/// state is ruled out by construction ([`Selection::bounds`] rejects it and
/// [`Selection::drag_to`] drops foreign points) — and they could not be
/// meaningfully compared anyway, since `row` / `col` mean something else in
/// each region (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelectionPoint {
    /// Region the point belongs to.
    pub region: SelectionRegion,
    /// `Chat`: content virtual row. `Composer`: logical line index.
    pub row: usize,
    /// `Chat`: column inside the chat band (0-based). `Composer`: char index
    /// inside the logical line.
    pub col: u16,
}

impl SelectionPoint {
    /// A point in the chat band's content space.
    pub fn chat(vrow: usize, col: u16) -> Self {
        Self {
            region: SelectionRegion::Chat,
            row: vrow,
            col,
        }
    }

    /// A point in the composer's logical space.
    pub fn composer(line: usize, col: u16) -> Self {
        Self {
            region: SelectionRegion::Composer,
            row: line,
            col,
        }
    }
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

/// Drag selection over one region's content.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    anchor: Option<SelectionPoint>,
    focus: Option<SelectionPoint>,
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
    ///
    /// Chat only: the composer is at most `max_lines` tall and has no edge
    /// scrolling, so its drags never arm this.
    auto_scroll: i8,
}

impl Selection {
    /// Start a selection at `at`. Any previous state is dropped (a new press
    /// always starts from scratch), including the region: the point's region
    /// decides which space this selection lives in.
    pub fn begin(&mut self, at: SelectionPoint) {
        self.anchor = Some(at);
        self.focus = Some(at);
        self.press_active = true;
        self.dragged = false;
        self.auto_scroll = 0;
    }

    /// Extend the selection to `at`. Ignored unless a press is active, and
    /// ignored when `at` belongs to another region: the app maps the pointer
    /// into the region the press started in, so a foreign region would be a
    /// bug — dropping it keeps the two coordinate spaces from ever mixing.
    pub fn drag_to(&mut self, at: SelectionPoint) {
        if !self.press_active {
            return;
        }
        if let Some(anchor) = self.anchor
            && anchor.region != at.region
        {
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
    pub fn release(&mut self, at: SelectionPoint) -> Option<(SelectionPoint, SelectionPoint)> {
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
    pub fn anchor(&self) -> Option<SelectionPoint> {
        self.anchor
    }

    /// The region this selection is anchored in, if any.
    ///
    /// This is the switch the app uses to map the pointer, to paint the
    /// highlight and to pick the invalidation rules — one selection, one
    /// region.
    pub fn region(&self) -> Option<SelectionRegion> {
        self.anchor.map(|anchor| anchor.region)
    }

    /// Ordered selection bounds, or `None` when the selection is empty.
    ///
    /// A zero-width selection (anchor == focus, i.e. a plain click) yields
    /// `None`: nothing is highlighted and nothing is copied.
    pub fn bounds(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        let anchor = self.anchor?;
        let focus = self.focus?;
        if anchor == focus || anchor.region != focus.region {
            return None;
        }
        Some(if anchor < focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        })
    }

    /// Ordered bounds, but only when the selection lives in `region`.
    ///
    /// Each region's painter / copier asks for its own bounds, so a highlight
    /// can never be drawn (or a text extracted) through the wrong coordinate
    /// mapping.
    pub fn bounds_in(&self, region: SelectionRegion) -> Option<(SelectionPoint, SelectionPoint)> {
        if self.region() != Some(region) {
            return None;
        }
        self.bounds()
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
    pub fn focus(&self) -> Option<SelectionPoint> {
        self.focus
    }
}

/// Column span of ordered `bounds` on content row `vrow` (half-open, content
/// columns), clamped to `width`. Chat coordinates only ([`SelectionRegion::Chat`]).
///
/// Rows outside the bounds yield `None`; the first / last row contribute
/// their own columns, rows in between are fully covered.
pub fn row_span(
    bounds: (SelectionPoint, SelectionPoint),
    vrow: usize,
    width: u16,
) -> Option<(u16, u16)> {
    let (start, end) = bounds;
    debug_assert_eq!(start.region, SelectionRegion::Chat);
    debug_assert_eq!(end.region, SelectionRegion::Chat);
    if vrow < start.row || vrow > end.row {
        return None;
    }
    let from = if vrow == start.row { start.col } else { 0 };
    let to = if vrow == end.row { end.col } else { width };
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
///
/// `content_width` limits every row to the columns that belong to the content
/// (see [`ChatView::set_content_width`]): rows strictly inside the selection
/// span run to the end of the band, which is where the overlay scrollbar sits
/// — without the limit the copy would end in its `│` glyph plus the padding in
/// front of it. A wide grapheme that *starts* before the limit is still taken
/// whole.
pub fn extract_text(
    rows: &[RenderedRow],
    scroll_offset: usize,
    bounds: (SelectionPoint, SelectionPoint),
    content_width: Option<u16>,
) -> Option<String> {
    let end_limit = content_width.unwrap_or(u16::MAX);
    let (start, end) = bounds;
    debug_assert_eq!(start.region, SelectionRegion::Chat);
    debug_assert_eq!(end.region, SelectionRegion::Chat);
    if rows.is_empty() {
        return None;
    }
    // Only the rows the snapshot actually holds can contribute text (an edge
    // drag may span far more content rows than were ever on screen).
    let first = start.row.max(scroll_offset);
    let last = end.row.min(scroll_offset + rows.len() - 1);
    let mut lines: Vec<String> = Vec::new();
    for vrow in first..=last {
        let row = &rows[vrow - scroll_offset];
        let from = if vrow == start.row {
            start.col.max(row.inset)
        } else {
            row.inset
        };
        let to = if vrow == end.row {
            end.col.min(end_limit)
        } else {
            end_limit
        };
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

    fn p(vrow: usize, col: u16) -> SelectionPoint {
        SelectionPoint::chat(vrow, col)
    }

    /// A composer point in logical coordinates.
    fn q(line: usize, col: u16) -> SelectionPoint {
        SelectionPoint::composer(line, col)
    }

    /// Row span of the chat selection over `vrow`, through the free function
    /// `ChatView::paint_selection` uses.
    fn span(sel: &Selection, vrow: usize, width: u16) -> Option<(u16, u16)> {
        row_span(sel.bounds().expect("bounds"), vrow, width)
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
        assert_eq!(span(&sel, 5, 20), Some((3, 8)));
        assert_eq!(span(&sel, 4, 20), None);
        assert_eq!(span(&sel, 6, 20), None);
    }

    #[test]
    fn test_row_span_cross_row() {
        let mut sel = Selection::default();
        sel.begin(p(5, 3));
        sel.drag_to(p(8, 4));
        assert_eq!(span(&sel, 5, 20), Some((3, 20)), "first row: to the edge");
        assert_eq!(span(&sel, 6, 20), Some((0, 20)), "middle row: full width");
        assert_eq!(span(&sel, 7, 20), Some((0, 20)));
        assert_eq!(span(&sel, 8, 20), Some((0, 4)), "last row: from the left");
    }

    #[test]
    fn test_row_span_clamps_to_width() {
        let mut sel = Selection::default();
        sel.begin(p(0, 30));
        sel.drag_to(p(1, 40));
        assert_eq!(span(&sel, 0, 10), None, "start past the row width");
        assert_eq!(span(&sel, 1, 10), Some((0, 10)), "end clamps to width");
    }

    #[test]
    fn test_row_span_zero_width_is_none() {
        let mut sel = Selection::default();
        sel.begin(p(0, 5));
        assert_eq!(sel.bounds(), None, "a press alone selects nothing");
        assert_eq!(row_span((p(0, 5), p(0, 5)), 0, 10), None);
    }

    // ── 区域 ────────────────────────────────────────────────

    #[test]
    fn test_region_comes_from_the_anchor_point() {
        let mut sel = Selection::default();
        assert_eq!(sel.region(), None, "no selection, no region");

        sel.begin(p(3, 4));
        assert_eq!(sel.region(), Some(SelectionRegion::Chat));

        // A new press starts a fresh selection, region included.
        sel.begin(q(1, 2));
        assert_eq!(sel.region(), Some(SelectionRegion::Composer));
        sel.cancel();
        assert_eq!(sel.region(), None);
    }

    #[test]
    fn test_bounds_in_matches_only_the_owning_region() {
        let mut sel = Selection::default();
        sel.begin(q(1, 2));
        sel.drag_to(q(1, 5));
        assert_eq!(
            sel.bounds_in(SelectionRegion::Composer),
            Some((q(1, 2), q(1, 5)))
        );
        assert_eq!(
            sel.bounds_in(SelectionRegion::Chat),
            None,
            "a composer selection has no chat bounds — the mappings must not mix"
        );
    }

    #[test]
    fn test_drag_from_another_region_is_ignored() {
        let mut sel = Selection::default();
        sel.begin(p(3, 4));
        sel.drag_to(q(1, 5));
        assert_eq!(
            sel.bounds(),
            None,
            "a foreign-region point must not become the focus"
        );
        assert_eq!(sel.anchor(), Some(p(3, 4)));

        sel.drag_to(p(3, 9));
        assert_eq!(sel.bounds(), Some((p(3, 4), p(3, 9))));
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
        let text = extract_text(&rows, 0, (p(0, 0), p(0, 2)), None);
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
        let text = extract_text(&rows, 0, (p(0, 0), p(0, 6)), None);
        assert_eq!(text.as_deref(), Some("你好🌍"));
    }

    #[test]
    fn test_extract_partially_selected_wide_grapheme_takes_it_whole() {
        let rows = vec![row(vec![grapheme(0, 2, "你"), grapheme(2, 2, "好")])];
        // Selecting only the first cell of the second grapheme still yields it.
        let text = extract_text(&rows, 0, (p(0, 2), p(0, 3)), None);
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
        let text = extract_text(&rows, 10, (p(10, 0), p(12, 1)), None);
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
        let text = extract_text(&rows, 0, (p(0, 0), p(2, 1)), None);
        assert_eq!(text.as_deref(), Some("x"));
    }

    #[test]
    fn test_extract_all_blank_is_none() {
        let rows = vec![
            row(vec![grapheme(0, 1, " ")]),
            row(vec![grapheme(0, 1, " ")]),
        ];
        assert_eq!(extract_text(&rows, 0, (p(0, 0), p(1, 1)), None), None);
    }

    #[test]
    fn test_extract_skips_rows_below_the_snapshot() {
        let rows = vec![row(vec![grapheme(0, 1, "x")])];
        // Selection starts above the captured band: the first line is empty
        // and therefore dropped, the visible row is still copied.
        let text = extract_text(&rows, 5, (p(4, 0), p(5, 1)), None);
        assert_eq!(text.as_deref(), Some("x"));
    }

    #[test]
    fn test_extract_before_snapshot_is_none() {
        let rows = vec![row(vec![grapheme(0, 1, "x")])];
        assert_eq!(extract_text(&rows, 9, (p(0, 0), p(1, 1)), None), None);
    }

    #[test]
    fn test_extract_ignores_rows_outside_the_snapshot() {
        let rows = vec![
            row(vec![grapheme(0, 1, "x")]),
            row(vec![grapheme(0, 1, "y")]),
        ];
        // Snapshot covers content rows 5..=6.
        assert_eq!(
            extract_text(&rows, 5, (p(10, 0), p(12, 3)), None),
            None,
            "a selection entirely below the snapshot copies nothing"
        );
        assert_eq!(
            extract_text(&rows, 5, (p(0, 0), p(3, 3)), None),
            None,
            "a selection entirely above the snapshot copies nothing"
        );
        // A selection overlapping the snapshot keeps exactly the covered rows.
        assert_eq!(
            extract_text(&rows, 5, (p(4, 0), p(6, 1)), None).as_deref(),
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
            extract_text(&rows, 0, (p(0, 0), p(0, 4)), None).as_deref(),
            Some("hi")
        );
        // Selecting *only* the padding is not a copyable selection.
        assert_eq!(extract_text(&rows, 0, (p(0, 0), p(0, 2)), None), None);

        // Indentation that is part of the content stays (inset 0 row).
        let rows = vec![row(vec![
            grapheme(0, 1, " "),
            grapheme(1, 1, " "),
            grapheme(2, 1, "x"),
        ])];
        assert_eq!(
            extract_text(&rows, 0, (p(0, 0), p(0, 3)), None).as_deref(),
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
            extract_text(&rows, 0, (p(0, 0), p(1, 2)), None).as_deref(),
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
