//! The link face: the per-frame hit boxes, masking and the OSC8 injection.
//!
//! ratatui has no hyperlink API, so a link exists twice: as a hit box in
//! [`LinkTable`] (what a click resolves against) and as an OSC8 sequence
//! injected into the cell symbols (what the terminal renders as clickable).
//! Both are built from the same `LinkSpan` table while a frame renders, so
//! the two views can never disagree.
//!
//! The table is installed in one assignment at the end of the frame
//! (`ChatViewWidget`), which is what keeps a half-rendered frame from being
//! hit-testable. `mask_links` is the app's escape hatch for overlays painted
//! over the chat afterwards (the toast): a hit box whose text is no longer
//! visible must not open anything.

use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;

use crate::render::markdown::LinkSpan;
use crate::render::markdown::osc8_close;
use crate::render::markdown::osc8_open;
use crate::render::markdown::sanitize_osc8_target;
use crate::render::markdown::symbol_width;

use super::ChatView;

/// One link of the last rendered frame, in **absolute screen columns**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameLink {
    /// First screen column of the link text (inclusive).
    pub start: u16,
    /// One past the last screen column of the link text (exclusive).
    pub end: u16,
    /// The markdown destination, as written (sanitised only for injection).
    pub target: String,
}

/// Link hit boxes of one rendered frame, grouped by screen row (ascending).
///
/// Built while the frame renders ([`place_links`]) and installed in one
/// assignment at the end, so the hit test never sees a half-built frame (see
/// `ChatViewWidget`). Every column is an absolute screen column, which is what
/// both the click lookup and [`ChatView::link_at`] speak.
#[derive(Debug, Clone)]
pub(super) struct LinkTable(Vec<(u16, Vec<FrameLink>)>);

impl LinkTable {
    /// An empty table — the one a frame starts collecting into.
    pub(super) fn new() -> Self {
        Self(Vec::new())
    }

    /// The installed rows (test seam / diagnostics).
    fn as_slice(&self) -> &[(u16, Vec<FrameLink>)] {
        &self.0
    }

    /// Record the hit boxes of one screen row, in render order.
    fn push_row(&mut self, row: u16, links: Vec<FrameLink>) {
        self.0.push((row, links));
    }

    /// Link target at an absolute screen position, if any.
    fn link_at(&self, column: u16, row: u16) -> Option<&str> {
        self.0
            .iter()
            .find(|(r, _)| *r == row)
            .and_then(|(_, links)| {
                links
                    .iter()
                    .find(|link| column >= link.start && column < link.end)
            })
            .map(|link| link.target.as_str())
    }

    /// Drop the hit boxes covered by `area`.
    fn mask(&mut self, area: Rect) {
        self.0.retain_mut(|(row, links)| {
            if *row < area.y || *row >= area.bottom() {
                return true;
            }
            links.retain(|link| link.end <= area.x || link.start >= area.right());
            !links.is_empty()
        });
    }
}

impl ChatView {
    /// Link target at a screen position of the last rendered frame.
    ///
    /// `None` when the position is not inside a link (or the frame had
    /// none) — a click there is a no-op, never a guessed open.
    pub fn link_at(&self, column: u16, row: u16) -> Option<&str> {
        self.frame_links.link_at(column, row)
    }

    /// Links of the last rendered frame (test seam / diagnostics).
    pub fn frame_links(&self) -> &[(u16, Vec<FrameLink>)] {
        self.frame_links.as_slice()
    }

    /// Drop the link hit boxes covered by `area` (an overlay painted on top of
    /// the chat after this frame's links were recorded).
    ///
    /// A hit box whose text is no longer visible would open a link the user
    /// cannot see. Today the only overlay over the chat band is the toast; the
    /// scrollbar column (`tui-scrollbar`) will be the next one — when it lands,
    /// its column must be masked here too (it paints over the last columns of
    /// every row, including link text), or clicks on it will open links.
    pub fn mask_links(&mut self, area: Rect) {
        self.frame_links.mask(area);
    }
}

/// The link table to place for one cell.
///
/// Cloned only when the cell actually has something to place (and its rows are
/// exact), so a frame full of cells without links allocates nothing — the
/// returned vector is `Vec::new()` (capacity 0) in that case.
pub(super) fn links_for_frame(cell: &crate::ui::cached_cell::CellLines<'_>) -> Vec<Vec<LinkSpan>> {
    if cell.rows_exact && cell.has_links() {
        cell.links.to_vec()
    } else {
        Vec::new()
    }
}

/// Record and inject the links of one cell's visible rows.
///
/// `first_line` is the cell's line index at the top of `first_row` (the blit
/// path passes the skip it blitted from; the `Paragraph` path its scroll
/// offset) — valid only because the caller checked `rows_exact`, i.e. every
/// line occupies exactly one screen row. Link columns are shifted into screen
/// space here, so both the hit test and the OSC8 injection work on absolute
/// columns.
pub(super) fn place_links(
    frame_links: &mut LinkTable,
    buf: &mut Buffer,
    cell_links: &[Vec<LinkSpan>],
    area: Rect,
    first_line: usize,
    first_row: u16,
    visible: usize,
) {
    if cell_links.is_empty() {
        return;
    }
    for offset in 0..visible {
        let Some(spans) = cell_links.get(first_line + offset) else {
            break;
        };
        if spans.is_empty() {
            continue;
        }
        let row = first_row + offset as u16;
        if row >= area.bottom() {
            break;
        }
        let mut links = Vec::with_capacity(spans.len());
        for span in spans {
            let start = area.x.saturating_add(span.start);
            let end = area.x.saturating_add(span.end).min(area.right());
            if start >= end {
                continue;
            }
            inject_osc8(buf, row, start, end, &span.target);
            links.push(FrameLink {
                start,
                end,
                target: span.target.clone(),
            });
        }
        if !links.is_empty() {
            frame_links.push_row(row, links);
        }
    }
}

/// Put an OSC8 hyperlink on every grapheme head cell of `start..end`.
///
/// ratatui has no hyperlink API, so the sequence rides in the cell symbol —
/// the one string the crossterm backend prints verbatim. Two details make that
/// safe:
///
/// * `CellDiffOption::ForcedWidth(visible width)`: `Cell::cell_width()`
///   otherwise measures the symbol (URL included) and `BufferDiff` uses that
///   width to skip the cells *after* a wide one — a 40-character URL would make
///   the diff skip the rest of the line. The forced value has to be the width
///   the **terminal** renders (1 for a narrow grapheme, 2 for CJK / emoji), not
///   a constant 1: `CrosstermBackend::draw` skips the `MoveTo` for a cell that
///   follows at `x + 1`, so a cell that claims one column but prints two
///   desynchronises the backend's cursor from the terminal's and every cell
///   after it in the row lands one column off (visible as `百度` turning into
///   `百 度` plus the row's tail bleeding onto the next line, but only on
///   partial repaints — a full repaint re-emits the row in order).
/// * every head cell carries its own open/close pair, so a partial repaint can
///   never leave a cell without its hyperlink (the alternative — open on the
///   first cell, close on the last — breaks when the diff re-emits a middle
///   cell only).
///
/// Wide graphemes are injected once, at the head cell: the terminal applies the
/// sequence to the whole glyph, and writing anything into the filler cells
/// would print a space over its right half.
fn inject_osc8(buf: &mut Buffer, row: u16, start: u16, end: u16, target: &str) {
    let open = osc8_open(&sanitize_osc8_target(target));
    let close = osc8_close();
    let end = end.min(buf.area.right());
    if row >= buf.area.bottom() {
        return;
    }
    let mut x = start;
    while x < end {
        let width = symbol_width(buf[(x, row)].symbol());
        if width == 0 {
            // A zero-width cell (or a malformed one) cannot advance the
            // cursor — bail instead of looping forever.
            break;
        }
        let symbol = buf[(x, row)].symbol().to_string();
        let cell = &mut buf[(x, row)];
        cell.set_symbol(&format!("{open}{symbol}{close}"))
            .set_diff_option(CellDiffOption::ForcedWidth(
                // The width the terminal renders, see the note above: the
                // diff and the backend's adjacency shortcut both have to
                // agree with it or the rest of the row shifts.
                std::num::NonZeroU16::new(width).expect("a grapheme is at least 1 column"),
            ));
        x += width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;
    use ratatui::style::Style;
    use ratatui::text::Line;

    use crate::render::markdown::ComposedLines;
    use crate::render::markdown::strip_osc8;
    use crate::ui::selection::Selection;
    use crate::ui::selection::SelectionPoint;

    use super::super::ChatCell;
    use super::super::frame::buffer_row_graphemes;
    use super::super::test_support::render_view as render;

    /// A row's displayed text — hyperlinks stripped, wide-grapheme filler
    /// cells skipped (exactly what the user reads).
    fn row_text(buf: &Buffer, row: u16) -> String {
        buffer_row_graphemes(buf, row, buf.area)
            .iter()
            .map(|g| g.symbol.as_str())
            .collect()
    }

    fn find_row(buf: &Buffer, needle: &str) -> u16 {
        (buf.area.y..buf.area.bottom())
            .find(|&y| row_text(buf, y).contains(needle))
            .unwrap_or_else(|| panic!("row not found: {needle}"))
    }

    /// Columns of a row whose symbol carries an OSC8 sequence.
    fn linked_columns(buf: &Buffer, row: u16) -> Vec<u16> {
        (buf.area.x..buf.area.right())
            .filter(|&x| buf[(x, row)].symbol().contains("\u{1b}]8;;"))
            .collect()
    }

    /// Displayed text of the columns `start..end` of a row.
    fn columns_text(buf: &Buffer, row: u16, start: u16, end: u16) -> String {
        (start..end)
            .map(|x| strip_osc8(buf[(x, row)].symbol()).into_owned())
            .collect()
    }

    /// The single link of `view`'s last frame.
    fn only_link(view: &ChatView) -> (u16, FrameLink) {
        let mut iter = view.frame_links().iter();
        let (row, links) = iter.next().expect("frame has a link");
        assert!(iter.next().is_none(), "expected exactly one linked row");
        assert_eq!(links.len(), 1, "expected exactly one link per row");
        (*row, links[0].clone())
    }

    /// Replay a frame-to-frame diff the way `CrosstermBackend::draw` writes it,
    /// onto a terminal that advances its cursor by what it *prints*.
    ///
    /// The backend skips its `MoveTo` when the next emitted cell sits at
    /// `last_x + 1` — sound only while every printed symbol advances the cursor
    /// by exactly the width the cell claimed. This returns the prints that
    /// landed somewhere the backend did not mean, which is the artifact class a
    /// `TestBackend` cannot see: it never models a cursor, it just copies cells.
    fn wired_desync(prev: &Buffer, next: &Buffer) -> Vec<String> {
        let mut cursor = (0u16, 0u16);
        let mut assumed: Option<(u16, u16)> = None;
        let mut drifted = Vec::new();
        for (x, y, cell) in prev.diff(next) {
            let rendered = strip_osc8(cell.symbol());
            let width = crate::ui::selection::grapheme_width(&rendered);
            let skip_move = assumed == Some((x.wrapping_sub(1), y));
            let at = if skip_move { cursor } else { (x, y) };
            if at != (x, y) {
                drifted.push(format!(
                    "{rendered:?} meant for ({x},{y}) was printed at {at:?}"
                ));
            }
            let next_x = at.0 + width;
            cursor = if next_x >= next.area.width {
                (next_x - next.area.width, at.1 + 1)
            } else {
                (next_x, at.1)
            };
            assumed = Some((x, y));
        }
        drifted
    }

    /// The click-invariant behind `place_links`: every row of the frame map
    /// really shows the link text it claims.
    fn assert_link_rows_show_their_text(view: &ChatView, buf: &Buffer) {
        for (row, links) in view.frame_links() {
            let text = row_text(buf, *row);
            for _link in links {
                assert!(
                    text.contains("docs") || text.contains("example.com"),
                    "row {row} carries a hit box but shows {text:?}"
                );
            }
        }
    }

    fn link(start: u16, end: u16) -> FrameLink {
        FrameLink {
            start,
            end,
            target: "https://example.com".into(),
        }
    }

    #[test]
    fn assistant_links_render_as_osc8_and_are_hit_testable() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "see [docs](https://example.com) now".into(),
        ));
        let buf = render(&mut view, 60, 6);

        // Remote targets are displayed after the label (existing markdown
        // rule) and the whole displayed run belongs to the link.
        let row = find_row(&buf, "see docs (https://example.com) now");
        let (link_row, link) = only_link(&view);
        assert_eq!(link_row, row);
        assert_eq!(link.start, 6, "`⦁ ` + `see ` precede the link");
        assert_eq!(
            columns_text(&buf, row, link.start, link.end),
            "docs (https://example.com)"
        );
        assert_eq!(link.target, "https://example.com");

        // Every column of the run is hyperlinked, each cell self-contained.
        assert_eq!(
            linked_columns(&buf, row),
            (link.start..link.end).collect::<Vec<_>>()
        );
        for x in link.start..link.end {
            let symbol = buf[(x, row)].symbol();
            assert!(
                symbol.starts_with(&osc8_open("https://example.com")),
                "{symbol:?}"
            );
            assert!(symbol.ends_with(&osc8_close()), "{symbol:?}");
        }
        // ...and nothing outside it.
        assert_eq!(buf[(0, row)].symbol(), "⦁");
        assert!(!buf[(5, row)].symbol().contains('\u{1b}'));
        assert!(!buf[(link.end, row)].symbol().contains('\u{1b}'));
        for other in buf.area.y..buf.area.bottom() {
            if other != row {
                assert!(linked_columns(&buf, other).is_empty());
            }
        }

        assert_eq!(view.link_at(link.start, row), Some("https://example.com"));
        assert_eq!(view.link_at(link.end - 1, row), Some("https://example.com"));
        assert_eq!(view.link_at(link.start - 1, row), None, "the space before");
        assert_eq!(view.link_at(link.end, row), None);
        assert_eq!(view.link_at(link.start, row + 1), None, "other rows");
    }

    #[test]
    fn cells_without_links_carry_no_sequences() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("plain text".into()));
        let buf = render(&mut view, 40, 6);
        for row in buf.area.y..buf.area.bottom() {
            assert!(linked_columns(&buf, row).is_empty());
        }
        assert!(view.frame_links().is_empty());
        assert_eq!(view.link_at(0, 0), None);
    }

    #[test]
    fn injected_link_cells_survive_text_extraction() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "see [docs](https://example.com) now".into(),
        ));
        let buf = render(&mut view, 60, 6);
        let row = find_row(&buf, "see docs");
        let graphemes = buffer_row_graphemes(&buf, row, buf.area);
        let text: String = graphemes.iter().map(|g| g.symbol.as_str()).collect();
        assert!(
            !text.contains('\u{1b}'),
            "extraction leaked escapes: {text:?}"
        );
        assert!(text.contains("see docs (https://example.com) now"));
        let d = graphemes.iter().find(|g| g.symbol == "d").expect("`d`");
        assert_eq!(d.width, 1, "the URL must not inflate the measured width");
    }

    #[test]
    fn wide_link_text_injects_one_sequence_per_grapheme_head() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "见 [你好](https://example.com)".into(),
        ));
        let buf = render(&mut view, 60, 6);
        let row = find_row(&buf, "你好");
        let (_, link) = only_link(&view);
        // `⦁ ` (2) + `见` (2) + space (1) => 你好 starts at column 5, and the
        // filler column of each wide grapheme stays untouched.
        assert_eq!(link.start, 5);
        let linked = linked_columns(&buf, row);
        assert_eq!(
            &linked[..3],
            &[5, 7, 9],
            "wide graphemes are injected at their head cell only"
        );
        assert_eq!(
            linked.len(),
            (link.end - link.start) as usize - 2,
            "the two wide graphemes contribute one head cell per two columns"
        );
        // The displayed text is unchanged (filler columns read back as spaces
        // and are skipped by the grapheme walk).
        assert!(row_text(&buf, row).contains("见 你好 (https://example.com)"));
        for col in [5u16, 7, 9] {
            assert_eq!(
                view.link_at(col, row),
                Some("https://example.com"),
                "col {col}"
            );
            assert_eq!(
                view.link_at(col + 1, row),
                Some("https://example.com"),
                "filler"
            );
        }
        assert_eq!(view.link_at(4, row), None, "the space before the link");
    }

    /// A frame-to-frame repaint must not smear on a real terminal.
    ///
    /// The regression: an OSC8-wrapped CJK grapheme used to pin its diff width
    /// to `1` while the terminal renders it in two columns. The backend then
    /// skipped the `MoveTo` for the filler column (it believed the cursor was
    /// already there) and printed it one column to the right — `百度` came out
    /// as `百 度` and the row's tail bled into the next line, but only on
    /// partial repaints (scrolling), because a full repaint emits the row in
    /// order.
    #[test]
    fn a_wide_linked_grapheme_moving_one_column_keeps_the_wire_in_step() {
        // Same message, one character longer: the linked wide graphemes sit one
        // column further right, so the repaint has to write a wide cell *and*
        // the filler column the previous frame held a glyph in.
        let mut before = ChatView::new();
        before.push(ChatCell::AssistantMessage(
            "见 [百度首页](https://www.baidu.com)".into(),
        ));
        let prev = render(&mut before, 40, 3);

        let mut after = ChatView::new();
        after.push(ChatCell::AssistantMessage(
            "见一 [百度首页](https://www.baidu.com)".into(),
        ));
        let next = render(&mut after, 40, 3);

        // Guard: the frame must really carry an injected wide grapheme, or the
        // test would pass by rendering nothing interesting.
        let linked: Vec<u16> = (0..40)
            .filter(|&x| prev[(x, 0)].symbol().contains("\u{1b}]8;;"))
            .collect();
        assert!(
            linked
                .iter()
                .any(|&x| symbol_width(prev[(x, 0)].symbol()) == 2),
            "expected an OSC8-wrapped CJK grapheme, got linked columns {linked:?}"
        );

        let drifted = wired_desync(&prev, &next);
        assert!(
            drifted.is_empty(),
            "the terminal cursor drifted from the backend's model:\n{}",
            drifted.join("\n")
        );
    }

    #[test]
    fn link_cells_set_forced_width_so_the_diff_does_not_skip_following_cells() {
        use ratatui::buffer::CellDiffOption;
        use std::num::NonZeroU16;

        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "see [docs](https://example.com/a/very/long/path) now".into(),
        ));
        let buf = render(&mut view, 80, 6);
        let row = find_row(&buf, "now");
        let linked = linked_columns(&buf, row);
        assert!(!linked.is_empty());
        for x in &linked {
            assert_eq!(
                buf[(*x, row)].diff_option,
                CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap()),
                "cell {x} must pin its diff width to 1"
            );
        }
        // The cells after the link must still show up in the diff output — a
        // symbol whose measured width includes the URL would make `BufferDiff`
        // skip them (`self.pos += cell_width - 1`).
        let diff = Buffer::empty(buf.area).diff(&buf);
        let tail_x = row_text(&buf, row).find("now").expect("tail") as u16;
        assert!(
            diff.iter().any(|(x, y, _)| *y == row && *x == tail_x),
            "the cell after the link is missing from the diff"
        );
    }

    #[test]
    fn injecting_links_keeps_width_wrap_and_height_unchanged() {
        // Same visible text, one side markdown-linked. The comparison is only
        // meaningful because the other side really has no links — otherwise it
        // is the same input rendered twice and every assertion holds trivially.
        let linked = "para one with [a link](https://example.com/x) and more words to wrap around";
        let plain_text =
            "para one with a link (https://example.com/x) and more words to wrap around";
        let mut with = ChatView::new();
        with.push(ChatCell::AssistantMessage(linked.into()));
        let mut without = ChatView::new();
        without.push(ChatCell::AssistantMessage(plain_text.into()));

        let buf = render(&mut with, 40, 12);
        let plain = render(&mut without, 40, 12);
        assert!(
            plain
                .area
                .positions()
                .all(|p| !plain[p].symbol().contains("\u{1b}]8;;")),
            "the reference frame must be link-free for this to be a comparison"
        );
        assert!(without.frame_links().is_empty());
        assert!(!with.frame_links().is_empty());
        for row in buf.area.y..buf.area.bottom() {
            assert_eq!(row_text(&buf, row), row_text(&plain, row), "row {row}");
        }
        assert_eq!(with.content_height(), without.content_height());
        assert_eq!(with.scroll_position(), without.scroll_position());
    }

    #[test]
    fn a_fitting_code_line_keeps_its_link_on_the_right_row() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "```rust\nlet x = 1;\n```\ntail [docs](https://example.com) end".into(),
        ));
        let buf = render(&mut view, 40, 12);
        assert!(!view.frame_links().is_empty(), "code that fits keeps links");
        assert_link_rows_show_their_text(&view, &buf);
        let (row, _) = only_link(&view);
        assert!(row_text(&buf, row).contains("docs"), "row {row}");
    }

    #[test]
    fn an_overwide_code_line_puts_the_cell_off_limits() {
        // `Paragraph` wraps the over-wide code line into two screen rows, so
        // screen row != line index from there on: the link later in the same
        // cell would be injected and hit-tested on the *code* rows. The cell
        // must produce no links at all instead (B1).
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "```rust\nlet some_extremely_long_variable_name_that_exceeds_terminal_width_by_a_lot = 1;\n```\ntail [docs](https://example.com) end".into(),
        ));
        let buf = render(&mut view, 40, 12);
        assert!(
            view.frame_links().is_empty(),
            "a cell whose rows are not exact must place no links: {:?}",
            view.frame_links()
        );
        let code_row = find_row(&buf, "some_extremely_long_variable_name");
        assert!(
            linked_columns(&buf, code_row).is_empty(),
            "no sequence may land on code text"
        );
        for row in buf.area.y..buf.area.bottom() {
            assert!(
                linked_columns(&buf, row).is_empty(),
                "row {row} was injected"
            );
        }
        // The link text itself still renders (only the link is dropped).
        assert!(row_text(&buf, find_row(&buf, "tail docs")).contains("docs"));
    }

    #[test]
    fn an_overwide_indented_code_line_puts_the_cell_off_limits() {
        // Indented code is exempt from the IR prose wrapper for the same reason
        // fenced code is, so it reaches `Paragraph` over-wide and breaks the row
        // arithmetic just like the fenced case.
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(format!(
            "    {}\n\ntail [docs](https://example.com) end",
            "let y = 1; // ".to_string() + &"z".repeat(60)
        )));
        let buf = render(&mut view, 40, 12);
        assert!(
            view.frame_links().is_empty(),
            "indented code row shifted the mapping: {:?}",
            view.frame_links()
        );
        for row in buf.area.y..buf.area.bottom() {
            assert!(
                linked_columns(&buf, row).is_empty(),
                "row {row} was injected"
            );
        }
        // Prose (a *wrapped* long unbreakable run) is hard-broken by the IR
        // wrapper instead, so it keeps its links — pin that difference.
        let mut prose = ChatView::new();
        prose.push(ChatCell::AssistantMessage(format!(
            "{}[docs](https://example.com)",
            "x".repeat(60)
        )));
        render(&mut prose, 40, 12);
        assert!(
            !prose.frame_links().is_empty(),
            "the IR wrapper hard-breaks long prose, so its rows stay exact"
        );
    }

    #[test]
    fn links_for_frame_allocates_nothing_without_links() {
        let cell = ComposedLines::plain(vec![Line::from("plain")]);
        let links: Vec<Vec<LinkSpan>> = Vec::new();
        let cell_lines = crate::ui::cached_cell::CellLines {
            lines: cell.lines(),
            links: &links,
            rows_exact: true,
        };
        assert!(!cell_lines.has_links());
        let table = links_for_frame(&cell_lines);
        assert!(table.is_empty() && table.capacity() == 0, "no allocation");

        // A row whose lines are not exact is skipped even when it has links.
        let with_link = ComposedLines::new(
            vec![Line::from("docs")],
            vec![vec![LinkSpan {
                start: 0,
                end: 4,
                target: "https://example.com".into(),
            }]],
        );
        let cell_lines = crate::ui::cached_cell::CellLines {
            lines: with_link.lines(),
            links: with_link.links(),
            rows_exact: false,
        };
        assert!(cell_lines.has_links());
        assert!(links_for_frame(&cell_lines).is_empty(), "inexact rows");
    }

    #[test]
    fn paint_selection_keeps_the_link_sequences() {
        // Spec: selecting a link must leave both the highlight and the
        // hyperlink in place (the patch merges styles, it never rewrites the
        // symbol).
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "see [docs](https://example.com) now".into(),
        ));
        let mut buf = render(&mut view, 60, 6);
        let (row, link) = only_link(&view);
        let selection = Selection::default();
        let mut selection = {
            let mut s = selection;
            s.begin(SelectionPoint::chat(row as usize, 0));
            s.drag_to(SelectionPoint::chat(row as usize, link.end));
            s
        };
        view.paint_selection(&mut buf, &selection);
        selection.cancel();

        let reversed: Vec<u16> = (buf.area.x..buf.area.right())
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::REVERSED))
            .collect();
        assert!(!reversed.is_empty());
        let mut still_linked = 0;
        for x in link.start..link.end {
            assert!(
                buf[(x, row)].symbol().contains("\u{1b}]8;;"),
                "column {x} lost its hyperlink under the highlight"
            );
            if buf[(x, row)].modifier.contains(Modifier::REVERSED) {
                still_linked += 1;
            }
        }
        assert!(still_linked > 0, "highlighted link cells keep the sequence");
        // Text extraction is unaffected by either.
        assert!(!row_text(&buf, row).contains('\u{1b}'));
    }

    #[test]
    fn consecutive_frames_produce_identical_link_cells() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "see [docs](https://example.com) now".into(),
        ));
        let first = render(&mut view, 60, 6);
        let second = render(&mut view, 60, 6);
        assert_eq!(first, second, "a stable frame must not jitter the diff");
    }

    #[test]
    fn scrolled_frame_moves_the_link_hit_box() {
        // Four messages above the linked one and two below: the link sits in
        // the middle of the band, so it stays visible across a 2-row scroll.
        let mut view = ChatView::new();
        for i in 0..4 {
            view.push(ChatCell::UserMessage(format!("above {i}")));
        }
        view.push(ChatCell::AssistantMessage(
            "[docs](https://example.com)".into(),
        ));
        for i in 0..2 {
            view.push(ChatCell::UserMessage(format!("below {i}")));
        }
        render(&mut view, 40, 12);
        let (row, link) = only_link(&view);
        assert_eq!(view.link_at(link.start, row), Some("https://example.com"));

        // Scroll up two rows: the content (and the hit box) move down.
        view.scroll_up(2);
        render(&mut view, 40, 12);
        let (new_row, new_link) = only_link(&view);
        assert_eq!(new_row, row + 2, "the link row follows the scroll");
        assert_eq!(new_link.start, link.start, "the column is content-anchored");
        assert_eq!(
            view.link_at(link.start, new_row),
            Some("https://example.com")
        );
        assert_eq!(
            view.link_at(link.start, row),
            None,
            "the stale screen position is no longer a link"
        );
    }

    #[test]
    fn collapsed_band_clears_links() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(
            "[docs](https://example.com)".into(),
        ));
        render(&mut view, 40, 6);
        assert!(!view.frame_links().is_empty());
        render(&mut view, 40, 0);
        assert!(view.frame_links().is_empty());
        assert_eq!(view.link_at(2, 2), None);
    }

    #[test]
    fn inject_osc8_sanitises_the_target() {
        let target = "https://e\u{1b}]8;;evil\u{7}.com";
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf.set_stringn(0, 0, "docs", 8, Style::default());
        inject_osc8(&mut buf, 0, 0, 4, target);

        let expected = osc8_open(&sanitize_osc8_target(target));
        for x in 0..4u16 {
            let symbol = buf[(x, 0)].symbol();
            assert!(symbol.starts_with(&expected), "{symbol:?}");
            assert!(symbol.ends_with(&osc8_close()), "{symbol:?}");
            assert_eq!(strip_osc8(symbol), columns_text(&buf, 0, x, x + 1));
        }
        // Only our own sequences survive: one opener, one closer, no BEL.
        let symbol = buf[(0, 0)].symbol();
        assert_eq!(symbol.matches("\u{1b}]8;;").count(), 2, "{symbol:?}");
        assert!(!symbol.contains('\u{7}'), "{symbol:?}");
    }

    #[test]
    fn inject_osc8_is_bounded_and_skips_filler_cells() {
        // A wide grapheme at column 0 followed by a normal one: injecting the
        // wide cell must not touch its filler column.
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        buf.set_stringn(0, 0, "你a", 6, Style::default());
        inject_osc8(&mut buf, 0, 0, 3, "https://example.com");
        assert!(buf[(0, 0)].symbol().contains("\u{1b}]8;;"));
        assert_eq!(buf[(1, 0)].symbol(), " ", "filler cell stays untouched");
        assert!(buf[(2, 0)].symbol().contains("\u{1b}]8;;"));
        // Out-of-range columns are clamped, never panicking.
        inject_osc8(&mut buf, 0, 0, u16::MAX, "https://example.com");
        inject_osc8(&mut buf, 5, 0, 6, "https://example.com");
        inject_osc8(&mut buf, 9, 0, 6, "https://example.com");
    }

    #[test]
    fn streaming_cell_links_survive_finalize_and_match_the_replay_path() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage(String::new()));
        view.append_to_last_assistant("see [docs](https://example.com)");
        render(&mut view, 60, 6);
        let (row, link) = only_link(&view);
        assert_eq!(view.link_at(link.start, row), Some("https://example.com"));

        view.finalize_streams();
        render(&mut view, 60, 6);
        let (final_row, final_link) = only_link(&view);
        assert_eq!(
            view.link_at(final_link.start, final_row),
            Some("https://example.com")
        );

        // The replayed (`to_lines`) path agrees with the streamed one.
        let mut replayed = ChatView::new();
        replayed.push(ChatCell::AssistantMessage(
            "see [docs](https://example.com)".into(),
        ));
        render(&mut replayed, 60, 6);
        assert_eq!(replayed.frame_links(), view.frame_links());
    }

    #[test]
    fn mask_links_drops_covered_rows_and_clips_columns() {
        let mut view = ChatView::new();
        view.frame_links = LinkTable(vec![
            (3, vec![link(0, 4), link(8, 12)]),
            (4, vec![link(0, 20)]),
            (9, vec![link(2, 6)]),
        ]);

        view.mask_links(Rect::new(6, 4, 10, 1)); // row 4, columns 6..16
        assert_eq!(
            view.frame_links().len(),
            2,
            "the fully covered row is dropped"
        );
        assert_eq!(view.link_at(2, 4), None, "the covered row has no link left");
        assert_eq!(view.link_at(2, 9), Some("https://example.com"), "untouched");
        assert_eq!(view.link_at(0, 3), Some("https://example.com"));
        assert_eq!(view.link_at(10, 3), Some("https://example.com"));

        view.mask_links(Rect::new(8, 3, 5, 2)); // rows 3..5, columns 8..13
        assert_eq!(view.link_at(0, 3), Some("https://example.com"), "outside");
        assert_eq!(view.link_at(8, 3), None, "inside the mask");
        assert_eq!(view.link_at(2, 9), Some("https://example.com"), "other row");
    }

    #[test]
    fn masking_an_empty_frame_is_a_no_op() {
        let mut view = ChatView::new();
        view.mask_links(Rect::new(0, 0, 10, 10));
        assert!(view.frame_links().is_empty());
    }

    // ── LinkTable ───────────────────────────────────────────────

    fn frame_link(start: u16, end: u16, target: &str) -> FrameLink {
        FrameLink {
            start,
            end,
            target: target.into(),
        }
    }

    #[test]
    fn link_table_hit_tests_absolute_columns_per_row() {
        let mut table = LinkTable::new();
        table.push_row(3, vec![frame_link(6, 10, "https://a.example")]);
        table.push_row(9, vec![frame_link(2, 6, "https://b.example")]);

        assert_eq!(table.link_at(6, 3), Some("https://a.example"));
        assert_eq!(table.link_at(9, 3), Some("https://a.example"));
        assert_eq!(table.link_at(5, 3), None, "before the span");
        assert_eq!(table.link_at(10, 3), None, "one past the span");
        assert_eq!(table.link_at(6, 4), None, "row without links");
        assert_eq!(table.link_at(2, 9), Some("https://b.example"));
    }

    #[test]
    fn link_table_row_takes_the_first_matching_span() {
        let mut table = LinkTable::new();
        table.push_row(
            1,
            vec![frame_link(0, 4, "first"), frame_link(4, 8, "second")],
        );
        assert_eq!(table.link_at(3, 1), Some("first"));
        assert_eq!(table.link_at(4, 1), Some("second"));
    }

    #[test]
    fn empty_link_table_never_hits() {
        let table = LinkTable::new();
        assert!(table.as_slice().is_empty());
        assert_eq!(table.link_at(0, 0), None);
    }

    #[test]
    fn link_table_mask_clips_spans_and_drops_covered_rows() {
        let mut table = LinkTable::new();
        table.push_row(
            3,
            vec![frame_link(0, 4, "left"), frame_link(8, 12, "right")],
        );
        table.push_row(4, vec![frame_link(0, 20, "full")]);
        table.push_row(9, vec![frame_link(2, 6, "other")]);

        table.mask(Rect::new(6, 4, 10, 1)); // row 4, columns 6..16
        assert_eq!(table.as_slice().len(), 2, "the covered row is dropped");
        assert_eq!(table.link_at(2, 4), None);

        table.mask(Rect::new(8, 3, 5, 2)); // rows 3..5, columns 8..13
        assert_eq!(table.link_at(0, 3), Some("left"), "outside the mask");
        assert_eq!(table.link_at(8, 3), None, "inside the mask");
        assert_eq!(table.link_at(2, 9), Some("other"), "untouched row");
    }
}
