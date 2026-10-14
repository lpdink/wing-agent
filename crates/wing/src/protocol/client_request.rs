//! ClientRequest — messages the TUI sends to Gateway.
//!
//! Mirrors: wing/gateway/protocol/ws.py ClientRequest

use serde::Deserialize;
use serde::Serialize;

use crate::protocol::MediaRef;

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
    /// 消息携带的图片引用（由 `POST /api/session/media` 上传得到，按提交顺序）。
    ///
    /// `None` = 无媒体：序列化**不产生 `media` 键**——旧网关（不认识这个字段）
    /// 收到的帧与引入该字段之前逐字节相同。`Some(vec![])` 与 `None` 等价，前端不发空表。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<MediaRef>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_ref() -> MediaRef {
        MediaRef {
            id: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            mime: "image/jpeg".into(),
            bytes: 1234,
            width: 64,
            height: 48,
            name: None,
        }
    }

    #[test]
    fn serialize_message_request() {
        let req = ClientRequest {
            request_id: "abc".into(),
            session_id: "sid".into(),
            content: "hello".into(),
            tool_call_id: None,
            media: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"silent\""));
        assert!(json.contains("\"content\":\"hello\""));
        // None tool_call_id is omitted from the payload.
        assert!(!json.contains("tool_call_id"));

        let req = ClientRequest {
            request_id: "abc2".into(),
            session_id: "sid".into(),
            content: "hi".into(),
            tool_call_id: Some("tc_1".into()),
            media: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"tool_call_id\":\"tc_1\""));
    }

    /// 兼容红线：无媒体的文本请求序列化结果与引入 `media` 之前**逐字节相同**。
    ///
    /// 整串比较（不是 `contains`）：字段顺序、缺键、值形状都被钉住；旧网关不会收到新键。
    #[test]
    fn a_text_only_request_is_byte_for_byte_the_old_frame() {
        let req = ClientRequest {
            request_id: "req-1".into(),
            session_id: "sid".into(),
            content: "hello".into(),
            tool_call_id: None,
            media: None,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"request_id":"req-1","session_id":"sid","content":"hello"}"#
        );

        // 老帧（无 media 键）照样能解码——双向兼容。
        let decoded: ClientRequest =
            serde_json::from_str(r#"{"request_id":"req-1","session_id":"sid","content":"hello"}"#)
                .unwrap();
        assert!(decoded.media.is_none());
    }

    /// 有媒体时的形状 == Python `MediaRef.model_dump()`：`id/mime/bytes/width/height/name`，
    /// `name` 缺席时显式 `null`（wire 不剥嵌套 null，pydantic 的 `model_dump()` 同形）。
    #[test]
    fn a_request_with_media_matches_the_python_media_ref_dump() {
        let req = ClientRequest {
            request_id: "req-2".into(),
            session_id: "sid".into(),
            content: "看这张图".into(),
            tool_call_id: None,
            media: Some(vec![media_ref()]),
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"request_id":"req-2","session_id":"sid","content":"看这张图","media":[{"id":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","mime":"image/jpeg","bytes":1234,"width":64,"height":48,"name":null}]}"#
        );

        // 回程（网关侧回放的帧 / 测试）能无损解码。
        let json = serde_json::to_string(&req).unwrap();
        let decoded: ClientRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.media, req.media);
    }
}
