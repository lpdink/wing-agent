//! WingEvent protocol types — Rust mirror of wing Python event definitions.
//!
//! Every event type from `wing/event/` is represented here with serde
//! derive for JSON (de)serialization. The `type` field acts as the
//! discriminator via `#[serde(tag = "type")]`.
//!
//! Session-history records (the Message projection from `wing/session/session.py`)
//! live in [`history`] and decode through `SessionMessage::from_json`.

mod client_request;
mod connect_response;
mod events;
mod history;
pub mod media;

pub use client_request::ClientRequest;
pub use connect_response::ConnectResponse;
pub use events::*;
pub use history::*;
pub use media::{is_media_id, media_id_of, predicted_ref};
/// 媒体引用（镜像 Python `wing.schema.MediaRef`）。
///
/// 与 `SyncSession.usage` 同一条纪律：同一份线格式只定义一次——本体在
/// [`wing_api_client::models::MediaRef`]，这里只是把它抬到协议层的公共名字
/// （上行 `ClientRequest.media`、上传回执、下行 `tool_media` 共用）。
pub use wing_api_client::models::MediaRef;

/// Generate a request ID for correlation across clients on the same session.
pub fn generate_request_id() -> String {
    format!("wing_{}", uuid::Uuid::new_v4().simple())
}

pub(crate) const FEEDBACK_WAITER_INACTIVE: &str = "feedback_waiter_inactive";
