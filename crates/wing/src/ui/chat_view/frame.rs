//! The frame snapshot and the selection face: screen ↔ content mapping, the
//! highlight, and the copy-on-select source.
//!
//! The one thing this module owns is [`FrameSnapshot`] — what the chat band
//! *looked like* in a drawn frame, as row-level graphemes with their cell
//! padding. A selection reads the snapshot for its text and the frame
//! geometry for the pointer mapping / highlight, and nothing here decides
//! anything about scrolling.
//!
//! The only place this module looks into the content model is
//! [`ChatView::visible_row_insets`]: the per-row padding is a property of the
//! *layout* the render walk produced, and the walk lives in `super::viewport`
//! (the two must stay in sync — see the note on that method).
//!
//! The buffer walk is deliberately the *only* way the rendered rows are
//! observed (`buffer_row_graphemes`): the frame is the authority on what the
//! user saw — links ride inside the cell symbols, wide graphemes occupy two
//! cells, and the render path's padding rows are recognized here. The pure
//! text maths (row spans, wide-grapheme handling, trimming) stays in
//! [`crate::ui::selection`], which knows nothing about ratatui.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;

use crate::render::markdown::strip_osc8;
use crate::ui::selection::Grapheme;
use crate::ui::selection::RenderedRow;
use crate::ui::selection::Selection;
use crate::ui::selection::SelectionPoint;
use crate::ui::selection::SelectionRegion;
use crate::ui::selection::extract_text;
use crate::ui::selection::grapheme_width;
use crate::ui::selection::row_span;

use super::ChatCell;
use super::ChatView;

/// Snapshot of the chat band as one frame drew it — the copy-on-select source.
///
/// Copy-on-select is WYSIWYG: the release event lands between frames, so the
/// text has to come from a frame the user was actually looking at. And
/// because the rows were captured out of *that* frame's buffer, the snapshot
/// carries that frame's own coordinates ([`Self::scroll_offset`],
/// [`Self::width`]) instead of borrowing the live geometry — the snapshot is
/// self-describing, so a copy can never mix rows from one frame with the
/// coordinates of another.
///
/// Refreshed only while a drag is in flight (see
/// [`ChatView::capture_visible_rows`]), so an idle app pays nothing. An empty
/// snapshot means "nothing to copy": the release path then produces no
/// clipboard intent and no feedback.
#[derive(Debug, Clone, Default)]
pub(super) struct FrameSnapshot {
    /// Content row shown by `rows[0]` in the captured frame.
    scroll_offset: usize,
    /// Content width of the captured frame (the chat band minus the scrollbar
    /// gutter) — the limit for a selection running to the end of a row.
    width: u16,
    /// Graphemes of every captured screen row, in screen order.
    rows: Vec<RenderedRow>,
}

impl FrameSnapshot {
    /// Capture the chat band out of the frame's buffer.
    ///
    /// `insets[i]` is the left padding (in columns) of the i-th band row, in
    /// row order (see [`ChatView::visible_row_insets`]); a missing entry means
    /// "no padding".
    fn capture(buf: &Buffer, area: Rect, scroll_offset: usize, insets: &[u16]) -> Self {
        Self {
            scroll_offset,
            width: area.width,
            rows: (area.y..area.bottom())
                .enumerate()
                .map(|(index, row)| RenderedRow {
                    graphemes: buffer_row_graphemes(buf, row, area),
                    inset: insets.get(index).copied().unwrap_or(0),
                })
                .collect(),
        }
    }

    /// Whether the snapshot holds no rows (nothing to copy).
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The captured row that showed content row `vrow`, if it was on screen.
    fn row(&self, vrow: usize) -> Option<&RenderedRow> {
        self.rows.get(vrow.checked_sub(self.scroll_offset)?)
    }

    /// Text of the selected content range, taken from this snapshot.
    ///
    /// `None` when nothing could be extracted (empty / all-blank selection,
    /// or the rows are not part of the snapshot) — in that case there is
    /// nothing to copy and no feedback is shown.
    ///
    /// Rows are bounded by the captured frame's content width (the band minus
    /// the scrollbar gutter), so the copy never picks up the bar's `│` glyph
    /// or the blank columns in front of it.
    fn text(&self, bounds: (SelectionPoint, SelectionPoint)) -> Option<String> {
        extract_text(
            self.rows.as_slice(),
            self.scroll_offset,
            bounds,
            Some(self.width),
        )
    }
}

/// Walk one rendered row of the chat band and return its graphemes.
///
/// The buffer holds what the terminal will show: a wide grapheme stores its
/// symbol in the first cell and leaves `width - 1` filler cells behind it
/// (their symbol reads back as `" "`). Advancing by [`grapheme_width`] — not
/// by one cell — is what keeps text extraction free of phantom spaces and
/// what lets the highlighter paint both cells of a wide character.
pub(super) fn buffer_row_graphemes(buf: &Buffer, row: u16, area: Rect) -> Vec<Grapheme> {
    let mut graphemes = Vec::new();
    let mut x = area.x;
    while x < area.right() {
        // Links ride in the cell symbol (see `inject_osc8`) — measure and copy
        // the *displayed* text, or the URL would inflate the width and leak
        // control characters into the clipboard.
        let symbol = strip_osc8(buf[(x, row)].symbol()).into_owned();
        let width = grapheme_width(&symbol).min(area.right() - x);
        graphemes.push(Grapheme {
            col: x - area.x,
            width,
            symbol,
        });
        x += width;
    }
    graphemes
}

/// Columns a user-message cell fills with background before its text starts
/// (`cell_area.x + 2` in the render path, `super::viewport`) — the padding the
/// copy skips. KEEP IN SYNC with that literal.
const USER_MESSAGE_INSET: u16 = 2;

/// Append the padding inset of `height` content rows to `out`, clipped to the
/// visible range `[top, bottom)`; `vrow` advances past them.
fn push_row_insets(
    out: &mut Vec<u16>,
    vrow: &mut usize,
    height: usize,
    inset: u16,
    top: usize,
    bottom: usize,
) {
    let end = *vrow + height;
    let first = (*vrow).max(top);
    let last = end.min(bottom);
    if last > first {
        out.resize(out.len() + (last - first), inset);
    }
    *vrow = end;
}

impl ChatView {
    // ── Text selection: content ↔ screen mapping ────────────────────────

    /// Whether a screen position lies inside the chat band of the last frame.
    ///
    /// Used as the "may a drag start here?" test: a press outside the chat
    /// band (status bar, composer, popup) never starts a selection.
    pub fn contains_screen(&self, column: u16, row: u16) -> bool {
        let area = self.geometry.area;
        area.width > 0
            && area.height > 0
            && column >= area.x
            && column < area.right()
            && row >= area.y
            && row < area.bottom()
    }

    /// Map a screen position to a content point, clamping the pointer into
    /// the visible band *and* into the content.
    ///
    /// Clamping (rather than rejecting out-of-band coordinates) is what makes
    /// edge drags work: the pointer may sit on the status bar / composer, yet
    /// the selection still ends on the chat band's first / last visible row —
    /// which is also what the edge auto-scroll then scrolls from. The extra
    /// clamp against the content height keeps the coordinates meaningful when
    /// the content is shorter than the band (the blank rows below it are not
    /// content).
    pub fn content_point_at(&self, column: u16, row: u16) -> Option<SelectionPoint> {
        let area = self.geometry.area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let row = row.clamp(area.y, area.bottom() - 1);
        let column = column.clamp(area.x, area.right() - 1);
        let vrow = self.geometry.scroll_offset + (row - area.y) as usize;
        Some(SelectionPoint::chat(
            vrow.min(self.last_total.saturating_sub(1)),
            column - area.x,
        ))
    }

    /// Screen row that showed content row `vrow` in the last frame
    /// (`None` when it is scrolled out of the visible band).
    ///
    /// Kept for the tests and for the follow-up changes that need to ask where
    /// a content row currently sits (panel-visible checks, scrollbar) —
    /// production code only needs the forward direction today.
    pub fn screen_row_of(&self, vrow: usize) -> Option<u16> {
        let area = self.geometry.area;
        if area.height == 0 {
            return None;
        }
        let offset = vrow.checked_sub(self.geometry.scroll_offset)?;
        if offset >= area.height as usize {
            return None;
        }
        Some(area.y + offset as u16)
    }

    /// Snap a drag focus to the right edge of the grapheme under it.
    ///
    /// The pointer selects the character it rests on (reference behaviour):
    /// stopping on `d` copies through the `d` instead of cutting before it.
    /// Uses the last snapshot, which describes exactly the frame the pointer
    /// coordinates were mapped through; rows outside it are left untouched, and
    /// a click never reaches here (no drag event), so "press and release
    /// without moving = no selection" is unaffected.
    ///
    /// # Precondition
    ///
    /// Like [`Self::selected_text`], `point` must come from the frame the
    /// snapshot was captured from (`content_point_at` on that frame's
    /// geometry) — that is what "the frame the pointer coordinates were
    /// mapped through" means, and the snapshot's own coordinates are what
    /// index its rows.
    pub fn snap_focus_right(&self, point: SelectionPoint) -> SelectionPoint {
        if self.snapshot.is_empty() {
            // No drag frame has been captured yet — there is no rendered
            // grapheme to snap to.
            return point;
        }
        let Some(row) = self.snapshot.row(point.row) else {
            return point;
        };
        for grapheme in &row.graphemes {
            let end = grapheme.col.saturating_add(grapheme.width);
            if point.col >= grapheme.col && point.col < end {
                return SelectionPoint::chat(point.row, end.max(row.inset));
            }
        }
        point
    }

    // ── Text selection: highlight + copy source ─────────────────────────

    /// Paint the highlight for `bounds` into the frame's buffer.
    ///
    /// A pure overlay: the cell styles coming out of the widget render are
    /// merged with `REVERSED` (`Cell::set_style` keeps fg/bg and only inserts
    /// the modifier), so the terminal's underlying colors survive. Nothing is
    /// cleaned up afterwards — the buffer is refilled by the widgets on the
    /// next frame, so a selection that is gone simply is not painted again.
    ///
    /// Clipping uses the chat band of the last frame only; rows outside the
    /// band (status bar, composer, popups) are never touched. Wide graphemes
    /// are painted as a whole (all of their cells), so a partially selected
    /// CJK / emoji character is never half-inverted.
    pub fn paint_selection(&self, buf: &mut Buffer, selection: &Selection) {
        let area = self.geometry.area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Nothing to paint for a click / released selection (`bounds_in` is
        // only `Some` for a non-empty drag) — and a composer selection never
        // paints into the chat band: each region owns its own bounds.
        let Some(bounds) = selection.bounds_in(SelectionRegion::Chat) else {
            return;
        };
        // Iterate the *band* rows (bounded by the terminal height), not the
        // selection rows: an edge drag can span thousands of content rows,
        // while only the visible ones can be painted anyway. The band is this
        // frame's render rect, which already stops short of the scrollbar
        // gutter (see `ui::scrollbar`), so the highlight cannot reach the bar.
        let width = area.width;
        for row in area.y..area.bottom() {
            let vrow = self.geometry.scroll_offset + (row - area.y) as usize;
            let Some((from, to)) = row_span(bounds, vrow, width) else {
                continue;
            };
            for grapheme in buffer_row_graphemes(buf, row, area) {
                let grapheme_end = grapheme.col.saturating_add(grapheme.width);
                if grapheme_end <= from || grapheme.col >= to {
                    continue;
                }
                for dx in 0..grapheme.width {
                    let cell = &mut buf[(area.x + grapheme.col + dx, row)];
                    cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }

    /// Snapshot the visible rows as graphemes — the copy-on-select source.
    ///
    /// Called from the draw pass while a drag is in flight (never on the
    /// release itself: the frame the user was looking at is the authority).
    /// Each row also records the columns its cell fills with *padding*, so the
    /// copy can skip the inset a user message draws before its text without
    /// touching indentation that is part of the content.
    ///
    /// The whole snapshot is replaced in one assignment, so a failure between
    /// rows could never leave a half-captured frame behind.
    pub fn capture_visible_rows(&mut self, buf: &Buffer) {
        let area = self.geometry.area;
        if area.width == 0 || area.height == 0 {
            self.snapshot = FrameSnapshot::default();
            return;
        }
        let insets = self.visible_row_insets(area.height);
        self.snapshot = FrameSnapshot::capture(buf, area, self.geometry.scroll_offset, &insets);
    }

    /// Left padding (in columns) of every visible band row, in row order.
    ///
    /// Only the user-message cells inset their text (`cell_area.x + 2`); every
    /// other cell starts at the band's left edge. The rows are walked exactly
    /// like the render walks them (header → cells → pending), so a row's inset
    /// belongs to whichever entry drew it. Rows of an entry that is scrolled
    /// out of the band contribute nothing, which the caller reads as "no
    /// padding".
    ///
    /// MIRRORS the render walk in `super::viewport`'s widget (same order, same
    /// cached heights, same user-message inset): a layout change there must be
    /// mirrored here, and the copy-side test
    /// `snapshot_row_insets_match_the_rendered_rows` is what turns red when the
    /// two drift apart.
    fn visible_row_insets(&self, visible: u16) -> Vec<u16> {
        let top = self.geometry.scroll_offset;
        let bottom = top + visible as usize;
        let mut insets = Vec::with_capacity(visible as usize);
        let mut vrow = top;
        push_row_insets(
            &mut insets,
            &mut vrow,
            self.header_lines.len(),
            0,
            top,
            bottom,
        );
        for (index, cached) in self.cells.iter().enumerate() {
            let height = self.cell_heights.get(index).copied().unwrap_or(0);
            let inset = if matches!(
                cached.cell(),
                ChatCell::UserMessage(_)
                    | ChatCell::PendingUserMessage(_)
                    | ChatCell::DiscardedUserMessage(_)
            ) {
                USER_MESSAGE_INSET
            } else {
                0
            };
            push_row_insets(&mut insets, &mut vrow, height, inset, top, bottom);
        }
        for index in 0..self.pending.len() {
            let height = self.pending_heights.get(index).copied().unwrap_or(0);
            push_row_insets(
                &mut insets,
                &mut vrow,
                height,
                USER_MESSAGE_INSET,
                top,
                bottom,
            );
        }
        insets.truncate(visible as usize);
        insets
    }

    /// Text of the selected content range, taken from the last snapshot.
    ///
    /// # Precondition
    ///
    /// `bounds` must be content coordinates of the frame the snapshot was
    /// captured from — map the pointer through `content_point_at` on that
    /// frame's geometry (which is what `App` does: every press-active draw
    /// captures, so the snapshot and the geometry always describe the same
    /// frame). The extraction uses the snapshot's own `scroll_offset` /
    /// `width`, so rows from another frame would silently index the wrong
    /// lines.
    pub fn selected_text(&self, bounds: (SelectionPoint, SelectionPoint)) -> Option<String> {
        self.snapshot.text(bounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;

    use crate::ui::selection::Selection;
    use crate::ui::selection::SelectionPoint;

    use super::super::test_support::{render_view_in, reversed_columns, selection_over};

    #[test]
    fn test_geometry_records_the_frame_that_was_rendered() {
        let mut view = ChatView::new();
        // 30 lines of content in a 12-row band: auto-scroll pins the frame to
        // the bottom, so the geometry must carry the *pinned* offset.
        let text = (0..30)
            .map(|i| format!("row-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.push(ChatCell::UserMessage(text));
        let band = Rect::new(0, 1, 40, 12);
        let _ = render_view_in(&mut view, Rect::new(0, 0, 40, 20), band);
        let geom = view.geometry();
        assert_eq!(geom.area, band);
        assert_eq!(geom.scroll_offset, view.scroll_position());
        assert_eq!(geom.scroll_offset, view.content_height() - 12);
    }

    #[test]
    fn test_content_point_maps_screen_rows_and_clamps() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let band = Rect::new(2, 1, 30, 10);
        let _ = render_view_in(&mut view, Rect::new(0, 0, 40, 20), band);

        // Inside the band: content row = scroll + (row - band.top), clamped
        // into the content — this conversation is only three rows tall, so a
        // row further down the band maps to its last row.
        let point = view.content_point_at(7, 4).expect("inside the band");
        assert_eq!((point.row, point.col), (2, 5));
        assert!(view.contains_screen(7, 4));

        // Pointer outside the band clamps to the nearest edge instead of
        // failing — edge drags (and their auto-scroll) depend on this. The
        // row clamps twice: into the band, then into the content (the blank
        // rows below a short conversation are not content).
        assert_eq!(view.content_point_at(0, 0).map(|p| p.row), Some(0));
        assert_eq!(view.content_point_at(0, 0).map(|p| p.col), Some(0));
        assert_eq!(view.content_point_at(99, 99).map(|p| p.row), Some(2));
        assert_eq!(view.content_point_at(99, 99).map(|p| p.col), Some(29));
        assert!(!view.contains_screen(1, 4), "left of the band");
        assert!(!view.contains_screen(7, 11), "below the band");

        // Reverse mapping agrees.
        assert_eq!(view.screen_row_of(3), Some(4));
        assert_eq!(view.screen_row_of(9), Some(10));
        assert_eq!(view.screen_row_of(10), None, "scrolled out of the band");
    }

    #[test]
    fn test_mapping_follows_a_scrolled_frame() {
        let mut view = ChatView::new();
        let text = (0..30)
            .map(|i| format!("row-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.push(ChatCell::UserMessage(text));
        let band = Rect::new(0, 0, 40, 10);
        let _ = render_view_in(&mut view, Rect::new(0, 0, 40, 10), band);

        // Pinned to the bottom: the first visible row is not content row 0.
        let first = view.geometry().scroll_offset;
        assert_eq!(view.screen_row_of(0), None);
        assert_eq!(view.content_point_at(0, 0).map(|p| p.row), Some(first));
        assert_eq!(view.screen_row_of(first), Some(0));

        // Scroll up five rows: the mapping follows the new frame, so the same
        // screen row now points at different content.
        view.scroll_up(5);
        let _ = render_view_in(&mut view, Rect::new(0, 0, 40, 10), band);
        assert_eq!(view.geometry().scroll_offset, first - 5);
        assert_eq!(view.content_point_at(0, 0).map(|p| p.row), Some(first - 5));
    }

    #[test]
    fn test_paint_selection_marks_only_the_selected_cells() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello world".into()));
        let band = Rect::new(2, 3, 30, 8);
        let mut buf = render_view_in(&mut view, Rect::new(0, 0, 40, 20), band);
        let before = buf.clone();

        // Row 1 of the user cell is the text row ("hello world" at column 2).
        let bounds = (SelectionPoint::chat(1, 2), SelectionPoint::chat(1, 13));
        view.paint_selection(&mut buf, &selection_over(bounds));

        assert_eq!(
            reversed_columns(&buf, band.y + 1),
            (band.x + 2..band.x + 13).collect::<Vec<u16>>(),
            "exactly the selected span is highlighted"
        );
        for y in buf.area.y..buf.area.bottom() {
            for x in buf.area.x..buf.area.right() {
                if y == band.y + 1 && x >= band.x + 2 && x < band.x + 13 {
                    continue;
                }
                assert_eq!(
                    buf[(x, y)],
                    before[(x, y)],
                    "cell ({x},{y}) must be untouched"
                );
            }
        }
    }

    #[test]
    fn test_paint_selection_covers_both_cells_of_a_wide_grapheme() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("你好世界".into()));
        let band = Rect::new(0, 0, 20, 6);
        let mut buf = render_view_in(&mut view, Rect::new(0, 0, 20, 6), band);

        // Text starts at column 2; "你好" occupies columns 2..6 (two 2-wide
        // graphemes). Selecting only the first cell of "好" must still paint
        // the whole character.
        let bounds = (SelectionPoint::chat(1, 2), SelectionPoint::chat(1, 5));
        view.paint_selection(&mut buf, &selection_over(bounds));
        assert_eq!(
            reversed_columns(&buf, 1),
            (2..6).collect::<Vec<u16>>(),
            "a partially selected wide grapheme is painted whole"
        );
        assert!(
            reversed_columns(&buf, 0).is_empty(),
            "the padding row stays clean"
        );
    }

    #[test]
    fn test_paint_selection_clips_to_the_band() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let band = Rect::new(5, 2, 10, 3);
        let mut buf = render_view_in(&mut view, Rect::new(0, 0, 20, 10), band);
        let before = buf.clone();

        // Greedy bounds covering the whole content — only the band may be
        // painted, and only up to the band's own columns.
        let bounds = (SelectionPoint::chat(0, 0), SelectionPoint::chat(0, 40));
        view.paint_selection(&mut buf, &selection_over(bounds));
        for y in buf.area.y..buf.area.bottom() {
            for x in buf.area.x..buf.area.right() {
                let inside_band =
                    x >= band.x && x < band.right() && y >= band.y && y < band.bottom();
                if inside_band && y == band.y {
                    continue;
                }
                assert_eq!(buf[(x, y)], before[(x, y)], "({x},{y}) is outside the band");
            }
        }
        assert_eq!(
            reversed_columns(&buf, band.y),
            (band.x..band.right()).collect::<Vec<u16>>()
        );
    }

    #[test]
    fn test_paint_selection_ignores_a_composer_selection() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let band = Rect::new(0, 0, 20, 6);
        let mut buf = render_view_in(&mut view, Rect::new(0, 0, 20, 6), band);
        let before = buf.clone();

        // A selection anchored in the other region has no chat bounds: the
        // two coordinate spaces never share a highlight.
        let mut selection = Selection::default();
        selection.begin(SelectionPoint::composer(0, 0));
        selection.drag_to(SelectionPoint::composer(0, 5));
        view.paint_selection(&mut buf, &selection);
        assert_eq!(buf, before, "a composer selection must not paint here");
    }

    #[test]
    fn test_capture_and_selected_text_from_the_rendered_frame() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello world".into()));
        let band = Rect::new(0, 0, 30, 8);
        let buf = render_view_in(&mut view, Rect::new(0, 0, 30, 8), band);
        view.capture_visible_rows(&buf);

        let text = view.selected_text((SelectionPoint::chat(1, 2), SelectionPoint::chat(1, 13)));
        assert_eq!(text.as_deref(), Some("hello world"));
    }

    #[test]
    fn test_selected_text_keeps_cjk_free_of_phantom_spaces() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("你好世界".into()));
        let band = Rect::new(0, 0, 30, 8);
        let buf = render_view_in(&mut view, Rect::new(0, 0, 30, 8), band);
        view.capture_visible_rows(&buf);

        // Columns 2..10 cover "你好世界" (four 2-wide graphemes).
        let text = view.selected_text((SelectionPoint::chat(1, 2), SelectionPoint::chat(1, 10)));
        assert_eq!(text.as_deref(), Some("你好世界"));
        assert!(
            !text.as_deref().unwrap().contains(' '),
            "no filler cell may leak a space inside the CJK run"
        );
    }

    #[test]
    fn test_selected_text_is_none_without_a_snapshot() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let _ = render_view_in(&mut view, Rect::new(0, 0, 30, 8), Rect::new(0, 0, 30, 8));
        // No capture (no drag in flight) → nothing to copy.
        assert_eq!(
            view.selected_text((SelectionPoint::chat(0, 0), SelectionPoint::chat(1, 5),)),
            None
        );
    }

    #[test]
    fn test_capture_clears_when_the_band_collapses() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let buf = render_view_in(&mut view, Rect::new(0, 0, 30, 8), Rect::new(0, 0, 30, 8));
        view.capture_visible_rows(&buf);
        assert!(!view.snapshot.is_empty());

        // A collapsed band (zero height) must drop the mapping *and* the
        // snapshot — a stale rect could otherwise accept presses.
        let _ = render_view_in(&mut view, Rect::new(0, 0, 30, 8), Rect::new(0, 0, 30, 0));
        assert_eq!(view.geometry().area, Rect::ZERO);
        assert!(view.content_point_at(1, 1).is_none());
        view.capture_visible_rows(&buf);
        assert!(view.snapshot.is_empty());
    }

    #[test]
    fn snapshot_capture_describes_the_frame_it_came_from() {
        let mut view = ChatView::new();
        let text = (0..30)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.push(ChatCell::UserMessage(text));
        let band = Rect::new(3, 2, 30, 8);
        let buf = render_view_in(&mut view, Rect::new(0, 0, 40, 20), band);
        view.capture_visible_rows(&buf);

        let snapshot = &view.snapshot;
        let offset = snapshot.scroll_offset;
        assert_eq!(offset, view.geometry().scroll_offset);
        assert_eq!(snapshot.width, band.width);
        assert_eq!(snapshot.rows.len(), band.height as usize);
        assert_eq!(
            offset, 24,
            "pinned to the bottom of 32 rows in an 8-row band"
        );
        assert_eq!(
            snapshot.row(offset).map(|row| row.inset),
            Some(USER_MESSAGE_INSET),
            "the user-message padding is recorded per row"
        );
        assert_eq!(
            snapshot.row(offset - 1),
            None,
            "row above the captured band"
        );
        assert_eq!(
            snapshot.row(offset + band.height as usize),
            None,
            "row below the captured band"
        );
    }

    #[test]
    fn snapshot_text_uses_its_own_frame_coordinates() {
        let mut view = ChatView::new();
        let text = (0..30)
            .map(|i| format!("row-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.push(ChatCell::UserMessage(text));
        let band = Rect::new(0, 0, 40, 10);
        let buf = render_view_in(&mut view, band, band);
        view.capture_visible_rows(&buf);
        // Row 0 of a user message is its top padding, the text lines follow,
        // and the view is pinned to the bottom of the 32-row content.
        let offset = view.geometry().scroll_offset;
        assert_eq!(offset, 22);

        // The text starts after the 2-column padding inset.
        let bounds = (
            SelectionPoint::chat(offset, 2),
            SelectionPoint::chat(offset, 8),
        );
        assert_eq!(view.selected_text(bounds).as_deref(), Some("row-21"));

        // A later frame at a different offset moves the *geometry*, not the
        // snapshot: the copy still answers from the frame the snapshot
        // describes (an extraction through the new offset would read row-24).
        view.scroll_up(3);
        let _ = render_view_in(&mut view, band, band);
        assert_eq!(view.geometry().scroll_offset, offset - 3);
        assert_eq!(view.selected_text(bounds).as_deref(), Some("row-21"));
    }

    #[test]
    fn snapshot_rows_below_the_content_default_to_inset_zero() {
        // `visible_row_insets` only reports the rows that carry content, so the
        // blank rows below a short conversation have no entry at all — they
        // must fall back to "no padding", not to the last cell's inset.
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hi".into()));
        let band = Rect::new(0, 0, 20, 6);
        let buf = render_view_in(&mut view, band, band);
        view.capture_visible_rows(&buf);
        let insets: Vec<u16> = view.snapshot.rows.iter().map(|row| row.inset).collect();
        assert_eq!(
            insets,
            vec![
                USER_MESSAGE_INSET,
                USER_MESSAGE_INSET,
                USER_MESSAGE_INSET,
                0,
                0,
                0
            ],
            "the three user-message rows are padded, the blank rows below are not"
        );
    }

    #[test]
    fn snapshot_is_empty_when_the_band_has_no_width() {
        // The degenerate band has two shapes — no height and no width. Both
        // must clear the mapping *and* the snapshot (a stale rect could accept
        // presses, a stale snapshot could copy invisible rows).
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let buf = render_view_in(&mut view, Rect::new(0, 0, 30, 8), Rect::new(0, 0, 0, 8));
        assert_eq!(view.geometry().area, Rect::ZERO);
        view.capture_visible_rows(&buf);
        assert!(view.snapshot.is_empty());
        assert_eq!(
            view.selected_text((SelectionPoint::chat(0, 0), SelectionPoint::chat(0, 5))),
            None
        );
    }

    #[test]
    fn snapshot_row_insets_match_the_rendered_rows() {
        // The inset table is derived by replaying the render walk
        // (`visible_row_insets` ↔ `ChatViewWidget::render`); this pins the two
        // together: if a layout change moves a cell's text start without
        // updating the table, the recorded inset and the buffer disagree here.
        let mut view = ChatView::new();
        view.set_header(vec![Line::from("wing header")]);
        view.push(ChatCell::UserMessage("first question".into()));
        view.push(ChatCell::AssistantMessage("answer".into()));
        view.push_pending("req-1".into(), "queued".into());
        let band = Rect::new(0, 0, 30, 14);
        let buf = render_view_in(&mut view, band, band);
        view.capture_visible_rows(&buf);

        let mut padded_rows = 0;
        let mut flush_rows = 0;
        for (index, row) in view.snapshot.rows.iter().enumerate() {
            let columns = buffer_row_graphemes(&buf, band.y + index as u16, band);
            let first_text = columns.iter().position(|g| !g.symbol.trim().is_empty());
            match (row.inset, first_text) {
                (0, Some(col)) => {
                    assert_eq!(col, 0, "row {index}: inset 0 must start at the band's edge");
                    flush_rows += 1;
                }
                // A blank row (padding / trailing blank line / below content).
                (_, None) => {}
                (inset, Some(col)) => {
                    assert_eq!(
                        col, inset as usize,
                        "row {index}: the recorded inset must be where the text starts"
                    );
                    padded_rows += 1;
                }
            }
        }
        assert!(
            padded_rows > 0 && flush_rows > 0,
            "the fixture must cover both a padded and an unpadded row              (padded={padded_rows}, flush={flush_rows})"
        );
    }

    #[test]
    fn empty_snapshot_yields_no_text_and_no_row() {
        let snapshot = FrameSnapshot::default();
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.row(0), None);
        assert_eq!(
            snapshot.text((SelectionPoint::chat(0, 0), SelectionPoint::chat(0, 5))),
            None
        );
    }
}
