//! Chat view — scrollable area for conversation cells with scrollbar.
//!
//! Uses width-aware virtualization with CachedCell for height caching.
//! Each cell's height is computed via `Paragraph::line_count(width)` and
//! cached with a generation counter for invalidation.

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
use crate::render::Renderable;
use crate::render::markdown::render_markdown_with_width;
use crate::render::markdown::render_plain;
use crate::render::renderable::CellContext;

use crate::app::constants::TOOL_BASH;

use super::cached_cell::CachedCell;
use super::cells::ask_msg::AskMessage;
use super::cells::diff_view::DiffView;
use super::cells::thinking::ThinkingBlock;
use super::cells::todo_msg::TodoMessage;
use super::cells::tool_call::ToolCallBlock;
use super::cells::tool_call::ToolStatus;
use super::status_bar::TurnUsage;

/// A single cell in the chat view.
#[derive(Debug, Clone)]
pub enum ChatCell {
    /// User message.
    UserMessage(String),
    /// Assistant text (may be accumulated from streaming TextEvents).
    AssistantMessage(String),
    /// System message.
    SystemMessage(String),
    /// Error message.
    ErrorMessage(String),
    /// Reasoning/thinking block (always expanded).
    Thinking(ThinkingBlock),
    /// Tool invocation + result.
    ToolCall(ToolCallBlock),
    /// File diff view.
    Diff(DiffView),
    /// Todo list.
    Todo(TodoMessage),
    /// Agent question.
    Ask(AskMessage),
    /// ReAct loop separator.
    Separator,
    /// Goal orchestration separator (marks agent role + round).
    GoalSeparator {
        role: crate::app::goal::GoalRole,
        round: u32,
    },
}

impl ChatCell {
    /// Render this cell to lines, width-aware for full-width elements.
    pub fn to_lines(&self, width: u16, ctx: &CellContext<'_>) -> Vec<Line<'static>> {
        let palette = ctx.palette;
        match self {
            Self::UserMessage(text) => {
                let bg_style = Style::default().bg(palette.surface).fg(palette.text);
                let mut lines = Vec::new();
                for line in render_plain(text) {
                    lines.push(Line::from(Span::styled(line.to_string(), bg_style)));
                }
                lines
            }
            Self::AssistantMessage(text) => {
                // Reserve 2 columns for the `⦁ ` / `  ` line prefix so tables
                // balance to fit and downstream wrapping never breaks a row.
                let md_width = Some(width.saturating_sub(2));
                let md_lines = render_markdown_with_width(text, md_width, palette);
                let bullet_style = Style::default().fg(palette.text);
                let mut lines = Vec::new();
                for (i, line) in md_lines.iter().enumerate() {
                    if i == 0 {
                        let mut spans = vec![Span::styled("⦁ ", bullet_style)];
                        spans.extend(line.spans.clone());
                        lines.push(Line::from(spans));
                    } else {
                        let mut spans = vec![Span::raw("  ")];
                        spans.extend(line.spans.clone());
                        lines.push(Line::from(spans));
                    }
                }
                lines.push(Line::from(""));
                lines
            }
            Self::SystemMessage(text) => {
                let label = Style::default().fg(palette.accent);
                let body = Style::default().fg(palette.text).italic();
                let mut lines = vec![Line::from(Span::styled("⦁ system", label))];
                for line in render_plain(text) {
                    lines.push(Span::styled(line.to_string(), body).into());
                }
                lines.push(Line::from(""));
                lines
            }
            Self::ErrorMessage(text) => {
                let danger = Style::default().fg(palette.danger);
                let mut lines = vec![Line::from(Span::styled("⦁ error", danger.bold()))];
                for line in render_plain(text) {
                    lines.push(Span::styled(line.to_string(), danger).into());
                }
                lines.push(Line::from(""));
                lines
            }
            Self::Thinking(block) => block.to_lines(palette, ctx.thinking_mode, width),
            Self::ToolCall(block) => block.to_lines(palette, ctx.layout.tool_output_max),
            Self::Diff(view) => view.to_lines(palette, ctx.layout.diff_context),
            Self::Todo(msg) => msg.to_lines(palette),
            Self::Ask(msg) => msg.to_lines(palette, width),
            Self::Separator => {
                let sep = "─".repeat(width as usize);
                vec![Line::from(Span::styled(
                    sep,
                    Style::default().fg(palette.dim),
                ))]
            }
            Self::GoalSeparator { role, round } => {
                use unicode_width::UnicodeWidthStr;
                let label = format!(" {} {} · Round {} ", role.icon(), role.label(), round);
                // Display width (not char count) — emoji like 🔍 are 2 columns.
                let label_w = UnicodeWidthStr::width(label.as_str());
                let dash_total = (width as usize).saturating_sub(label_w);
                let left = dash_total / 2;
                let right = dash_total - left;
                let text = format!("{}{}{}", "─".repeat(left), label, "─".repeat(right));
                vec![
                    Line::from(""),
                    Line::from(Span::styled(text, Style::default().fg(palette.dim))),
                    Line::from(""),
                ]
            }
        }
    }
}

impl Renderable for ChatCell {
    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &CellContext<'_>) {
        Paragraph::new(self.to_lines(area.width, ctx))
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    /// Wrap-aware line count — the single source of truth for cell height.
    fn desired_height(&self, width: u16, ctx: &CellContext<'_>) -> usize {
        // UserMessage: text rendered in inset area (2 left, 1 top, 1 bottom padding)
        if matches!(self, Self::UserMessage(_)) {
            let text_width = width.saturating_sub(3);
            return Paragraph::new(self.to_lines(width, ctx))
                .wrap(Wrap { trim: false })
                .line_count(text_width)
                + 2; // top + bottom padding
        }
        Paragraph::new(self.to_lines(width, ctx))
            .wrap(Wrap { trim: false })
            .line_count(width)
    }
}

/// Scrollable chat view with scrollbar indicator.
pub struct ChatView {
    pub(crate) cells: Vec<CachedCell>,
    /// Cached wrap-aware line count per cell (mirrors cells.len()).
    cell_heights: Vec<usize>,
    /// Scroll offset in lines (0 = top).
    pub(crate) scroll_offset: usize,
    /// Whether auto-scroll is active (follow bottom).
    auto_scroll: bool,
    /// Header lines (wing logo + MOTD) — always rendered at the top,
    /// scroll with the content. Preserved across `clear()`.
    header_lines: Vec<Line<'static>>,
    /// Total content height (header + cells) from the last render.
    /// Used by `scroll_down` to detect the bottom edge and re-arm
    /// auto-scroll immediately, without waiting for the next render pass.
    pub(crate) last_total: usize,
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            cell_heights: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            header_lines: Vec::new(),
            last_total: 0,
        }
    }

    /// Set the header lines (wing logo + MOTD).
    /// These are rendered at the top of the scrollable area and
    /// preserved across `clear()`.
    pub fn set_header(&mut self, lines: Vec<Line<'static>>) {
        self.header_lines = lines;
    }

    /// Append a cell. Automatically inserts a ReAct separator when
    /// transitioning from ToolCall to Thinking/AssistantMessage.
    pub fn push(&mut self, cell: ChatCell) {
        // ReAct separator detection: if the new cell is Thinking or AssistantMessage,
        // and the last non-Separator cell is a ToolCall, insert a separator.
        let needs_separator =
            matches!(&cell, ChatCell::Thinking(_) | ChatCell::AssistantMessage(_))
                && self.last_non_separator_is_tool_call();

        if needs_separator {
            self.cells.push(CachedCell::new(ChatCell::Separator));
        }

        self.cells.push(CachedCell::new(cell));
    }

    /// Find the cell index of the ToolCall block with the given id.
    ///
    /// Reverse scan (most recent first); tool_call_ids are unique per
    /// session. This is the ONLY supported way to address tool call cells:
    /// indices must be computed and consumed within one handler frame,
    /// never stored across events — anchored insertion below moves cells,
    /// so any cached index would silently go stale.
    pub fn tool_call_index(&self, tool_call_id: &str) -> Option<usize> {
        self.cells.iter().rposition(
            |c| matches!(c.cell(), ChatCell::ToolCall(block) if block.tool_call_id == tool_call_id),
        )
    }

    /// Anchor a derived cell (Todo/Diff) directly after the ToolCall cell
    /// that produced it.
    ///
    /// Concurrent tool execution makes derived events arrive out of order;
    /// appending them would misplace them below unrelated cells. If the
    /// ToolCall already has anchored derived cells (they sit contiguously
    /// right after it — anchoring never interleaves foreign cells into the
    /// group), the new cell is inserted after them so multiple emissions
    /// from one tool call keep their original order.
    ///
    /// When no ToolCall cell matches (unknown id, older gateway, lost
    /// event), the cell is handed back via `Err` (boxed to keep the
    /// `Result` small) so the caller can fall back to `push` without a
    /// pre-check or clone.
    ///
    /// `cell_heights` needs no maintenance — it is resized/recomputed
    /// lazily in `update_heights` on every draw (same as `remove_ask`).
    pub fn insert_after_tool_call(
        &mut self,
        tool_call_id: &str,
        cell: ChatCell,
    ) -> Result<(), Box<ChatCell>> {
        let Some(mut at) = self.tool_call_index(tool_call_id) else {
            return Err(Box::new(cell));
        };
        // Sibling group = contiguous derived cells anchored to this
        // ToolCall. NOTE: new anchored derived cell types must be
        // registered in this matches!, or they will be inserted before
        // earlier siblings and break emission order.
        while self
            .cells
            .get(at + 1)
            .is_some_and(|c| matches!(c.cell(), ChatCell::Diff(_) | ChatCell::Todo(_)))
        {
            at += 1;
        }
        self.cells.insert(at + 1, CachedCell::new(cell));
        Ok(())
    }

    /// Update the selection cursor on the Ask cell with the given tool_call_id.
    ///
    /// Addressed by id so concurrent Ask cells don't clobber each other.
    /// Returns true if the cell was found and updated.
    pub fn update_ask_selection(&mut self, tool_call_id: &str, selected: usize) -> bool {
        for cell in self.cells.iter_mut().rev() {
            if let ChatCell::Ask(msg) = cell.cell()
                && msg.tool_call_id == tool_call_id
            {
                cell.mutate(|c| {
                    if let ChatCell::Ask(msg) = c {
                        msg.selected = Some(selected);
                    }
                });
                return true;
            }
        }
        false
    }

    /// Remove the Ask cell with the given tool_call_id from the chat view.
    ///
    /// Called after the ask is answered/interrupted to clean up the prompt.
    pub fn remove_ask(&mut self, tool_call_id: &str) {
        let idx = self.cells.iter().position(
            |c| matches!(c.cell(), ChatCell::Ask(msg) if msg.tool_call_id == tool_call_id),
        );
        if let Some(i) = idx {
            self.cells.remove(i);
        }
    }

    /// Update the multi-question progress on the Ask cell with the given
    /// tool_call_id. Sets `current_idx` and `answers` to reflect progress.
    pub fn update_ask_progress(
        &mut self,
        tool_call_id: &str,
        current_idx: usize,
        answers: Vec<String>,
    ) {
        for cell in self.cells.iter_mut().rev() {
            if let ChatCell::Ask(msg) = cell.cell()
                && msg.tool_call_id == tool_call_id
            {
                cell.mutate(|c| {
                    if let ChatCell::Ask(msg) = c {
                        msg.current_idx = current_idx;
                        msg.answers = answers;
                    }
                });
                return;
            }
        }
    }

    /// Check if the last non-Separator cell is a ToolCall.
    fn last_non_separator_is_tool_call(&self) -> bool {
        for cell in self.cells.iter().rev() {
            match cell.cell() {
                ChatCell::Separator => continue,
                ChatCell::ToolCall(_) => return true,
                _ => return false,
            }
        }
        false
    }

    /// Get the number of cells.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Whether auto-scroll is active (view is pinned to the bottom).
    ///
    /// Approximated by the auto-scroll flag rather than a geometric
    /// comparison of `scroll_offset` vs total height.
    pub fn is_at_bottom(&self) -> bool {
        self.auto_scroll
    }

    /// Scroll up by N lines.
    pub fn scroll_up(&mut self, n: usize) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Scroll down by N lines within a `viewport_h`-tall viewport.
    ///
    /// The bottom edge is derived from `last_total` (content height
    /// recorded by the last render). Reaching it re-arms `auto_scroll`
    /// synchronously — without waiting for the next render pass — so the
    /// Up/Down routing in the app returns control to the composer as soon
    /// as the view hits the bottom, even while new content is streaming.
    pub fn scroll_down(&mut self, n: usize, viewport_h: usize) {
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

    /// Scroll by one page up.
    pub fn page_up(&mut self, page_height: usize) {
        self.scroll_up(page_height);
    }

    /// Scroll by one page down.
    pub fn page_down(&mut self, page_height: usize, viewport_h: usize) {
        self.scroll_down(page_height, viewport_h);
    }

    /// Jump to top.
    pub fn jump_top(&mut self) {
        self.auto_scroll = false;
        self.scroll_offset = 0;
    }

    /// Jump to bottom.
    pub fn jump_bottom(&mut self) {
        self.auto_scroll = true;
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

    /// Clear all cells.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.cell_heights.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
    }

    /// Return the raw text of the last assistant message, if any.
    pub fn last_assistant_text(&self) -> Option<&str> {
        for cached in self.cells.iter().rev() {
            if let ChatCell::AssistantMessage(text) = cached.cell() {
                return Some(text);
            }
        }
        None
    }

    /// Collect all assistant messages as `(1-based index, first-line preview)` pairs.
    /// Used as `/copy` sub-command candidates.
    pub fn collect_assistant_messages(&self) -> Vec<(String, String)> {
        self.cells
            .iter()
            .filter_map(|c| match c.cell() {
                ChatCell::AssistantMessage(text) => Some(text),
                _ => None,
            })
            .enumerate()
            .map(|(i, text)| {
                let preview = text
                    .lines()
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect();
                (format!("{}", i + 1), preview)
            })
            .collect()
    }

    /// Return the raw text of the N-th assistant message (1-based), if any.
    pub fn nth_assistant_text(&self, n: usize) -> Option<&str> {
        self.cells
            .iter()
            .filter_map(|c| match c.cell() {
                ChatCell::AssistantMessage(text) => Some(text.as_str()),
                _ => None,
            })
            .nth(n.checked_sub(1)?)
    }

    /// Append text to the last assistant message (for streaming).
    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(last) = self.cells.last_mut()
            && matches!(last.cell(), ChatCell::AssistantMessage(_))
        {
            last.mutate(|cell| {
                if let ChatCell::AssistantMessage(content) = cell {
                    content.push_str(text);
                }
            });
            return;
        }
        self.push(ChatCell::AssistantMessage(text.to_string()));
    }

    /// Append reasoning content to the last thinking block (for streaming).
    pub fn append_to_last_thinking(&mut self, text: &str) {
        if let Some(last) = self.cells.last_mut()
            && matches!(last.cell(), ChatCell::Thinking(_))
        {
            last.mutate(|cell| {
                if let ChatCell::Thinking(block) = cell {
                    block.append(text);
                }
            });
            return;
        }
        let mut block = ThinkingBlock::new();
        block.append(text);
        self.push(ChatCell::Thinking(block));
    }

    /// Increment the thinking event counter on the last thinking block.
    pub fn increment_thinking_count(&mut self) {
        if let Some(last) = self.cells.last_mut()
            && matches!(last.cell(), ChatCell::Thinking(_))
        {
            last.mutate(|cell| {
                if let ChatCell::Thinking(block) = cell {
                    block.event_count += 1;
                }
            });
        }
    }

    /// Reset thinking event counts on all thinking blocks (for new turn).
    pub fn reset_thinking_count(&mut self) {
        for cached in &mut self.cells {
            if matches!(cached.cell(), ChatCell::Thinking(_)) {
                cached.mutate(|cell| {
                    if let ChatCell::Thinking(block) = cell {
                        block.event_count = 0;
                    }
                });
            }
        }
    }

    /// Set the result on a tool call block by index (from RenderContext).
    pub fn set_tool_result_by_index(&mut self, index: usize, result: String, success: bool) {
        if let Some(cached) = self.cells.get_mut(index)
            && matches!(cached.cell(), ChatCell::ToolCall(_))
        {
            cached.mutate(|cell| {
                if let ChatCell::ToolCall(block) = cell {
                    block.set_result(result, success);
                }
            });
        } else {
            tracing::warn!(
                "set_tool_result_by_index: cell {index} is not a ToolCall (was {:?})",
                self.cells
                    .get(index)
                    .map(|c| std::mem::discriminant(c.cell()))
            );
        }
    }

    /// Append a raw args fragment to a streaming tool call block by index.
    pub fn append_tool_args_fragment_by_index(&mut self, index: usize, fragment: &str) {
        if let Some(cached) = self.cells.get_mut(index)
            && matches!(cached.cell(), ChatCell::ToolCall(_))
        {
            cached.mutate(|cell| {
                if let ChatCell::ToolCall(block) = cell {
                    block.append_args_fragment(fragment);
                }
            });
        }
    }

    /// Set authoritative tool args on a tool call block by index
    /// (execution start — releases any streaming buffer).
    pub fn update_tool_args_by_index(&mut self, index: usize, args: serde_json::Value) {
        if let Some(cached) = self.cells.get_mut(index)
            && matches!(cached.cell(), ChatCell::ToolCall(_))
        {
            cached.mutate(|cell| {
                if let ChatCell::ToolCall(block) = cell {
                    block.set_final_args(args);
                }
            });
        }
    }

    /// Set tool status on a tool call block by index.
    pub fn set_tool_status_by_index(&mut self, index: usize, status: ToolStatus) {
        if let Some(cached) = self.cells.get_mut(index)
            && matches!(cached.cell(), ChatCell::ToolCall(_))
        {
            cached.mutate(|cell| {
                if let ChatCell::ToolCall(block) = cell {
                    block.status = status;
                }
            });
        }
    }

    /// Set started_at on a tool call block by index (for Bash timer).
    pub fn set_tool_started_at_by_index(&mut self, index: usize) {
        if let Some(cached) = self.cells.get_mut(index)
            && matches!(cached.cell(), ChatCell::ToolCall(_))
        {
            cached.mutate(|cell| {
                if let ChatCell::ToolCall(block) = cell {
                    block.started_at = Some(std::time::Instant::now());
                }
            });
        }
    }

    /// Replace a cell at the given index with a new cell (e.g., ToolCallBlock → TodoMessage).
    ///
    /// Used during replay when a tool result requires a different cell type.
    pub fn replace_cell(&mut self, index: usize, cell: ChatCell) {
        if let Some(cached) = self.cells.get_mut(index) {
            cached.replace(cell);
        }
    }

    /// Invalidate cache on pending Bash tool calls so their elapsed timer
    /// redraws on the next `compute_lines()` call.
    ///
    /// The `mutate(|_| {})` call intentionally does nothing to the cell
    /// content — it only bumps the generation counter to invalidate the
    /// `CachedCell` render cache, forcing `to_lines()` to recompute with
    /// the current `Instant::now()`.
    ///
    /// Pending Bash tools are selected by their authoritative cell status —
    /// there is no separately maintained counter, so a new status-mutating
    /// path (e.g. `set_final_args`) cannot silently desync the timer. The
    /// caller gates this on `turn.working` (a tool can only be pending
    /// mid-turn), so idle sessions pay nothing for the scan.
    pub fn tick_bash_timers(&mut self) {
        for cached in &mut self.cells {
            if let ChatCell::ToolCall(block) = cached.cell()
                && block.tool_name == TOOL_BASH
                && block.status == ToolStatus::Pending
            {
                cached.mutate(|_| {});
            }
        }
    }

    /// Recompute heights for all cells whose cache is stale. Updates `cell_heights` in place.
    fn update_heights(&mut self, width: u16, ctx: &CellContext<'_>) {
        self.cell_heights.resize(self.cells.len(), 0);
        for (i, cached) in self.cells.iter_mut().enumerate() {
            self.cell_heights[i] = cached.compute_height(width, ctx);
        }
    }
}

impl Default for ChatView {
    fn default() -> Self {
        Self::new()
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

/// Widget for rendering the chat view with scrollbar.
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
        let total = header_height + cell_total;

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

        // Re-enable auto_scroll if scrolled to bottom.
        if self.view.scroll_offset >= max_scroll && total > 0 {
            self.view.auto_scroll = true;
        }

        let scroll = self.view.scroll_offset;

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

        // Render cells (accumulated starts after header).
        let mut accumulated = header_height;

        for (i, &height) in self.view.cell_heights.iter().enumerate() {
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
            let cell_lines = self.view.cells[i]
                .compute_lines(content_area.width, &self.ctx)
                .to_vec();
            let cell_area = Rect::new(
                content_area.x,
                render_y,
                content_area.width,
                cell_visible as u16,
            );

            // UserMessage: fill full-width background before text rendering.
            // Line.style(bg) only covers text width (ratatui Paragraph limitation),
            // so we pre-fill the cell area with the background color.
            // Text is rendered in an inset area for padding (2 left, 1 top, 1 bottom).
            if matches!(self.view.cells[i].cell(), ChatCell::UserMessage(_)) {
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

            render_y += cell_visible as u16;
            if render_y >= content_area.bottom() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;
    use crate::config::{LayoutConfig, ThemePalette};
    use crate::render::renderable::CellContext;
    use std::time::Instant;

    fn test_ctx() -> (ThemePalette, LayoutConfig) {
        (ThemePalette::default(), LayoutConfig::default())
    }

    fn make_ctx<'a>(p: &'a ThemePalette, l: &'a LayoutConfig) -> CellContext<'a> {
        CellContext {
            palette: p,
            thinking_mode: ThinkingMode::Visible,
            layout: l,
        }
    }

    #[test]
    fn test_chat_view_new() {
        let view = ChatView::new();
        assert!(view.is_empty());
        assert_eq!(view.len(), 0);
    }

    #[test]
    fn test_chat_view_push() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        assert_eq!(view.len(), 1);
        assert!(!view.is_empty());
    }

    #[test]
    fn test_chat_view_clear() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        view.push(ChatCell::AssistantMessage("hi".into()));
        assert_eq!(view.len(), 2);
        view.clear();
        assert!(view.is_empty());
    }

    #[test]
    fn test_last_assistant_text_empty() {
        let view = ChatView::new();
        assert!(view.last_assistant_text().is_none());
    }

    #[test]
    fn test_last_assistant_text_no_assistant() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        assert!(view.last_assistant_text().is_none());
    }

    #[test]
    fn test_last_assistant_text_single() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("reply".into()));
        assert_eq!(view.last_assistant_text(), Some("reply"));
    }

    #[test]
    fn test_last_assistant_text_returns_last() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("first".into()));
        view.push(ChatCell::UserMessage("question".into()));
        view.push(ChatCell::AssistantMessage("second".into()));
        assert_eq!(view.last_assistant_text(), Some("second"));
    }

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
    fn test_append_to_last_assistant_creates_cell() {
        let mut view = ChatView::new();
        view.append_to_last_assistant("hello");
        assert_eq!(view.len(), 1);
    }

    #[test]
    fn test_append_to_last_assistant_appends() {
        let mut view = ChatView::new();
        view.append_to_last_assistant("hello");
        view.append_to_last_assistant(" world");
        assert_eq!(view.len(), 1);
        if let ChatCell::AssistantMessage(text) = view.cells[0].cell() {
            assert_eq!(text, "hello world");
        }
    }

    #[test]
    fn test_thinking_cell() {
        let mut view = ChatView::new();
        view.append_to_last_thinking("Let me analyze...");
        assert_eq!(view.len(), 1);
        assert!(matches!(view.cells[0].cell(), ChatCell::Thinking(_)));
    }

    #[test]
    fn test_set_tool_result_by_index() {
        use super::super::cells::tool_call::ToolCallBlock;
        let mut view = ChatView::new();
        let block = ToolCallBlock::new("Read".into(), serde_json::json!({}), "tc1".into());
        view.push(ChatCell::ToolCall(block));

        let idx = view.len() - 1;
        view.set_tool_result_by_index(idx, "file contents".into(), true);
        if let ChatCell::ToolCall(block) = view.cells[idx].cell() {
            assert!(block.result.is_some());
        }
    }

    #[test]
    fn test_tool_call_index_finds_by_id() {
        use super::super::cells::tool_call::ToolCallBlock;
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("a".into()));
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Edit".into(),
            serde_json::json!({}),
            "tc1".into(),
        )));
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Bash".into(),
            serde_json::json!({}),
            "tc2".into(),
        )));

        assert_eq!(view.tool_call_index("tc1"), Some(1));
        assert_eq!(view.tool_call_index("tc2"), Some(2));
        assert_eq!(view.tool_call_index("nope"), None);
        assert_eq!(view.tool_call_index(""), None);
    }

    #[test]
    fn test_insert_after_tool_call_anchors_under_its_call() {
        use super::super::cells::tool_call::ToolCallBlock;
        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "TodoWrite".into(),
            serde_json::json!({}),
            "tc_todo".into(),
        )));
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Bash".into(),
            serde_json::json!({}),
            "tc_bash".into(),
        )));

        // Anchored insert lands between the two ToolCall cells even though
        // the event arrived "late" (concurrent out-of-order completion).
        assert!(
            view.insert_after_tool_call("tc_todo", ChatCell::SystemMessage("todo".into()))
                .is_ok()
        );

        assert_eq!(view.len(), 3);
        assert!(matches!(view.cells[1].cell(), ChatCell::SystemMessage(_)));
        match view.cells[2].cell() {
            ChatCell::ToolCall(b) => assert_eq!(b.tool_call_id, "tc_bash"),
            other => panic!("expected Bash ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn test_insert_after_tool_call_preserves_sibling_order() {
        use super::super::cells::diff_view::DiffView;
        use super::super::cells::tool_call::ToolCallBlock;
        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Edit".into(),
            serde_json::json!({}),
            "tc_edit".into(),
        )));

        // Two derived cells for the same tool call keep emission order
        // (the second anchoring skips past the first sibling).
        let diff = |p: &str| ChatCell::Diff(DiffView::new(p.into(), None, "new".into()));
        assert!(view.insert_after_tool_call("tc_edit", diff("d1")).is_ok());
        assert!(view.insert_after_tool_call("tc_edit", diff("d2")).is_ok());

        match (view.cells[1].cell(), view.cells[2].cell()) {
            (ChatCell::Diff(a), ChatCell::Diff(b)) => {
                assert_eq!(a.path, "d1");
                assert_eq!(b.path, "d2");
            }
            other => panic!("expected d1, d2 in order, got {other:?}"),
        }
    }

    #[test]
    fn test_insert_after_tool_call_unknown_id_hands_cell_back() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("a".into()));
        let err = view
            .insert_after_tool_call("missing", ChatCell::SystemMessage("x".into()))
            .unwrap_err();
        // Cell is handed back so the caller can fall back to push.
        assert!(matches!(*err, ChatCell::SystemMessage(_)));
        assert_eq!(view.len(), 1); // unchanged
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
    fn test_renderable_desired_height() {
        let cell = ChatCell::UserMessage("hello".into());
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let h = cell.desired_height(80, &ctx);
        assert!(h > 0);
        // " hello \n\n" = 2 lines at any reasonable width
        assert!(h >= 2);
    }

    #[test]
    fn test_user_message_no_you_prefix() {
        let cell = ChatCell::UserMessage("hello world".into());
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let lines = cell.to_lines(80, &ctx);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("⦁ You"),
            "should not contain '⦁ You': {text}"
        );
        assert!(
            text.contains("hello world"),
            "should contain message: {text}"
        );
    }

    #[test]
    fn test_append_to_last_assistant_creates_new_cell_when_last_not_assistant() {
        let mut view = ChatView::new();
        // Push a user message first.
        view.push(ChatCell::UserMessage("hi".into()));
        // Now append assistant text — last cell is UserMessage, not AssistantMessage.
        view.append_to_last_assistant("reply text");
        // Should create a NEW AssistantMessage cell, not drop the text.
        assert_eq!(view.len(), 2);
        assert!(matches!(
            view.cells[1].cell(),
            ChatCell::AssistantMessage(_)
        ));
        if let ChatCell::AssistantMessage(text) = view.cells[1].cell() {
            assert_eq!(text, "reply text");
        }
    }

    #[test]
    fn test_append_to_last_assistant_after_thinking() {
        let mut view = ChatView::new();
        view.append_to_last_thinking("Let me think...");
        assert!(matches!(view.cells[0].cell(), ChatCell::Thinking(_)));
        // Now append assistant text — last cell is Thinking.
        view.append_to_last_assistant("my answer");
        // Should create a NEW cell.
        assert_eq!(view.len(), 2);
        assert!(matches!(
            view.cells[1].cell(),
            ChatCell::AssistantMessage(_)
        ));
    }

    #[test]
    fn test_react_separator_toolcall_to_thinking() {
        use super::super::cells::tool_call::ToolCallBlock;

        let mut view = ChatView::new();
        // Simulate: tool call → thinking
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Read".into(),
            serde_json::json!({}),
            "call_1".into(),
        )));
        view.append_to_last_thinking("Let me reason...");

        // Should have: ToolCall, Separator, Thinking
        assert_eq!(view.len(), 3);
        assert!(matches!(view.cells[0].cell(), ChatCell::ToolCall(_)));
        assert!(matches!(view.cells[1].cell(), ChatCell::Separator));
        assert!(matches!(view.cells[2].cell(), ChatCell::Thinking(_)));
    }

    #[test]
    fn test_react_separator_toolcall_to_assistant() {
        use super::super::cells::tool_call::ToolCallBlock;

        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Bash".into(),
            serde_json::json!({}),
            "call_2".into(),
        )));
        view.push(ChatCell::AssistantMessage("Done!".into()));

        assert_eq!(view.len(), 3);
        assert!(matches!(view.cells[1].cell(), ChatCell::Separator));
    }

    #[test]
    fn test_no_separator_between_toolcalls() {
        use super::super::cells::tool_call::ToolCallBlock;

        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Read".into(),
            serde_json::json!({}),
            "call_1".into(),
        )));
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Grep".into(),
            serde_json::json!({}),
            "call_2".into(),
        )));

        // No separator between consecutive tool calls
        assert_eq!(view.len(), 2);
    }

    #[test]
    fn test_no_duplicate_separator() {
        use super::super::cells::tool_call::ToolCallBlock;

        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Read".into(),
            serde_json::json!({}),
            "call_1".into(),
        )));
        // First transition: inserts separator + thinking
        view.append_to_last_thinking("reasoning...");
        assert_eq!(view.len(), 3); // ToolCall, Separator, Thinking

        // Push another assistant message after thinking — no new separator
        // because last non-separator is Thinking, not ToolCall
        view.push(ChatCell::AssistantMessage("answer".into()));
        assert_eq!(view.len(), 4); // no extra separator
        assert!(!matches!(view.cells[3].cell(), ChatCell::Separator));
    }

    /// Flatten a Buffer into a string (one line per row) for assertions.
    fn buffer_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in buf.area.y..buf.area.bottom() {
            for x in buf.area.x..buf.area.right() {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Render the view into a buffer at the given size.
    fn render_view(view: &mut ChatView, width: u16, height: u16) -> Buffer {
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        ChatViewWidget::new(view, ctx).render(area, &mut buf);
        buf
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

    // ── Info separator tests ───────────────────────────────────

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

    /// Regression: the Bash timer must advance only while the cell is
    /// Pending. `tick_bash_timers` selects cells by their authoritative
    /// status, so a pending Bash cell's render cache is invalidated every
    /// tick (timer advances); once a result flips the status away from
    /// Pending, ticks stop touching it and the displayed elapsed freezes.
    #[test]
    fn test_bash_timer_freezes_after_result() {
        let mut view = ChatView::new();
        let mut block = ToolCallBlock::new(
            TOOL_BASH.into(),
            serde_json::json!({"command": "sleep 5"}),
            "tc-timer".into(),
        );
        block.started_at = Some(Instant::now());
        view.push(ChatCell::ToolCall(block));

        let idx = view.tool_call_index("tc-timer").unwrap();

        // Pending: every tick invalidates the render cache (timer advances).
        let before = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), before + 1);

        // Result lands (interrupted turns send a synthesized failure result):
        // status flips away from Pending, and ticks stop touching the cell —
        // the displayed elapsed time is frozen.
        // NOTE: 字符串内容与 Python 端 _INTERRUPTED_RESULT 语义对应，但此处
        // 仅作为"任意失败结果"触发状态翻转，内容本身不影响断言。
        view.set_tool_result_by_index(idx, "Tool call interrupted by user.".into(), false);
        let frozen = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), frozen);
    }

    /// Regression (#54ee2e9): a Bash cell that reaches Pending through the
    /// streaming-finalization path — `update_tool_args_by_index` →
    /// `set_final_args`, which flips the status itself — must still tick.
    ///
    /// The old materialized `pending_bash_count` missed this transition:
    /// `set_final_args` set the status to Pending opaquely, so the separate
    /// `set_tool_status_by_index` call saw an already-Pending cell and
    /// skipped its bookkeeping, leaving the counter at zero. `tick_bash_timers`
    /// then returned early and the timer froze at 0s until the result landed.
    /// Selecting by authoritative status makes the path irrelevant.
    #[test]
    fn test_bash_timer_ticks_after_streaming_finalize() {
        let mut view = ChatView::new();
        // Streaming cell, as created by a ToolCallStream fragment.
        let block = ToolCallBlock::new_streaming(TOOL_BASH.into(), "tc-stream".into());
        view.push(ChatCell::ToolCall(block));
        let idx = view.tool_call_index("tc-stream").unwrap();

        // Still Streaming — a tick must not touch it.
        let streaming_gen = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), streaming_gen);

        // ToolCall event finalizes args; set_final_args flips status to Pending.
        view.update_tool_args_by_index(idx, serde_json::json!({"command": "sleep 5"}));
        view.set_tool_started_at_by_index(idx);

        // Now Pending — a tick must invalidate the cache so the timer advances.
        let before = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), before + 1);
    }
}
