//! Request / response types — mirrors `wing.gateway.protocol` (Python).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================
// Shared data types
// ============================================================

/// Session 摘要信息（用于列表/详情）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub template_name: Option<String>,
    pub workspace: Option<String>,
    pub last_interaction: Option<String>,
}

/// Agent 配置信息。
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

// ============================================================
// Session 生命周期 — Request
// ============================================================

/// Agent 参数覆盖。所有字段可选，`None` 表示不覆盖（保留 template 值）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateSessionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentOverride>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumeSessionRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForkSessionRequest {
    pub source_session_id: String,
    pub target_uuid: String,
}

// ============================================================
// 订阅管理 — Request
// ============================================================

#[derive(Debug, Clone, Serialize)]
pub struct SubscribeRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnsubscribeRequest {
    pub session_id: String,
}

// ============================================================
// 消息发送 — Request
// ============================================================

#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    pub session_id: String,
    pub content: String,
    #[serde(default)]
    pub silent: bool,
}

// ============================================================
// Response types
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub template_name: String,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResumeSessionResponse {
    pub session_id: String,
    pub template_name: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForkSessionResponse {
    pub session_id: String,
    pub draft: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageResponse {
    pub ok: bool,
    pub request_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionGetResponse {
    pub session_id: String,
    pub name: Option<String>,
    pub template_name: Option<String>,
    pub workspace: Option<String>,
    pub messages: Vec<serde_json::Value>,
    pub agent: Option<AgentInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// 服务端返回的错误详情（HTTP 4xx/5xx 时反序列化）。
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub detail: Option<String>,
    pub session_id: Option<String>,
    pub uuid: Option<String>,
}
