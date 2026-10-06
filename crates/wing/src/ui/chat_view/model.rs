//! The content model: cells, pending messages and every mutation the app
//! performs on them.
//!
//! This is the only place that changes *what* the view holds — pushing cells,
//! queueing / promoting / discarding pending user messages, anchoring derived
//! cells under their tool call and the streaming (by-index) mutations.
//!
//! It never **reads** scroll state, geometry or rendering. The only writes to
//! the viewport state are the three app-visible entries that are part of the
//! model's own contract: `push_pending` and `show_model_picker` bring the
//! user's own content into view, and `clear()` resets the viewport for a
//! rebuilt content list (top of the content, follow re-armed). Everything
//! else about scrolling belongs to `super::viewport`.

use ratatui::text::Line;

use crate::shared::constants::TOOL_BASH;
use crate::shared::panels::ask::AskPanel;
use crate::shared::panels::picker::ModelPanel;
use crate::ui::cached_cell::CachedCell;
use crate::ui::cells::thinking::ThinkingBlock;
use crate::ui::cells::tool_call::ToolStatus;

use super::ChatCell;
use super::ChatView;
use super::PendingMessage;

impl ChatView {
    /// The header lines as last set (tests read them back).
    #[cfg(test)]
    pub(crate) fn header_lines(&self) -> &[Line<'static>] {
        &self.header_lines
    }

    /// Set the header lines (the welcome block — see `ui::welcome`).
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
    /// Returns `false` when nothing matches (the message originated from
    /// another client — not tracked here).
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

    /// Interrupted: the backend discarded the listed pending requests from
    /// its inbox (they never reached the model) — commit exactly those as
    /// discarded (dim + struck through) instead of silently vanishing.
    ///
    /// Every other pending message stays queued: it arrived after the
    /// interrupt request and is still going to be consumed (promoted by
    /// `user_message_accepted` when that happens).
    pub fn discard_pending(&mut self, request_ids: &[String]) {
        let mut kept = Vec::with_capacity(self.pending.len());
        let mut discarded: Vec<String> = Vec::new();
        for msg in self.pending.drain(..) {
            if request_ids.contains(&msg.request_id) {
                if let ChatCell::PendingUserMessage(content) = msg.cell.into_inner() {
                    discarded.push(content);
                }
            } else {
                kept.push(msg);
            }
        }
        self.pending = kept;
        for content in discarded {
            self.push(ChatCell::DiscardedUserMessage(content));
        }
    }

    /// Interrupted without a drop list (legacy gateway): assume every
    /// pending message was dropped and commit them as discarded (dim +
    /// struck through).
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

    /// Remove the Ask cell with the given tool_call_id from the chat view.
    ///
    /// Called when a *queued* (still unanswered) ask is dropped — the turn
    /// ended or was interrupted and the backend cancelled its waiter.
    ///
    /// Searched from the tail, like [`ChatView::update_ask_panel`]: the App
    /// mirrors into — and therefore drops — the **newest** cell carrying the
    /// id. A tool call may legitimately reuse its `tool_call_id` (the Bash
    /// confirmation re-asks after a rejected answer), and a first-match
    /// removal would strand a stale card behind the live one.
    pub fn remove_ask(&mut self, tool_call_id: &str) {
        let idx = self.cells.iter().rposition(
            |c| matches!(c.cell(), ChatCell::Ask(msg) if msg.panel.tool_call_id == tool_call_id),
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
                && msg.panel.tool_call_id == tool_call_id
            {
                cell.mutate(|c| {
                    if let ChatCell::Ask(msg) = c {
                        msg.panel = panel;
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

    /// Drop the ReAct separators that no longer sit directly after a ToolCall.
    ///
    /// [`ChatView::push`] only ever inserts a separator in that position, so
    /// any other one is a replay artifact: resume renders the message
    /// projections first (separator inserted — the diff was not anchored yet)
    /// and applies the fact events afterwards, which slips the diff *behind*
    /// the separator. Live, where the diff is already anchored when the next
    /// round's text arrives, never grows it. Called at the end of the replay
    /// assembly to restore cell-for-cell parity.
    pub fn drop_stale_separators(&mut self) {
        let mut after_tool_call = false;
        self.cells.retain(|cell| match cell.cell() {
            ChatCell::ToolCall(_) => {
                after_tool_call = true;
                true
            }
            ChatCell::Separator => {
                let keep = after_tool_call;
                after_tool_call = false;
                keep
            }
            _ => {
                after_tool_call = false;
                true
            }
        });
    }

    /// Get the number of cells.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
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
        // `thinking_expanded`（Ctrl+O 的模式）刻意**不**重置：会话内切一次、
        // 之后 resume / compaction / sync 重建都保持 —— 用户选的是"怎么看"，
        // 不是"这一帧怎么看"。
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
    ///
    /// 承接规则：
    /// * 最后一个 cell 是**未收尾**的思考块 → 继续这一段（重放半截块没有计时，
    ///   第一个 live delta 在这里补上开始时刻）；
    /// * 否则开新块 —— 开之前先把还挂着的活跃块冻上：锚定插入（Diff / Todo）、
    ///   `/model` 面板、Ask 卡片都可能把"最后一块"与活跃块劈开，活跃块的
    ///   冻结不能依赖"它是最后一个 cell"。
    pub fn append_to_last_thinking(&mut self, text: &str) {
        self.append_to_last_thinking_at(text, std::time::Instant::now());
    }

    /// [`append_to_last_thinking`](Self::append_to_last_thinking) with the
    /// clock injected — tests pin the timing, production passes `now`.
    pub(crate) fn append_to_last_thinking_at(&mut self, text: &str, now: std::time::Instant) {
        let continues = matches!(
            self.cells.last().map(|cached| cached.cell()),
            Some(ChatCell::Thinking(block)) if !block.is_finished()
        );
        if continues {
            if let Some(last) = self.cells.last_mut() {
                last.mutate(|cell| {
                    if let ChatCell::Thinking(block) = cell {
                        // 重放半截块：首个 live delta 补开始时刻。
                        block.start(now);
                    }
                });
                last.append_stream(text);
            }
            return;
        }
        // 上一段推理到此为止（幂等；没有活跃块时是一次纯匹配扫描）。
        self.finish_active_thinking(now);
        // New cell: push it EMPTY and route the first delta through the
        // stream too (see append_to_last_assistant).
        let mut block = ThinkingBlock::new();
        block.start(now);
        self.push(ChatCell::Thinking(block));
        if let Some(last) = self.cells.last_mut() {
            last.append_stream(text);
        }
    }

    /// 冻结所有还在计时的思考块：模型进入下一阶段（正文 / 工具调用）或回合
    /// 结束。
    ///
    /// 扫描全表而不是缓存下标 —— 下标会被锚定插入 / 面板开合 / Ask 卡片顶偏
    /// （见 `app/render_context.rs` 的文件头约定），扫描是自愈的：哪怕活跃块
    /// 被劈开过，任何一个转折点都会把它收掉。幂等，没有活跃块时零失效成本。
    pub fn finish_active_thinking(&mut self, now: std::time::Instant) {
        for cached in self.cells.iter_mut() {
            let active = matches!(cached.cell(), ChatCell::Thinking(block) if block.is_active());
            if active {
                cached.mutate(|cell| {
                    if let ChatCell::Thinking(block) = cell {
                        block.finish(now);
                    }
                });
            }
        }
    }

    /// 帧 tick：推进活跃思考块（最后一块仍在计时的）的刷光相位与显示秒数，
    /// 并作废它的缓存。返回是否有块被推进（App 据此把这一帧标记为 dirty）。
    pub fn tick_active_thinking(&mut self, now: std::time::Instant) -> bool {
        let Some(index) = self.last_active_thinking() else {
            return false;
        };
        let Some(cached) = self.cells.get_mut(index) else {
            return false;
        };
        cached.tick_thinking(now)
    }

    /// 帧驱动用的活跃思考块下标（从尾部扫描 —— 下标绝不跨帧缓存）。
    ///
    /// `pub(super)`：视口层判断「它在不在屏幕上」时要先找到它
    /// （`reasoning_sweep_deadline` 在 `viewport.rs`），内容扫描本身留在这里。
    pub(super) fn last_active_thinking(&self) -> Option<usize> {
        self.cells.iter().rposition(
            |cached| matches!(cached.cell(), ChatCell::Thinking(block) if block.is_active()),
        )
    }

    /// Ctrl+O：翻转思考块的展开 —— **全局**（所有轮一起切），会话内一直有效。
    ///
    /// 默认（初始）展开还是折叠由 `rendering.thinking` 给，`default_expanded`
    /// 是它的布尔投影。返回是否真的动了（App 据此决定要不要重画）：没有一次
    /// 翻转是"半生效"的 —— 覆盖是渲染期读的，切一下所有块跟着变。
    pub fn toggle_thinking_expansion(&mut self, default_expanded: bool) -> bool {
        let next = !self.thinking_expanded.unwrap_or(default_expanded);
        self.thinking_expanded = Some(next);
        let mut changed = false;
        for cached in self.cells.iter_mut() {
            if matches!(cached.cell(), ChatCell::Thinking(_)) {
                // presentation 不在缓存键里（见 `CellContext`）：必须显式作废
                // 行 / 高度缓存，下一帧才会按新模式重排。
                cached.invalidate();
                changed = true;
            }
        }
        changed
    }

    /// 当前的全局展开覆盖（App 把它放进 `CellContext`）。
    pub fn thinking_expansion(&self) -> Option<bool> {
        self.thinking_expanded
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
    ///
    /// O(1) and cache-neutral: the fragment is buffered, and the parse (plus
    /// the render-cache invalidation it implies) runs at the frame boundary
    /// — see [`ChatView::flush_tool_args_by_index`] for the eager path.
    pub fn append_tool_args_fragment_by_index(&mut self, index: usize, fragment: &str) {
        if let Some(cached) = self.cells.get_mut(index) {
            cached.append_tool_args_fragment(fragment);
        }
    }

    /// Force the deferred args parse for one cell (`is_final` / authoritative
    /// args). Idempotent — a cell with nothing pending is left untouched.
    pub fn flush_tool_args_by_index(&mut self, index: usize) {
        if let Some(cached) = self.cells.get_mut(index) {
            cached.flush_pending_args();
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
                    block.start_timer(std::time::Instant::now());
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
                        block.start_timer(started_at);
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

    /// Refresh pending Bash timers at the frame boundary — invalidating a
    /// cell only when its *displayed* elapsed value actually moves.
    ///
    /// The heartbeat that calls this runs at 100 ms, but the display is
    /// whole seconds (`format_bash_timer`), so nine out of ten ticks are
    /// no-ops; invalidating on every tick re-rendered each pending cell
    /// (full `to_lines` + the height recompute's `to_vec` clone) 10×/s for
    /// a value that changed 1×/s.
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
                cached.tick_bash_timer();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;
    use crate::render::markdown::Profile;
    use crate::render::renderable::CellContext;
    use std::time::Instant;

    use super::super::test_support::{buffer_text, make_ctx, render_view, span_texts, test_ctx};
    use crate::ui::cells::tool_call::ToolCallBlock;
    use std::time::Duration;

    /// A panel carrying one question with the given text (the cell content
    /// that tells the two same-id cells apart).
    fn ask_panel(text: &str) -> AskPanel {
        use crate::protocol::AskQuestion;
        AskPanel::new(
            "ask-1".into(),
            vec![AskQuestion {
                id: "q".into(),
                header: String::new(),
                question: text.into(),
                multi_select: false,
                options: vec![],
                choices: vec![],
            }],
        )
    }

    #[test]
    fn header_in_view_boundaries() {
        // 这是"看不见不花钱"那条契约的总闸门（App::sync_welcome 用它决定是否
        // 短路整条动画时钟）：边界写错不会有别的测试变红。
        let mut view = ChatView::new();
        assert!(!view.header_in_view(), "没有 header 就谈不上可见");

        view.set_header((0..5).map(|i| Line::from(format!("h{i}"))).collect());
        view.scroll_offset = 0;
        assert!(view.header_in_view(), "顶部：整块在视口里");
        view.scroll_offset = 4;
        assert!(view.header_in_view(), "滚到最后一行 header：仍可见");
        view.scroll_offset = 5;
        assert!(!view.header_in_view(), "正好滚过头：不可见");
        view.scroll_offset = 999;
        assert!(!view.header_in_view(), "滚到底：不可见");
    }

    /// The question texts of the Ask cells, in cell order.
    fn ask_texts(view: &ChatView) -> Vec<String> {
        view.cells
            .iter()
            .filter_map(|c| match c.cell() {
                ChatCell::Ask(msg) => Some(msg.panel.questions[0].question.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_ask_cells_are_addressed_from_the_tail() {
        // A tool call may reuse its `tool_call_id` (the Bash confirmation
        // re-asks after a rejected answer), leaving two Ask cells with the same
        // id. The mirror and the removal must both address the **newest** one —
        // the panel the app actually owns — or the stale card is stranded.
        use crate::ui::cells::ask_msg::AskMessage;

        let mut view = ChatView::new();
        view.push(ChatCell::Ask(AskMessage::new(ask_panel("old"))));
        view.push(ChatCell::Ask(AskMessage::new(ask_panel("new"))));

        view.update_ask_panel("ask-1", ask_panel("mirrored"));
        assert_eq!(ask_texts(&view), vec!["old", "mirrored"]);

        view.remove_ask("ask-1");
        assert_eq!(ask_texts(&view), vec!["old"]);
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
        use crate::ui::cells::tool_call::ToolCallBlock;
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
        use crate::ui::cells::tool_call::ToolCallBlock;
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
        use crate::ui::cells::tool_call::ToolCallBlock;
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
        use crate::ui::cells::diff_view::DiffView;
        use crate::ui::cells::tool_call::ToolCallBlock;
        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new(
            "Edit".into(),
            serde_json::json!({}),
            "tc_edit".into(),
        )));

        // Two derived cells for the same tool call keep emission order
        // (the second anchoring skips past the first sibling).
        let diff = |p: &str| ChatCell::Diff(DiffView::new(p.into(), None, "new".into(), 1, 1));
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
        use crate::ui::cells::tool_call::ToolCallBlock;

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
        use crate::ui::cells::tool_call::ToolCallBlock;

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
        use crate::ui::cells::tool_call::ToolCallBlock;

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
        use crate::ui::cells::tool_call::ToolCallBlock;

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

    /// The Bash timer advances only while the cell is Pending AND the
    /// displayed whole-second value moved: the heartbeat ticks at 100 ms,
    /// but a tick inside the same second is a no-op (the display is
    /// second-granular). Once a result flips the status away from Pending,
    /// ticks stop touching the cell and the displayed elapsed freezes.
    #[test]
    fn test_bash_timer_freezes_after_result() {
        let mut view = ChatView::new();
        let mut block = ToolCallBlock::new(
            TOOL_BASH.into(),
            serde_json::json!({"command": "sleep 5"}),
            "tc-timer".into(),
        );
        block.start_timer(Instant::now());
        view.push(ChatCell::ToolCall(block));

        let idx = view.tool_call_index("tc-timer").unwrap();

        // Same second — the tick changes nothing the display shows.
        let before = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), before);

        // The second moves (simulated by backdating the start instant): the
        // next tick invalidates the cache, and the one after it is a no-op
        // again.
        view.cells[idx].mutate(|cell| {
            if let ChatCell::ToolCall(block) = cell {
                block.started_at = Some(Instant::now() - Duration::from_secs(1));
            }
        });
        let before = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), before + 1);
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
    /// `set_final_args`, which flips the status itself — must still be
    /// selected by the timer tick.
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

        // Now Pending — a tick inside the same second stays a no-op...
        let before = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), before);

        // ...and the tick that sees the displayed second move invalidates.
        view.cells[idx].mutate(|cell| {
            if let ChatCell::ToolCall(block) = cell {
                block.started_at = Some(Instant::now() - Duration::from_secs(1));
            }
        });
        let before = view.cells[idx].generation();
        view.tick_bash_timers();
        assert_eq!(view.cells[idx].generation(), before + 1);
    }

    /// Acceptance (#98): a burst of args fragments is O(1) per fragment —
    /// no parse, no cache invalidation. The parse runs once per frame (with
    /// data), driven by the render path, and an idle frame is free.
    #[test]
    fn test_streaming_args_fragments_parse_once_per_frame() {
        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new_streaming(
            TOOL_BASH.into(),
            "tc-o1".into(),
        )));
        let idx = view.tool_call_index("tc-o1").unwrap();

        for _ in 0..50 {
            view.append_tool_args_fragment_by_index(idx, r#"{"command": "echo hi"#);
        }
        let before = view.cells[idx].generation();
        assert_eq!(
            args_parse_count(&view, idx),
            0,
            "fragments must not parse on arrival"
        );
        assert_eq!(
            view.cells[idx].generation(),
            before,
            "fragments must not invalidate the render cache"
        );

        // One frame with the cell in view: exactly one parse + invalidation.
        render_view(&mut view, 80, 24);
        assert_eq!(args_parse_count(&view, idx), 1);
        let after_frame = view.cells[idx].generation();
        assert_eq!(after_frame, before + 1);

        // A frame with no new fragments: nothing.
        render_view(&mut view, 80, 24);
        assert_eq!(args_parse_count(&view, idx), 1);
        assert_eq!(view.cells[idx].generation(), after_frame);
    }

    /// The height path is a frame boundary too: heights feed the scroll
    /// maths, and a cell whose height is already cached takes the early
    /// return — without the flush there, the stale (pre-fragment) height
    /// would be handed back as-is.
    #[test]
    fn test_compute_height_flushes_deferred_args() {
        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new_streaming(
            TOOL_BASH.into(),
            "tc-height".into(),
        )));
        let idx = view.tool_call_index("tc-height").unwrap();
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);

        // Establish the height cache for the empty header.
        let empty_height = view.cells[idx].compute_height(80, &ctx);
        assert_eq!(args_parse_count(&view, idx), 0, "nothing appended yet");

        // A wrapping-length command arrives; nothing invalidates the cache
        // yet (that is the O(1) append contract).
        let command = "x".repeat(200);
        view.append_tool_args_fragment_by_index(idx, &format!(r#"{{"command": "{command}"}}"#));

        // Measuring must flush first: the parsed command wraps the header,
        // so the fresh height grows — a cache hit without the flush would
        // hand back the stale baseline.
        let height = view.cells[idx].compute_height(80, &ctx);
        assert_eq!(args_parse_count(&view, idx), 1, "height must flush");
        assert!(
            height > empty_height,
            "stale height survived the deferred args; got {height}, baseline {empty_height}"
        );
    }

    /// `is_final` forces the deferred parse so the completed args
    /// materialize without waiting for the next frame.
    #[test]
    fn test_flush_tool_args_by_index_parses_eagerly() {
        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new_streaming(
            TOOL_BASH.into(),
            "tc-final".into(),
        )));
        let idx = view.tool_call_index("tc-final").unwrap();

        view.append_tool_args_fragment_by_index(idx, r#"{"command":"ls -la"}"#);
        assert_eq!(args_parse_count(&view, idx), 0);

        view.flush_tool_args_by_index(idx);
        assert_eq!(args_parse_count(&view, idx), 1);
        if let ChatCell::ToolCall(block) = view.cells[idx].cell() {
            assert_eq!(block.tool_args["command"], "ls -la");
        } else {
            panic!("expected tool call cell");
        }

        // Idempotent: a cell with nothing pending is left untouched.
        view.flush_tool_args_by_index(idx);
        assert_eq!(args_parse_count(&view, idx), 1);
    }

    /// Parse counter of the deferred args path (the #98 acceptance seam).
    fn args_parse_count(view: &ChatView, index: usize) -> usize {
        match view.cells[index].cell() {
            ChatCell::ToolCall(block) => block.args_parse_count,
            _ => panic!("expected tool call cell"),
        }
    }

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
    fn test_discard_pending_keeps_unlisted_messages() {
        let mut view = ChatView::new();
        view.push_pending("req-1".into(), "dropped".into());
        view.push_pending("req-2".into(), "survives".into());

        view.discard_pending(&["req-1".to_string()]);

        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].request_id, "req-2");
        assert!(matches!(
            view.cells[0].cell(),
            ChatCell::DiscardedUserMessage(text) if text == "dropped"
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

        // 标题行在第 0 行；正文与参考渲染一致 —— 首行只差前缀
        // （`⦁ ` 归标题、正文拿 `  `），其余逐 span 相同。
        let reference =
            crate::render::markdown::stream::full_lines(text, 80, Profile::Thinking, &p);
        assert_eq!(finalized.len(), reference.len() + 1, "标题只多一行");
        assert!(
            finalized[0].to_string().contains("深度思考"),
            "标题应在第 0 行：{:?}",
            finalized[0]
        );
        let mut body = span_texts(&finalized[1..]);
        let mut want = span_texts(&reference);
        assert_eq!(body.remove(0), "  ", "正文首行拿续行前缀");
        assert_eq!(want.remove(0), "⦁ ", "参考渲染首行拿子弹");
        assert_eq!(body, want, "正文（前缀除外）与参考逐 span 相同");
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
        // visible lines come from the cell's own renderer (the collapsed
        // label) — the reasoning content must never leak, neither while
        // streaming nor after the turn-end finalize.
        let (p, l) = test_ctx();
        let ctx = CellContext {
            palette: &p,
            thinking_mode: ThinkingMode::Hidden,
            thinking_expanded: None,
            layout: &l,
            images: crate::render::markdown::ImageOpts::off(),
        };
        let mut view = ChatView::new();
        let started = Instant::now();
        view.append_to_last_thinking_at("SECRET-REASONING-CONTENT", started);
        view.tick_active_thinking(started + Duration::from_secs(3));
        let lines: Vec<Line<'static>> = view.cells[0].compute_lines(80, &ctx).to_vec();
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("深度思考中 3s"), "label missing: {text}");
        assert!(
            !text.contains("SECRET-REASONING"),
            "hidden reasoning leaked while streaming: {text}"
        );
        let h_streaming = view.cells[0].compute_height(80, &ctx);
        assert!(
            h_streaming <= 2,
            "collapsed label height wrong: {h_streaming}"
        );

        // Turn end: finalize must not install the visible full render, and the
        // timing freezes into the completed wording.
        view.finish_active_thinking(started + Duration::from_secs(9));
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
        assert!(text.contains("深度思考 9s"), "frozen duration lost: {text}");
        assert!(!text.contains("深度思考中"), "still says active: {text}");
        assert_eq!(view.cells[0].compute_height(80, &ctx), h_streaming);
    }

    #[test]
    fn toggle_flips_every_turn_and_survives_a_rebuild() {
        let (p, l) = test_ctx();
        let ctx = |explicit: Option<bool>| CellContext {
            palette: &p,
            thinking_mode: ThinkingMode::Hidden,
            thinking_expanded: explicit,
            layout: &l,
            images: crate::render::markdown::ImageOpts::off(),
        };
        let mut view = ChatView::new();
        // 上一回合的块 + 新回合的块。
        view.append_to_last_thinking_at("OLD-REASONING", Instant::now());
        view.finish_active_thinking(Instant::now());
        view.push(ChatCell::UserMessage("next".into()));
        view.append_to_last_thinking_at("NEW-REASONING", Instant::now());

        // 标题行在（默认 hidden），两块都不带正文。
        for index in [0usize, 2] {
            let text = view.cells[index]
                .compute_lines(80, &ctx(None))
                .iter()
                .map(|l| l.to_string())
                .collect::<String>();
            assert!(!text.contains("REASONING"), "{text}");
        }

        // Ctrl+O：**全局**翻转 —— 两轮一起展开（渲染期读同一个覆盖）。
        assert!(view.toggle_thinking_expansion(false), "有块就该翻转");
        assert_eq!(view.thinking_expansion(), Some(true));
        for index in [0usize, 2] {
            let text = view.cells[index]
                .compute_lines(80, &ctx(Some(true)))
                .iter()
                .map(|l| l.to_string())
                .collect::<String>();
            assert!(
                text.contains("REASONING"),
                "第 {index} 块应一并展开：{text}"
            );
        }

        // 切换之后新起的块天然跟随同一个模式（覆盖是渲染期读的）。
        view.append_to_last_thinking_at("LATER-REASONING", Instant::now());
        let later_index = *thinking_indices(&view).last().expect("新块在");
        let later = view.cells[later_index]
            .compute_lines(80, &ctx(Some(true)))
            .iter()
            .map(|l| l.to_string())
            .collect::<String>();
        assert!(
            later.contains("LATER-REASONING"),
            "新块应跟随已切换的模式：{later}"
        );

        // 会话内重建（resume / compaction / sync）：模式保留。
        view.clear();
        assert_eq!(view.thinking_expansion(), Some(true), "重建后模式应保留");
    }

    #[test]
    fn freeze_is_self_healing_when_the_cell_order_shifts() {
        // 回归（审查 B）：锚定插入 / 面板开合 / Ask 卡片会把活跃块从"最后
        // 一块"挤走 —— 冻结与 tick 不许依赖缓存下标。
        let (p, l) = test_ctx();
        let ctx = CellContext {
            palette: &p,
            thinking_mode: ThinkingMode::Hidden,
            thinking_expanded: None,
            layout: &l,
            images: crate::render::markdown::ImageOpts::off(),
        };
        let started = Instant::now();

        // 1) 锚定插入：Diff cell 插在活跃块之前（工具调用锚定）。
        let mut view = ChatView::new();
        let mut call = crate::ui::cells::tool_call::ToolCallBlock::new(
            "Read".into(),
            serde_json::json!({ "file_path": "/tmp/x" }),
            "call-1".into(),
        );
        call.status = crate::ui::cells::tool_call::ToolStatus::Success;
        view.push(ChatCell::ToolCall(call));
        view.append_to_last_thinking_at("reasoning", started);
        view.insert_after_tool_call(
            "call-1",
            ChatCell::Diff(crate::ui::cells::diff_view::DiffView::new(
                "x".into(),
                Some("a\n".into()),
                "b\n".into(),
                1,
                1,
            )),
        )
        .expect("锚点存在");
        // 活跃块现在不是最后一块也不是原位 —— 冻结仍然要找到它。
        view.finish_active_thinking(started + Duration::from_secs(5));
        let [index] = thinking_indices(&view)[..] else {
            panic!("fixture 里应只有一个思考块")
        };
        let text = render_text(&mut view, index, 80, &ctx);
        assert!(
            text.contains("深度思考 5s"),
            "锚定插入后冻结失效（还挂在进行中）：{text}"
        );
        assert!(!text.contains("深度思考中"), "{text}");

        // 2) picker 开合：尾插 + 删除，活跃块被夹在中间。
        let mut view = ChatView::new();
        view.append_to_last_thinking_at("reasoning", started);
        view.show_model_picker(crate::shared::panels::picker::ModelPanel::new(
            vec![wing_api_client::models::ProviderModels {
                provider: "p".into(),
                models: vec!["m".into()],
                model_details: vec![],
            }],
            None,
        ));
        view.remove_model_picker();
        assert!(
            view.tick_active_thinking(started + Duration::from_secs(3)),
            "picker 开合后仍应 tick 到活跃块"
        );
        let [index] = thinking_indices(&view)[..] else {
            panic!("fixture 里应只有一个思考块")
        };
        let text = render_text(&mut view, index, 80, &ctx);
        assert!(text.contains("深度思考中 3s"), "{text}");

        // 3) Ask 卡片尾插把流劈开：后续 reasoning 开新块，旧块必须收尾。
        let mut view = ChatView::new();
        view.append_to_last_thinking_at("first round", started);
        view.push(ChatCell::SystemMessage("ask card".into()));
        view.append_to_last_thinking_at("second round", started + Duration::from_secs(7));
        let [old_index, new_index] = thinking_indices(&view)[..] else {
            panic!("应有两段推理（被 ask 卡片劈开）")
        };
        let old_text = render_text(&mut view, old_index, 80, &ctx);
        let new_text = render_text(&mut view, new_index, 80, &ctx);
        assert!(
            old_text.contains("深度思考 7s"),
            "被劈开的旧块应冻结（而不是永久思考中）：{old_text}"
        );
        assert!(
            new_text.contains("深度思考中"),
            "新块应在进行中：{new_text}"
        );
        assert!(
            !new_text.contains("深度思考 7s"),
            "新块不该继承旧块的时长：{new_text}"
        );

        // 4) 重连半截块：重放建出的块没有计时，首个 live delta 补上开始时刻。
        let mut view = ChatView::new();
        let mut replayed = ThinkingBlock::new();
        replayed.append("replayed reasoning");
        view.push(ChatCell::Thinking(replayed));
        view.append_to_last_thinking_at(" live delta", started);
        view.tick_active_thinking(started + Duration::from_secs(4));
        let [index] = thinking_indices(&view)[..] else {
            panic!("fixture 里应只有一个思考块")
        };
        let text = render_text(&mut view, index, 80, &ctx);
        assert!(
            text.contains("深度思考中 4s"),
            "重放半截块应在首个 live delta 上开始计时：{text}"
        );
    }

    /// 全部 Thinking cell 的下标（测试里现找，不缓存下标 —— 与实现同款约定）。
    fn thinking_indices(view: &ChatView) -> Vec<usize> {
        view.cells
            .iter()
            .enumerate()
            .filter(|(_, cached)| matches!(cached.cell(), ChatCell::Thinking(_)))
            .map(|(index, _)| index)
            .collect()
    }

    /// 把第 `index` 个 cell 渲染成文本（拼接所有行）。
    fn render_text(
        view: &mut ChatView,
        index: usize,
        width: u16,
        ctx: &crate::render::renderable::CellContext<'_>,
    ) -> String {
        view.cells[index]
            .compute_lines(width, ctx)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
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
}
