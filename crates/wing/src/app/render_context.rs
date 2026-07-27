//! RenderContext — per-turn tracking of streaming target cells.
//!
//! Tracks which cell is the current streaming target for assistant text
//! and thinking content. Tool call cells are deliberately NOT tracked
//! here — they are addressed by tool_call_id via `ChatView::tool_call_index`
//! at each event. Indices must never be cached across handler frames:
//! `ChatView::insert_after_tool_call` moves cells around, so any stored
//! index would silently go stale. The positional fields below are safe
//! because they point at assistant/thinking cells, which always precede
//! tool call cells (anchored insertion only happens after tool calls),
//! and they are cleared as soon as the tool phase begins.

/// Tracks the active rendering state within a single agent turn.
///
/// Reset on DoneEvent, InterruptedEvent, or SyncSessionEvent.
pub struct RenderContext {
    /// Index of the current streaming AssistantMessage cell (if any).
    pub current_assistant: Option<usize>,
    /// Index of the current streaming ThinkingBlock cell (if any).
    pub current_thinking: Option<usize>,
    /// Index of the cell that should receive usage metrics (LLMCallMetricsEvent).
    pub last_usage_target: Option<usize>,
}

impl RenderContext {
    pub fn new() -> Self {
        Self {
            current_assistant: None,
            current_thinking: None,
            last_usage_target: None,
        }
    }

    /// Reset all tracking state (new turn).
    pub fn reset(&mut self) {
        self.current_assistant = None;
        self.current_thinking = None;
        self.last_usage_target = None;
    }
}

impl Default for RenderContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_context_reset() {
        let mut ctx = RenderContext::new();
        ctx.current_assistant = Some(0);
        ctx.current_thinking = Some(1);
        ctx.last_usage_target = Some(3);

        ctx.reset();

        assert!(ctx.current_assistant.is_none());
        assert!(ctx.current_thinking.is_none());
        assert!(ctx.last_usage_target.is_none());
    }
}
