//! WingEvent enum and all associated types.
//!
//! Mirrors:
//!   - wing/event/base.py
//!   - wing/event/react.py
//!   - wing/event/state_change.py
//!   - wing/event/query_response.py

use serde::Deserialize;
use serde::Serialize;

// ============================================================
// Shared / nested types
// ============================================================

/// Common metadata present on every event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMeta {
    /// UTC ISO-8601 timestamp string.
    pub created_at: String,
    /// Session this event belongs to (may be absent for global events).
    pub session_id: Option<String>,
    /// Unique request correlation id.
    pub request_id: String,
}

/// Routing target injected by EventBus — the TUI can ignore this but must
/// tolerate it in the JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTarget {
    pub scope: String,
    #[serde(default)]
    pub client_ids: Vec<String>,
}

/// Agent configuration snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub model_name: String,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    pub workspace: Option<String>,
}

/// A single question in a multi-question Ask event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub choices: Vec<String>,
}

/// Magic command metadata for command list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: String,
}

/// Session summary info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub created_at: Option<String>,
    pub template_name: Option<String>,
    pub workspace: Option<String>,
    pub last_interaction: Option<String>,
}

/// Branch target for /rewind and /fork.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTargetInfo {
    pub uuid: String,
    pub content: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "user".into()
}

// ============================================================
// WingEvent — the main event enum
// ============================================================

/// All events from wing Gateway, discriminated by the `type` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WingEvent {
    // ---- base ----
    /// Error event.
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default = "default_status_code")]
        status_code: i32,
        error_code: Option<String>,
        detail: Option<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Request delivered confirmation.
    #[serde(rename = "delivered")]
    Delivered {
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- react ----
    /// Assistant text output (streaming chunks).
    #[serde(rename = "text")]
    Text {
        content: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Reasoning / thinking output.
    #[serde(rename = "reasoning")]
    Reasoning {
        content: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Tool call initiated.
    #[serde(rename = "tool_call")]
    ToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Streaming tool call args fragment (during LLM generation).
    ///
    /// Carries the incremental raw args text emitted since the last event
    /// for this call (the first event carries the full prefix accumulated
    /// so far). Clients accumulate fragments into a buffer and parse it
    /// locally for rendering — the backend never parses partial args.
    /// `ToolCall` (authoritative parsed args) follows at execution start.
    #[serde(rename = "tool_call_stream")]
    ToolCallStream {
        tool_call_id: String,
        tool_name: String,
        #[serde(default)]
        args_fragment: String,
        #[serde(default)]
        is_final: bool,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Tool call result.
    #[serde(rename = "tool_call_result")]
    ToolCallResult {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        tool_result: String,
        tool_success: bool,
        #[serde(default)]
        model: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// LLM call metrics.
    #[serde(rename = "llm_call_metrics")]
    LlmCallMetrics {
        #[serde(default)]
        model: String,
        prompt_tokens: i64,
        completion_tokens: i64,
        cached_tokens: i64,
        first_chunk_rt_ms: f64,
        tokens_per_sec: f64,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Agent asks the user a question.
    #[serde(rename = "ask")]
    Ask {
        /// Correlation id — echo back via send_message to resolve this ask's
        /// feedback waiter (distinguishes replies under concurrent asks).
        #[serde(default)]
        tool_call_id: String,
        /// Multi-question format (AskUserQuestion tool).
        #[serde(default)]
        questions: Vec<AskQuestion>,
        /// Legacy single-question (Bash dangerous command confirmation).
        #[serde(default)]
        question: String,
        #[serde(default)]
        choices: Vec<String>,
        /// When true, the user must pick from choices (selection menu).
        #[serde(default)]
        required: bool,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Turn completed.
    #[serde(rename = "done")]
    Done {
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Agent turn started — emitted when the agent begins processing a message.
    /// Unlike Delivered (transport ack), this only fires for actual agent turns,
    /// not magic commands.
    #[serde(rename = "turn_started")]
    TurnStarted {
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Diff content for file edits.
    #[serde(rename = "diff_content")]
    DiffContent {
        path: String,
        old_text: Option<String>,
        new_text: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- state_change ----
    /// Full session state sync.
    #[serde(rename = "sync_session")]
    SyncSession {
        session_id: String,
        #[serde(default)]
        messages: Vec<serde_json::Value>,
        agent: Option<AgentInfo>,
        name: Option<String>,
        draft: Option<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Session state changed — unified event for model/thinking/yolo/title/agent updates.
    #[serde(rename = "session_state_changed")]
    SessionStateChanged {
        model: Option<String>,
        thinking: Option<bool>,
        reasoning_effort: Option<String>,
        yolo: Option<bool>,
        title: Option<String>,
        agent: Option<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Agent interrupted.
    #[serde(rename = "interrupted")]
    Interrupted {
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Context compaction completed.
    #[serde(rename = "compact_done")]
    CompactDone {
        original_tokens: i64,
        compressed_tokens: i64,
        #[serde(default)]
        model: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- query_response ----
    /// Context usage statistics.
    #[serde(rename = "context_stats")]
    ContextStats {
        message_count: i64,
        total_tokens: i64,
        #[serde(default)]
        context_window_tokens: i64,
        #[serde(default)]
        system_prompt_parts: Vec<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Branch targets for /rewind and /fork.
    #[serde(rename = "branch_targets")]
    BranchTargets {
        #[serde(default)]
        targets: Vec<BranchTargetInfo>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- turn-level events (for stdio / SDK consumers) ----
    /// Turn-level assistant message (complete, not streaming).
    #[serde(rename = "assistant_turn")]
    AssistantTurn {
        #[serde(default)]
        uuid: String,
        content_blocks: Vec<serde_json::Value>,
        #[serde(default)]
        model: String,
        stop_reason: Option<String>,
        usage: Option<serde_json::Value>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Turn-level tool result.
    #[serde(rename = "tool_result_turn")]
    ToolResultTurn {
        #[serde(default)]
        uuid: String,
        tool_use_id: String,
        tool_name: String,
        content: String,
        #[serde(default)]
        is_error: bool,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Turn-level final result of the entire agent loop.
    #[serde(rename = "turn_result")]
    TurnResult {
        #[serde(default)]
        uuid: String,
        #[serde(default = "default_subtype")]
        subtype: String,
        #[serde(default)]
        is_error: bool,
        result: Option<String>,
        #[serde(default)]
        num_turns: i64,
        #[serde(default)]
        duration_ms: i64,
        usage: Option<serde_json::Value>,
        #[serde(default)]
        errors: Vec<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- session init (for stdio mode) ----
    /// Session initialization event — emitted on subscribe/fork.
    /// Carries authoritative session state for stdio consumers.
    #[serde(rename = "session_init")]
    SessionInit {
        #[serde(default)]
        uuid: String,
        #[serde(default)]
        tools: Vec<String>,
        #[serde(default)]
        model: String,
        #[serde(default = "default_permission_mode")]
        permission_mode: String,
        #[serde(default)]
        cwd: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Catch-all for unknown event types — prevents deserialization failures
    /// when wing adds new event types.
    #[serde(other)]
    Unknown,
}

fn default_status_code() -> i32 {
    500
}

fn default_subtype() -> String {
    "success".into()
}

fn default_permission_mode() -> String {
    "default".into()
}

impl WingEvent {
    /// Returns the `type` discriminator string for this event.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Error { .. } => "error",
            Self::Delivered { .. } => "delivered",
            Self::Text { .. } => "text",
            Self::Reasoning { .. } => "reasoning",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolCallStream { .. } => "tool_call_stream",
            Self::ToolCallResult { .. } => "tool_call_result",
            Self::LlmCallMetrics { .. } => "llm_call_metrics",
            Self::Ask { .. } => "ask",
            Self::Done { .. } => "done",
            Self::TurnStarted { .. } => "turn_started",
            Self::DiffContent { .. } => "diff_content",
            Self::SyncSession { .. } => "sync_session",
            Self::SessionStateChanged { .. } => "session_state_changed",
            Self::Interrupted { .. } => "interrupted",
            Self::CompactDone { .. } => "compact_done",
            Self::ContextStats { .. } => "context_stats",
            Self::BranchTargets { .. } => "branch_targets",
            Self::AssistantTurn { .. } => "assistant_turn",
            Self::ToolResultTurn { .. } => "tool_result_turn",
            Self::TurnResult { .. } => "turn_result",
            Self::SessionInit { .. } => "session_init",
            Self::Unknown => "unknown",
        }
    }

    /// Returns a reference to the event metadata, if this is a known variant.
    pub fn meta(&self) -> Option<&EventMeta> {
        match self {
            Self::Error { meta, .. }
            | Self::Delivered { meta, .. }
            | Self::Text { meta, .. }
            | Self::Reasoning { meta, .. }
            | Self::ToolCall { meta, .. }
            | Self::ToolCallStream { meta, .. }
            | Self::ToolCallResult { meta, .. }
            | Self::LlmCallMetrics { meta, .. }
            | Self::Ask { meta, .. }
            | Self::Done { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::DiffContent { meta, .. }
            | Self::SyncSession { meta, .. }
            | Self::SessionStateChanged { meta, .. }
            | Self::Interrupted { meta, .. }
            | Self::CompactDone { meta, .. }
            | Self::ContextStats { meta, .. }
            | Self::BranchTargets { meta, .. }
            | Self::AssistantTurn { meta, .. }
            | Self::ToolResultTurn { meta, .. }
            | Self::TurnResult { meta, .. }
            | Self::SessionInit { meta, .. } => Some(meta),
            Self::Unknown => None,
        }
    }

    /// Returns the session_id from the event meta, if present.
    pub fn session_id(&self) -> Option<&str> {
        self.meta().and_then(|m| m.session_id.as_deref())
    }

    /// Returns the request_id from the event meta.
    pub fn request_id(&self) -> Option<&str> {
        self.meta().map(|m| m.request_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_text_event() {
        let json = r#"{
            "type": "text",
            "content": "Hello world",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req1"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::Text { ref content, .. } if content == "Hello world"));
        assert_eq!(event.event_type(), "text");
        assert_eq!(event.session_id(), Some("abc123"));
    }

    #[test]
    fn deserialize_tool_call_event() {
        let json = r#"{
            "type": "tool_call",
            "tool_name": "Bash",
            "tool_args": {"command": "ls"},
            "tool_call_id": "tc_1",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req2"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::ToolCall { ref tool_name, .. } if tool_name == "Bash"));
    }

    #[test]
    fn deserialize_session_state_changed_event() {
        let json = r#"{
            "type": "session_state_changed",
            "model": "gpt-4o",
            "thinking": true,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req3"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::SessionStateChanged {
                model,
                thinking,
                yolo,
                title,
                agent,
                ..
            } => {
                assert_eq!(model, Some("gpt-4o".to_string()));
                assert_eq!(thinking, Some(true));
                assert_eq!(yolo, None);
                assert_eq!(title, None);
                assert_eq!(agent, None);
            }
            _ => panic!("expected SessionStateChanged"),
        }
    }

    #[test]
    fn deserialize_unknown_event() {
        let json = r#"{
            "type": "some_future_event",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req4"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::Unknown));
        assert_eq!(event.event_type(), "unknown");
    }

    #[test]
    fn deserialize_delivered_event() {
        let json = r#"{
            "type": "delivered",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req5"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::Delivered { .. }));
    }

    #[test]
    fn deserialize_error_event_default_status() {
        let json = r#"{
            "type": "error",
            "message": "something broke",
            "created_at": "2025-01-01T00:00:00",
            "session_id": null,
            "request_id": "req6"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Error {
                status_code,
                message,
                ..
            } => {
                assert_eq!(status_code, 500);
                assert_eq!(message, "something broke");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn deserialize_diff_content_event() {
        let json = r#"{
            "type": "diff_content",
            "path": "src/main.rs",
            "old_text": "fn old() {}",
            "new_text": "fn new() {}",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req8"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::DiffContent {
                path,
                old_text,
                new_text,
                ..
            } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(old_text.unwrap(), "fn old() {}");
                assert_eq!(new_text, "fn new() {}");
            }
            _ => panic!("expected DiffContent"),
        }
    }

    #[test]
    fn deserialize_context_stats_event() {
        let json = r#"{
            "type": "context_stats",
            "message_count": 10,
            "total_tokens": 5000,
            "context_window_tokens": 80000,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req9"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::ContextStats {
                total_tokens,
                context_window_tokens,
                ..
            } => {
                assert_eq!(total_tokens, 5000);
                assert_eq!(context_window_tokens, 80000);
            }
            _ => panic!("expected ContextStats"),
        }
    }

    #[test]
    fn deserialize_tool_call_stream_event() {
        let json = r#"{
            "type": "tool_call_stream",
            "tool_call_id": "tc_stream_1",
            "tool_name": "Bash",
            "args_fragment": "{\"command\": \"ls\"}",
            "is_final": false,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req10"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), "tool_call_stream");
        match event {
            WingEvent::ToolCallStream {
                tool_call_id,
                tool_name,
                args_fragment,
                is_final,
                ..
            } => {
                assert_eq!(tool_call_id, "tc_stream_1");
                assert_eq!(tool_name, "Bash");
                assert_eq!(args_fragment, "{\"command\": \"ls\"}");
                assert!(!is_final);
            }
            _ => panic!("expected ToolCallStream"),
        }
    }

    #[test]
    fn deserialize_tool_call_stream_defaults() {
        let json = r#"{
            "type": "tool_call_stream",
            "tool_call_id": "tc_2",
            "tool_name": "Read",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req11"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::ToolCallStream {
                args_fragment,
                is_final,
                ..
            } => {
                assert_eq!(args_fragment, "");
                assert!(!is_final);
            }
            _ => panic!("expected ToolCallStream"),
        }
    }
}
