//! Chat view — scrollable area for conversation cells with scrollbar.
//!
//! Uses width-aware virtualization with CachedCell for height caching.
//! Each cell's height is computed via `Paragraph::line_count(width)` and
//! cached with a generation counter for invalidation.

use std::time::Instant;

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
use super::input_area::InputArea;
use super::input_area::InputAreaWidget;
use super::popup::selection::SelectionPopup;
use super::popup::selection::SelectionRow;
use super::popup::selection::SelectionState;
use super::popup::selection::popup_height;
use super::spinner::SpinnerState;
use super::spinner::WorkingIndicatorWidget;
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
    scroll_offset: usize,
    /// Whether auto-scroll is active (follow bottom).
    auto_scroll: bool,
    /// Count of pending (unresolved) Bash tool calls — used to skip
    /// timer scanning when zero.
    pending_bash_count: usize,
    /// Header lines (wing logo + MOTD) — always rendered at the top,
    /// scroll with the content. Preserved across `clear()`.
    header_lines: Vec<Line<'static>>,
    /// On-screen rect of the composer input text area recorded during the
    /// last render (None when the input is scrolled out of view).
    /// Used by the draw loop to position/hide the cursor.
    pub(crate) input_card_rect: Option<Rect>,
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            cell_heights: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            pending_bash_count: 0,
            header_lines: Vec::new(),
            input_card_rect: None,
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

        // Track pending Bash tool calls for timer refresh.
        if let ChatCell::ToolCall(ref block) = cell
            && block.tool_name == TOOL_BASH
            && block.status == ToolStatus::Pending
        {
            self.pending_bash_count += 1;
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

    /// Scroll down by N lines.
    pub fn scroll_down(&mut self, n: usize, _visible_height: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    /// Scroll by one page up.
    pub fn page_up(&mut self, page_height: usize) {
        self.scroll_up(page_height);
    }

    /// Scroll by one page down.
    pub fn page_down(&mut self, page_height: usize, visible_height: usize) {
        self.scroll_down(page_height, visible_height);
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
        // Check if this is a pending Bash tool before borrowing for mutate.
        let is_pending_bash = self
            .cells
            .get(index)
            .and_then(|c| {
                if let ChatCell::ToolCall(block) = c.cell() {
                    Some(block.tool_name == TOOL_BASH && block.status == ToolStatus::Pending)
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if is_pending_bash {
            self.pending_bash_count = self.pending_bash_count.saturating_sub(1);
        }

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
    ///
    /// Tracks `pending_bash_count` when a Bash tool transitions into
    /// Pending (e.g. from Streaming after `ToolCallStream` created the cell).
    pub fn set_tool_status_by_index(&mut self, index: usize, status: ToolStatus) {
        if let Some(cached) = self.cells.get_mut(index)
            && matches!(cached.cell(), ChatCell::ToolCall(_))
        {
            let is_bash_entering_pending = matches!(
                cached.cell(),
                ChatCell::ToolCall(b)
                    if b.tool_name == TOOL_BASH && b.status != ToolStatus::Pending
            ) && status == ToolStatus::Pending;

            cached.mutate(|cell| {
                if let ChatCell::ToolCall(block) = cell {
                    block.status = status;
                }
            });

            if is_bash_entering_pending {
                self.pending_bash_count += 1;
            }
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
        // Decrement pending_bash_count if replacing a pending Bash tool.
        if let Some(cached) = self.cells.get(index)
            && let ChatCell::ToolCall(block) = cached.cell()
            && block.tool_name == TOOL_BASH
            && block.status == ToolStatus::Pending
        {
            self.pending_bash_count = self.pending_bash_count.saturating_sub(1);
        }

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
    /// Guarded by `pending_bash_count` to skip the O(n) scan when no
    /// Bash tools are pending.
    pub fn tick_bash_timers(&mut self) {
        if self.pending_bash_count == 0 {
            return;
        }
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

/// Bundled inputs for rendering the scrollable composer tail:
/// working line + separator + input + pop-down command popup + telemetry bar.
///
/// The tail is rendered as the trailing segment of the scrollable content,
/// so it follows the conversation and scrolls out of view when the user
/// scrolls up.
pub struct ComposerTail<'a> {
    pub input: &'a mut InputArea,
    pub usage: &'a TurnUsage,
    pub workdir: Option<&'a str>,
    pub working: bool,
    pub spinner: &'a SpinnerState,
    pub started_at: Option<Instant>,
    pub role_label: Option<&'a str>,
    /// Active command popup render data (rows, state, filter) for pop-down.
    pub popup: Option<(&'a [SelectionRow], &'a SelectionState, &'a str)>,
}

/// Geometry (row heights) of the composer tail segments.
struct TailGeom {
    /// Working indicator line (0 or 1).
    working_h: usize,
    /// Info separator line (always 1) — carries usage/workdir/scroll.
    separator_h: usize,
    /// Input text rows.
    input_h: usize,
    /// Pop-down popup height (0 when inactive).
    popup_h: usize,
    /// Total tail height.
    total: usize,
}

/// Compute tail segment heights for the given terminal width.
fn compute_tail_geom(tail: &ComposerTail<'_>, width: u16) -> TailGeom {
    let working_h = usize::from(tail.working);
    let separator_h = 1;
    let input_h = tail.input.height(width) as usize;
    let popup_h = tail
        .popup
        .as_ref()
        .map(|(rows, state, _)| popup_height(rows, state.max_visible) as usize)
        .unwrap_or(0);
    let total = working_h + separator_h + input_h + popup_h;
    TailGeom {
        working_h,
        separator_h,
        input_h,
        popup_h,
        total,
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
/// usage and scroll position — the same visual language as the classic
/// scroll indicator, but living inside the scrollable tail.
///
/// Layout: `─[workdir · usage · pos/total] ───── [percent%] ─`
#[allow(clippy::too_many_arguments)]
fn render_info_separator(
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

/// Render the composer tail into a sub-buffer, then blit the visible slice
/// (accounting for scroll) into `buf`. Records the on-screen input text rect
/// into `out_input_rect` (None when the input is scrolled out of view).
#[allow(clippy::too_many_arguments)]
fn render_composer_tail(
    tail: ComposerTail<'_>,
    geom: &TailGeom,
    palette: &ThemePalette,
    content_area: Rect,
    render_y: u16,
    tail_start: usize,
    scroll: usize,
    view_end: usize,
    buf: &mut Buffer,
    out_input_rect: &mut Option<Rect>,
) {
    let tail_height = geom.total;
    let tail_end = tail_start + tail_height;

    // Tail fully outside the visible window.
    if tail_end <= scroll || tail_start >= view_end {
        *out_input_rect = None;
        return;
    }
    let skip = scroll.saturating_sub(tail_start);
    let vis = tail_end.min(view_end) - tail_start.max(scroll);

    let width = content_area.width;
    let mut tbuf = Buffer::empty(Rect::new(0, 0, width, tail_height as u16));

    let ComposerTail {
        input,
        usage,
        workdir,
        working,
        spinner,
        started_at,
        role_label,
        popup,
    } = tail;

    let mut ly: u16 = 0;

    // 1. Working indicator line.
    if working {
        if let Some(started) = started_at {
            WorkingIndicatorWidget::new(spinner, started, palette)
                .with_role_label(role_label)
                .render(Rect::new(0, ly, width, 1), &mut tbuf);
        }
        ly += geom.working_h as u16;
    }

    // 2. Info separator line (─ with workdir/usage/scroll embedded).
    let sep_y = ly;
    let total_lines = tail_start + geom.total;
    let visible_height = content_area.height as usize;
    render_info_separator(
        workdir,
        usage,
        total_lines,
        visible_height,
        scroll,
        palette,
        Rect::new(0, sep_y, width, 1),
        &mut tbuf,
    );
    ly += geom.separator_h as u16;

    // 3. Input text (no background — flows naturally with the content).
    let input_top = ly;
    let text_area = Rect::new(0, input_top, width, geom.input_h as u16);
    InputAreaWidget::new(input, palette).render(text_area, &mut tbuf);
    ly = input_top + geom.input_h as u16;

    // 4. Pop-down command popup (below the input).
    if let Some((rows, state, filter)) = popup
        && geom.popup_h > 0
    {
        SelectionPopup::new(rows, state, filter, palette)
            .render(Rect::new(0, ly, width, geom.popup_h as u16), &mut tbuf);
    }

    // Blit the visible slice [skip .. skip + vis) into the main buffer.
    // Row-level `clone_from_slice` copies each visible row in one shot,
    // reusing destination cell allocations and skipping per-cell `Index`
    // bounds checks (Cell is Clone, not Copy). Bounds mirror the naive
    // per-cell loop exactly: x spans [0, width) in tbuf and
    // [content_area.x, content_area.x + width) in buf.
    //
    // `w > 0` 守卫使本函数自包含安全：`Buffer::index_of` 在零宽 area 上会
    // panic，而旧的逐 cell 空循环天然无此问题——因此不依赖调用方
    // （ChatViewWidget::render）的 area.width 守卫。行内越界由几何保证
    // （skip + vis <= tail_height，tbuf/buf 的 area 宽度均为 width），
    // 由 debug_assert 钉住。
    let w = width as usize;
    if w > 0 {
        for dy in 0..vis {
            let dst_y = render_y + dy as u16;
            if dst_y >= content_area.bottom() {
                break;
            }
            let src_y = (skip + dy) as u16;
            let src = tbuf.index_of(0, src_y);
            let dst = buf.index_of(content_area.x, dst_y);
            debug_assert!(src + w <= tbuf.content.len());
            debug_assert!(dst + w <= buf.content.len());
            buf.content[dst..dst + w].clone_from_slice(&tbuf.content[src..src + w]);
        }
    }

    // Record the on-screen input text rect for cursor positioning.
    // Tail-local row `r` maps to screen y = render_y + (r - skip).
    let text_screen_y = render_y as i32 + text_area.y as i32 - skip as i32;
    if text_screen_y >= content_area.y as i32 && (text_screen_y as u16) < content_area.bottom() {
        *out_input_rect = Some(Rect::new(
            content_area.x + text_area.x,
            text_screen_y as u16,
            text_area.width,
            geom.input_h as u16,
        ));
    } else {
        *out_input_rect = None;
    }
}

/// Widget for rendering the chat view with scrollbar.
pub struct ChatViewWidget<'a> {
    view: &'a mut ChatView,
    ctx: CellContext<'a>,
    tail: Option<ComposerTail<'a>>,
}

impl<'a> ChatViewWidget<'a> {
    pub fn new(view: &'a mut ChatView, ctx: CellContext<'a>) -> Self {
        Self {
            view,
            ctx,
            tail: None,
        }
    }

    /// Attach the composer tail (input + popup + telemetry) to render
    /// as the trailing segment of the scrollable content.
    pub fn with_tail(mut self, tail: ComposerTail<'a>) -> Self {
        self.tail = Some(tail);
        self
    }
}

impl Widget for ChatViewWidget<'_> {
    fn render(mut self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        // Take the composer tail out of self so we can still borrow view/ctx.
        let tail = self.tail.take();

        // The whole area is the scroll viewport (the composer tail is part
        // of the scrollable content, not a reserved layout block).
        let content_area = area;
        let visible = area.height as usize;

        // Update heights for stale cells.
        self.view.update_heights(area.width, &self.ctx);
        let header_height = self.view.header_lines.len();
        let cell_total: usize = self.view.cell_heights.iter().sum();

        // Composer tail geometry (working + separator + input + popup + telemetry).
        let tail_geom = tail.as_ref().map(|t| compute_tail_geom(t, area.width));
        let tail_height = tail_geom.as_ref().map(|g| g.total).unwrap_or(0);
        let total = header_height + cell_total + tail_height;

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

        // Render the composer tail (working + separator + input + popup + telemetry)
        // as the trailing segment of the scrollable content.
        if let (Some(tail), Some(geom)) = (tail, tail_geom) {
            let palette = self.ctx.palette;
            let tail_start = header_height + cell_total;
            render_composer_tail(
                tail,
                &geom,
                palette,
                content_area,
                render_y,
                tail_start,
                scroll,
                view_end,
                buf,
                &mut self.view.input_card_rect,
            );
        } else {
            self.view.input_card_rect = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;
    use crate::config::{LayoutConfig, ThemePalette};
    use crate::render::renderable::CellContext;
    use crate::ui::popup::selection::plain_row;

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

        view.scroll_up(5);
        assert!(!view.auto_scroll);
        assert_eq!(view.scroll_offset, 0);

        view.scroll_down(10, 5);
        assert!(view.scroll_offset > 0);
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

    // ── Composer tail tests ────────────────────────────────────

    /// Render the view with a composer tail (input + telemetry, etc.).
    fn render_view_with_tail(
        view: &mut ChatView,
        input: &mut InputArea,
        usage: &TurnUsage,
        workdir: Option<&str>,
        popup: Option<(&[SelectionRow], &SelectionState, &str)>,
        width: u16,
        height: u16,
    ) -> Buffer {
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let spinner = SpinnerState::new();
        let tail = ComposerTail {
            input,
            usage,
            workdir,
            working: false,
            spinner: &spinner,
            started_at: None,
            role_label: None,
            popup,
        };
        ChatViewWidget::new(view, ctx)
            .with_tail(tail)
            .render(area, &mut buf);
        buf
    }

    #[test]
    fn test_tail_input_visible_when_pinned_bottom() {
        let mut view = ChatView::new();
        for i in 0..20 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        let mut input = InputArea::new("");
        input.set_text("tail_marker_xyz");
        let usage = TurnUsage::default();
        let buf = render_view_with_tail(&mut view, &mut input, &usage, None, None, 40, 10);
        let rendered = buffer_text(&buf);
        assert!(
            rendered.contains("tail_marker_xyz"),
            "input (tail) should be visible when pinned to bottom:\n{rendered}"
        );
        assert!(
            view.input_card_rect.is_some(),
            "input rect should be recorded when visible"
        );
    }

    #[test]
    fn test_tail_scrolls_out_when_at_top() {
        let mut view = ChatView::new();
        for i in 0..30 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        let mut input = InputArea::new("");
        input.set_text("tail_marker_xyz");
        let usage = TurnUsage::default();
        view.jump_top();
        let buf = render_view_with_tail(&mut view, &mut input, &usage, None, None, 40, 10);
        let rendered = buffer_text(&buf);
        assert!(
            !rendered.contains("tail_marker_xyz"),
            "input (tail) should scroll out of view at top:\n{rendered}"
        );
        assert!(
            view.input_card_rect.is_none(),
            "input rect should be None when scrolled out"
        );
    }

    #[test]
    fn test_popdown_popup_rendered_and_not_clipped() {
        let mut view = ChatView::new();
        for i in 0..5 {
            view.push(ChatCell::AssistantMessage(format!("msg {i}")));
        }
        let mut input = InputArea::new("");
        input.set_text("/he");
        let usage = TurnUsage::default();
        let rows = vec![
            plain_row("/help", "show help"),
            plain_row("/hello", "say hi"),
        ];
        let state = SelectionState::new(rows.len());
        let buf = render_view_with_tail(
            &mut view,
            &mut input,
            &usage,
            None,
            Some((&rows, &state, "he")),
            50,
            12,
        );
        let rendered = buffer_text(&buf);
        assert!(
            rendered.contains("/help"),
            "popup row should render:\n{rendered}"
        );
        assert!(
            rendered.contains("/hello"),
            "popup row should render:\n{rendered}"
        );
        assert!(
            rendered.contains("/he"),
            "input text should still render above popup:\n{rendered}"
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

    #[test]
    fn test_tail_composes_with_ask_cell() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("question?".into()));
        view.push(ChatCell::Ask(AskMessage::new(
            "tc".into(),
            "pick one".into(),
            vec!["a".into(), "b".into()],
        )));
        let mut input = InputArea::new("");
        input.set_text("reply");
        let usage = TurnUsage::default();
        let buf = render_view_with_tail(&mut view, &mut input, &usage, Some("/ws"), None, 50, 14);
        let rendered = buffer_text(&buf);
        assert!(
            rendered.contains("pick one"),
            "ask cell should render:\n{rendered}"
        );
        assert!(
            rendered.contains("reply"),
            "input tail should render:\n{rendered}"
        );
        assert!(
            rendered.contains("/ws"),
            "telemetry workdir should render:\n{rendered}"
        );
    }
}
