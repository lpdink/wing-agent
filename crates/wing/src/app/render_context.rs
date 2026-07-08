//! RenderContext — tracks the current turn's active cells.
//!
//! Maps tool_call_ids to cell indices so ToolCallResultEvent can find
//! the corresponding ToolCallBlock. Tracks which cell is the current
//! streaming target for assistant text and thinking content.

use std::collections::HashMap;

/// Tracks the active rendering state within a single agent turn.
///
/// Reset on DoneEvent, InterruptedEvent, or SyncSessionEvent.
pub struct RenderContext {
    /// Index of the current streaming AssistantMessage cell (if any).
    pub current_assistant: Option<usize>,
    /// Index of the current streaming ThinkingBlock cell (if any).
    pub current_thinking: Option<usize>,
    /// Map from tool_call_id → cell index in ChatView.cells.
    pub tool_call_indices: HashMap<String, usize>,
    /// Index of the cell that should receive usage metrics (LLMCallMetricsEvent).
    pub last_usage_target: Option<usize>,
}

impl RenderContext {
    pub fn new() -> Self {
        Self {
            current_assistant: None,
            current_thinking: None,
            tool_call_indices: HashMap::new(),
            last_usage_target: None,
        }
    }

    /// Reset all tracking state (new turn).
    pub fn reset(&mut self) {
        self.current_assistant = None;
        self.current_thinking = None;
        self.tool_call_indices.clear();
        self.last_usage_target = None;
    }

    /// Register a tool call ID with its cell index.
    pub fn register_tool_call(&mut self, tool_call_id: String, index: usize) {
        self.tool_call_indices.insert(tool_call_id, index);
    }

    /// Look up a tool call's cell index.
    pub fn get_tool_call_index(&self, tool_call_id: &str) -> Option<usize> {
        self.tool_call_indices.get(tool_call_id).copied()
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
    fn test_render_context_tool_call_tracking() {
        let mut ctx = RenderContext::new();
        ctx.register_tool_call("tc1".into(), 5);
        ctx.register_tool_call("tc2".into(), 8);

        assert_eq!(ctx.get_tool_call_index("tc1"), Some(5));
        assert_eq!(ctx.get_tool_call_index("tc2"), Some(8));
        assert_eq!(ctx.get_tool_call_index("nonexistent"), None);
    }

    #[test]
    fn test_render_context_reset() {
        let mut ctx = RenderContext::new();
        ctx.current_assistant = Some(0);
        ctx.current_thinking = Some(1);
        ctx.register_tool_call("tc1".into(), 2);
        ctx.last_usage_target = Some(3);

        ctx.reset();

        assert!(ctx.current_assistant.is_none());
        assert!(ctx.current_thinking.is_none());
        assert!(ctx.tool_call_indices.is_empty());
        assert!(ctx.last_usage_target.is_none());
    }
}
