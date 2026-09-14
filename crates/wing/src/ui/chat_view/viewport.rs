//! The viewport: scroll position, the follow contract, the per-frame geometry
//! and the widget that draws the band.
//!
//! VIRTUALIZED drawing with a height cache: `update_heights` refreshes the
//! per-cell wrap-aware heights (through `CachedCell`), the widget walks the
//! cells that intersect the visible window and renders each one — pre-wrapped
//! streaming lines are blitted directly, everything else goes through
//! `Paragraph`.
//!
//! This layer knows *how tall* and *where*, never *what* a cell means: the
//! only cell-type branch is the user-message row layout — full-width
//! background plus the text inset — which is a layout fact rather than a
//! semantic decision, and no business field (tool call ids, session state, …)
//! is ever read here.
//!
//! `render_info_separator` lives here too: it paints the scroll position /
//! usage line that reports this viewport's state.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

use crate::config::ThemePalette;
use crate::render::renderable::CellContext;
use crate::ui::status_bar::TurnUsage;

use super::ChatCell;
use super::ChatView;
use super::link::LinkTable;
use super::link::links_for_frame;
use super::link::place_links;

/// Screen geometry of the last chat render.
///
/// Both fields describe *the frame that was actually drawn*: the chat band's
/// rect and the scroll offset that frame ended up using. Auto-scroll pins
/// `scroll_offset` to the bottom **during** the render, so the value read
/// before the frame is not the one the user saw — every content ↔ screen
/// mapping (selection, hit testing) must go through this struct instead of
/// re-deriving it from the "current" state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatGeometry {
    /// The chat viewport (chat band) as laid out in the last frame.
    pub area: Rect,
    /// Scroll offset the last frame was rendered with.
    pub scroll_offset: usize,
}

impl ChatView {
    /// Whether auto-scroll is active (view is pinned to the bottom).
    ///
    /// The follow contract: pinned → new content scrolls into view; the user
    /// scrolling up clears the pin (reading history is never yanked back by
    /// streaming deltas); scrolling back to the bottom edge re-arms it. All
    /// scrolling entries (wheel, PageUp/PageDown, Ctrl+arrows, jumps) share
    /// this state, so the contract lives in the scroll methods below.
    ///
    /// A drag selection freezes the contract outright ([`Self::unfollow`]) —
    /// see `follow_frozen`.
    pub fn is_at_bottom(&self) -> bool {
        self.auto_scroll
    }

    /// Scroll up by N lines — leaves the follow state (reading history).
    pub fn scroll_up(&mut self, n: usize) {
        self.follow_frozen = false;
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Scroll down by N lines within a `viewport_h`-tall viewport.
    ///
    /// The bottom edge is derived from `last_total` (content height
    /// recorded by the last render). Reaching it re-arms `auto_scroll`
    /// synchronously — without waiting for the next render pass — so every
    /// scrolling entry (wheel, PageDown, Ctrl+Down) re-arms the follow
    /// contract the moment the view hits the bottom, even while new content
    /// is streaming.
    pub fn scroll_down(&mut self, n: usize, viewport_h: usize) {
        self.follow_frozen = false;
        let max_scroll = self.last_total.saturating_sub(viewport_h);
        if self.scroll_offset >= max_scroll {
            self.auto_scroll = true;
            return;
        }
        self.scroll_offset = (self.scroll_offset + n).min(max_scroll);
        if self.scroll_offset >= max_scroll {
            self.auto_scroll = true;
        }
    }

    /// Scroll by one page up (leaves the follow state).
    pub fn page_up(&mut self, page_height: usize) {
        self.scroll_up(page_height);
    }

    /// Scroll by one page down (re-arms the follow state at the bottom edge).
    pub fn page_down(&mut self, page_height: usize, viewport_h: usize) {
        self.scroll_down(page_height, viewport_h);
    }

    /// Jump to top (leaves the follow state).
    pub fn jump_top(&mut self) {
        self.follow_frozen = false;
        self.auto_scroll = false;
        self.scroll_offset = 0;
    }

    /// Jump to bottom (re-arms the follow state).
    pub fn jump_bottom(&mut self) {
        self.follow_frozen = false;
        self.auto_scroll = true;
    }

    /// Freeze the follow contract without moving the viewport.
    ///
    /// Used while a text selection is in progress: the render pins the
    /// viewport to the bottom edge and re-arms `auto_scroll` whenever the
    /// offset sits there, so clearing the flag alone would be undone by the
    /// very next frame — exactly the "drag on the newest output" case. While
    /// frozen, only a scroll entry ([`Self::scroll_up`] / [`Self::scroll_down`]
    /// / [`Self::jump_top`] / [`Self::jump_bottom`]) may lift it: the release path calls
    /// `scroll_down(0, viewport_h)`, which judges the bottom edge without
    /// moving and re-arms follow when the view is still there.
    pub fn unfollow(&mut self) {
        self.auto_scroll = false;
        self.follow_frozen = true;
    }

    /// Whether the follow state is frozen by an in-flight drag.
    pub fn is_follow_frozen(&self) -> bool {
        self.follow_frozen
    }

    /// Geometry of the last render — see [`ChatGeometry`].
    pub fn geometry(&self) -> ChatGeometry {
        self.geometry
    }

    /// Set an absolute scroll offset in lines — the scrollbar's track click
    /// and thumb drag.
    ///
    /// The follow contract applies to this entry as well: any offset above the
    /// bottom edge leaves the follow state (the user chose where to read),
    /// and reaching the bottom edge re-arms it (dragging the bar all the way
    /// down behaves like rolling the wheel to the end). The bottom edge comes
    /// from `last_total`, so the judgement does not wait for the next render.
    /// Like every other scroll entry it lifts the drag freeze, see
    /// [`Self::unfollow`].
    pub fn scroll_to(&mut self, offset: usize, viewport_h: usize) {
        self.follow_frozen = false;
        let max_scroll = self.last_total.saturating_sub(viewport_h);
        self.scroll_offset = offset.min(max_scroll);
        self.auto_scroll = self.scroll_offset >= max_scroll;
    }

    /// Total content height from the last render (header + cells).
    /// `0` before the first render.
    pub fn content_height(&self) -> usize {
        self.last_total
    }

    /// Current scroll offset in lines.
    pub fn scroll_position(&self) -> usize {
        self.scroll_offset
    }

    fn update_heights(&mut self, width: u16, ctx: &CellContext<'_>) {
        self.cell_heights.resize(self.cells.len(), 0);
        for (i, cached) in self.cells.iter_mut().enumerate() {
            self.cell_heights[i] = cached.compute_height(width, ctx);
        }
        self.pending_heights.resize(self.pending.len(), 0);
        for (i, msg) in self.pending.iter_mut().enumerate() {
            self.pending_heights[i] = msg.cell.compute_height(width, ctx);
        }
    }
}

/// Collapse the user's home directory prefix to `~` for compact display.
fn collapse_home(path: &str) -> String {
    use std::sync::OnceLock;
    static HOME: OnceLock<Option<String>> = OnceLock::new();
    let home = HOME.get_or_init(|| std::env::var("HOME").ok().filter(|h| !h.is_empty()));
    if let Some(home) = home
        && let Some(rest) = path.strip_prefix(home.as_str())
    {
        return format!("~{rest}");
    }
    path.to_string()
}

/// Render the info separator line: a `─` rule carrying workdir, per-turn
/// usage and scroll position — the classic scroll indicator, now living in
/// a fixed layout block right above the composer input.
///
/// Layout: `─[workdir · usage · pos/total] ───── [percent%] ─`
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_info_separator(
    workdir: Option<&str>,
    usage: &TurnUsage,
    total_lines: usize,
    visible_height: usize,
    scroll_offset: usize,
    palette: &ThemePalette,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let dim = Style::default().add_modifier(Modifier::DIM);

    // Fill the entire row with `─` as background.
    let sep = "─".repeat(area.width as usize);
    Span::styled(sep, dim).render(area, buf);

    // Right side: scroll percentage (only when content overflows).
    let mut pct_w: u16 = 0;
    if total_lines > 0 && visible_height < total_lines {
        let max_scroll = total_lines.saturating_sub(visible_height);
        let percent = if max_scroll == 0 {
            100
        } else {
            ((scroll_offset.min(max_scroll) as f32 / max_scroll as f32) * 100.0).round() as u8
        };
        let pct_text = format!(" {percent}% ");
        pct_w = pct_text.len() as u16;
        let pct_x = area.right().saturating_sub(pct_w + 1);
        Span::styled(pct_text, dim).render(Rect::new(pct_x, area.y, pct_w, 1), buf);
    }

    // Left side: workdir + usage + scroll position.
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(wd) = workdir {
        spans.push(Span::styled(
            collapse_home(wd),
            Style::default().fg(palette.accent),
        ));
    }
    let usage_spans = usage.to_spans();
    if !usage_spans.is_empty() {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", dim));
        }
        spans.extend(usage_spans);
    }
    if total_lines > 0 && visible_height < total_lines {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", dim));
        }
        let pos_text = format!("{}/{}", scroll_offset + visible_height, total_lines);
        spans.push(Span::styled(pos_text, dim));
    }
    if !spans.is_empty() {
        let left_w = area.width.saturating_sub(pct_w + 2);
        Line::from(spans).render(Rect::new(area.x + 1, area.y, left_w, 1), buf);
    }
}

/// Widget for rendering the chat viewport (content only — the overlay
/// scrollbar is painted separately by `App::draw`).
pub struct ChatViewWidget<'a> {
    view: &'a mut ChatView,
    ctx: CellContext<'a>,
}

impl<'a> ChatViewWidget<'a> {
    pub fn new(view: &'a mut ChatView, ctx: CellContext<'a>) -> Self {
        Self { view, ctx }
    }
}

impl Widget for ChatViewWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            // A collapsed band cannot be selected — clear the mapping so the
            // app's hit test rejects every press instead of using a stale rect.
            self.view.geometry = ChatGeometry::default();
            self.view.frame_links = LinkTable::new();
            return;
        }

        // The whole area is the chat viewport (header + cells only — the
        // composer lives in a fixed layout block owned by the App).
        let content_area = area;
        let visible = area.height as usize;

        // Update heights for stale cells.
        self.view.update_heights(area.width, &self.ctx);
        let header_height = self.view.header_lines.len();
        let cell_total: usize = self.view.cell_heights.iter().sum();
        let pending_total: usize = self.view.pending_heights.iter().sum();
        let total = header_height + cell_total + pending_total;

        // Record the content height so `scroll_down` can detect the bottom
        // edge and re-arm auto-scroll between renders.
        self.view.last_total = total;

        // Auto-scroll: pin to bottom.
        if self.view.auto_scroll {
            self.view.scroll_offset = total.saturating_sub(visible);
        }

        // Clamp scroll offset.
        let max_scroll = total.saturating_sub(visible);
        if self.view.scroll_offset > max_scroll {
            self.view.scroll_offset = max_scroll;
        }

        // Re-enable auto_scroll if scrolled to bottom — unless a drag has the
        // contract frozen (`unfollow`): "at the bottom" is exactly where such
        // a drag starts, and re-arming here would let the next streaming delta
        // yank the view (and the highlighted rows) away.
        if !self.view.follow_frozen && self.view.scroll_offset >= max_scroll && total > 0 {
            self.view.auto_scroll = true;
        }

        let scroll = self.view.scroll_offset;

        // Record the geometry this frame was rendered with. The selection
        // maps content rows ↔ screen rows through it, so it must be the value
        // the frame *ended up* using — auto-scroll pinning and the clamp
        // above may have moved it after the frame started.
        self.view.geometry = ChatGeometry {
            area,
            scroll_offset: scroll,
        };

        // Links of this frame: screen row -> intervals (absolute columns).
        // Collected while the cells render and installed at the end, so a
        // half-rendered frame can never be hit-tested.
        let mut frame_links = LinkTable::new();

        // Virtualized rendering: header + cells.
        let view_end = scroll + visible;

        // Track current y position in the content area.
        let mut render_y: u16 = content_area.y;

        // Render header lines (scroll-aware).
        if header_height > 0 {
            let cell_start = 0usize;
            let cell_end = header_height;

            if cell_end > scroll && cell_start < view_end {
                let skip = scroll.saturating_sub(cell_start);
                let cell_visible = cell_end.min(view_end) - cell_start.max(scroll);
                let header_area = Rect::new(
                    content_area.x,
                    render_y,
                    content_area.width,
                    cell_visible as u16,
                );
                Paragraph::new(self.view.header_lines.clone())
                    .scroll((skip as u16, 0))
                    .render(header_area, buf);
                render_y += cell_visible as u16;
            }
        }

        // Render cells + pending messages in one virtual coordinate space
        // (accumulated starts after header). Pending messages extend the
        // space below all committed cells, so auto-scroll keeps the user's
        // queued submissions in view while the turn streams on above them.
        //
        // KEEP IN SYNC with `frame::visible_row_insets`: the copy's per-row
        // inset table is derived by replaying this walk (header → cells →
        // pending, advanced by the same cached heights), so a layout change
        // here without the matching change there puts the padding skip on the
        // wrong columns — the copy-side test
        // `frame::tests::snapshot_row_insets_match_the_rendered_rows` is what
        // turns red in that case.
        let mut accumulated = header_height;
        let cell_count = self.view.cell_heights.len();
        let pending_count = self.view.pending_heights.len();

        for i in 0..cell_count + pending_count {
            let height = if i < cell_count {
                self.view.cell_heights[i]
            } else {
                self.view.pending_heights[i - cell_count]
            };
            let cell_start = accumulated;
            let cell_end = accumulated + height;
            accumulated = cell_end;

            // Skip cells entirely before the visible window.
            if cell_end <= scroll {
                continue;
            }
            // Stop once we've passed the visible window.
            if cell_start >= view_end {
                break;
            }

            // Calculate how much of this cell to skip (scroll into it).
            let skip = scroll.saturating_sub(cell_start);
            // Calculate visible height of this cell.
            let cell_visible = cell_end.min(view_end) - cell_start.max(scroll);

            if cell_visible == 0 {
                continue;
            }

            // This cell is visible — use cached lines and render directly.
            let cached = if i < cell_count {
                &mut self.view.cells[i]
            } else {
                &mut self.view.pending[i - cell_count].cell
            };
            let cell_area = Rect::new(
                content_area.x,
                render_y,
                content_area.width,
                cell_visible as u16,
            );

            // Pre-wrapped cells (streaming Thinking / AssistantMessage,
            // before and after their turn-end reconcile): every line is
            // already ≤ width — blit the visible slice directly, no
            // Paragraph wrap Composer, no to_vec clone.
            if cached.is_prewrapped(content_area.width, &self.ctx) {
                let cell = cached.compute_cell_lines(content_area.width, &self.ctx);
                let rows_exact = cell.rows_exact;
                let cell_links = links_for_frame(&cell);
                let lines = cell.lines;
                let skip_lines = skip.min(lines.len());
                let end = (skip_lines + cell_visible).min(lines.len());
                for (row_in_cell, line) in lines[skip_lines..end].iter().enumerate() {
                    let row = Rect::new(
                        content_area.x,
                        render_y + row_in_cell as u16,
                        content_area.width,
                        1,
                    );
                    line.render(row, buf);
                }
                if rows_exact {
                    place_links(
                        &mut frame_links,
                        buf,
                        &cell_links,
                        content_area,
                        skip_lines,
                        render_y,
                        cell_visible,
                    );
                }
                render_y += cell_visible as u16;
                if render_y >= content_area.bottom() {
                    break;
                }
                continue;
            }

            let cell = cached.compute_cell_lines(content_area.width, &self.ctx);
            let rows_exact = cell.rows_exact;
            let cell_links = links_for_frame(&cell);
            let cell_lines = cell.lines.to_vec();

            // User messages (normal / pending / discarded): fill full-width
            // background before text rendering.
            // Line.style(bg) only covers text width (ratatui Paragraph limitation),
            // so we pre-fill the cell area with the background color.
            // Text is rendered in an inset area for padding (2 left, 1 top, 1 bottom).
            //
            // KEEP IN SYNC with `frame::USER_MESSAGE_INSET`: the copy skips
            // exactly this many leading columns on these rows.
            if matches!(
                cached.cell(),
                ChatCell::UserMessage(_)
                    | ChatCell::PendingUserMessage(_)
                    | ChatCell::DiscardedUserMessage(_)
            ) {
                let bg = Style::default().bg(self.ctx.palette.surface);
                for y in cell_area.y..cell_area.y + cell_area.height {
                    for x in cell_area.x..cell_area.x + cell_area.width {
                        buf[(x, y)].set_style(bg);
                    }
                }
                // Inset text area: 2 left, 1 top, 1 bottom padding.
                // Padding rows only consume visible height while actually on
                // screen: once the cell top is scrolled past (skip > 0) the
                // top padding is gone, and the bottom padding only exists
                // when the cell end lies inside the window. Reserving both
                // unconditionally clipped the last text row whenever the
                // cell top was scrolled off (big-paste / replay bug).
                let top_pad = usize::from(skip == 0);
                let bottom_pad = usize::from(cell_end <= view_end);
                let text_area = Rect::new(
                    cell_area.x + 2,
                    cell_area.y + top_pad as u16,
                    cell_area.width.saturating_sub(3),
                    cell_visible.saturating_sub(top_pad + bottom_pad) as u16,
                );
                Paragraph::new(cell_lines)
                    .wrap(Wrap { trim: false })
                    .scroll((skip.saturating_sub(1) as u16, 0)) // row 0 is top padding
                    .render(text_area, buf);
                render_y += cell_visible as u16;
                if render_y >= content_area.bottom() {
                    break;
                }
                continue;
            }

            Paragraph::new(cell_lines)
                .wrap(Wrap { trim: false })
                .scroll((skip as u16, 0))
                .render(cell_area, buf);

            // The user-message branch above owns its own padding rows, so link
            // placement is confined to the plain cells (which is where markdown
            // links can appear anyway).
            if rows_exact {
                place_links(
                    &mut frame_links,
                    buf,
                    &cell_links,
                    content_area,
                    skip,
                    render_y,
                    cell_visible,
                );
            }

            render_y += cell_visible as u16;
            if render_y >= content_area.bottom() {
                break;
            }
        }

        // Install this frame's links (the hit test reads them between frames).
        self.view.frame_links = frame_links;
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        buffer_text, make_ctx, render_view, render_view_in, test_ctx,
    };
    use super::*;

    #[test]
    fn test_chat_view_scroll() {
        let mut view = ChatView::new();
        for i in 0..10 {
            view.push(ChatCell::UserMessage(format!("message {i}")));
        }
        assert!(view.auto_scroll);

        // Simulate the post-render state of a pinned view
        // (content height 100, viewport 20 → bottom edge at offset 80).
        view.last_total = 100;
        view.scroll_offset = 80;
        assert!(view.auto_scroll);

        // Up: leaves the bottom and disables auto-scroll.
        view.scroll_up(5);
        assert!(!view.auto_scroll);
        assert_eq!(view.scroll_offset, 75);

        // Down within the window: offset grows, still not at bottom.
        view.scroll_down(3, 20);
        assert_eq!(view.scroll_offset, 78);
        assert!(!view.auto_scroll);

        // Down reaching the bottom edge: clamped + auto-scroll re-armed.
        view.scroll_down(10, 20);
        assert_eq!(view.scroll_offset, 80);
        assert!(
            view.auto_scroll,
            "reaching the bottom must re-arm auto-scroll"
        );

        // Further down at the bottom is idempotent.
        view.scroll_down(5, 20);
        assert_eq!(view.scroll_offset, 80);
        assert!(view.auto_scroll);

        // Up beyond the top clamps at zero and stays unpinned.
        view.scroll_up(1000);
        assert_eq!(view.scroll_offset, 0);
        assert!(!view.auto_scroll);
    }

    #[test]
    fn test_scroll_down_bottom_before_render_is_idempotent() {
        // Before the first render `last_total` is 0: the view cannot scroll
        // past a bottom edge it does not know yet — scroll_down re-arms
        // auto-scroll and stays put instead of drifting the offset.
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        view.scroll_down(3, 20);
        assert_eq!(view.scroll_offset, 0);
        assert!(view.auto_scroll);
    }

    #[test]
    fn test_update_heights() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        view.push(ChatCell::AssistantMessage("world".into()));
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        view.update_heights(80, &ctx);
        assert_eq!(view.cell_heights.len(), 2);
        assert!(view.cell_heights[0] > 0);
        assert!(view.cell_heights[1] > 0);
    }

    #[test]
    fn test_user_message_last_line_rendered_when_top_scrolled_off() {
        // Regression: a tall UserMessage whose top is scrolled past the
        // viewport must still render its last text line (the "big paste
        // drops its last line" bug — padding rows were double-counted).
        let mut view = ChatView::new();
        let text = (0..30)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.push(ChatCell::UserMessage(text));
        view.push(ChatCell::AssistantMessage("reply".into()));

        // Window of 12 rows: auto-scroll pins to bottom, so the top of the
        // 32-row user message (30 text + 2 padding) is scrolled off.
        let buf = render_view(&mut view, 40, 12);
        let rendered = buffer_text(&buf);
        assert!(
            rendered.contains("line-29"),
            "last text line must survive top-scroll clipping:\n{rendered}"
        );
        assert!(
            rendered.contains("line-22"),
            "first visible text line must be rendered:\n{rendered}"
        );
    }

    #[test]
    fn test_user_message_last_line_rendered_when_cell_taller_than_window() {
        // A single UserMessage taller than the whole window: pinned to
        // bottom, its last line must be the row above the bottom padding.
        let mut view = ChatView::new();
        let text = (0..50)
            .map(|i| format!("row-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.push(ChatCell::UserMessage(text));

        let buf = render_view(&mut view, 40, 10);
        let rendered = buffer_text(&buf);
        assert!(
            rendered.contains("row-49"),
            "last line must render when the cell alone overflows the window:\n{rendered}"
        );
    }

    #[test]
    fn test_user_message_fully_visible_renders_all_lines() {
        // Fully visible cell (no scroll into it): all lines render and
        // keep the 1-row top/bottom padding.
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("first\nsecond\nthird".into()));

        let buf = render_view(&mut view, 40, 10);
        let rendered = buffer_text(&buf);
        for line in ["first", "second", "third"] {
            assert!(rendered.contains(line), "missing {line}:\n{rendered}");
        }
        // Top padding: row 0 of the cell must be blank (background only).
        let first_row = rendered.lines().next().unwrap_or("");
        assert!(
            first_row.trim().is_empty(),
            "top padding row should be blank, got: {first_row:?}"
        );
    }

    #[test]
    fn test_info_separator_contains_workdir_and_usage() {
        let (p, _l) = test_ctx();
        let usage = TurnUsage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            cached_tokens: 800,
            tokens_per_sec: 42.5,
            ttft_ms: 320.0,
        };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        render_info_separator(Some("/tmp/ws"), &usage, 100, 20, 80, &p, area, &mut buf);
        let rendered = buffer_text(&buf);
        assert!(rendered.contains("/tmp/ws"), "workdir:\n{rendered}");
        assert!(rendered.contains("1.2k in"), "input tokens:\n{rendered}");
        assert!(rendered.contains("340 out"), "output tokens:\n{rendered}");
        assert!(rendered.contains("66.7% cache"), "cache hit:\n{rendered}");
        assert!(rendered.contains("320ms ttft"), "ttft:\n{rendered}");
        assert!(rendered.contains("100/100"), "scroll pos:\n{rendered}");
        assert!(rendered.contains("100%"), "scroll percent:\n{rendered}");
    }

    /// Follow contract: pinned → new content follows; scrolled up → the
    /// reader is never yanked back; back at the bottom → follow re-armed.
    /// Every scrolling entry (wheel, keyboard, jumps) shares this state.
    #[test]
    fn test_follow_contract_across_new_content() {
        let mut view = ChatView::new();
        for i in 0..20 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        render_view(&mut view, 40, 10);
        let bottom = view.scroll_offset;
        assert!(view.is_at_bottom(), "a fresh view follows the bottom");

        // Scrolling up leaves the bottom edge → reading history.
        view.scroll_up(3);
        render_view(&mut view, 40, 10);
        let reading = view.scroll_offset;
        assert_eq!(reading, bottom - 3);
        assert!(!view.is_at_bottom());

        // New content streams in: the viewport must not move.
        for i in 20..30 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        render_view(&mut view, 40, 10);
        assert_eq!(
            view.scroll_offset, reading,
            "streaming content must not yank the reader back to the bottom"
        );
        assert!(!view.is_at_bottom());

        // Scrolling back to the bottom re-arms follow.
        view.scroll_down(1000, 10);
        assert!(view.is_at_bottom(), "reaching the bottom re-arms follow");
        for i in 30..40 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        let buf = render_view(&mut view, 40, 10);
        assert!(view.is_at_bottom());
        assert_eq!(
            view.scroll_offset,
            view.content_height() - 10,
            "the re-armed view is pinned to the new bottom edge"
        );
        let rendered = buffer_text(&buf);
        assert!(
            rendered.contains("msg 39"),
            "followed content must be visible:\n{rendered}"
        );
    }

    /// `scroll_to` (the scrollbar's track click / thumb drag) is an absolute
    /// offset entry, but it obeys the same follow contract as the relative
    /// ones: leaving the bottom starts reading, reaching it re-arms follow.
    #[test]
    fn test_scroll_to_follows_the_shared_contract() {
        let mut view = ChatView::new();
        for i in 0..20 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        render_view(&mut view, 40, 10);
        let bottom = view.content_height() - 10;
        assert!(view.is_at_bottom());

        // Anywhere above the bottom → reading.
        view.scroll_to(5, 10);
        assert_eq!(view.scroll_position(), 5);
        assert!(!view.is_at_bottom(), "a jump off the bottom starts reading");
        for i in 20..30 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        render_view(&mut view, 40, 10);
        assert_eq!(view.scroll_position(), 5, "streaming must not pull it back");

        // The bottom edge re-arms follow, exactly like scrolling down to it.
        view.scroll_to(bottom + 100, 10);
        assert_eq!(view.scroll_position(), view.content_height() - 10);
        assert!(view.is_at_bottom(), "the bottom edge re-arms follow");
        for i in 30..40 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        render_view(&mut view, 40, 10);
        assert!(view.is_at_bottom());
        assert_eq!(view.scroll_position(), view.content_height() - 10);
    }

    /// Content that fits: `scroll_to` is a no-op at offset 0 and the view
    /// stays in the follow state (there is no "reading" position to hold).
    #[test]
    fn test_scroll_to_clamps_when_the_content_fits() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("short".into()));
        render_view(&mut view, 40, 10);
        view.scroll_to(7, 10);
        assert_eq!(view.scroll_position(), 0);
        assert!(view.is_at_bottom());
    }

    /// The render pass must record the content height so `scroll_down`
    /// can detect the bottom edge between renders.
    #[test]
    fn test_last_total_recorded_after_render() {
        let mut view = ChatView::new();
        for i in 0..20 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        let buf = render_view(&mut view, 40, 10);
        assert!(
            view.last_total > 10,
            "content height must be recorded from the render: {}",
            view.last_total
        );
        // Pinned view shows the trailing content.
        let rendered = buffer_text(&buf);
        assert!(
            rendered.contains("msg 19"),
            "last message should be visible when pinned to bottom:\n{rendered}"
        );
    }

    #[test]
    fn test_unfollow_freezes_the_pin_and_scroll_down_rearms_it() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let band = Rect::new(0, 0, 30, 4);
        let _ = render_view_in(&mut view, Rect::new(0, 0, 30, 4), band);
        assert!(view.is_at_bottom());

        view.unfollow();
        assert!(!view.is_at_bottom(), "the drag froze the follow state");

        // Still at the bottom edge → `scroll_down(0, …)` re-arms without moving.
        let before = view.scroll_position();
        view.scroll_down(0, band.height as usize);
        assert_eq!(view.scroll_position(), before);
        assert!(view.is_at_bottom(), "release at the bottom re-arms follow");
    }
}
