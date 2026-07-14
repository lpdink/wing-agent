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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
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

// ============================================================
// Session 查询端点 Response
// ============================================================

/// GET /api/session/info 响应——session 运行时状态。
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfoResponse {
    pub model: String,
    pub api_url: String,
    pub tools: Vec<String>,
    pub total_tokens: i64,
    pub context_window_tokens: i64,
    pub thinking: bool,
    pub yolo: bool,
    pub session_name: Option<String>,
}

/// 单个可分叉/回退的消息节点信息。
#[derive(Debug, Clone, Deserialize)]
pub struct BranchTargetInfo {
    pub uuid: String,
    pub content: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "user".to_owned()
}

/// GET /api/session/branches 响应——可回退/分叉的消息节点列表。
#[derive(Debug, Clone, Deserialize)]
pub struct BranchesResponse {
    #[serde(default)]
    pub targets: Vec<BranchTargetInfo>,
}

// ============================================================
// Session 更新端点 Request / Response
// ============================================================

/// POST /api/session/update 请求——统一 session 状态变更。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateSessionRequest {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
}

/// POST /api/session/update 响应。
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateSessionResponse {
    pub ok: bool,
}

// ============================================================
// 系统级查询端点 Response
// ============================================================

/// 魔术命令元信息。
#[derive(Debug, Clone, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: String,
}

/// GET /api/commands 响应——可用命令列表。
#[derive(Debug, Clone, Deserialize)]
pub struct CommandsResponse {
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
}

/// GET /api/models 响应——可用模型列表。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub models: Vec<String>,
}

/// GET /api/agents 响应——可用 agent 模板列表。
#[derive(Debug, Clone, Deserialize)]
pub struct AgentsResponse {
    #[serde(default)]
    pub agents: Vec<String>,
    pub default_agent: String,
}

/// 服务端返回的错误详情（HTTP 4xx/5xx 时反序列化）。
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub detail: Option<String>,
    pub session_id: Option<String>,
    pub uuid: Option<String>,
}
