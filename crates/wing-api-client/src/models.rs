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
    /// 当前活跃 provider 的名称；旧网关/降级路径缺失时为 None。
    #[serde(default)]
    pub provider_name: Option<String>,
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
    /// Optional user-directed compaction focus, appended to the compact
    /// prompt (e.g. "keep architecture decisions and pending TODOs").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterruptRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseResponse {
    pub ok: bool,
    /// 本次调用是否真的把会话逐出了内存（false = 本就不在内存，幂等）。
    pub released: bool,
    /// 结果说明（released / not loaded）。
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewindRequest {
    pub session_id: String,
    pub target_uuid: String,
}

// ============================================================
// Response types
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub template_name: String,
    pub workspace: Option<String>,
    /// Storage backend ("file" | "memory"). Optional for backward
    /// compatibility with gateways predating backend selection.
    #[serde(default)]
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeSessionResponse {
    pub session_id: String,
    pub template_name: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSessionResponse {
    pub session_id: String,
    pub draft: Option<String>,
}

/// 通用成功响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub ok: bool,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub service: String,
    pub status: String,
    pub version: String,
    /// 构建时注入的 commit hash（短）；旧网关不返回该字段时为 None。
    #[serde(default)]
    pub commit: Option<String>,
    pub uptime: i64,
}

// ============================================================
// Session 查询端点 Response
// ============================================================

/// 上下文统计信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextStatsInfo {
    pub message_count: i64,
    pub total_tokens: i64,
}

/// GET /api/session/info 响应——session 运行时状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactResponse {
    pub ok: bool,
    #[serde(default)]
    pub original_tokens: i64,
    #[serde(default)]
    pub compressed_tokens: i64,
}

/// POST /api/session/rewind 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindResponse {
    pub ok: bool,
    pub draft: Option<String>,
}

/// reload 端点中每一项的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadResultItem {
    pub name: String,
    pub ok: bool,
    pub detail: Option<String>,
}

/// POST /api/system/reload 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadResponse {
    pub ok: bool,
    #[serde(default)]
    pub results: Vec<ReloadResultItem>,
}

/// 单个可分叉/回退的消息节点信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSessionResponse {
    pub ok: bool,
}

// ============================================================
// 系统级查询端点 Response
// ============================================================

/// 魔术命令元信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandsResponse {
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
}

/// 模型的声明能力（镜像 Python `ModelCapabilities`）。
///
/// 旧网关没有该字段；缺省即 text-only（安全默认——后端同样按"未声明 = 无视觉"处理）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub vision: bool,
}

/// 单个模型的展示详情（镜像 Python `ModelSpec` 的 API 投影）。
///
/// `display_name` / `description` / `capabilities` 都可能在旧网关或部分
/// 覆盖的响应里缺席，解码必须容忍——**缺省不影响任何一份数据行**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDetail {
    /// 实际调用名（身份）：Apply / 匹配 / 上报一律用它。
    pub name: String,
    /// 人类可读展示名（展示层专用；可能缺省或为空串）。
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_capabilities")]
    pub capabilities: ModelCapabilities,
}

/// `capabilities: null` 与缺省等价（安全默认：text-only）。
///
/// 缺键由字段级 `#[serde(default)]` 覆盖；显式 null 需要这一层，
/// 否则会直接抛"invalid type: null"，把整条 `/api/models` 响应打死。
fn deserialize_capabilities<'de, D>(deserializer: D) -> Result<ModelCapabilities, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<ModelCapabilities>::deserialize(deserializer)?.unwrap_or_default())
}

/// 单个 provider 的可用模型（嵌套模型列表条目）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModels {
    pub provider: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// 模型展示详情（追加属性；旧网关不返回时为缺省空表，且可能只覆盖部分模型）。
    #[serde(default)]
    pub model_details: Vec<ModelDetail>,
}

impl ProviderModels {
    /// 按调用名查找模型详情。未覆盖（旧网关 / 部分覆盖）时返回 `None`。
    pub fn detail_for(&self, name: &str) -> Option<&ModelDetail> {
        self.model_details.iter().find(|detail| detail.name == name)
    }

    /// 展示名：优先该模型声明的 `display_name`（非空），缺省 / 空串 / 未声明
    /// 一律回落调用名 `name`。
    ///
    /// 展示层专用——任何 Apply / 匹配 / 上报都必须使用 `name`（身份层）。
    pub fn label_for<'a>(&'a self, name: &'a str) -> &'a str {
        self.detail_for(name)
            .and_then(|detail| detail.display_name.as_deref())
            .filter(|label| !label.is_empty())
            .unwrap_or(name)
    }
}

/// GET /api/models 响应——可用模型列表（按 provider 分组嵌套）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub providers: Vec<ProviderModels>,
}

/// GET /api/agents 响应——可用 agent 模板列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsResponse {
    #[serde(default)]
    pub agents: Vec<String>,
    pub default_agent: String,
}

/// 服务端返回的错误详情（HTTP 4xx/5xx 时反序列化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterToolsResponse {
    pub ok: bool,
    #[serde(default)]
    pub registered: Vec<String>,
}

// ============================================================
// 远程工具 WS 帧
// ============================================================

/// Gateway → tool host 的工具调用请求帧。
#[derive(Debug, Clone, Serialize, Deserialize)]
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

// ============================================================
// Tools list — GET /api/tools
// ============================================================

/// Single tool metadata (mirrors Python `wing.gateway.protocol.ToolInfo`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    #[serde(rename = "ref")]
    pub ref_field: String,
    #[serde(default)]
    pub namespace: String,
    pub name: String,
    pub llm_name: String,
    #[serde(default)]
    pub description: String,
}

/// GET /api/tools response — all registered tools (built-in + remote).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResponse {
    #[serde(default)]
    pub tools: Vec<ToolInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_request_omits_none_instruction() {
        let req = CompactRequest {
            session_id: "s1".into(),
            instruction: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["session_id"], "s1");
        assert!(
            json.get("instruction").is_none(),
            "None instruction must be omitted, got {json}"
        );
    }

    #[test]
    fn compact_request_serializes_instruction() {
        let req = CompactRequest {
            session_id: "s1".into(),
            instruction: Some("keep architecture decisions".into()),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["instruction"], "keep architecture decisions");
    }

    #[test]
    fn agent_info_provider_name_optional() {
        // Legacy response without provider_name → None.
        let legacy = r#"{
            "model_name": "gpt-4",
            "system_prompt": null,
            "tools": [],
            "skills": [],
            "rules": [],
            "workspace": null
        }"#;
        let info: AgentInfo = serde_json::from_str(legacy).unwrap();
        assert_eq!(info.provider_name, None);

        // Round trip keeps the field.
        let info = AgentInfo {
            model_name: "gpt-4".into(),
            system_prompt: None,
            tools: vec![],
            skills: vec![],
            rules: vec![],
            workspace: None,
            provider_name: Some("alt".into()),
        };
        let back: AgentInfo = serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(back.provider_name.as_deref(), Some("alt"));
    }

    #[test]
    fn health_commit_optional() {
        // Legacy gateway (before commit injection) returns no commit field → None.
        let legacy = r#"{
            "service": "wing-gateway",
            "status": "ok",
            "version": "dev",
            "uptime": 42
        }"#;
        let health: HealthResponse = serde_json::from_str(legacy).unwrap();
        assert_eq!(health.commit, None);

        // Current gateway reports the build commit.
        let current = r#"{
            "service": "wing-gateway",
            "status": "ok",
            "version": "0.4.1",
            "commit": "3e6e472",
            "uptime": 42
        }"#;
        let health: HealthResponse = serde_json::from_str(current).unwrap();
        assert_eq!(health.commit.as_deref(), Some("3e6e472"));
    }

    // ── /api/models model_details（旧网关容忍 + 展示名回落） ──────

    /// 旧网关：没有 `model_details` 字段，`models` 照旧可用。
    #[test]
    fn models_response_without_model_details() {
        let legacy = r#"{
            "providers": [{"provider": "qoder", "models": ["dfmodel", "gpt-x"]}]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(legacy).unwrap();
        assert_eq!(resp.providers.len(), 1);
        let group = &resp.providers[0];
        assert_eq!(group.models, vec!["dfmodel", "gpt-x"]);
        assert!(group.model_details.is_empty());
        // 未声明 → 展示名回落调用名（身份层值）。
        assert_eq!(group.label_for("dfmodel"), "dfmodel");
        assert_eq!(group.label_for("unknown"), "unknown");
    }

    /// 新网关：追加属性解析；`display_name: null` / 缺 `description` /
    /// 缺 `capabilities` 全部容忍。
    #[test]
    fn model_details_tolerate_null_and_missing_optional_fields() {
        let json = r#"{
            "providers": [{
                "provider": "qoder",
                "models": ["dfmodel", "dfmodel-2026"],
                "model_details": [
                    {"name": "dfmodel", "display_name": null,
                     "description": null, "capabilities": null},
                    {"name": "dfmodel-2026", "display_name": "DeepSeek-Flash"}
                ]
            }]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(json).unwrap();
        let group = &resp.providers[0];
        assert_eq!(group.model_details.len(), 2);

        // null display_name → 回落调用名；null capabilities → vision=false。
        let first = &group.model_details[0];
        assert_eq!(first.display_name, None);
        assert_eq!(first.description, None);
        assert!(!first.capabilities.vision);
        assert_eq!(group.label_for("dfmodel"), "dfmodel");

        // 部分覆盖：第二条只有展示名，其余缺省。
        let second = group.detail_for("dfmodel-2026").unwrap();
        assert_eq!(second.display_name.as_deref(), Some("DeepSeek-Flash"));
        assert_eq!(second.description, None);
        assert!(!second.capabilities.vision);
        assert_eq!(group.label_for("dfmodel-2026"), "DeepSeek-Flash");
    }

    /// 空串 display_name 与缺省等价；capabilities.vision 如实解析。
    #[test]
    fn label_for_falls_back_on_empty_display_name_and_reads_capabilities() {
        let json = r#"{
            "providers": [{
                "provider": "p",
                "models": ["a", "b"],
                "model_details": [
                    {"name": "a", "display_name": "",
                     "capabilities": {"vision": true}},
                    {"name": "b", "capabilities": {"vision": false}}
                ]
            }]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(json).unwrap();
        let group = &resp.providers[0];
        assert_eq!(group.label_for("a"), "a", "空串展示名回落调用名");
        assert!(group.detail_for("a").unwrap().capabilities.vision);
        assert!(!group.detail_for("b").unwrap().capabilities.vision);
        // provider 级响应也容忍缺省（旧网关的最小响应）。
        let empty: ModelsResponse = serde_json::from_str(r#"{"providers": []}"#).unwrap();
        assert!(empty.providers.is_empty());
        let no_providers: ModelsResponse = serde_json::from_str("{}").unwrap();
        assert!(no_providers.providers.is_empty());
    }
}
