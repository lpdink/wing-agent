//! NDJSON message type definitions for stream-json output.
//!
//! These types define the NDJSON messages that the stdio renderer outputs.
//! They follow the Anthropic Messages API format for compatibility with
//! external consumers (e.g., Omnigent, harness).
//!
//! Naming: no "Claude" prefix — these are protocol-compatible format types.

use serde::{Deserialize, Serialize};

// ============================================================
// system/init message
// ============================================================

/// System initialization message (mapped from SessionInitEvent).
///
/// Backend event type is "session_init", NDJSON output maps to
/// type="system" + subtype="init".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInitMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub subtype: String,
    pub tools: Vec<String>,
    pub model: String,
    #[serde(rename = "permissionMode")]
    pub permission_mode: String,
    pub cwd: String,
    pub session_id: String,
    pub uuid: String,
}

// ============================================================
// assistant message
// ============================================================

/// Assistant turn message (mapped from AssistantTurnEvent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub message: AssistantMessageInner,
    pub parent_tool_use_id: Option<String>,
    pub session_id: String,
    pub uuid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageInner {
    pub id: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub role: String,
    pub model: String,
    pub content: Vec<MessageContent>,
    pub stop_reason: String,
    pub usage: serde_json::Value,
}

/// Content block in an assistant message.
///
/// Matches Anthropic Messages API content block types:
/// - thinking
/// - text
/// - tool_use
/// - Raw (fallback for unknown types)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Thinking {
        #[serde(rename = "type")]
        content_type: String,
        thinking: String,
        /// Required by Claude Agent SDK's ThinkingBlock parser.
        /// Non-Anthropic models don't produce signatures; emit empty string.
        #[serde(default)]
        signature: String,
    },
    Text {
        #[serde(rename = "type")]
        content_type: String,
        text: String,
    },
    ToolUse {
        #[serde(rename = "type")]
        content_type: String,
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Raw {
        #[serde(flatten)]
        raw: serde_json::Value,
    },
}

// ============================================================
// user message (tool result)
// ============================================================

/// User turn message — tool result (mapped from ToolResultTurnEvent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub message: UserMessageInner,
    pub parent_tool_use_id: Option<String>,
    pub tool_use_result: ToolUseResult,
    pub session_id: String,
    pub uuid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessageInner {
    pub role: String,
    pub content: Vec<ToolResultContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseResult {
    pub tool_use_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

// ============================================================
// result message
// ============================================================

/// Final result message (mapped from TurnResultEvent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub subtype: String,
    pub is_error: bool,
    pub result: String,
    pub duration_ms: i64,
    pub duration_api_ms: i64,
    pub num_turns: i64,
    pub total_cost_usd: f64,
    pub usage: serde_json::Value,
    pub session_id: String,
    pub uuid: String,
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_init_serializes_to_valid_json() {
        let msg = SystemInitMessage {
            msg_type: "system".into(),
            subtype: "init".into(),
            tools: vec!["Read".into(), "Write".into(), "Edit".into(), "Bash".into()],
            model: "qwen-max".into(),
            permission_mode: "bypassPermissions".into(),
            cwd: "/home/user/project".into(),
            session_id: "sess-123".into(),
            uuid: "abc123def456".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "system");
        assert_eq!(parsed["subtype"], "init");
        assert_eq!(parsed["permissionMode"], "bypassPermissions");
        assert_eq!(parsed["model"], "qwen-max");
        assert_eq!(parsed["tools"].as_array().unwrap().len(), 4);
        assert_eq!(parsed["session_id"], "sess-123");
        assert_eq!(parsed["uuid"], "abc123def456");
    }

    #[test]
    fn assistant_message_serializes_to_valid_json() {
        let msg = AssistantMessage {
            msg_type: "assistant".into(),
            message: AssistantMessageInner {
                id: "msg_abc123".into(),
                msg_type: "message".into(),
                role: "assistant".into(),
                model: "qwen-max".into(),
                content: vec![MessageContent::Text {
                    content_type: "text".into(),
                    text: "Hello!".into(),
                }],
                stop_reason: "end_turn".into(),
                usage: serde_json::json!({"input_tokens": 10, "output_tokens": 5, "cached_tokens": 0}),
            },
            parent_tool_use_id: None,
            session_id: "sess-123".into(),
            uuid: "uuid-456".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "assistant");
        assert_eq!(parsed["message"]["id"], "msg_abc123");
        assert_eq!(parsed["message"]["role"], "assistant");
        assert_eq!(parsed["message"]["content"][0]["type"], "text");
        assert_eq!(parsed["message"]["content"][0]["text"], "Hello!");
        assert_eq!(parsed["session_id"], "sess-123");
        assert_eq!(parsed["uuid"], "uuid-456");
    }

    #[test]
    fn user_message_serializes_to_valid_json() {
        let msg = UserMessage {
            msg_type: "user".into(),
            message: UserMessageInner {
                role: "user".into(),
                content: vec![ToolResultContent {
                    content_type: "tool_result".into(),
                    tool_use_id: "call_abc".into(),
                    content: "file contents here".into(),
                    is_error: false,
                }],
            },
            parent_tool_use_id: None,
            tool_use_result: ToolUseResult {
                tool_use_id: "call_abc".into(),
                tool_name: "Read".into(),
                content: "file contents here".into(),
                is_error: false,
            },
            session_id: "sess-123".into(),
            uuid: "uuid-789".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["content"][0]["type"], "tool_result");
        assert_eq!(parsed["tool_use_result"]["tool_name"], "Read");
        assert_eq!(parsed["tool_use_result"]["is_error"], false);
        assert_eq!(parsed["session_id"], "sess-123");
        assert_eq!(parsed["uuid"], "uuid-789");
    }

    #[test]
    fn result_message_serializes_to_valid_json() {
        let msg = ResultMessage {
            msg_type: "result".into(),
            subtype: "success".into(),
            is_error: false,
            result: "Done!".into(),
            duration_ms: 5000,
            duration_api_ms: 3000,
            num_turns: 3,
            total_cost_usd: 0.0,
            usage: serde_json::json!({"input_tokens": 100, "output_tokens": 50, "cached_tokens": 20}),
            session_id: "sess-123".into(),
            uuid: "uuid-result".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "result");
        assert_eq!(parsed["subtype"], "success");
        assert_eq!(parsed["is_error"], false);
        assert_eq!(parsed["result"], "Done!");
        assert_eq!(parsed["duration_ms"], 5000);
        assert_eq!(parsed["duration_api_ms"], 3000);
        assert_eq!(parsed["num_turns"], 3);
        assert_eq!(parsed["total_cost_usd"], 0.0);
        assert_eq!(parsed["session_id"], "sess-123");
        assert_eq!(parsed["uuid"], "uuid-result");
    }

    #[test]
    fn all_messages_produce_single_line_json() {
        // NDJSON requires each message to be a single JSON line (no embedded newlines).
        let init = SystemInitMessage {
            msg_type: "system".into(),
            subtype: "init".into(),
            tools: vec![],
            model: "m".into(),
            permission_mode: "default".into(),
            cwd: "".into(),
            session_id: "s".into(),
            uuid: "u".into(),
        };
        let result = ResultMessage {
            msg_type: "result".into(),
            subtype: "success".into(),
            is_error: false,
            result: "".into(),
            duration_ms: 0,
            duration_api_ms: 0,
            num_turns: 0,
            total_cost_usd: 0.0,
            usage: serde_json::json!({}),
            session_id: "s".into(),
            uuid: "u".into(),
        };
        let init_json = serde_json::to_string(&init).unwrap();
        let result_json = serde_json::to_string(&result).unwrap();
        assert!(!init_json.contains('\n'));
        assert!(!result_json.contains('\n'));
    }
}
