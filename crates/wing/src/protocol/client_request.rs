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
}

impl ClientRequest {
    /// Create a normal (non-silent) message request.
    pub fn message(session_id: &str, content: &str) -> Self {
        Self {
            request_id: super::generate_request_id(),
            session_id: session_id.to_string(),
            content: content.to_string(),
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
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"silent\""));
        assert!(json.contains("\"content\":\"hello\""));
    }
}
