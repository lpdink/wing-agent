//! Session-history record types — the typed mirror of the Message projection.
//!
//! Mirrors `wing/session.py::serialize_message`: the per-message dict the
//! backend sends as `SyncSessionEvent.messages` / `SyncSessionEvent.uncommitted`
//! and as `GET /api/session/get` → `messages`. Fact-event nodes are not
//! records — they decode through [`crate::protocol::WingEvent`]
//! ([`WingEvent::from_history_value`]).
//!
//! Decoding rules are deliberately the same as the decoders this module
//! replaced (in-place shadow structs / raw `Value` sniffing): missing optional
//! fields default, `null` optional fields count as absent, unknown fields are
//! ignored (forward tolerant), and a payload that is not a Message projection
//! fails to decode — callers skip it.
//!
//! Decode-only on purpose: serializing a mirror would drop unknown fields and
//! change key order, which would break `wing tail --json` (it emits the raw
//! payloads verbatim).

use serde::Deserialize;

/// One tool call on a history message.
///
/// Mirrors the projection's `tool_calls[]` entries (`{id, name, arguments}`).
/// `arguments` keeps "absent" ([`None`]) apart from "explicit null"
/// (`Some(Value::Null)`): replay renders both as null, while `wing tail`
/// prints `()` vs `(null)`.
///
/// Decode-only on purpose — serializing this mirror would drop unknown fields
/// and change key order, which would break `wing tail --json` (it emits the
/// raw payloads).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionToolCall {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "optional_value")]
    pub arguments: Option<serde_json::Value>,
}

/// Deserialize an optional value while keeping an explicit `null` (`Some(Value::Null)`)
/// apart from an absent key (`None` — never reaches this function).
///
/// Serde's plain `Option<Value>` would collapse both to `None`.
fn optional_value<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// A Message record of the session history (the frontend replay projection).
///
/// Decode-only (`SessionMessage::from_json`); see [`SessionToolCall`] for why
/// there is no `Serialize` mirror.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionMessage {
    /// `user` | `assistant` | `tool` | `system`; empty when the key is absent.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// `null` / absent → `None`; `[]` → `Some(vec![])` (not a tool call, see
    /// [`SessionMessage::has_tool_calls`]).
    #[serde(default)]
    pub tool_calls: Option<Vec<SessionToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl SessionMessage {
    /// Decode one history payload — the single entry point for the Message
    /// projection (TUI replay, `wing tail/head`, `wing wait`).
    ///
    /// The projection is always a JSON object; sequences are rejected here
    /// because the derived decoder would otherwise read them positionally.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "history payload is not a JSON object",
            ));
        }
        serde_json::from_value(value.clone())
    }

    /// Tool calls as a slice; absent / `null` / empty list → `&[]`.
    pub fn tool_calls(&self) -> &[SessionToolCall] {
        self.tool_calls.as_deref().unwrap_or(&[])
    }

    /// Whether the message carries at least one tool call.
    ///
    /// Absent, `null` and empty list all count as none — mirrors the old
    /// sniffing rule (`tool_calls` must be a non-empty array).
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_full_projection() {
        let msg = SessionMessage::from_json(&json!({
            "role": "assistant",
            "content": "the answer",
            "uuid": "u1",
            "reasoning_content": "thinking",
            "tool_calls": [
                {"id": "tc1", "name": "Bash", "arguments": {"command": "ls"}}
            ],
            "tool_call_id": null
        }))
        .unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "the answer");
        assert_eq!(msg.uuid.as_deref(), Some("u1"));
        assert_eq!(msg.reasoning_content.as_deref(), Some("thinking"));
        assert!(msg.has_tool_calls());
        let tc = &msg.tool_calls()[0];
        assert_eq!(tc.id, "tc1");
        assert_eq!(tc.name, "Bash");
        assert_eq!(tc.arguments, Some(json!({"command": "ls"})));
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn missing_fields_default() {
        // The projection omits reasoning_content / tool_calls / tool_call_id
        // when falsy — every one of them must default rather than fail.
        let msg = SessionMessage::from_json(&json!({"role": "assistant"})).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "");
        assert!(msg.uuid.is_none());
        assert!(msg.reasoning_content.is_none());
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_call_id.is_none());
        assert!(!msg.has_tool_calls());
    }

    #[test]
    fn absent_role_defaults_to_empty() {
        // Legacy behavior: a payload without `role` renders as `[unknown]`
        // (CLI) / is skipped (replay) — it must decode, not fail.
        let msg = SessionMessage::from_json(&json!({"content": "hi"})).unwrap();
        assert_eq!(msg.role, "");
        assert_eq!(msg.content, "hi");
    }

    #[test]
    fn empty_object_decodes_with_defaults() {
        let msg = SessionMessage::from_json(&json!({})).unwrap();
        assert_eq!(msg.role, "");
        assert_eq!(msg.content, "");
        assert!(!msg.has_tool_calls());
    }

    #[test]
    fn null_optionals_count_as_absent() {
        let msg = SessionMessage::from_json(&json!({
            "role": "assistant",
            "uuid": null,
            "reasoning_content": null,
            "tool_calls": null,
            "tool_call_id": null
        }))
        .unwrap();
        assert!(msg.uuid.is_none());
        assert!(msg.reasoning_content.is_none());
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_call_id.is_none());
        assert!(!msg.has_tool_calls());
    }

    #[test]
    fn null_content_is_not_tolerated() {
        // `content` is a required string in the projection (the backend emits
        // `msg.content or ""`), so `null` keeps the old decoder's behavior:
        // decode failure, the caller skips the record.
        assert!(SessionMessage::from_json(&json!({"role": "assistant", "content": null})).is_err());
    }

    #[test]
    fn empty_tool_calls_is_no_tool_call() {
        // Old sniffing required a *non-empty* array — an empty one is not a
        // tool call.
        let msg =
            SessionMessage::from_json(&json!({"role": "assistant", "tool_calls": []})).unwrap();
        assert!(msg.tool_calls.is_some());
        assert_eq!(msg.tool_calls(), &[] as &[SessionToolCall]);
        assert!(!msg.has_tool_calls());
    }

    #[test]
    fn arguments_keep_absent_apart_from_null() {
        // Replay renders both as null; `wing tail` prints `()` vs `(null)`.
        let absent = SessionMessage::from_json(&json!({
            "role": "assistant",
            "tool_calls": [{"id": "a", "name": "Bash"}]
        }))
        .unwrap();
        assert_eq!(absent.tool_calls()[0].arguments, None);

        let null = SessionMessage::from_json(&json!({
            "role": "assistant",
            "tool_calls": [{"id": "a", "name": "Bash", "arguments": null}]
        }))
        .unwrap();
        assert_eq!(
            null.tool_calls()[0].arguments,
            Some(serde_json::Value::Null)
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Forward tolerance: a projection gaining fields must not break the
        // mirror.
        let msg = SessionMessage::from_json(&json!({
            "role": "tool",
            "content": "ok",
            "tool_call_id": "tc1",
            "usage": {"input_tokens": 1},
            "stop_reason": "end_turn",
            "ts": "2026-01-01T00:00:00"
        }))
        .unwrap();
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.tool_call_id.as_deref(), Some("tc1"));
    }

    #[test]
    fn non_projection_payloads_fail() {
        // Not an object.
        assert!(SessionMessage::from_json(&json!(["role", "user"])).is_err());
        assert!(SessionMessage::from_json(&json!(42)).is_err());
        assert!(SessionMessage::from_json(&json!(null)).is_err());
        // Key field of the wrong type.
        assert!(SessionMessage::from_json(&json!({"role": 5})).is_err());
        assert!(SessionMessage::from_json(&json!({"role": "user", "content": 5})).is_err());
        assert!(
            SessionMessage::from_json(&json!({"role": "user", "reasoning_content": 5})).is_err()
        );
        assert!(
            SessionMessage::from_json(&json!({"role": "assistant", "tool_calls": {}})).is_err()
        );
        // Tool call missing its required id / name.
        assert!(
            SessionMessage::from_json(&json!({
                "role": "assistant",
                "tool_calls": [{"id": "a"}]
            }))
            .is_err()
        );
        assert!(
            SessionMessage::from_json(&json!({
                "role": "assistant",
                "tool_calls": [{"name": "Bash"}]
            }))
            .is_err()
        );
    }
}
