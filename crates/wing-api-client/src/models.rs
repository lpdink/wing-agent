//! Request / response types — mirrors `wing.gateway.protocol` (Python).

use std::collections::HashMap;

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
    /// 会话级标签（插入序；无标签为空数组。旧网关缺省为空）。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 每个标签的记录（键集 ⊆ tags）；当前含 `added_at` 打标时间。
    /// 旧网关 / 老数据缺省为空。
    #[serde(default)]
    pub tag_meta: HashMap<String, TagMeta>,
}

/// 单个标签的记录（`SessionMetadata.tag_meta` 的值）。
///
/// 值是**对象**（面向增量：后续审计维度在此扩展）；当前只有 `added_at`
/// ——标签**实际加入**的时间（本地 naive ISO，与 `last_interaction` 同口径；
/// 幂等 no-op 不刷新，移除即删记录）。时间缺失 / 不可解析 = 未知，
/// 消费方自行降级。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TagMeta {
    #[serde(default)]
    pub added_at: Option<String>,
}

/// Agent 配置信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub model_name: String,
    /// 模型的**引用词**（∈ 配置声明的 id 空间）——前端的选择态 / 匹配以它为准。
    /// 陈旧数据 / 降级路径（旧会话的 id 已被删除且反查不中）为 None，
    /// 前端容忍：展示回落 `model_display_name`‖`model_name`，picker 无标记。
    #[serde(default)]
    pub model_id: Option<String>,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    pub workspace: Option<String>,
    /// 当前活跃 provider 的名称（运行期事实）；旧网关/降级路径缺失时为 None。
    #[serde(default)]
    pub provider_name: Option<String>,
    /// `model_name` 的展示名（网关配置声明）；旧网关 / 未声明时为 None。
    /// 展示层专用——身份是 `model_id`。
    #[serde(default)]
    pub model_display_name: Option<String>,
}

// ============================================================
// Session 生命周期 — Request
// ============================================================

/// Agent 参数覆盖。所有字段可选，`None` 表示不覆盖（保留 template 值）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentOverride {
    /// 覆盖模型（引用 `providers[].models` 的 id）；未命中由网关报错（400）。
    /// provider 是运行期事实，随映射而来——不再是覆盖字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
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
    /// Tags attached at creation (validation identical to `/api/session/tag`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Requested session id — **create-or-adopt**: when the id does not exist
    /// the new session gets exactly this id; when it exists (in memory or in
    /// any store) the existing session is adopted (resume semantics, with the
    /// `agent` override applied as the resume subset). `None` = the backend
    /// generates an id (the default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumeSessionRequest {
    pub session_id: String,
    /// Resume-time override: only `model_id` / `effort` / `tools` are applied
    /// (the rest would change the conversation prefix).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentOverride>,
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
    /// 当前模型的调用名（发给上游的值；展示回落素材）。
    pub model: String,
    /// 当前模型的**引用词**（∈ 配置声明的 id 空间）；不可用时 None（旧会话 / 反查不中）。
    /// 前端的选择态 / 匹配以它为准。
    #[serde(default)]
    pub model_id: Option<String>,
    /// 当前模型的 provider 名（运行期事实；未取到时 None）。
    #[serde(default)]
    pub provider_name: Option<String>,
    /// 当前模型的展示名（未声明 / 旧网关为 None，前端回落 `model`）。
    #[serde(default)]
    pub model_display_name: Option<String>,
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
    /// 会话标签（插入序；无标签为空数组。旧网关缺省为空）。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 每个标签的记录（键集 ⊆ tags）；当前含 `added_at` 打标时间。
    #[serde(default)]
    pub tag_meta: HashMap<String, TagMeta>,
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
    /// 切换模型（引用 `providers[].models` 的 id；未命中 → 400 含 available ids）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
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

/// POST /api/session/tag 请求——读取（add / remove 皆缺省）或原子增删标签。
#[derive(Debug, Clone, Serialize)]
pub struct TagSessionRequest {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
}

/// POST /api/session/tag 响应——变更后的全量标签 + 实际增删。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagSessionResponse {
    pub ok: bool,
    pub session_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    /// 变更后的全量标签记录（键集 ⊆ tags）。
    #[serde(default)]
    pub tag_meta: HashMap<String, TagMeta>,
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

/// 单个模型的声明（镜像 Python `ModelSpec` 的 API 投影）。
///
/// 字段即线格式：`id` 是全局唯一**引用词**（一切请求 / 协议引用它），`name` 是发给
/// 上游的调用名，`display_name` 可空（展示层回落 `name`）。
///
/// `display_name` / `description` / `capabilities` 都可能在部分覆盖的响应里缺席，
/// 解码必须容忍——**缺省不影响任何一份数据行**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDetail {
    /// 全局唯一引用词（Apply / 匹配 / 上报一律用它）。
    pub id: String,
    /// 实际调用名（发给 provider API 的值）。
    pub name: String,
    /// 人类可读展示名（展示层专用；可能缺省或为空串）。
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_capabilities")]
    pub capabilities: ModelCapabilities,
}

impl ModelDetail {
    /// 展示名：优先声明的 `display_name`（去空白后非空），缺省 / 空串 / 纯空白一律
    /// 回落调用名 `name`。
    ///
    /// 展示层专用——任何 Apply / 匹配 / 上报都必须使用 `id`（身份层）。
    pub fn display_label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|label| !label.trim().is_empty())
            .unwrap_or(&self.name)
    }
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

/// 向量字段的宽容解码：`null` 与缺省等价（空表）。
///
/// 缺键由字段级 `#[serde(default)]` 覆盖；显式 null 需要这一层，否则会抛
/// "invalid type: null, expected a sequence" 把整条 `/api/models` 响应打死
/// （真实生产者不会发 null，但中间代理 / 测试替身可能）。
fn deserialize_vec_or_default<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// 单个 provider 的模型目录（嵌套模型列表条目）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModels {
    pub provider: String,
    #[serde(default, deserialize_with = "deserialize_vec_or_default")]
    pub models: Vec<ModelDetail>,
}

impl ProviderModels {
    /// 按 **id**（引用词）查找模型声明；目录未覆盖该 id 时返回 `None`。
    ///
    /// 单键查表：id 全局唯一，命中 / 未命中是二值判断，没有候选集合、没有优先级。
    pub fn find(&self, model_id: &str) -> Option<&ModelDetail> {
        self.models.iter().find(|detail| detail.id == model_id)
    }

    /// `model_id` 的展示名：命中声明则用 [`ModelDetail::display_label`]，未命中回落
    /// id 本身（调用方拿到的永远是「能显示的东西」）。
    ///
    /// 展示层专用——任何 Apply / 匹配 / 上报都必须使用 id（身份层）。
    pub fn label_for<'a>(&'a self, model_id: &'a str) -> &'a str {
        self.find(model_id)
            .map(|detail| detail.display_label())
            .unwrap_or(model_id)
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
    fn agent_info_model_id_optional() {
        // Legacy response without model_id / provider_name / model_display_name → None.
        let legacy = r#"{
            "model_name": "gpt-4",
            "system_prompt": null,
            "tools": [],
            "skills": [],
            "rules": [],
            "workspace": null
        }"#;
        let info: AgentInfo = serde_json::from_str(legacy).unwrap();
        assert_eq!(info.model_id, None);
        assert_eq!(info.provider_name, None);
        assert_eq!(info.model_display_name, None);

        // Round trip keeps the fields.
        let info = AgentInfo {
            model_name: "gpt-4".into(),
            model_id: Some("fancy-gpt".into()),
            system_prompt: None,
            tools: vec![],
            skills: vec![],
            rules: vec![],
            workspace: None,
            provider_name: Some("alt".into()),
            model_display_name: Some("Fancy Flash".into()),
        };
        let back: AgentInfo = serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(back.model_id.as_deref(), Some("fancy-gpt"));
        assert_eq!(back.provider_name.as_deref(), Some("alt"));
        assert_eq!(back.model_display_name.as_deref(), Some("Fancy Flash"));
    }

    #[test]
    fn session_info_carries_model_identity_and_tolerates_absent_fields() {
        // Current gateway: reference word + provider fact + display label.
        let json = r#"{
            "model": "dfmodel-2026",
            "model_id": "ds-flash",
            "provider_name": "qoder",
            "model_display_name": "DeepSeek-Flash",
            "api_url": "http://x",
            "tools": [],
            "total_tokens": 0,
            "context_window_tokens": 128000,
            "thinking": false,
            "yolo": false,
            "context_stats": {"message_count": 0, "total_tokens": 0}
        }"#;
        let info: SessionInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(info.model, "dfmodel-2026");
        assert_eq!(info.model_id.as_deref(), Some("ds-flash"));
        assert_eq!(info.provider_name.as_deref(), Some("qoder"));
        assert_eq!(info.model_display_name.as_deref(), Some("DeepSeek-Flash"));

        // Old gateway (no new fields) → None, no error.
        let legacy = r#"{
            "model": "gpt-4o",
            "api_url": "http://x",
            "tools": [],
            "total_tokens": 0,
            "context_window_tokens": 128000,
            "thinking": false,
            "yolo": false,
            "context_stats": {"message_count": 0, "total_tokens": 0}
        }"#;
        let info: SessionInfoResponse = serde_json::from_str(legacy).unwrap();
        assert_eq!(info.model_id, None);
        assert_eq!(info.provider_name, None);
        assert_eq!(info.model_display_name, None);
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

    /// 请求体只发 `model_id`：`model` / `provider` 必须不存在（legacy 字段已删）。
    #[test]
    fn update_and_override_requests_carry_only_model_id() {
        let update = UpdateSessionRequest {
            session_id: "s1".into(),
            model_id: Some("ds-flash".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&update).unwrap(),
            serde_json::json!({"session_id": "s1", "model_id": "ds-flash"})
        );

        let override_ = AgentOverride {
            model_id: Some("ds-flash".into()),
            yolo: Some(true),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&override_).unwrap(),
            serde_json::json!({"model_id": "ds-flash", "yolo": true})
        );

        // 全空体里没有任何模型字段残留。
        let empty = serde_json::to_value(UpdateSessionRequest {
            session_id: "s1".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(empty.get("model").is_none() && empty.get("provider").is_none());
    }

    // ── /api/models 目录（对象数组 + id 引用词） ──────

    /// 现行网关的形状：对象数组，`id` / `name` / `display_name` / `description` /
    /// `capabilities`；`display_name: null` / 缺 `description` / 缺 `capabilities` 全部容忍。
    #[test]
    fn models_response_decodes_the_object_array_with_ids() {
        let json = r#"{
            "providers": [{
                "provider": "qoder",
                "models": [
                    {"id": "dfmodel", "name": "dfmodel", "display_name": null,
                     "description": null, "capabilities": null},
                    {"id": "ds-flash", "name": "dfmodel-2026", "display_name": "DeepSeek-Flash"},
                    {"id": "bare", "name": "bare"}
                ]
            }]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(json).unwrap();
        let group = &resp.providers[0];
        assert_eq!(group.models.len(), 3);

        // null display_name / capabilities → 回落调用名 / vision=false。
        let first = &group.models[0];
        assert_eq!(first.id, "dfmodel");
        assert_eq!(first.name, "dfmodel");
        assert_eq!(first.display_name, None);
        assert_eq!(first.description, None);
        assert!(!first.capabilities.vision);
        assert_eq!(first.display_label(), "dfmodel");

        // id ≠ name：展示名 / 描述如实解析。
        let second = group.find("ds-flash").expect("id lookup");
        assert_eq!(second.name, "dfmodel-2026");
        assert_eq!(second.display_name.as_deref(), Some("DeepSeek-Flash"));
        assert_eq!(second.display_label(), "DeepSeek-Flash");
        assert_eq!(group.label_for("ds-flash"), "DeepSeek-Flash");

        // 未命中：`find` = None，`label_for` 回落 id 本身。
        assert!(group.find("nope").is_none());
        assert_eq!(group.label_for("nope"), "nope");
    }

    /// 空串 / 纯空白 `display_name` 与缺省等价；capabilities.vision 如实解析。
    #[test]
    fn display_label_falls_back_on_blank_names_and_reads_capabilities() {
        let json = r#"{
            "providers": [{
                "provider": "p",
                "models": [
                    {"id": "a", "name": "a", "display_name": "",
                     "capabilities": {"vision": true}},
                    {"id": "b", "name": "b", "display_name": "   "},
                    {"id": "c", "name": "c", "display_name": " DeepSeek-Flash "}
                ]
            }]
        }"#;
        let resp: ModelsResponse = serde_json::from_str(json).unwrap();
        let group = &resp.providers[0];
        assert_eq!(group.label_for("a"), "a", "空串展示名回落调用名");
        assert_eq!(group.label_for("b"), "b", "纯空白展示名回落调用名");
        assert!(group.find("a").unwrap().capabilities.vision);
        assert!(!group.find("b").unwrap().capabilities.vision);
        // 带内容但两端有空白仍然有效（只做判定，不改写声明的值）。
        assert_eq!(group.label_for("c"), " DeepSeek-Flash ");

        // provider 级响应也容忍缺省（最小响应）。
        let empty: ModelsResponse = serde_json::from_str(r#"{"providers": []}"#).unwrap();
        assert!(empty.providers.is_empty());
        let no_providers: ModelsResponse = serde_json::from_str("{}").unwrap();
        assert!(no_providers.providers.is_empty());
    }

    /// `models: null` 与缺省等价（空表）——整条响应仍可解析。
    #[test]
    fn models_null_decodes_as_empty() {
        let json = r#"{"providers": [{"provider": "qoder", "models": null}]}"#;
        let resp: ModelsResponse = serde_json::from_str(json).unwrap();
        assert!(resp.providers[0].models.is_empty());
        assert_eq!(resp.providers[0].label_for("dfmodel"), "dfmodel");
    }
}
