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
    /// 运行时状态: inactive|idle|working|waiting（旧网关缺省时为空串，前端降级为 inactive）。
    #[serde(default)]
    pub status: String,
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
    /// Provider name (references config `providers[].name`).
    /// When set with `model`, switches to that provider's endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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
    /// Storage backend: "file" (default, durable) | "memory" (ephemeral).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
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
    /// When replying to an Ask event, its tool_call_id — routes the message
    /// to the matching feedback waiter instead of the session inbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

// ============================================================
// Session 操作 — Request
// ============================================================

#[derive(Debug, Clone, Serialize)]
pub struct CompactRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterruptRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewindRequest {
    pub session_id: String,
    pub target_uuid: String,
}

// ============================================================
// Response types
// ============================================================

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub template_name: String,
    pub workspace: Option<String>,
    /// Storage backend ("file" | "memory"). Optional for backward
    /// compatibility with gateways predating backend selection.
    #[serde(default)]
    pub backend: Option<String>,
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

/// 通用成功响应。
#[derive(Debug, Clone, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
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
    #[serde(default)]
    pub status: String,
    pub messages: Vec<serde_json::Value>,
    pub agent: Option<AgentInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    pub service: String,
    pub status: String,
    pub version: String,
    pub uptime: i64,
}

// ============================================================
// Session 查询端点 Response
// ============================================================

/// 上下文统计信息。
#[derive(Debug, Clone, Deserialize)]
pub struct ContextStatsInfo {
    pub message_count: i64,
    pub total_tokens: i64,
}

/// GET /api/session/info 响应——session 运行时状态。
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfoResponse {
    pub model: String,
    pub api_url: String,
    pub tools: Vec<String>,
    pub total_tokens: i64,
    pub context_window_tokens: i64,
    pub thinking: bool,
    pub reasoning_effort: Option<String>,
    pub yolo: bool,
    pub session_name: Option<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub status: String,
    pub context_stats: ContextStatsInfo,
    #[serde(default)]
    pub skills_info: String,
    #[serde(default)]
    pub system_prompt: String,
}

/// POST /api/session/compact 响应。
#[derive(Debug, Clone, Deserialize)]
pub struct CompactResponse {
    pub ok: bool,
    #[serde(default)]
    pub original_tokens: i64,
    #[serde(default)]
    pub compressed_tokens: i64,
}

/// POST /api/session/rewind 响应。
#[derive(Debug, Clone, Deserialize)]
pub struct RewindResponse {
    pub ok: bool,
    pub draft: Option<String>,
}

/// reload 端点中每一项的结果。
#[derive(Debug, Clone, Deserialize)]
pub struct ReloadResultItem {
    pub name: String,
    pub ok: bool,
    pub detail: Option<String>,
}

/// POST /api/system/reload 响应。
#[derive(Debug, Clone, Deserialize)]
pub struct ReloadResponse {
    pub ok: bool,
    #[serde(default)]
    pub results: Vec<ReloadResultItem>,
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
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateSessionRequest {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
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

/// 单个 provider 的可用模型（嵌套模型列表条目）。
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderModels {
    pub provider: String,
    #[serde(default)]
    pub models: Vec<String>,
}

/// GET /api/models 响应——可用模型列表（按 provider 分组嵌套）。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub providers: Vec<ProviderModels>,
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

// ============================================================
// 远程工具注册 — Request / Response
// ============================================================

/// 工具参数规格（镜像 Python `wing.schema.ToolParam`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    #[serde(rename = "type")]
    pub param_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<serde_json::Value>,
}

/// 远程工具规格（镜像 Python `RemoteToolSpec`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteToolSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ToolParam>,
}

/// POST /api/tools/register 请求体。
#[derive(Debug, Clone, Serialize)]
pub struct RegisterToolsRequest {
    pub tools: Vec<RemoteToolSpec>,
}

/// POST /api/tools/register 响应。
#[derive(Debug, Clone, Deserialize)]
pub struct RegisterToolsResponse {
    pub ok: bool,
    #[serde(default)]
    pub registered: Vec<String>,
}

// ============================================================
// 远程工具 WS 帧
// ============================================================

/// Gateway → tool host 的工具调用请求帧。
#[derive(Debug, Clone, Deserialize)]
pub struct WsToolCallRequest {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub call_id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// tool host → Gateway 的工具调用结果帧。
#[derive(Debug, Clone, Serialize)]
pub struct WsToolCallResult {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub call_id: String,
    pub result: String,
    pub is_error: bool,
}

impl WsToolCallResult {
    pub fn success(call_id: &str, result: String) -> Self {
        Self {
            frame_type: "tool_call_result".to_owned(),
            call_id: call_id.to_owned(),
            result,
            is_error: false,
        }
    }

    pub fn error(call_id: &str, result: String) -> Self {
        Self {
            frame_type: "tool_call_result".to_owned(),
            call_id: call_id.to_owned(),
            result,
            is_error: true,
        }
    }
}
