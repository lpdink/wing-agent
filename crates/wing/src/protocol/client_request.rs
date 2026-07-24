//! ClientRequest — messages the TUI sends to Gateway.
//!
//! Mirrors: wing_gateway/protocol.py ClientRequest

use serde::Deserialize;
use serde::Serialize;

/// Request sent from TUI → Gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRequest {
    /// Correlation id for the request.
    pub request_id: String,
    /// Target session id.
    pub session_id: String,
    /// Message content or magic command.
    pub content: String,
    /// When replying to an Ask event, its tool_call_id — routes the message
    /// to the matching feedback waiter instead of the session inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ClientRequest {
    /// Create a normal (non-silent) message request.
    pub fn message(session_id: &str, content: &str, tool_call_id: Option<String>) -> Self {
        Self {
            request_id: super::generate_request_id(),
            session_id: session_id.to_string(),
            content: content.to_string(),
            tool_call_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_message_request() {
        let req = ClientRequest {
            request_id: "abc".into(),
            session_id: "sid".into(),
            content: "hello".into(),
            tool_call_id: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"silent\""));
        assert!(json.contains("\"content\":\"hello\""));
        // None tool_call_id is omitted from the payload.
        assert!(!json.contains("tool_call_id"));

        let req = ClientRequest::message("sid", "hi", Some("tc_1".into()));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"tool_call_id\":\"tc_1\""));
    }
}
