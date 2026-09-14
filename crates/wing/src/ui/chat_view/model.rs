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

use crate::app::ask_panel::AskPanel;
use crate::app::constants::TOOL_BASH;
use crate::app::model_panel::ModelPanel;
use crate::ui::cached_cell::CachedCell;
use crate::ui::cells::thinking::ThinkingBlock;
use crate::ui::cells::tool_call::ToolStatus;

use super::ChatCell;
use super::ChatView;
use super::PendingMessage;

impl ChatView {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;
    use crate::render::markdown::stream::Profile;
    use crate::render::renderable::CellContext;
    use std::time::Instant;

    use super::super::test_support::{buffer_text, make_ctx, render_view, span_texts, test_ctx};
    use crate::ui::cells::tool_call::ToolCallBlock;

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
}
