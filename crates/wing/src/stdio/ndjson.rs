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
///
/// Two shapes share this struct, mirroring the SDK's `SDKResultMessage` union:
/// - success (`subtype="success"`) carries `result`;
/// - error (`subtype="error_*"`, `is_error=true`) carries `errors` and no
///   `result` — `SDKResultError` has no such field.
///
/// `terminal_reason` is the SDK's `TerminalReason`: the interrupted terminal
/// frame uses `aborted_streaming` / `aborted_tools`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub subtype: String,
    pub is_error: bool,
    /// Final text of a successful turn. Absent on error results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub duration_ms: i64,
    pub duration_api_ms: i64,
    pub num_turns: i64,
    pub total_cost_usd: f64,
    pub usage: serde_json::Value,
    /// Why the query loop terminated (`TerminalReason` in `sdk.d.ts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    /// Error texts of a failed turn (`SDKResultError.errors`).
    ///
    /// **错误结果帧不得省略这个字段**：SDK 对 `is_error=true && subtype !=
    /// "success"` 的帧无条件读它（0.3.165 `e.errors.join("; ")` / 0.3.291
    /// `e.errors.map(…)`），缺字段直接抛 `TypeError`，把后端错误原文换成 JS
    /// 内部错误。渲染器为此保证：`is_error` 为真时该数组非空（后端没给文本
    /// 就补通用文案）——`skip_serializing_if` 只是让成功帧不带它。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
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
            result: Some("Done!".into()),
            duration_ms: 5000,
            duration_api_ms: 3000,
            num_turns: 3,
            total_cost_usd: 0.0,
            usage: serde_json::json!({"input_tokens": 100, "output_tokens": 50, "cached_tokens": 20}),
            terminal_reason: None,
            errors: Vec::new(),
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
        // 成功形状不因新增字段漂移：terminal_reason / errors 缺省即不出现。
        assert!(parsed.get("terminal_reason").is_none(), "{parsed}");
        assert!(parsed.get("errors").is_none(), "{parsed}");
    }

    #[test]
    fn error_result_shape_matches_sdk_result_error() {
        // 错误结果按 `SDKResultError` 出帧：无 `result`，有 `errors` 与
        // `terminal_reason`（被打断的轮次用它收尾）。
        let msg = ResultMessage {
            msg_type: "result".into(),
            subtype: "error_during_execution".into(),
            is_error: true,
            result: None,
            duration_ms: 1200,
            duration_api_ms: 900,
            num_turns: 1,
            total_cost_usd: 0.0,
            usage: serde_json::json!({"input_tokens": 1, "output_tokens": 2, "cached_tokens": 0}),
            terminal_reason: Some("aborted_streaming".into()),
            errors: vec!["Interrupted by user".into()],
            session_id: "sess-123".into(),
            uuid: "uuid-aborted".into(),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(parsed["type"], "result");
        assert_eq!(parsed["subtype"], "error_during_execution");
        assert_eq!(parsed["is_error"], true);
        assert_eq!(parsed["terminal_reason"], "aborted_streaming");
        assert_eq!(parsed["errors"], serde_json::json!(["Interrupted by user"]));
        assert!(parsed.get("result").is_none(), "{parsed}");
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
            result: Some("".into()),
            duration_ms: 0,
            duration_api_ms: 0,
            num_turns: 0,
            total_cost_usd: 0.0,
            usage: serde_json::json!({}),
            terminal_reason: None,
            errors: Vec::new(),
            session_id: "s".into(),
            uuid: "u".into(),
        };
        let init_json = serde_json::to_string(&init).unwrap();
        let result_json = serde_json::to_string(&result).unwrap();
        assert!(!init_json.contains('\n'));
        assert!(!result_json.contains('\n'));
    }
}
