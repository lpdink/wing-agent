//! stdin handler for `--input-format stream-json` mode.
//!
//! Handles the Claude Agent SDK's NDJSON protocol over stdin:
//! 1. Reads `control_request` (initialize) and replies with `control_response`
//! 2. Reads `user` message and extracts the prompt text
//! 3. Ignores `keep_alive` and unknown message types
//!
//! The handler is sequentially blocking: initialize → user, in that order.
#![allow(clippy::print_stdout)]

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

// ============================================================
// Incoming message types (stdin → wing)
// ============================================================

/// Parsed stdin message from the SDK.
///
/// Uses explicit dispatch via [`StdinMessage::from_value`] rather than
/// `#[serde(untagged)]` on the enum, because `flatten` interacts poorly
/// with untagged enums. Each variant's inner struct uses standard
/// `#[derive(Deserialize)]` with `#[serde(flatten)]` for forward compat.
#[derive(Debug)]
pub enum StdinMessage {
    ControlRequest(ControlRequest),
    User(UserMessage),
    KeepAlive,
    Unknown,
}

impl StdinMessage {
    /// Parse a JSON value into a [`StdinMessage`].
    ///
    /// Dispatches on the `type` field, falling back to [`StdinMessage::Unknown`]
    /// for unrecognized types or malformed messages.
    pub fn from_value(value: serde_json::Value) -> Self {
        let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "control_request" => serde_json::from_value(value)
                .map(Self::ControlRequest)
                .unwrap_or(Self::Unknown),
            "user" => serde_json::from_value(value)
                .map(Self::User)
                .unwrap_or(Self::Unknown),
            "keep_alive" => Self::KeepAlive,
            _ => Self::Unknown,
        }
    }
}

/// SDK → wing: `control_request` message.
///
/// The SDK sends this for initialize handshake and potentially other control
/// operations. We reply with a `control_response` for every subtype to avoid
/// SDK-side timeouts.
#[derive(Debug, Deserialize)]
pub struct ControlRequest {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub request_id: String,
    /// The inner request payload (subtype: "initialize", etc.).
    /// Parsed as `Value` to absorb hooks/agents/skills fields we don't use.
    pub request: serde_json::Value,
    /// Absorb any additional fields the SDK may send.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// SDK → wing: `user` message carrying the prompt.
#[derive(Debug, Deserialize)]
pub struct UserMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub message: UserMessageBody,
    /// Absorb `session_id`, `parent_tool_use_id`, and other SDK fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The body of a user message: role + content.
#[derive(Debug, Deserialize)]
pub struct UserMessageBody {
    pub role: String,
    pub content: MessageContent,
}

/// `content` can be a plain string or an array of content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A single content block inside a user message.
///
/// Only `type == "text"` blocks are used; other types (image, etc.) are skipped.
#[derive(Debug, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
}

// ============================================================
// Outgoing message types (wing → stdout)
// ============================================================

/// wing → SDK: `control_response` message.
#[derive(Serialize)]
struct ControlResponse {
    #[serde(rename = "type")]
    msg_type: &'static str,
    response: ControlResponseBody,
}

#[derive(Serialize)]
struct ControlResponseBody {
    subtype: &'static str,
    request_id: String,
    response: serde_json::Value,
}

// ============================================================
// Main handler
// ============================================================

/// Read NDJSON from stdin, perform the initialize handshake, and extract
/// the user prompt.
///
/// This function is sequentially blocking:
/// 1. Wait for `control_request` (any subtype) → reply `control_response`
/// 2. Wait for `user` message → extract and return the prompt text
///
/// `keep_alive` and unknown message types are silently ignored.
/// Returns an error if stdin reaches EOF before a `user` message is received.
pub async fn handle_stdin_stream() -> Result<String> {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    handle_stdin_stream_with(reader, &mut stdout).await
}

/// Testable inner: reads NDJSON from `reader`, writes control_response to `writer`,
/// returns the user prompt.
pub(crate) async fn handle_stdin_stream_with<R, W>(reader: R, writer: &mut W) -> Result<String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse JSON; skip lines that aren't valid JSON.
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, line = %trimmed, "skipping non-JSON stdin line");
                continue;
            }
        };

        let msg = StdinMessage::from_value(value);

        match msg {
            StdinMessage::ControlRequest(req) => {
                let subtype = req
                    .request
                    .get("subtype")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown");

                if subtype != "initialize" {
                    tracing::warn!(
                        subtype = subtype,
                        request_id = %req.request_id,
                        "received unexpected control_request subtype, replying success"
                    );
                } else {
                    tracing::info!(
                        request_id = %req.request_id,
                        "received initialize control_request"
                    );
                }

                // Reply with control_response (always success).
                let response = ControlResponse {
                    msg_type: "control_response",
                    response: ControlResponseBody {
                        subtype: "success",
                        request_id: req.request_id,
                        response: serde_json::json!({}),
                    },
                };

                let json_line = serde_json::to_string(&response)?;
                writer
                    .write_all(format!("{json_line}\n").as_bytes())
                    .await?;
                writer.flush().await?;
                tracing::info!("sent control_response");
            }

            StdinMessage::User(user_msg) => {
                let prompt = extract_prompt_text(&user_msg.message.content);
                tracing::info!(prompt_len = prompt.len(), "received user prompt from stdin");
                return Ok(prompt);
            }

            StdinMessage::KeepAlive => {
                tracing::debug!("ignoring keep_alive");
            }

            StdinMessage::Unknown => {
                tracing::debug!("ignoring unknown stdin message type");
            }
        }
    }

    // stdin closed without receiving a user message.
    anyhow::bail!("stdin closed before receiving user message")
}

/// Extract prompt text from a [`MessageContent`].
///
/// - `Text(s)` → returns `s` directly
/// - `Blocks(blocks)` → filters `type == "text"`, joins `text` fields with spaces
fn extract_prompt_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- StdinMessage::from_value ----

    #[test]
    fn parse_control_request_initialize() {
        let json = serde_json::json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {
                "subtype": "initialize",
                "hooks": null,
                "agents": null
            }
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::ControlRequest(req) => {
                assert_eq!(req.request_id, "req-1");
                assert_eq!(req.request["subtype"], "initialize");
            }
            _ => panic!("expected ControlRequest, got {:?}", msg),
        }
    }

    #[test]
    fn parse_control_request_with_extra_fields() {
        let json = serde_json::json!({
            "type": "control_request",
            "request_id": "req-2",
            "request": { "subtype": "initialize" },
            "extra_field": "should be absorbed"
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::ControlRequest(req) => {
                assert_eq!(req.request_id, "req-2");
                assert!(req.extra.contains_key("extra_field"));
            }
            _ => panic!("expected ControlRequest"),
        }
    }

    #[test]
    fn parse_user_message_string_content() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "hello world"
            },
            "session_id": "sess-123",
            "parent_tool_use_id": null
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::User(user) => {
                assert_eq!(user.message.role, "user");
                match &user.message.content {
                    MessageContent::Text(s) => assert_eq!(s, "hello world"),
                    _ => panic!("expected Text content"),
                }
                // Extra fields absorbed
                assert!(user.extra.contains_key("session_id"));
                assert!(user.extra.contains_key("parent_tool_use_id"));
            }
            _ => panic!("expected User, got {:?}", msg),
        }
    }

    #[test]
    fn parse_user_message_content_blocks() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "part1" },
                    { "type": "text", "text": "part2" },
                    { "type": "image", "source": "..." }
                ]
            }
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::User(user) => {
                let prompt = extract_prompt_text(&user.message.content);
                assert_eq!(prompt, "part1 part2");
            }
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn parse_keep_alive() {
        let json = serde_json::json!({ "type": "keep_alive" });
        let msg = StdinMessage::from_value(json);
        assert!(matches!(msg, StdinMessage::KeepAlive));
    }

    #[test]
    fn parse_unknown_type() {
        let json = serde_json::json!({
            "type": "some_future_type",
            "data": "whatever"
        });
        let msg = StdinMessage::from_value(json);
        assert!(matches!(msg, StdinMessage::Unknown));
    }

    #[test]
    fn parse_no_type_field() {
        let json = serde_json::json!({ "foo": "bar" });
        let msg = StdinMessage::from_value(json);
        assert!(matches!(msg, StdinMessage::Unknown));
    }

    #[test]
    fn extract_prompt_from_string() {
        let content = MessageContent::Text("do something".into());
        assert_eq!(extract_prompt_text(&content), "do something");
    }

    #[test]
    fn extract_prompt_from_blocks_text_only() {
        let content = MessageContent::Blocks(vec![
            ContentBlock {
                block_type: "text".into(),
                text: Some("hello".into()),
            },
            ContentBlock {
                block_type: "text".into(),
                text: Some("world".into()),
            },
        ]);
        assert_eq!(extract_prompt_text(&content), "hello world");
    }

    #[test]
    fn extract_prompt_from_blocks_skips_non_text() {
        let content = MessageContent::Blocks(vec![
            ContentBlock {
                block_type: "text".into(),
                text: Some("visible".into()),
            },
            ContentBlock {
                block_type: "image".into(),
                text: None,
            },
            ContentBlock {
                block_type: "text".into(),
                text: Some("also visible".into()),
            },
        ]);
        assert_eq!(extract_prompt_text(&content), "visible also visible");
    }

    #[test]
    fn extract_prompt_from_empty_blocks() {
        let content = MessageContent::Blocks(vec![]);
        assert_eq!(extract_prompt_text(&content), "");
    }

    // ---- control_response serialization ----

    #[test]
    fn control_response_serializes_correctly() {
        let resp = ControlResponse {
            msg_type: "control_response",
            response: ControlResponseBody {
                subtype: "success",
                request_id: "test-123".into(),
                response: serde_json::json!({}),
            },
        };

        let json_str = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["type"], "control_response");
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["request_id"], "test-123");
        assert_eq!(parsed["response"]["response"], serde_json::json!({}));
    }

    // ---- handle_stdin_stream_with integration tests ----

    use std::io::Cursor;

    #[tokio::test]
    async fn stdin_stream_full_handshake() {
        let input = concat!(
            r#"{"type":"control_request","request_id":"init-1","request":{"subtype":"initialize"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"say hello"}}"#,
            "\n",
        );

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output = Vec::new();

        let prompt = handle_stdin_stream_with(reader, &mut output).await.unwrap();

        assert_eq!(prompt, "say hello");

        // Verify control_response was written
        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        assert_eq!(lines.len(), 1);

        let resp: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp["type"], "control_response");
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "init-1");
    }

    #[tokio::test]
    async fn stdin_stream_with_keep_alive_and_empty_lines() {
        let input = concat!(
            "\n",
            r#"{"type":"keep_alive"}"#,
            "\n",
            r#"{"type":"control_request","request_id":"k-1","request":{"subtype":"initialize"}}"#,
            "\n",
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"prompt text"}}"#,
            "\n",
        );

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output = Vec::new();

        let prompt = handle_stdin_stream_with(reader, &mut output).await.unwrap();
        assert_eq!(prompt, "prompt text");
    }

    #[tokio::test]
    async fn stdin_stream_unknown_type_ignored() {
        let input = concat!(
            r#"{"type":"unknown_msg","data":"ignored"}"#,
            "\n",
            r#"not valid json at all"#,
            "\n",
            r#"{"type":"control_request","request_id":"x-1","request":{"subtype":"initialize"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
        );

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output = Vec::new();

        let prompt = handle_stdin_stream_with(reader, &mut output).await.unwrap();
        assert_eq!(prompt, "hi");
    }

    #[tokio::test]
    async fn stdin_stream_eof_without_user_message() {
        let input =
            r#"{"type":"control_request","request_id":"e-1","request":{"subtype":"initialize"}}"#;

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output = Vec::new();

        let result = handle_stdin_stream_with(reader, &mut output).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("stdin closed before receiving user message")
        );
    }

    #[tokio::test]
    async fn stdin_stream_content_blocks_prompt() {
        let input = concat!(
            r#"{"type":"control_request","request_id":"cb-1","request":{"subtype":"initialize"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"block1"},{"type":"image","source":"x"},{"type":"text","text":"block2"}]}}"#,
            "\n",
        );

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output = Vec::new();

        let prompt = handle_stdin_stream_with(reader, &mut output).await.unwrap();
        assert_eq!(prompt, "block1 block2");
    }

    #[tokio::test]
    async fn stdin_stream_unknown_control_request_subtype() {
        let input = concat!(
            r#"{"type":"control_request","request_id":"unk-1","request":{"subtype":"some_future_op"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            "\n",
        );

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output = Vec::new();

        let prompt = handle_stdin_stream_with(reader, &mut output).await.unwrap();
        assert_eq!(prompt, "go");

        // Should still produce a control_response for unknown subtypes
        let output_str = String::from_utf8(output).unwrap();
        let resp: serde_json::Value = serde_json::from_str(output_str.trim()).unwrap();
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "unk-1");
    }

    #[tokio::test]
    async fn stdin_stream_empty_input() {
        let input = "";
        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output = Vec::new();

        let result = handle_stdin_stream_with(reader, &mut output).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("stdin closed before receiving user message")
        );
    }
}
