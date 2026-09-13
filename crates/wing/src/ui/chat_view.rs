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
use unicode_width::UnicodeWidthStr;

use ratatui::buffer::CellDiffOption;

use super::selection::ContentPoint;
use super::selection::Grapheme;
use super::selection::RenderedRow;
use super::selection::Selection;
use super::selection::extract_text;
use super::selection::grapheme_width;

use crate::config::ThemePalette;
use crate::render::Renderable;
use crate::render::markdown::ComposedLines;
use crate::render::markdown::LinkSpan;
use crate::render::markdown::compose_lines;
use crate::render::markdown::links::CELL_PREFIX_WIDTH;
use crate::render::markdown::osc8_close;
use crate::render::markdown::osc8_open;
use crate::render::markdown::render_markdown_lines;
use crate::render::markdown::render_plain;
use crate::render::markdown::sanitize_osc8_target;
use crate::render::markdown::strip_osc8;
use crate::render::markdown::symbol_width;
use crate::render::renderable::CellContext;

use crate::app::ask_panel::AskPanel;
use crate::app::constants::TOOL_BASH;
use crate::app::model_panel::ModelPanel;

use super::cached_cell::CachedCell;
use super::cells::ask_msg::AskMessage;
use super::cells::diff_view::DiffView;
use super::cells::model_picker::model_picker_lines;
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
    /// User message awaiting model acceptance — rendered in the pending
    /// area below all committed cells, dimmed to signal "queued, not yet
    /// fed to the model". Promoted to `UserMessage` on
    /// `user_message_accepted`, or demoted to `DiscardedUserMessage` when
    /// an interrupt discards it.
    PendingUserMessage(String),
    /// User message discarded by an interrupt before reaching the model —
    /// dimmed + struck through to signal "never sent".
    DiscardedUserMessage(String),
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
    /// `/model` picker (transient — removed when applied or closed).
    ModelPicker(ModelPanel),
    /// ReAct loop separator.
    Separator,
    /// Goal orchestration separator (marks agent role + round).
    GoalSeparator {
        role: crate::app::goal::GoalRole,
        round: u32,
    },
}

impl ChatCell {
    /// Render this cell to lines **plus their markdown link spans**.
    ///
    /// Cells without markdown links (everything but assistant messages and
    /// thinking blocks) return [`ComposedLines::plain`] — the spans exist so
    /// the widget can inject OSC8 hyperlinks and hit-test clicks without ever
    /// re-deriving what the renderer produced (see `tui-link-open`).
    pub fn render_lines(&self, width: u16, ctx: &CellContext<'_>) -> ComposedLines {
        match self {
            Self::AssistantMessage(text) => assistant_message_lines(text, width, ctx.palette),
            Self::Thinking(block) => block.render_lines(ctx.palette, ctx.thinking_mode, width),
            _ => ComposedLines::plain(self.to_lines(width, ctx)),
        }
    }

    /// Render this cell to lines, width-aware for full-width elements.
    pub fn to_lines(&self, width: u16, ctx: &CellContext<'_>) -> Vec<Line<'static>> {
        let palette = ctx.palette;
        match self {
            Self::UserMessage(text) => {
                let style = Style::default().bg(palette.surface).fg(palette.text);
                Self::user_message_lines(text, style)
            }
            Self::PendingUserMessage(text) => {
                // Queued, not yet accepted by the model — dimmed.
                let style = Style::default().bg(palette.surface).fg(palette.dim);
                Self::user_message_lines(text, style)
            }
            Self::DiscardedUserMessage(text) => {
                // Discarded by an interrupt before reaching the model —
                // dimmed + struck through.
                let style = Style::default()
                    .bg(palette.surface)
                    .fg(palette.dim)
                    .add_modifier(Modifier::CROSSED_OUT);
                Self::user_message_lines(text, style)
            }
            Self::AssistantMessage(text) => {
                assistant_message_lines(text, width, palette).into_lines()
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
            Self::Diff(view) => view.to_lines(palette, ctx.layout.diff_context, width),
            Self::Todo(msg) => msg.to_lines(palette),
            Self::Ask(msg) => msg.to_lines(palette, width),
            Self::ModelPicker(panel) => model_picker_lines(panel, palette),
            Self::Separator => {
                let sep = "─".repeat(width as usize);
                vec![Line::from(Span::styled(
                    sep,
                    Style::default().fg(palette.dim),
                ))]
            }
            Self::GoalSeparator { role, round } => {
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

    /// Shared rendering for the three user-message cell states (normal /
    /// pending / discarded) — plain text lines under the given style.
    fn user_message_lines(text: &str, style: Style) -> Vec<Line<'static>> {
        render_plain(text)
            .iter()
            .map(|line| Line::from(Span::styled(line.to_string(), style)))
            .collect()
    }
}

/// Assistant message lines: markdown prefixed with `⦁ ` / `  `, plus the link
/// spans of every rendered line (shifted by the 2-column prefix).
///
/// Reserves 2 columns for the prefix so tables balance to fit and downstream
/// wrapping never breaks a row.
fn assistant_message_lines(text: &str, width: u16, palette: &ThemePalette) -> ComposedLines {
    let md_width = Some(width.saturating_sub(2));
    let md_lines = render_markdown_lines(text, md_width, palette);
    let bullet_style = Style::default().fg(palette.text);
    let mut composed = compose_lines(
        &md_lines,
        CELL_PREFIX_WIDTH,
        |i| {
            if i == 0 {
                Span::styled("⦁ ", bullet_style)
            } else {
                Span::raw("  ")
            }
        },
        |_kind, style| style,
    );
    composed.push_blank();
    composed
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
        if matches!(
            self,
            Self::UserMessage(_) | Self::PendingUserMessage(_) | Self::DiscardedUserMessage(_)
        ) {
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

/// A user message that has been sent but not yet accepted by the model.
///
/// Rendered in the pending area below all committed cells ("stuck at the
/// bottom") until `user_message_accepted` promotes it into history.
pub struct PendingMessage {
    /// ClientRequest.request_id — correlates with UserMessageAccepted.
    pub request_id: String,
    pub(crate) cell: CachedCell, // PendingUserMessage(content)
}

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

/// Scrollable chat view with scrollbar indicator.
pub struct ChatView {
    pub(crate) cells: Vec<CachedCell>,
    /// Cached wrap-aware line count per cell (mirrors cells.len()).
    cell_heights: Vec<usize>,
    /// User messages awaiting model acceptance, in send order. Rendered
    /// below all cells; promoted/discarded by the app on acceptance
    /// events (see `push_pending` and friends).
    pub(crate) pending: Vec<PendingMessage>,
    /// Cached wrap-aware line count per pending message.
    pending_heights: Vec<usize>,
    /// Scroll offset in lines (0 = top).
    pub(crate) scroll_offset: usize,
    /// Whether auto-scroll is active (follow bottom).
    auto_scroll: bool,
    /// Follow is frozen for the duration of a drag selection.
    ///
    /// `auto_scroll = false` alone is not enough: the render re-arms
    /// `auto_scroll` whenever the offset sits at the bottom edge, which is
    /// exactly where a drag on the newest output starts — the very next frame
    /// would undo the freeze. Only the scroll entries (the sole owners of the
    /// follow contract) may lift it.
    follow_frozen: bool,
    /// Header lines (wing logo + MOTD) — always rendered at the top,
    /// scroll with the content. Preserved across `clear()`.
    header_lines: Vec<Line<'static>>,
    /// Total content height (header + cells) from the last render.
    /// Used by `scroll_down` to detect the bottom edge and re-arm
    /// auto-scroll immediately, without waiting for the next render pass.
    pub(crate) last_total: usize,
    /// Counts full content rebuilds ([`Self::clear`]: session switch,
    /// compaction re-sync, rewind replay). A selection is anchored to content
    /// rows, so a rebuild invalidates it even when the rebuilt list happens to
    /// end up with the same number of cells — see `App::selection_guard`.
    rebuilds: u64,
    /// Geometry of the last render (see [`ChatGeometry`]).
    geometry: ChatGeometry,
    /// Links of the last rendered frame, per screen row (absolute columns).
    ///
    /// Rebuilt from scratch on every render: a press lands between frames, so
    /// the hit test has to answer from the frame the user was looking at —
    /// same "WYSIWYG" contract as the selection's row snapshot. Rows without
    /// links have no entry.
    frame_links: Vec<(u16, Vec<FrameLink>)>,
    /// Graphemes of the visible chat rows as of the last *drag* frame.
    ///
    /// Copy-on-select is WYSIWYG: the release event lands between frames, so
    /// the text has to be taken from a frame the user was actually looking
    /// at. Refreshed only while a drag is in flight ([`Self::capture_visible_rows`]),
    /// so an idle app pays nothing.
    visible_rows: Vec<RenderedRow>,
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            cell_heights: Vec::new(),
            pending: Vec::new(),
            pending_heights: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            follow_frozen: false,
            header_lines: Vec::new(),
            last_total: 0,
            rebuilds: 0,
            geometry: ChatGeometry::default(),
            frame_links: Vec::new(),
            visible_rows: Vec::new(),
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

    // ── Pending user messages (sent, awaiting model acceptance) ──
    //
    // A user message only moves up into chat history when it has really
    // been fed to the model. Until then it sits in this queue, rendered
    // below all committed cells ("stuck at the latest"), and is promoted
    // by `user_message_accepted` or discarded by an interrupt.

    /// Queue a sent-but-not-yet-accepted user message.
    pub fn push_pending(&mut self, request_id: String, content: String) {
        self.pending.push(PendingMessage {
            request_id,
            cell: CachedCell::new(ChatCell::PendingUserMessage(content)),
        });
        // The user's own submission must be visible immediately — pending
        // messages render below in-flight streaming content.
        self.jump_bottom();
    }

    /// Promote the pending message matching `request_id` into history.
    ///
    /// Returns `false` when nothing matches (message originated from
    /// another client / goal orchestration — not tracked here).
    pub fn promote_pending(&mut self, request_id: &str) -> bool {
        let Some(pos) = self.pending.iter().position(|p| p.request_id == request_id) else {
            return false;
        };
        let msg = self.pending.remove(pos);
        let ChatCell::PendingUserMessage(content) = msg.cell.into_inner() else {
            return false;
        };
        self.push(ChatCell::UserMessage(content));
        true
    }

    /// Promote all pending messages into history (turn-end safety net —
    /// e.g. accepted events lost across a disconnect/reconnect).
    pub fn promote_all_pending(&mut self) {
        let contents: Vec<String> = self
            .pending
            .drain(..)
            .filter_map(|msg| match msg.cell.into_inner() {
                ChatCell::PendingUserMessage(content) => Some(content),
                _ => None,
            })
            .collect();
        for content in contents {
            self.push(ChatCell::UserMessage(content));
        }
    }

    /// Interrupted: pending messages were dropped from the backend inbox
    /// without reaching the model — commit them as discarded (dim +
    /// struck through) instead of silently vanishing.
    pub fn discard_all_pending(&mut self) {
        let contents: Vec<String> = self
            .pending
            .drain(..)
            .filter_map(|msg| match msg.cell.into_inner() {
                ChatCell::PendingUserMessage(content) => Some(content),
                _ => None,
            })
            .collect();
        for content in contents {
            self.push(ChatCell::DiscardedUserMessage(content));
        }
    }

    /// Remove a pending message without committing it (send failed /
    /// gateway unreachable — the message never left the client).
    pub fn remove_pending(&mut self, request_id: &str) {
        if let Some(pos) = self.pending.iter().position(|p| p.request_id == request_id) {
            self.pending.remove(pos);
        }
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

    /// Replace the interactive panel state on the Ask cell with the given
    /// tool_call_id (render snapshot of the app-owned `AskPanel`).
    ///
    /// Addressed by id so concurrent Ask cells don't clobber each other.
    pub fn update_ask_panel(&mut self, tool_call_id: &str, panel: AskPanel) {
        for cell in self.cells.iter_mut().rev() {
            if let ChatCell::Ask(msg) = cell.cell()
                && msg.tool_call_id == tool_call_id
            {
                cell.mutate(|c| {
                    if let ChatCell::Ask(msg) = c {
                        msg.panel = Some(panel);
                    }
                });
                return;
            }
        }
    }

    /// Show the `/model` picker at the tail of the transcript (replacing any
    /// previous picker cell) and bring it into view.
    pub fn show_model_picker(&mut self, panel: ModelPanel) {
        self.remove_model_picker();
        self.cells
            .push(CachedCell::new(ChatCell::ModelPicker(panel)));
        self.jump_bottom();
    }

    /// Replace the picker's render snapshot (no-op when the cell is absent).
    pub fn update_model_picker(&mut self, panel: ModelPanel) {
        for cell in self.cells.iter_mut().rev() {
            if matches!(cell.cell(), ChatCell::ModelPicker(_)) {
                cell.mutate(|c| *c = ChatCell::ModelPicker(panel));
                return;
            }
        }
    }

    /// Remove the picker cell — the panel is transient (applied / closed).
    pub fn remove_model_picker(&mut self) {
        let idx = self
            .cells
            .iter()
            .position(|c| matches!(c.cell(), ChatCell::ModelPicker(_)));
        if let Some(i) = idx {
            self.cells.remove(i);
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
    /// / [`Self::jump_*`]) may lift it: the release path calls
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

    /// Number of pending (sent, not yet accepted) messages.
    ///
    /// Together with [`Self::len`] and the terminal width this is the
    /// structural fingerprint a text selection is validated against: adding /
    /// removing / promoting cells shifts virtual rows, streaming text growth
    /// does not.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    // ── Text selection: content ↔ screen mapping ────────────────────────

    /// Geometry of the last render — see [`ChatGeometry`].
    pub fn geometry(&self) -> ChatGeometry {
        self.geometry
    }

    /// Link target at a screen position of the last rendered frame.
    ///
    /// `None` when the position is not inside a link (or the frame had
    /// none) — a click there is a no-op, never a guessed open.
    pub fn link_at(&self, column: u16, row: u16) -> Option<&str> {
        self.frame_links
            .iter()
            .find(|(r, _)| *r == row)
            .and_then(|(_, links)| {
                links
                    .iter()
                    .find(|link| column >= link.start && column < link.end)
            })
            .map(|link| link.target.as_str())
    }

    /// Links of the last rendered frame (test seam / diagnostics).
    pub fn frame_links(&self) -> &[(u16, Vec<FrameLink>)] {
        &self.frame_links
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
        self.frame_links.retain_mut(|(row, links)| {
            if *row < area.y || *row >= area.bottom() {
                return true;
            }
            links.retain(|link| link.end <= area.x || link.start >= area.right());
            !links.is_empty()
        });
    }

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
    pub fn content_point_at(&self, column: u16, row: u16) -> Option<ContentPoint> {
        let area = self.geometry.area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let row = row.clamp(area.y, area.bottom() - 1);
        let column = column.clamp(area.x, area.right() - 1);
        let vrow = self.geometry.scroll_offset + (row - area.y) as usize;
        Some(ContentPoint {
            vrow: vrow.min(self.last_total.saturating_sub(1)),
            col: column - area.x,
        })
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
    pub fn snap_focus_right(&self, point: ContentPoint) -> ContentPoint {
        let Some(row) = point
            .vrow
            .checked_sub(self.geometry.scroll_offset)
            .and_then(|index| self.visible_rows.get(index))
        else {
            return point;
        };
        for grapheme in &row.graphemes {
            let end = grapheme.col.saturating_add(grapheme.width);
            if point.col >= grapheme.col && point.col < end {
                return ContentPoint {
                    vrow: point.vrow,
                    col: end.max(row.inset),
                };
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
        // Nothing to paint for a click / released selection (`bounds` is only
        // `Some` for a non-empty drag).
        if selection.bounds().is_none() {
            return;
        }
        // Iterate the *band* rows (bounded by the terminal height), not the
        // selection rows: an edge drag can span thousands of content rows,
        // while only the visible ones can be painted anyway.
        for row in area.y..area.bottom() {
            let vrow = self.geometry.scroll_offset + (row - area.y) as usize;
            let Some((from, to)) = selection.row_span(vrow, area.width) else {
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
    pub fn capture_visible_rows(&mut self, buf: &Buffer) {
        let area = self.geometry.area;
        if area.width == 0 || area.height == 0 {
            self.visible_rows.clear();
            return;
        }
        let insets = self.visible_row_insets(area.height);
        self.visible_rows = (area.y..area.bottom())
            .enumerate()
            .map(|(index, row)| RenderedRow {
                graphemes: buffer_row_graphemes(buf, row, area),
                inset: insets.get(index).copied().unwrap_or(0),
            })
            .collect();
    }

    /// Left padding (in columns) of every visible band row, in row order.
    ///
    /// Only the user-message cells inset their text (`cell_area.x + 2`); every
    /// other cell starts at the band's left edge. The rows are walked exactly
    /// like the render walks them (header → cells → pending), so a row's inset
    /// belongs to whichever entry drew it. Rows of an entry that is scrolled
    /// out of the band contribute nothing, which the caller reads as "no
    /// padding".
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
    /// `None` when nothing could be extracted (empty / all-blank selection,
    /// or the rows are not part of the snapshot) — in that case there is
    /// nothing to copy and no feedback is shown.
    pub fn selected_text(&self, bounds: (ContentPoint, ContentPoint)) -> Option<String> {
        extract_text(&self.visible_rows, self.geometry.scroll_offset, bounds)
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

    /// Clear all cells — a content rebuild (session switch, compaction
    /// re-sync, rewind replay). Bumps [`Self::structure_epoch`] so an
    /// in-flight text selection is dropped even if the rebuilt content ends
    /// up with the same cell count.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.cell_heights.clear();
        self.pending.clear();
        self.pending_heights.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.follow_frozen = false;
        self.rebuilds = self.rebuilds.wrapping_add(1);
    }

    /// Number of full content rebuilds so far ([`Self::clear`]).
    pub fn structure_epoch(&self) -> u64 {
        self.rebuilds
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
    ///
    /// Routes through the incremental `StreamingRender` (stable prefix +
    /// active tail) — no full re-render per delta.
    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(last) = self.cells.last_mut()
            && matches!(last.cell(), ChatCell::AssistantMessage(_))
        {
            last.append_stream(text);
            return;
        }
        // New cell: push it EMPTY and route the first delta through the
        // stream too — otherwise the incremental state would start one
        // delta late and never render the first block.
        self.push(ChatCell::AssistantMessage(String::new()));
        if let Some(last) = self.cells.last_mut() {
            last.append_stream(text);
        }
    }

    /// Append reasoning content to the last thinking block (for streaming).
    ///
    /// Routes through the incremental `StreamingRender` (stable prefix +
    /// active tail) — no full re-render per delta.
    pub fn append_to_last_thinking(&mut self, text: &str) {
        if let Some(last) = self.cells.last_mut()
            && matches!(last.cell(), ChatCell::Thinking(_))
        {
            last.append_stream(text);
            return;
        }
        // New cell: push it EMPTY and route the first delta through the
        // stream too (see append_to_last_assistant).
        self.push(ChatCell::Thinking(ThinkingBlock::new()));
        if let Some(last) = self.cells.last_mut() {
            last.append_stream(text);
        }
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

    /// On resume, anchor the elapsed timer of still-running Bash cards.
    ///
    /// A mid-execution Bash tool replays from the uncommitted Message
    /// projection as a Pending cell with no `started_at` (the live ToolCall
    /// event that would set it never arrives for a late subscriber). Set it to
    /// the turn-start instant so the card shows elapsed time and keeps
    /// advancing (via `tick_bash_timers`) instead of showing nothing. Cells
    /// that already have a timer, or that already finished (Success/Failed),
    /// are left untouched.
    pub fn mark_pending_bash_running(&mut self, started_at: std::time::Instant) {
        for cached in &mut self.cells {
            if let ChatCell::ToolCall(block) = cached.cell()
                && block.tool_name == TOOL_BASH
                && block.status == ToolStatus::Pending
                && block.started_at.is_none()
            {
                cached.mutate(|cell| {
                    if let ChatCell::ToolCall(block) = cell {
                        block.started_at = Some(started_at);
                    }
                });
            }
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

    /// Request the turn-end reconcile for all streaming cells. The next
    /// render (width known there) installs the full reference render and
    /// drops the incremental state.
    pub fn finalize_streams(&mut self) {
        for cached in &mut self.cells {
            cached.request_finalize();
        }
    }

    /// Recompute heights for all cells whose cache is stale. Updates `cell_heights` in place.
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

/// Columns a user-message cell fills with background before its text starts
/// (`cell_area.x + 2` in the render path) — the padding the copy skips.
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

/// Widget for rendering the chat view with scrollbar.
pub struct ChatViewWidget<'a> {
    view: &'a mut ChatView,
    ctx: CellContext<'a>,
}

/// Walk one rendered row of the chat band and return its graphemes.
///
/// The buffer holds what the terminal will show: a wide grapheme stores its
/// symbol in the first cell and leaves `width - 1` filler cells behind it
/// (their symbol reads back as `" "`). Advancing by [`grapheme_width`] — not
/// by one cell — is what keeps text extraction free of phantom spaces and
/// what lets the highlighter paint both cells of a wide character.
fn buffer_row_graphemes(buf: &Buffer, row: u16, area: Rect) -> Vec<Grapheme> {
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
            self.view.frame_links.clear();
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
        let mut frame_links: Vec<(u16, Vec<FrameLink>)> = Vec::new();

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

/// The link table to place for one cell.
///
/// Cloned only when the cell actually has something to place (and its rows are
/// exact), so a frame full of cells without links allocates nothing — the
/// returned vector is `Vec::new()` (capacity 0) in that case.
fn links_for_frame(cell: &crate::ui::cached_cell::CellLines<'_>) -> Vec<Vec<LinkSpan>> {
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
fn place_links(
    frame_links: &mut Vec<(u16, Vec<FrameLink>)>,
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
            frame_links.push((row, links));
        }
    }
}

/// Put an OSC8 hyperlink on every grapheme head cell of `start..end`.
///
/// ratatui has no hyperlink API, so the sequence rides in the cell symbol —
/// the one string the crossterm backend prints verbatim. Two details make that
/// safe:
///
/// * `CellDiffOption::ForcedWidth(1)`: `Cell::cell_width()` otherwise measures
///   the symbol (URL included) and `BufferDiff` uses that width to skip the
///   cells *after* a wide one — a 40-character URL would make the diff skip the
///   rest of the line.
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
                std::num::NonZeroU16::new(1).expect("1 is non-zero"),
            ));
        x += width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;
    use crate::config::{LayoutConfig, ThemePalette};
    use crate::render::markdown::stream::Profile;
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

    // ── Pending user messages (sent, awaiting model acceptance) ──

    #[test]
    fn test_push_pending_keeps_message_out_of_history() {
        let mut view = ChatView::new();
        view.push_pending("req-1".into(), "queued".into());

        assert!(view.cells.is_empty());
        assert_eq!(view.pending.len(), 1);
        assert!(matches!(
            view.pending[0].cell.cell(),
            ChatCell::PendingUserMessage(text) if text == "queued"
        ));
        // The user's own submission must be visible — auto-scroll re-armed.
        assert!(view.is_at_bottom());
    }

    #[test]
    fn test_promote_pending_moves_message_into_history() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("answer".into()));
        view.push_pending("req-1".into(), "follow-up".into());

        assert!(view.promote_pending("req-1"));
        assert!(view.pending.is_empty());
        assert_eq!(view.cells.len(), 2);
        assert!(matches!(
            view.cells[1].cell(),
            ChatCell::UserMessage(text) if text == "follow-up"
        ));
    }

    #[test]
    fn test_promote_pending_unknown_id_returns_false() {
        let mut view = ChatView::new();
        view.push_pending("req-1".into(), "queued".into());

        assert!(!view.promote_pending("req-other"));
        assert_eq!(view.pending.len(), 1);
        assert!(view.cells.is_empty());
    }

    #[test]
    fn test_promote_all_pending_preserves_send_order() {
        let mut view = ChatView::new();
        view.push_pending("req-1".into(), "first".into());
        view.push_pending("req-2".into(), "second".into());

        view.promote_all_pending();

        assert!(view.pending.is_empty());
        let texts: Vec<&str> = view
            .cells
            .iter()
            .filter_map(|c| match c.cell() {
                ChatCell::UserMessage(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["first", "second"]);
    }

    #[test]
    fn test_discard_all_pending_marks_messages_unsent() {
        let mut view = ChatView::new();
        view.push_pending("req-1".into(), "lost".into());

        view.discard_all_pending();

        assert!(view.pending.is_empty());
        assert_eq!(view.cells.len(), 1);
        assert!(matches!(
            view.cells[0].cell(),
            ChatCell::DiscardedUserMessage(text) if text == "lost"
        ));
    }

    #[test]
    fn test_remove_pending_drops_unsent_message() {
        let mut view = ChatView::new();
        view.push_pending("req-1".into(), "never sent".into());

        view.remove_pending("req-1");

        assert!(view.pending.is_empty());
        assert!(view.cells.is_empty());
    }

    #[test]
    fn test_clear_removes_pending() {
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("a".into()));
        view.push_pending("req-1".into(), "queued".into());

        view.clear();

        assert!(view.cells.is_empty());
        assert!(view.pending.is_empty());
    }

    #[test]
    fn test_user_message_state_lines_use_distinct_styles() {
        let (palette, layout) = test_ctx();
        let ctx = make_ctx(&palette, &layout);

        let normal = ChatCell::UserMessage("m".into()).to_lines(80, &ctx);
        let pending = ChatCell::PendingUserMessage("m".into()).to_lines(80, &ctx);
        let discarded = ChatCell::DiscardedUserMessage("m".into()).to_lines(80, &ctx);

        // Pending is dimmed; discarded is dimmed + struck through; normal is neither.
        let style_of = |lines: &Vec<Line<'static>>| lines[0].spans[0].style;
        assert_eq!(style_of(&pending).fg, Some(palette.dim));
        assert_eq!(style_of(&discarded).fg, Some(palette.dim));
        assert!(
            style_of(&discarded)
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert!(
            !style_of(&pending)
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert_eq!(style_of(&normal).fg, Some(palette.text));
    }

    // ============================================================
    // Streaming (incremental render) integration
    // ============================================================

    fn span_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn test_streaming_append_matches_full_render() {
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let text = "First paragraph.\n\nSecond with `code`.\n\n- item one\n- item two\n\n```rust\nlet x = 1;\n```\n\nAfter.";
        let mut view = ChatView::new();
        // Feed as odd-sized chunks, syncing (compute_lines) after each.
        let mut fed = String::new();
        for chunk in text.as_bytes().chunks(7) {
            // cut at char boundary
            let mut s = String::new();
            for b in chunk {
                s.push(*b as char);
            }
            // Rebuild from bytes safely: only ASCII in this corpus.
            let mut owned = String::new();
            for c in s.chars() {
                owned.push(c);
            }
            fed.push_str(&owned);
            view.append_to_last_assistant(&owned);
            let _ = view.cells[0].compute_lines(80, &ctx);
        }
        let streaming: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();
        let reference = crate::render::markdown::stream::full_lines(&fed, 80, Profile::Content, &p);
        assert_eq!(span_texts(&streaming), span_texts(&reference));

        // Height is the flat line count (pre-wrapped).
        let h = view.cells[0].compute_height(80, &ctx);
        assert_eq!(h, streaming.len());
    }

    #[test]
    fn test_streaming_finalize_installs_reference() {
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let text = "Reasoning paragraph.\n\n```rust\nfn main() {}\n```\n\nDone.";
        let mut view = ChatView::new();
        view.append_to_last_thinking("Reasoning par");
        view.append_to_last_thinking("agraph.\n\n```rust\nfn main() {}\n```\n\nDone.");
        let _ = view.cells[0].compute_lines(80, &ctx);
        assert!(view.cells[0].is_streaming());

        view.finalize_streams();
        let finalized: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();
        assert!(!view.cells[0].is_streaming());
        assert!(view.cells[0].is_prewrapped(80, &ctx));

        let reference =
            crate::render::markdown::stream::full_lines(text, 80, Profile::Thinking, &p);
        assert_eq!(span_texts(&finalized), span_texts(&reference));
        // Height survives as the line count after finalize.
        assert_eq!(view.cells[0].compute_height(80, &ctx), finalized.len());
    }

    #[test]
    fn test_streaming_width_change_rebuild() {
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let text = "A reasonably long paragraph that wraps at narrow widths.";
        let mut view = ChatView::new();
        view.append_to_last_assistant(text);
        let wide: Vec<Line<'static>> = view.cells[0].compute_lines(100, &ctx).to_vec();
        let reference_wide =
            crate::render::markdown::stream::full_lines(text, 100, Profile::Content, &p);
        assert_eq!(span_texts(&wide), span_texts(&reference_wide));

        // Narrow: full rebuild from the buffer.
        let narrow: Vec<Line<'static>> = view.cells[0].compute_lines(30, &ctx).to_vec();
        let reference_narrow =
            crate::render::markdown::stream::full_lines(text, 30, Profile::Content, &p);
        assert_eq!(span_texts(&narrow), span_texts(&reference_narrow));
        assert!(narrow.len() > wide.len(), "should wrap more when narrower");
    }

    // ============================================================
    // Review-fix regressions (P0-1/2/3)
    // ============================================================

    #[test]
    fn test_streaming_replayed_prefix_stays_visible() {
        // Mid-turn resume: the cell is replayed with existing content
        // (replay_messages → push), then live deltas land on the SAME
        // cell. The stream must be seeded with the replayed prefix —
        // the rendered view must contain BOTH prefix and delta.
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let mut view = ChatView::new();
        view.push(ChatCell::AssistantMessage("REPLAYED-PREFIX-TEXT ".into()));
        view.append_to_last_assistant("live delta");
        let lines: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("REPLAYED-PREFIX-TEXT") && text.contains("live delta"),
            "replayed prefix lost from view: {text}"
        );

        // Same through the turn-end reconcile (finalize must not drop it
        // either — stream buffer == cell text invariant).
        view.finalize_streams();
        let lines: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("REPLAYED-PREFIX-TEXT") && text.contains("live delta"),
            "replayed prefix lost after finalize: {text}"
        );
    }

    #[test]
    fn test_streaming_thinking_hidden_mode_no_leak() {
        // Hidden thinking mode: the stream keeps accumulating but the
        // visible lines come from the cell's own renderer (the hidden
        // indicator) — the reasoning content must never leak, neither
        // while streaming nor after the turn-end finalize.
        let (p, l) = test_ctx();
        let ctx = CellContext {
            palette: &p,
            thinking_mode: ThinkingMode::Hidden,
            layout: &l,
        };
        let mut view = ChatView::new();
        view.append_to_last_thinking("SECRET-REASONING-CONTENT");
        view.increment_thinking_count();
        let lines: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Thinking..."), "indicator missing: {text}");
        assert!(
            !text.contains("SECRET-REASONING"),
            "hidden reasoning leaked while streaming: {text}"
        );
        let h_streaming = view.cells[0].compute_height(80, &ctx);
        assert!(
            h_streaming <= 2,
            "hidden indicator height wrong: {h_streaming}"
        );

        // Turn end: finalize must not install the visible full render.
        view.finalize_streams();
        let lines: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("SECRET-REASONING"),
            "hidden reasoning leaked after finalize: {text}"
        );
        assert!(text.contains("1 events"), "event count lost: {text}");
        assert_eq!(view.cells[0].compute_height(80, &ctx), h_streaming);
    }

    #[test]
    fn test_streaming_resize_after_finalize_wraps_not_truncates() {
        // P0-3: after the turn-end finalize, a width change must render
        // through the normal (wrapping) path — a long code line must
        // WRAP at the narrower width, not truncate.
        let long_line = format!("let x = \"{}\";", "A".repeat(160));
        let text = format!("intro\n\n```rust\n{long_line}\n```\n\nafter");

        // A: streaming + finalize.
        let (p, l) = test_ctx();
        let ctx_wide = make_ctx(&p, &l);
        let mut view_a = ChatView::new();
        view_a.append_to_last_assistant(&text);
        let _ = view_a.cells[0].compute_lines(100, &ctx_wide);
        view_a.finalize_streams();
        let _ = view_a.cells[0].compute_lines(100, &ctx_wide);

        // B: same content, never streamed.
        let mut view_b = ChatView::new();
        view_b.push(ChatCell::AssistantMessage(text.clone()));

        // Resize both to width 40 and compare.
        let buf_a = render_view(&mut view_a, 40, 30);
        let buf_b = render_view(&mut view_b, 40, 30);
        let text_a = buffer_text(&buf_a);
        let text_b = buffer_text(&buf_b);
        // The tail of the long line must survive wrapping in BOTH.
        let tail = "A".repeat(100);
        assert!(
            text_b.contains(&tail[..20]),
            "control (never-streamed) lost the line tail — test broken: {text_b}"
        );
        assert!(
            text_a.contains(&tail[..20]),
            "finalized streaming cell truncated instead of wrapping after resize: {text_a}"
        );
        // And heights agree (blit vs Paragraph path).
        let h_a = view_a.cells[0].compute_height(40, &ctx_wide);
        let h_b = view_b.cells[0].compute_height(40, &ctx_wide);
        assert_eq!(h_a, h_b, "height mismatch after resize: {h_a} vs {h_b}");
    }

    #[test]
    fn test_streaming_widget_blit_render() {
        // A streaming cell renders through the direct-blit path (no
        // Paragraph Composer) — verify the buffer contents match the
        // reference full render and every row fits the width.
        let mut view = ChatView::new();
        view.append_to_last_assistant("streaming answer with **bold** and `code`.\n\n- item");
        let mut buf = render_view(&mut view, 40, 10);
        let text = buffer_text(&buf);
        assert!(
            text.contains("streaming answer"),
            "streaming content missing from blit render: {text}"
        );
        assert!(text.contains("item"), "list content missing: {text}");
        for line in text.lines() {
            assert!(line.chars().count() <= 40, "row exceeds width: {line:?}");
        }

        // After finalize the cell stays on the blit path.
        view.finalize_streams();
        buf = render_view(&mut view, 40, 10);
        let text2 = buffer_text(&buf);
        assert!(
            text2.contains("streaming answer"),
            "finalized content missing: {text2}"
        );
        assert!(text2.contains("item"));
    }

    #[test]
    fn test_streaming_blit_scroll() {
        // A long streaming cell, scrolled to the bottom via auto-scroll,
        // renders the LAST lines through the blit path.
        let mut view = ChatView::new();
        let mut text = String::new();
        for i in 0..60 {
            text.push_str(&format!("line {i:03} of the stream\n"));
        }
        view.append_to_last_assistant(&text);
        let buf = render_view(&mut view, 40, 8);
        let text = buffer_text(&buf);
        // Auto-scroll pins to bottom: the last visible row region shows
        // the tail lines, not the head.
        assert!(
            !text.contains("line 000"),
            "auto-scroll lost — head visible: {text}"
        );
        assert!(
            text.contains("line 05"),
            "tail lines missing from blit render: {text}"
        );
    }

    #[test]
    fn test_streaming_stable_prefix_not_rewritten() {
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let mut view = ChatView::new();
        view.append_to_last_assistant("first block.\n\n");
        let _ = view.cells[0].compute_lines(80, &ctx);
        let before: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();

        view.append_to_last_assistant("second block grows ");
        view.append_to_last_assistant("more");
        let after: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();

        // The promoted first block's lines are unchanged prefix-wise.
        let prefix_len = before.len().saturating_sub(1); // minus trailing cell blank
        assert!(after.len() >= prefix_len);
        for i in 0..prefix_len {
            assert_eq!(
                before[i].to_string(),
                after[i].to_string(),
                "stable prefix line {i} changed"
            );
        }
    }

    // ── Text selection: mapping, highlight, copy source ────────────────

    /// Render the view into a `screen`-sized buffer, laying the chat band out
    /// at `band` — lets tests assert that nothing outside the band is touched.
    fn render_view_in(view: &mut ChatView, screen: Rect, band: Rect) -> Buffer {
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let mut buf = Buffer::empty(screen);
        ChatViewWidget::new(view, ctx).render(band, &mut buf);
        buf
    }

    /// A selection covering `bounds` (press + drag), for the paint tests.
    fn selection_over(bounds: (ContentPoint, ContentPoint)) -> Selection {
        let mut selection = Selection::default();
        selection.begin(bounds.0);
        selection.drag_to(bounds.1);
        selection
    }

    fn reversed_columns(buf: &Buffer, row: u16) -> Vec<u16> {
        (buf.area.x..buf.area.right())
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::REVERSED))
            .collect()
    }

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
        assert_eq!((point.vrow, point.col), (2, 5));
        assert!(view.contains_screen(7, 4));

        // Pointer outside the band clamps to the nearest edge instead of
        // failing — edge drags (and their auto-scroll) depend on this. The
        // row clamps twice: into the band, then into the content (the blank
        // rows below a short conversation are not content).
        assert_eq!(view.content_point_at(0, 0).map(|p| p.vrow), Some(0));
        assert_eq!(view.content_point_at(0, 0).map(|p| p.col), Some(0));
        assert_eq!(view.content_point_at(99, 99).map(|p| p.vrow), Some(2));
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
        assert_eq!(view.content_point_at(0, 0).map(|p| p.vrow), Some(first));
        assert_eq!(view.screen_row_of(first), Some(0));

        // Scroll up five rows: the mapping follows the new frame, so the same
        // screen row now points at different content.
        view.scroll_up(5);
        let _ = render_view_in(&mut view, Rect::new(0, 0, 40, 10), band);
        assert_eq!(view.geometry().scroll_offset, first - 5);
        assert_eq!(view.content_point_at(0, 0).map(|p| p.vrow), Some(first - 5));
    }

    #[test]
    fn test_paint_selection_marks_only_the_selected_cells() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello world".into()));
        let band = Rect::new(2, 3, 30, 8);
        let mut buf = render_view_in(&mut view, Rect::new(0, 0, 40, 20), band);
        let before = buf.clone();

        // Row 1 of the user cell is the text row ("hello world" at column 2).
        let bounds = (
            ContentPoint { vrow: 1, col: 2 },
            ContentPoint { vrow: 1, col: 13 },
        );
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
        let bounds = (
            ContentPoint { vrow: 1, col: 2 },
            ContentPoint { vrow: 1, col: 5 },
        );
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
        let bounds = (
            ContentPoint { vrow: 0, col: 0 },
            ContentPoint { vrow: 0, col: 40 },
        );
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
    fn test_capture_and_selected_text_from_the_rendered_frame() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello world".into()));
        let band = Rect::new(0, 0, 30, 8);
        let buf = render_view_in(&mut view, Rect::new(0, 0, 30, 8), band);
        view.capture_visible_rows(&buf);

        let text = view.selected_text((
            ContentPoint { vrow: 1, col: 2 },
            ContentPoint { vrow: 1, col: 13 },
        ));
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
        let text = view.selected_text((
            ContentPoint { vrow: 1, col: 2 },
            ContentPoint { vrow: 1, col: 10 },
        ));
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
            view.selected_text((
                ContentPoint { vrow: 0, col: 0 },
                ContentPoint { vrow: 1, col: 5 },
            )),
            None
        );
    }

    #[test]
    fn test_capture_clears_when_the_band_collapses() {
        let mut view = ChatView::new();
        view.push(ChatCell::UserMessage("hello".into()));
        let buf = render_view_in(&mut view, Rect::new(0, 0, 30, 8), Rect::new(0, 0, 30, 8));
        view.capture_visible_rows(&buf);
        assert!(!view.visible_rows.is_empty());

        // A collapsed band (zero height) must drop the mapping *and* the
        // snapshot — a stale rect could otherwise accept presses.
        let _ = render_view_in(&mut view, Rect::new(0, 0, 30, 8), Rect::new(0, 0, 30, 0));
        assert_eq!(view.geometry().area, Rect::ZERO);
        assert!(view.content_point_at(1, 1).is_none());
        view.capture_visible_rows(&buf);
        assert!(view.visible_rows.is_empty());
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

#[cfg(test)]
mod link_tests {
    use super::*;
    use crate::config::LayoutConfig;
    use crate::config::ThemePalette;
    use crate::render::markdown::osc8_close;
    use crate::render::markdown::osc8_open;
    use crate::render::markdown::sanitize_osc8_target;
    use crate::render::markdown::strip_osc8;
    use crate::render::renderable::CellContext;

    fn render(view: &mut ChatView, width: u16, height: u16) -> Buffer {
        let (palette, layout) = (ThemePalette::default(), LayoutConfig::default());
        let ctx = CellContext {
            palette: &palette,
            thinking_mode: crate::config::rendering::ThinkingMode::Visible,
            layout: &layout,
        };
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        ChatViewWidget::new(view, ctx).render(area, &mut buf);
        buf
    }

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
            s.begin(ContentPoint {
                vrow: row as usize,
                col: 0,
            });
            s.drag_to(ContentPoint {
                vrow: row as usize,
                col: link.end,
            });
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
}

#[cfg(test)]
mod mask_tests {
    use super::*;

    fn link(start: u16, end: u16) -> FrameLink {
        FrameLink {
            start,
            end,
            target: "https://example.com".into(),
        }
    }

    #[test]
    fn mask_links_drops_covered_rows_and_clips_columns() {
        let mut view = ChatView::new();
        view.frame_links = vec![
            (3, vec![link(0, 4), link(8, 12)]),
            (4, vec![link(0, 20)]),
            (9, vec![link(2, 6)]),
        ];

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
}
