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
    /// 会话生效模型的**引用词**（∈ 配置声明的 id 空间）；身份字段。
    /// 未加载的会话按 resume 链解析盘上记录，解析不出的降级路径为 None
    /// （旧网关缺省）。
    #[serde(default)]
    pub model_id: Option<String>,
    /// 生效模型的调用名（发给上游的值；展示回落素材）。旧网关 / 未解析时为
    /// None。
    #[serde(default)]
    pub model_name: Option<String>,
    /// 承载该模型的 provider 名（运行期事实）；旧网关 / 未解析时为 None。
    #[serde(default)]
    pub provider_name: Option<String>,
    /// `model_name` 的展示名（网关配置声明）；旧网关 / 未声明时为 None。
    /// 展示层专用——身份是 `model_id`（与 `AgentInfo` / `/api/session/info`
    /// 同一口径）。
    #[serde(default)]
    pub model_display_name: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReloadResultItem {
    pub name: String,
    pub ok: bool,
    pub detail: Option<String>,
}

/// POST /api/system/reload 响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

// ============================================================
// Settings — GET /api/settings/{schema,get,status} · POST /api/settings/set
// ============================================================
//
// 镜像 Python `wing.gateway.protocol.settings`（design.md §9，已冻结）。两条不变量贯穿本节：
//
// 1. **前向兼容**：未知 enum 值落 `Unknown(String)`（原样保留拼写，反序列化永不失败）；
//    多余字段一律忽略（绝不用 `deny_unknown_fields`）；可选字段缺失时按 §9 声明的默认值补齐。
//    网关与 TUI 各自升级（`pip install -U` 了后端但没换二进制，或反过来）都必须能用。
// 2. **值形状照抄**：`values` / `document` / `default` 是任意 JSON（`serde_json::Value`），
//    其余字段一律 typed——不给下游留"这里是啥"的悬念。

/// 生成一个「wire 字符串枚举 + 未知值兜底」类型。
///
/// `SettingKind` / `ApplyScope` / `SecretPresence` 三个枚举的样板逐字相同：已知拼写 → 变体，
/// 其余原样收进 `Unknown(String)`。手写三遍是同构代码，这里收口一次。
///
/// 未知值必须能解码：`#[serde(other)]` 只支持内部/邻接标签的 enum（这三个是外部标签的字符串
/// enum，写了编译不过），fallback 到固定 unit 变体则会丢掉"网关到底说了什么"这个信息。
/// `as_str()` 是唯一的 wire 拼写来源，`Serialize` 与 `Deserialize` 因此严格对称。
macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident => $wire:literal,
            )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )*
            /// 未知值（来自更新的网关）：原始 wire 拼写原样保留，反序列化永不失败。
            Unknown(String),
        }

        impl $name {
            /// wire 上的拼写（`Unknown` 返回原样保留的字符串）。
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)*
                    Self::Unknown(raw) => raw.as_str(),
                }
            }

            /// 已知拼写 → 变体；其余进 `Unknown`。
            fn from_wire(raw: &str) -> Self {
                match raw {
                    $($wire => Self::$variant,)*
                    other => Self::Unknown(other.to_owned()),
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Ok(Self::from_wire(&String::deserialize(deserializer)?))
            }
        }
    };
}

wire_enum! {
    /// 配置项的取值类型（镜像 Python `SettingKind`）。
    ///
    /// 决定面板用什么编辑器、树怎么下钻（`object` / `list` 是结构节点）。
    pub enum SettingKind {
        /// 字符串
        Str => "str",
        /// 整数
        Int => "int",
        /// 浮点数
        Float => "float",
        /// 布尔（面板里 Enter/Space 直接切换）
        Bool => "bool",
        /// 枚举（取值集在 `choices`）
        Enum => "enum",
        /// 密文（只写不回显，只下发末 4 位 hint）
        Secret => "secret",
        /// freeform map（面板里用单行 JSON 编辑器）
        Map => "map",
        /// 对象（下钻 `children`）
        Object => "object",
        /// 列表（下钻 `element`，或 union 元素的 `variants`）
        List => "list",
    }
}

wire_enum! {
    /// 生效域：改了这个键，什么时候生效（镜像 Python `ApplyScope`）。
    pub enum ApplyScope {
        /// 保存即热重载生效
        Hot => "hot",
        /// 新建会话生效；进行中的会话保持自己的状态
        NextSession => "next_session",
        /// 进程级，必须重启网关
        Restart => "restart",
        /// 派生值 / env 覆盖 / 只读事实，面板不可编辑
        Readonly => "readonly",
    }
}

impl Default for ApplyScope {
    /// 镜像 §9 的 `apply: ApplyScope = ApplyScope.HOT`（旧网关不发该键时的默认）。
    fn default() -> Self {
        Self::Hot
    }
}

wire_enum! {
    /// 密文在磁盘上的三态（镜像 §7.5）。前端据此渲染 `•••••••• ab12` / `(empty)` / `(not set)`。
    pub enum SecretPresence {
        /// 键在文档里且值非空
        Set => "set",
        /// 键在文档里但是空串（用户显式清空过）
        Empty => "empty",
        /// 键不在文档里（未设置 / 用默认）
        Absent => "absent",
    }
}

/// 枚举项：取值 + 这个值是什么意思（镜像 Python `SettingChoice`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingChoice {
    pub value: String,
    /// 该选项的含义；未声明时为 `None`。
    #[serde(default)]
    pub doc: Option<String>,
}

/// 设置目录里的一棵树节点（镜像 Python `SettingNode` / 协议 `SettingNodeProto`）。
///
/// `path` 是规范地址（§5.2 文法）：catalog 里用 `[]` 表示元素模板
/// （`providers[].models[].id`），文档 / problems / changed 里用具体下标
/// （`providers[0].models[2].id`）。寻址见 [`SettingNode::node_at`]。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingNode {
    // ── 身份 ──
    /// 字段名；列表元素模板的 key 恒为 `[]`。
    pub key: String,
    /// 规范地址（见类型文档）。
    pub path: String,
    /// 人类标签（缺省 = key；展示一律走 [`SettingNode::display_label`]）。
    pub title: String,
    /// 一行摘要（面板行内提示 + YAML 行上注释）。
    pub doc: String,
    /// 详解（按行切分；面板详情栏 + YAML 块注释）。
    #[serde(default)]
    pub notes: Vec<String>,
    /// 示例值（详情栏 + YAML 注释）。
    #[serde(default)]
    pub example: Option<String>,
    /// 同级排序（= 声明序）。
    #[serde(default)]
    pub order: i64,

    // ── 类型与约束 ──
    /// 取值类型；未知值见 [`SettingKind::Unknown`]。
    pub kind: SettingKind,
    /// 必填（= pydantic 字段无默认值）。
    #[serde(default)]
    pub required: bool,
    /// 注解含 `None`（面板允许把它复位成 JSON null）。
    #[serde(default)]
    pub nullable: bool,
    /// 标量默认值；结构节点（object / list / map）为 `null`——结构由
    /// `children` / `element` / `variants` 表达。是否声明过默认值看 `has_default`。
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub has_default: bool,
    /// 下界（来自 gt / ge）；配合 `exclusive_min` 区分开闭区间。
    #[serde(default)]
    pub min: Option<f64>,
    /// 上界（来自 lt / le）。
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub exclusive_min: bool,
    #[serde(default)]
    pub exclusive_max: bool,
    /// 字符串最小长度。
    #[serde(default)]
    pub min_length: Option<i64>,
    /// 字符串正则（Python 语法）。
    #[serde(default)]
    pub pattern: Option<String>,
    /// 枚举取值集（`kind == enum`）；`value` 来自 Literal，`doc` 来自声明的 choices。
    #[serde(default)]
    pub choices: Vec<SettingChoice>,
    /// 列表最小长度（"不得为空" = `min_items: 1`）。
    #[serde(default)]
    pub min_items: Option<i64>,
    #[serde(default)]
    pub max_items: Option<i64>,

    // ── 语义 ──
    /// 密文：只写不回显，读侧只下发末 4 位 hint。
    #[serde(default)]
    pub secret: bool,
    /// 生效域（缺省 = `hot`，见 [`ApplyScope::default`]）。
    #[serde(default)]
    pub apply: ApplyScope,
    /// 面板可编辑（`false` = 灰显只读）。
    #[serde(default = "default_true")]
    pub editable: bool,
    /// 预留：废弃说明。本期不消费（design.md 非目标）。
    #[serde(default)]
    pub deprecated: Option<String>,

    // ── 分组 ──
    /// 顶层分组名（面板按它分组，顺序 = 声明序）；`None` = 不分组。
    #[serde(default)]
    pub section: Option<String>,
    /// 分组说明（emitter 的块注释来源）。
    #[serde(default)]
    pub section_doc: Option<String>,

    // ── 结构 ──
    /// 子节点（`kind == object`）。
    #[serde(default)]
    pub children: Vec<SettingNode>,
    /// 元素模板（`kind == list` 且元素是**单一**类型）。
    #[serde(default)]
    pub element: Option<Box<SettingNode>>,
    /// 元素形态（`kind == list` 且元素是 **union**，如 `models: list[str | ModelSpec]`）：
    /// 面板的"新增一项"据此给用户两个选择。与 `element` 互斥。
    #[serde(default)]
    pub variants: Option<Vec<SettingNode>>,
    /// 列表项的标题行取哪几个子字段（`["name", "protocol", "base_url"]`）；
    /// 缺省空 ⇒ 前端回落"第一个标量子字段"。
    #[serde(default)]
    pub summary_fields: Vec<String>,

    // ── 渲染增强 ──
    /// 值的展示提示：`"color"` = 选择项视图渲染色块（Interface 根专用；后端恒为 `null`）。
    #[serde(default)]
    pub value_hint: Option<String>,
}

/// `bool` 字段的缺省为 `true`（§9 的 `editable: bool = True`）。
fn default_true() -> bool {
    true
}

/// 规范路径的一步（§5.2 文法：`segment := name | name "[" idx "]" | name "[]"`）。
///
/// `Key` 与它的下标总成对出现：下标属于**前一个** `Key`（`a[0].b` → `Key("a"), Index(0), Key("b")`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStep {
    /// `name`
    Key(String),
    /// `name[i]` 的**具体**下标（values / problems / changed 里的路径用）。
    Index(usize),
    /// `name[]` 的**元素模板**（catalog 自己的路径用）；模板 → 具体是**一对多**展开，
    /// 由前端按文档实际长度完成，所以它不是 `Index(0)`。
    Element,
}

impl SettingNode {
    /// 按规范路径查节点（§5.2 文法：`a.b` / `a[i].b` / `a[].b`）。
    ///
    /// - 根节点的 `key` 是**可选**前缀：`config.gateway.port` 与 `gateway.port` 等价
    ///   （仅当首段不命中任何子节点、且等于本节点的 `key` 时吃掉它）；
    /// - `[i]` / `[]` 走 `element`（单一元素类型）；列表声明的是 `variants`（union 元素）时
    ///   返回 `None`——元素形态只有**文档里的实际值**才能判定，调用方改用
    ///   [`Self::select_variant`]；
    /// - 下标**不查界**（catalog 是模板，不知道文档有多长）：`providers[7]` 与 `providers[0]`
    ///   解析到同一个元素模板；
    /// - 空路径 = 自己。
    pub fn node_at(&self, path: &str) -> Option<&SettingNode> {
        let steps = parse_path(path)?;
        // 根名可选：只有"首段不命中子节点"时才把它当作前缀吃掉。
        let steps: &[PathStep] = match steps.as_slice() {
            [PathStep::Key(first), rest @ ..]
                if !first.is_empty() && first == &self.key && self.child(first).is_none() =>
            {
                rest
            }
            all => all,
        };
        let mut node = self;
        for step in steps {
            node = match step {
                PathStep::Key(name) => node.child(name)?,
                PathStep::Index(_) | PathStep::Element => node.element.as_deref()?,
            };
        }
        Some(node)
    }

    /// `variants`（union 元素）列表按**文档实际值**选形态：取声明序里第一个 JSON 形状与
    /// `kind` 相符的 variant。
    ///
    /// `models: list[str | ModelSpec]` 因此既能渲染裸字符串、也能渲染对象。非 variants 列表 /
    /// 没有形状相符的 variant（含未知 kind）→ `None`。
    pub fn select_variant<'a>(&'a self, value: &serde_json::Value) -> Option<&'a SettingNode> {
        self.variants
            .as_deref()?
            .iter()
            .find(|variant| variant.accepts_json_shape(value))
    }

    /// `kind` 与 JSON 形状是否相符（未知 kind 一律不符——形态未知就选不出编辑器）。
    fn accepts_json_shape(&self, value: &serde_json::Value) -> bool {
        match self.kind {
            SettingKind::Str | SettingKind::Enum | SettingKind::Secret => value.is_string(),
            SettingKind::Int => value.as_i64().is_some() || value.as_u64().is_some(),
            SettingKind::Float => value.is_number(),
            SettingKind::Bool => value.is_boolean(),
            SettingKind::Map | SettingKind::Object => value.is_object(),
            SettingKind::List => value.is_array(),
            SettingKind::Unknown(_) => false,
        }
    }

    /// 深度优先（声明序）产出全部叶子：**不能再下钻**的节点。
    ///
    /// 下钻规则：`element` → 下钻它；`variants` → 逐个展开（同一模板路径可能出现多次）；
    /// `children` 非空 → 下钻它们；否则自己就是叶子（"空 object"与"没声明元素形态的 list"
    /// 这种退化节点没有可下钻的东西）。
    pub fn leaves(&self) -> impl Iterator<Item = &SettingNode> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out.into_iter()
    }

    /// 递归收集叶子（深度优先、声明序）。
    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a SettingNode>) {
        if let Some(element) = self.element.as_deref() {
            element.collect_leaves(out);
            return;
        }
        if let Some(variants) = self.variants.as_deref() {
            for variant in variants {
                variant.collect_leaves(out);
            }
            return;
        }
        if !self.children.is_empty() {
            for child in &self.children {
                child.collect_leaves(out);
            }
            return;
        }
        out.push(self);
    }

    /// 结构节点（可展开 / 折叠：`object` / `list`）。
    ///
    /// 回答的是**声明层形态**，不是"这棵树里有没有东西"——未知 kind 即使带了 `children`
    /// 也返回 `false`（形态未知时前端只读展示原始值）；扁平化该不该产生子行由前端看
    /// `children` / `element` / `variants` 决定。
    pub fn is_structural(&self) -> bool {
        matches!(self.kind, SettingKind::Object | SettingKind::List)
    }

    /// 展示标签：`title` 非空白时用它，否则回落 `key`（空 title 不该渲染成空白行）。
    pub fn display_label(&self) -> &str {
        if self.title.trim().is_empty() {
            &self.key
        } else {
            &self.title
        }
    }

    /// 直接子节点查找（按 `key` 匹配）。
    fn child(&self, key: &str) -> Option<&SettingNode> {
        self.children.iter().find(|child| child.key == key)
    }
}

/// 解析规范路径；文法之外的输入返回 `None`。
///
/// 这是 §5.2 文法的 Rust 侧实现（Python 侧是 `document.py` 的同名函数）：catalog 寻址
/// （[`SettingNode::node_at`]）与前端路径工具共用它，避免第二份实现漂移。
///
/// 空串是合法路径（= 根自己）。`[i]` 必须是**非负十进制整数**：没有符号位（`a[+1]` / `a[-1]`
/// 都非法），且超出 `usize` 即非法——注意这与"下标越界"无关，见 [`SettingNode::node_at`]；
/// `[]` 是元素模板（[`PathStep::Element`]）。
pub fn parse_path(path: &str) -> Option<Vec<PathStep>> {
    if path.is_empty() {
        return Some(Vec::new());
    }
    let mut steps = Vec::new();
    for segment in path.split('.') {
        let (name, index) = match segment.find('[') {
            None => (segment, None),
            Some(open) => (
                &segment[..open],
                Some(segment[open + 1..].strip_suffix(']')?),
            ),
        };
        if !is_path_name(name) {
            return None;
        }
        steps.push(PathStep::Key(name.to_owned()));
        match index {
            None => {}
            Some("") => steps.push(PathStep::Element),
            Some(raw) => {
                // §5.2 的下标是**非负十进制整数**，没有符号位：`usize::from_str` 会接受
                // 前导 `+`（`a[+1]`），而 Python 侧按 `str.isdigit()` 会拒 —— 两边同口径
                // （AD6）。溢出仍由 `parse` 的 `Err` 兜住。
                if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                steps.push(PathStep::Index(raw.parse().ok()?));
            }
        }
    }
    Some(steps)
}

/// `name := [A-Za-z_][A-Za-z0-9_]*`（§5.2）。
fn is_path_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 一个业务分组（镜像 Python `SettingGroup` / 协议 `SettingGroupProto`）。
///
/// 分组是**界面分类**的唯一声明来源（后端 `config/groups.py`）：设置面板左列的锚点、
/// `wing config list` 的分组头都读它，前端不许硬编码组名 / 顺序 / 成员。
/// `members` 是 `Config` 的顶层键名（= catalog root 直接子节点的 `key`），
/// 每个顶层键恰好属于一个组。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingGroup {
    /// 稳定标识（改名 / 调序都不该改它）。
    pub id: String,
    /// 显示名（面板锚点 / YAML 分隔行 / CLI 分组头）。
    pub title: String,
    /// 一行说明。
    #[serde(default)]
    pub doc: String,
    /// 成员 = 顶层键名。
    #[serde(default)]
    pub members: Vec<String>,
}

/// GET /api/settings/schema 响应——设置目录（catalog 树）+ 业务分组 + 版本 + 文件位置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsSchemaResponse {
    /// 网关版本（前端可据此提示"网关比面板新"）。
    pub version: String,
    /// `Config` 的节点（catalog 的根）。
    pub root: SettingNode,
    /// config.yaml 的绝对路径（面板标题栏展示）。
    pub config_path: String,
    /// 业务分组（顺序即界面顺序；TUI 设置面板左栏锚点的唯一来源）。
    ///
    /// `#[serde(default)]`：老网关不发这个键 ⇒ 空表。空表**怎么兜底**是前端的策略，
    /// 不是 wire 的一部分（住在 `crates/wing` 的 `shared::panels::settings::groups`）——
    /// 这一层只镜像后端发的东西。
    #[serde(default)]
    pub groups: Vec<SettingGroup>,
}

/// 一条配置问题（镜像 Python `ConfigProblem`）。
///
/// `kind` 保持裸字符串（§9 就是 `kind: str`）：种类只用于展示与排序，**未知种类必须容忍**
/// ——枚举化会多一个 fallback 变体而无消费者。已知种类见 design.md §7.2 的 `ProblemKind`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingProblem {
    /// 规范路径（含具体下标）；`None` = 文档级问题（如 YAML 语法错，无法定位到单个字段）。
    #[serde(default)]
    pub path: Option<String>,
    pub kind: String,
    /// 人类可读描述（加载期文案与 pydantic 错误一致）。
    pub message: String,
    /// 可操作建议（"给其中一个声明显式 id"）。
    #[serde(default)]
    pub hint: Option<String>,
}

/// 单个密文字段的状态（只写不回显：真实值不出网关）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretState {
    pub state: SecretPresence,
    /// 值长度 ≥ 8 时的末 4 位；否则 `None`（短密钥不给 hint，避免泄露比例过高）。
    #[serde(default)]
    pub hint: Option<String>,
}

/// GET /api/settings/get 响应——稀疏文档 + 指纹 + 密文状态 + 问题。
///
/// `values` 里的密文叶子恒为 `null`；**前端必须原样回传 `null`**（`null` = 保留磁盘现值，
/// 丢键 = 清空密钥 = 用户下次调用 401）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsGetResponse {
    /// 稀疏文档（只有用户显式写下的键；密文叶子 = `null`）。
    pub values: serde_json::Value,
    /// 密文状态表，键 = 规范路径（`providers[0].api_key`）。
    #[serde(default)]
    pub secrets: HashMap<String, SecretState>,
    /// 磁盘文件的 sha256 指纹（乐观并发的唯一凭据；文件不存在 = `"absent"`）。
    pub fingerprint: String,
    #[serde(default)]
    pub problems: Vec<SettingProblem>,
    /// 网关此刻是否处于 setup mode（降级启动）。
    #[serde(default)]
    pub setup_mode: bool,
    pub config_path: String,
}

/// GET /api/settings/status 响应——启动路径上的最便宜预检。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsStatusResponse {
    /// 当前配置是否可加载（`false` ⇒ 网关处于 setup mode）。
    pub valid: bool,
    pub setup_mode: bool,
    #[serde(default)]
    pub problems: Vec<SettingProblem>,
    #[serde(default)]
    pub fingerprint: Option<String>,
}

/// POST /api/settings/set 请求——全文档替换 + 乐观并发指纹。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsSetRequest {
    /// 保存基线指纹。`None` ⇒ **不做并发检查**（CLI `--force` 的路径）。
    ///
    /// 注意：`None` 会显式序列化成 `"base": null`（不省键）——§9 的 `base: str | None`
    /// 没有默认值，省键会被 pydantic 判 422。
    pub base: Option<String>,
    /// 稀疏文档（密文三态：`null` = 保留现值 / 字符串 = 设为该值 / 缺键 = 不被覆盖）。
    pub document: serde_json::Value,
}

/// POST /api/settings/set 响应——保存回执。
///
/// **校验失败不是 HTTP 4xx**：请求本身合法，是用户填的内容不合法，所以走
/// `HTTP 200 + ok=false + problems`（design.md D16）。HTTP 错误码只留给协议级失败
/// （409 指纹不匹配 / 401·403 鉴权 / 500 写盘失败）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsSetResponse {
    pub ok: bool,
    /// 落盘后的新指纹（`ok=false` 时是磁盘当前指纹）。
    pub fingerprint: String,
    #[serde(default)]
    pub problems: Vec<SettingProblem>,
    /// 相对保存前的变更路径（规范路径，含具体下标）。
    #[serde(default)]
    pub changed: Vec<String>,
    /// 变更里生效域为 `restart` 的路径（回执据此提示"需重启网关"）。
    #[serde(default)]
    pub restart_required: Vec<String>,
    /// 热重载逐项结果（复用 `/api/system/reload` 的形状）。
    #[serde(default)]
    pub reload: Option<ReloadResponse>,
    /// 这次保存是否让网关从 setup mode 就地转入正常模式。
    #[serde(default)]
    pub setup_mode_exited: bool,
    /// 备份文件路径（保存前复制的 `.bak`）；没有旧文件时为 `None`。
    #[serde(default)]
    pub backup_path: Option<String>,
    /// 保存过程中**必须告知用户**的非致命说明（AD13：当前文件无法解析时，
    /// 密文无从回填 —— 回执里要说明"旧密钥没有保留，请重新填写"）。
    /// 老网关不带这个字段，所以默认空。
    #[serde(default)]
    pub warnings: Vec<String>,
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
    fn session_list_entry_carries_the_model_quartet_and_tolerates_absent_fields() {
        // Current gateway: the list entry answers "which model does this session
        // run" without a per-session /info call.
        let json = r#"{
            "id": "20261009-210702-217d85f2",
            "name": "ps-model-display",
            "status": "inactive",
            "tags": [],
            "tag_meta": {},
            "model_id": "ds-flash",
            "model_name": "dfmodel-2026",
            "provider_name": "qoder",
            "model_display_name": "DeepSeek-Flash"
        }"#;
        let entry: SessionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(entry.model_id.as_deref(), Some("ds-flash"));
        assert_eq!(entry.model_name.as_deref(), Some("dfmodel-2026"));
        assert_eq!(entry.provider_name.as_deref(), Some("qoder"));
        assert_eq!(entry.model_display_name.as_deref(), Some("DeepSeek-Flash"));

        // Old gateway (before the model fields existed) → None, no error.
        let legacy = r#"{
            "id": "20260101-000000-abcdef01",
            "name": "legacy-gateway",
            "status": "working",
            "tags": ["pin"],
            "tag_meta": {}
        }"#;
        let entry: SessionInfo = serde_json::from_str(legacy).unwrap();
        assert_eq!(entry.model_id, None);
        assert_eq!(entry.model_name, None);
        assert_eq!(entry.provider_name, None);
        assert_eq!(entry.model_display_name, None);

        // Reachable gateway degradation: the call name survives while the
        // reference word is gone (the recorded id was deleted from config).
        // The display layer falls back on its own — `model_name` alone is
        // enough to say what the session runs.
        let degraded = r#"{"id": "s1", "model_name": "ghost-upstream"}"#;
        let entry: SessionInfo = serde_json::from_str(degraded).unwrap();
        assert_eq!(entry.model_id, None);
        assert_eq!(entry.model_name.as_deref(), Some("ghost-upstream"));
        assert_eq!(entry.model_display_name, None);
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

#[cfg(test)]
mod settings_tests {
    use super::*;
    use serde_json::Value;
    use serde_json::json;

    /// 全字段节点：§5.1 的**每个**字段都显式给出。
    ///
    /// 用它做「输入 ↔ 结构」的逐值 round-trip——键全在场时，解码 → 编码必须与输入
    /// **逐值相等**，这是"没有漏字段 / 没有改名 / 没有默认值污染"的机器证据。
    const FULL_SCALAR_NODE: &str = r#"{
        "key": "port",
        "path": "gateway.port",
        "title": "监听端口",
        "doc": "网关监听的 TCP 端口。",
        "notes": ["改它需要重启网关。", "0 会被操作系统当成随机端口。"],
        "example": "32523",
        "order": 7,
        "kind": "int",
        "required": true,
        "nullable": true,
        "default": 32523,
        "has_default": true,
        "min": 1.0,
        "max": 65535.0,
        "exclusive_min": true,
        "exclusive_max": false,
        "min_length": 1,
        "pattern": "^[0-9]+$",
        "choices": [{"value": "fast", "doc": "快速"}, {"value": "slow", "doc": null}],
        "min_items": 1,
        "max_items": 8,
        "secret": false,
        "apply": "restart",
        "editable": false,
        "deprecated": "改用 gateway.port_next",
        "section": "Gateway",
        "section_doc": "网关进程的绑定与超时。",
        "children": [],
        "element": null,
        "variants": null,
        "summary_fields": ["name", "protocol"],
        "value_hint": "color"
    }"#;

    /// 完整目录树样本：递归（providers → models → variants → capabilities）、三种列表
    /// （标量列表 / 对象列表 / union 列表）、四个生效域、密文、map、分组、readonly。
    const SCHEMA_FIXTURE: &str = r#"{
        "version": "0.4.1",
        "config_path": "/home/u/.wing/core/config.yaml",
        "groups": [
            {"id": "providers", "title": "Providers", "doc": "至少声明一个 provider。",
             "members": ["providers"]},
            {"id": "advanced", "title": "Advanced", "doc": "网关与日志。",
             "members": ["gateway", "log"]}
        ],
        "root": {
            "key": "config", "path": "config", "title": "Wing 配置", "doc": "网关的全部配置。",
            "kind": "object",
            "children": [
                {
                    "key": "providers", "path": "providers", "title": "Providers",
                    "doc": "模型 provider 声明。", "kind": "list", "min_items": 1,
                    "has_default": true, "order": 0,
                    "section": "Providers", "section_doc": "至少声明一个 provider。",
                    "element": {
                        "key": "[]", "path": "providers[]", "title": "Provider",
                        "doc": "一个 provider。", "kind": "object",
                        "summary_fields": ["name", "protocol", "base_url"],
                        "children": [
                            {"key": "name", "path": "providers[].name", "title": "名称",
                             "doc": "Provider 标识（全局唯一）。", "kind": "str", "required": true,
                             "pattern": "^[a-zA-Z0-9_-]+$", "example": "default",
                             "apply": "next_session", "order": 0},
                            {"key": "protocol", "path": "providers[].protocol", "title": "协议",
                             "doc": "协议类型。", "kind": "enum", "has_default": true,
                             "default": "openai", "order": 1,
                             "choices": [
                                 {"value": "openai", "doc": "OpenAI 兼容协议"},
                                 {"value": "anthropic", "doc": "Anthropic Messages 协议"}]},
                            {"key": "api_key", "path": "providers[].api_key", "title": "API 密钥",
                             "doc": "API 密钥。", "kind": "secret", "secret": true, "order": 2},
                            {"key": "timeout_first_chunk", "path": "providers[].timeout_first_chunk",
                             "title": "流式首块超时（秒）", "doc": "这是响应头超时。",
                             "notes": ["响应体停滞另有硬编码 120s 判定。"], "kind": "float",
                             "has_default": true, "default": 300.0, "min": 0.0,
                             "exclusive_min": true, "order": 3},
                            {"key": "extra_body", "path": "providers[].extra_body",
                             "title": "额外字段", "doc": "透传到 request body 顶层。",
                             "kind": "map", "has_default": true, "order": 4},
                            {
                                "key": "models", "path": "providers[].models", "title": "模型声明",
                                "doc": "模型声明（目录的唯一来源）。", "kind": "list",
                                "min_items": 1, "order": 5,
                                "variants": [
                                    {"key": "[]", "path": "providers[].models[]",
                                     "title": "模型 id", "doc": "简单形态：裸字符串。", "kind": "str"},
                                    {"key": "[]", "path": "providers[].models[]",
                                     "title": "模型声明", "doc": "完整形态：对象。", "kind": "object",
                                     "children": [
                                         {"key": "id", "path": "providers[].models[].id",
                                          "title": "引用词", "doc": "全局唯一引用词。",
                                          "kind": "str", "required": true},
                                         {"key": "display_name",
                                          "path": "providers[].models[].display_name",
                                          "title": "展示名", "doc": "人类可读展示名。",
                                          "kind": "str", "nullable": true},
                                         {"key": "capabilities",
                                          "path": "providers[].models[].capabilities",
                                          "title": "能力", "doc": "声明能力。", "kind": "object",
                                          "children": [
                                              {"key": "vision",
                                               "path": "providers[].models[].capabilities.vision",
                                               "title": "视觉", "doc": "是否支持图片输入。",
                                               "kind": "bool", "has_default": true,
                                               "default": false}]}
                                     ]}
                                ]
                            }
                        ]
                    }
                },
                {
                    "key": "gateway", "path": "gateway", "title": "Gateway",
                    "doc": "网关进程。", "kind": "object", "section": "Gateway", "order": 1,
                    "children": [
                        {"key": "host", "path": "gateway.host", "title": "监听地址",
                         "doc": "监听地址。", "kind": "str", "has_default": true,
                         "default": "127.0.0.1", "apply": "restart", "order": 0},
                        {"key": "port", "path": "gateway.port", "title": "端口",
                         "doc": "监听端口。", "kind": "int", "has_default": true,
                         "default": 32523, "min": 1.0, "max": 65535.0, "apply": "restart",
                         "order": 1},
                        {"key": "auth", "path": "gateway.auth", "title": "鉴权",
                         "doc": "API key 鉴权。", "kind": "object", "order": 2,
                         "children": [
                             {"key": "enabled", "path": "gateway.auth.enabled", "title": "启用",
                              "doc": "是否启用。", "kind": "bool", "has_default": true,
                              "default": false},
                             {"key": "keys", "path": "gateway.auth.keys", "title": "密钥",
                              "doc": "允许的 key。", "kind": "list",
                              "element": {"key": "[]", "path": "gateway.auth.keys[]",
                                          "title": "密钥项", "doc": "一个 key。", "kind": "object",
                                          "children": [
                                              {"key": "name", "path": "gateway.auth.keys[].name",
                                               "title": "名字", "doc": "标识。", "kind": "str"},
                                              {"key": "key", "path": "gateway.auth.keys[].key",
                                               "title": "key", "doc": "密钥。", "kind": "secret",
                                               "secret": true}]}}
                         ]}
                    ]
                },
                {
                    "key": "log", "path": "log", "title": "Logging", "doc": "日志。",
                    "kind": "object", "section": "Logging", "order": 2,
                    "children": [
                        {"key": "level", "path": "log.level", "title": "级别", "doc": "日志级别。",
                         "kind": "enum", "has_default": true, "default": "INFO", "apply": "hot",
                         "choices": [{"value": "DEBUG", "doc": null}, {"value": "INFO", "doc": null},
                                     {"value": "WARNING", "doc": null},
                                     {"value": "ERROR", "doc": null}]},
                        {"key": "file", "path": "log.file", "title": "日志文件",
                         "doc": "由启动参数决定。", "kind": "str", "apply": "readonly",
                         "editable": false, "deprecated": "改用 WING_LOG_DIR"}
                    ]
                }
            ]
        }
    }"#;

    fn schema() -> SettingsSchemaResponse {
        serde_json::from_str(SCHEMA_FIXTURE).expect("schema fixture 必须是合法响应")
    }

    // ── 模型形状 ──────────────────────────────────────────────

    #[test]
    fn full_scalar_node_round_trips_field_by_field() {
        let input: Value = serde_json::from_str(FULL_SCALAR_NODE).unwrap();
        let node: SettingNode = serde_json::from_str(FULL_SCALAR_NODE).unwrap();

        // 身份
        assert_eq!(node.key, "port");
        assert_eq!(node.path, "gateway.port");
        assert_eq!(node.title, "监听端口");
        assert_eq!(node.doc, "网关监听的 TCP 端口。");
        assert_eq!(
            node.notes,
            vec![
                "改它需要重启网关。".to_string(),
                "0 会被操作系统当成随机端口。".to_string()
            ]
        );
        assert_eq!(node.example.as_deref(), Some("32523"));
        assert_eq!(node.order, 7);
        // 类型与约束
        assert_eq!(node.kind, SettingKind::Int);
        assert!(node.required);
        assert!(node.nullable);
        assert_eq!(node.default, Some(json!(32523)));
        assert!(node.has_default);
        assert_eq!(node.min, Some(1.0));
        assert_eq!(node.max, Some(65535.0));
        assert!(node.exclusive_min);
        assert!(!node.exclusive_max);
        assert_eq!(node.min_length, Some(1));
        assert_eq!(node.pattern.as_deref(), Some("^[0-9]+$"));
        assert_eq!(node.choices.len(), 2);
        assert_eq!(node.choices[0].value, "fast");
        assert_eq!(node.choices[0].doc.as_deref(), Some("快速"));
        assert_eq!(node.choices[1].value, "slow");
        assert_eq!(node.choices[1].doc, None);
        assert_eq!(node.min_items, Some(1));
        assert_eq!(node.max_items, Some(8));
        // 语义 / 分组
        assert!(!node.secret);
        assert_eq!(node.apply, ApplyScope::Restart);
        assert!(!node.editable);
        assert_eq!(node.deprecated.as_deref(), Some("改用 gateway.port_next"));
        assert_eq!(node.section.as_deref(), Some("Gateway"));
        assert_eq!(node.section_doc.as_deref(), Some("网关进程的绑定与超时。"));
        // 结构 / 渲染
        assert!(node.children.is_empty());
        assert!(node.element.is_none());
        assert!(node.variants.is_none());
        assert_eq!(
            node.summary_fields,
            vec!["name".to_string(), "protocol".to_string()]
        );
        assert_eq!(node.value_hint.as_deref(), Some("color"));

        // 键全在场时：解码 → 编码与输入逐值相等（漏字段 / 改名 / 默认值污染都过不了这关）
        assert_eq!(serde_json::to_value(&node).unwrap(), input);
        // 文本往返（结构视图）仍然相等
        let again: SettingNode =
            serde_json::from_str(&serde_json::to_string(&node).unwrap()).unwrap();
        assert_eq!(again, node);
    }

    #[test]
    fn absent_optional_keys_fall_back_to_the_declared_defaults() {
        // 只有必填键（key / path / title / doc / kind）——旧网关或最小实现的形状。
        let json = r#"{"key": "yolo", "path": "yolo", "title": "Yolo",
                       "doc": "免确认执行。", "kind": "bool"}"#;
        let node: SettingNode = serde_json::from_str(json).unwrap();

        assert!(node.notes.is_empty());
        assert!(node.example.is_none());
        assert_eq!(node.order, 0);
        assert!(!node.required && !node.nullable && !node.has_default);
        assert!(node.default.is_none() && node.min.is_none() && node.max.is_none());
        assert!(!node.exclusive_min && !node.exclusive_max);
        assert!(node.min_length.is_none() && node.min_items.is_none() && node.max_items.is_none());
        assert!(node.choices.is_empty() && node.pattern.is_none());
        assert!(!node.secret);
        assert_eq!(node.apply, ApplyScope::Hot, "§9: apply 缺省 = hot");
        assert!(node.editable, "§9: editable 缺省 = true");
        assert!(node.deprecated.is_none() && node.section.is_none() && node.section_doc.is_none());
        assert!(node.children.is_empty() && node.element.is_none() && node.variants.is_none());
        assert!(node.summary_fields.is_empty() && node.value_hint.is_none());
    }

    #[test]
    fn schema_response_decodes_the_recursive_catalog() {
        let resp = schema();
        assert_eq!(resp.version, "0.4.1");
        assert_eq!(resp.config_path, "/home/u/.wing/core/config.yaml");

        let root = &resp.root;
        assert_eq!(root.kind, SettingKind::Object);
        assert_eq!(root.key, "config");
        assert_eq!(
            root.path, "config",
            "P2：真实网关的根节点 key == path == \"config\""
        );
        assert_eq!(root.display_label(), "Wing 配置");
        assert!(root.is_structural());

        let providers = root.node_at("providers").unwrap();
        assert_eq!(providers.kind, SettingKind::List);
        assert_eq!(providers.min_items, Some(1));
        assert!(providers.has_default, "list 也可以有默认值（默认工厂）");
        assert_eq!(providers.section.as_deref(), Some("Providers"));
        assert_eq!(
            providers.section_doc.as_deref(),
            Some("至少声明一个 provider。")
        );
        assert!(providers.variants.is_none(), "单一元素类型的列表走 element");

        let element = providers.element.as_deref().unwrap();
        assert_eq!(element.key, "[]");
        assert_eq!(element.path, "providers[]");
        assert_eq!(
            element.summary_fields,
            vec![
                "name".to_string(),
                "protocol".to_string(),
                "base_url".to_string()
            ]
        );

        // 深层标量：path 里 [] 与 [i] 混用；kind / 默认值 / 约束如实解码
        let models = root.node_at("providers[0].models").unwrap();
        assert_eq!(models.path, "providers[].models");
        assert!(models.element.is_none(), "union 元素的列表走 variants");
        let variants = models.variants.as_deref().unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].kind, SettingKind::Str);
        assert_eq!(variants[1].kind, SettingKind::Object);
        assert_eq!(variants[0].path, "providers[].models[]");
        assert_eq!(variants[1].path, "providers[].models[]");

        let object_variant = models.select_variant(&json!({"id": "ds-flash"})).unwrap();
        let vision = object_variant.node_at("capabilities.vision").unwrap();
        assert_eq!(vision.path, "providers[].models[].capabilities.vision");
        assert_eq!(vision.kind, SettingKind::Bool);
        assert_eq!(vision.default, Some(json!(false)));

        // map 与密文
        assert_eq!(
            root.node_at("providers[0].extra_body").unwrap().kind,
            SettingKind::Map
        );
        assert!(root.node_at("providers[0].api_key").unwrap().secret);
        assert_eq!(
            root.node_at("gateway.auth.keys[0].key").unwrap().kind,
            SettingKind::Secret
        );

        // 四个生效域都到位
        assert_eq!(
            root.node_at("providers[0].name").unwrap().apply,
            ApplyScope::NextSession
        );
        assert_eq!(
            root.node_at("gateway.port").unwrap().apply,
            ApplyScope::Restart
        );
        assert_eq!(root.node_at("log.level").unwrap().apply, ApplyScope::Hot);
        let log_file = root.node_at("log.file").unwrap();
        assert_eq!(log_file.apply, ApplyScope::Readonly);
        assert!(!log_file.editable && log_file.deprecated.is_some());

        // 分组表（面板左列锚点的唯一来源）：顺序 / id / 成员都在 wire 上。
        assert_eq!(resp.groups.len(), 2);
        assert_eq!(resp.groups[0].id, "providers");
        assert_eq!(resp.groups[0].title, "Providers");
        assert_eq!(resp.groups[0].doc, "至少声明一个 provider。");
        assert_eq!(resp.groups[0].members, vec!["providers".to_string()]);
        assert_eq!(
            resp.groups[1].members,
            vec!["gateway".to_string(), "log".to_string()]
        );

        // 整棵树的不动点：编码 → 解码 → 编码必须逐值相同（字段名双向一致、无字段丢失）
        let v1 = serde_json::to_value(&resp).unwrap();
        let again: SettingsSchemaResponse = serde_json::from_value(v1.clone()).unwrap();
        assert_eq!(serde_json::to_value(&again).unwrap(), v1);
    }

    #[test]
    fn a_schema_without_groups_decodes_to_an_empty_table() {
        // 老网关不发 groups[]：`#[serde(default)]` 让它成为空表（面板据此走 section 推导）。
        let json = r#"{
            "version": "0.4.0",
            "config_path": "/home/u/.wing/core/config.yaml",
            "root": {"key": "config", "path": "config", "title": "配置", "doc": "",
                     "kind": "object"}
        }"#;
        let resp: SettingsSchemaResponse = serde_json::from_str(json).unwrap();
        assert!(resp.groups.is_empty());
    }

    // ── 前向兼容 ──────────────────────────────────────────────

    #[test]
    fn unknown_enum_values_and_extra_fields_do_not_break_decoding() {
        // "来自更新版网关"的节点：不认识的 kind / apply / 字段，但已知字段必须照常解出来。
        let json = r#"{
            "key": "shiny", "path": "shiny", "title": "T", "doc": "D",
            "kind": "quantum", "apply": "eventually", "editable": true,
            "notes": ["旧版本读不了这个 kind，但要照常显示说明。"],
            "min": 1.5, "section": "Future",
            "future_field": {"nested": [1, 2, 3]},
            "children": [
                {"key": "n", "path": "shiny.n", "title": "N", "doc": "D", "kind": "int"},
                {"key": "hatch", "path": "shiny.hatch", "title": "H", "doc": "D",
                 "kind": "hologram"}
            ]
        }"#;
        let node: SettingNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.kind, SettingKind::Unknown("quantum".into()));
        assert_eq!(node.kind.as_str(), "quantum");
        assert_eq!(node.apply, ApplyScope::Unknown("eventually".into()));
        assert_eq!(node.apply.as_str(), "eventually");
        // 同一个 payload 里的已知字段不受影响
        assert_eq!(node.notes.len(), 1);
        assert_eq!(node.min, Some(1.5));
        assert_eq!(node.section.as_deref(), Some("Future"));
        assert!(node.editable);
        assert_eq!(node.children.len(), 2);
        assert_eq!(node.children[0].kind, SettingKind::Int);
        assert_eq!(
            node.children[1].kind,
            SettingKind::Unknown("hologram".into())
        );
        // 未知值原样回写（拼写不丢：诊断与 round-trip 都需要它）
        let encoded = serde_json::to_value(&node).unwrap();
        assert_eq!(encoded["kind"], json!("quantum"));
        assert_eq!(encoded["apply"], json!("eventually"));
        assert_eq!(encoded["children"][1]["kind"], json!("hologram"));

        // 密文状态同样容错
        let state: SecretState =
            serde_json::from_str(r#"{"state": "rotated", "hint": "ab12"}"#).unwrap();
        assert_eq!(state.state, SecretPresence::Unknown("rotated".into()));
        assert_eq!(state.state.as_str(), "rotated");
        assert_eq!(state.hint.as_deref(), Some("ab12"));

        // 已知拼写仍走已知变体（容错不能吞掉正常值）
        assert_eq!(SettingKind::from_wire("secret"), SettingKind::Secret);
        assert_eq!(SettingKind::from_wire("enum"), SettingKind::Enum);
        assert_eq!(ApplyScope::from_wire("hot"), ApplyScope::Hot);
        assert_eq!(ApplyScope::from_wire("readonly"), ApplyScope::Readonly);
        assert_eq!(SecretPresence::from_wire("absent"), SecretPresence::Absent);
        assert_eq!(SecretPresence::from_wire("empty"), SecretPresence::Empty);
        assert_eq!(SecretPresence::from_wire("set"), SecretPresence::Set);
    }

    #[test]
    fn responses_tolerate_extra_fields_and_unknown_problem_kinds() {
        // 新网关在响应里加了字段 / 新问题种类：旧面板必须照常读到已知字段。
        let resp: SettingsGetResponse = serde_json::from_str(
            r#"{
                "values": {"gateway": {"port": 1}},
                "fingerprint": "sha256:0",
                "config_path": "/tmp/c.yaml",
                "future_top_level": true,
                "problems": [{"path": "gateway.port", "kind": "brand_new_kind",
                              "message": "m", "hint": null, "code": 7}]
            }"#,
        )
        .unwrap();
        assert_eq!(resp.fingerprint, "sha256:0");
        assert_eq!(resp.values["gateway"]["port"], json!(1));
        assert_eq!(resp.problems[0].kind, "brand_new_kind");
        assert_eq!(resp.problems[0].hint, None);

        let status: SettingsStatusResponse = serde_json::from_str(
            r#"{"valid": true, "setup_mode": false, "problems": [], "fingerprint": "sha256:1",
                "extra": [1]}"#,
        )
        .unwrap();
        assert!(status.valid);
        assert_eq!(status.fingerprint.as_deref(), Some("sha256:1"));

        let set: SettingsSetResponse =
            serde_json::from_str(r#"{"ok": true, "fingerprint": "sha256:2", "future": "ok"}"#)
                .unwrap();
        assert!(set.ok);
        assert_eq!(set.fingerprint, "sha256:2");
    }

    // ── 路径文法与寻址 ────────────────────────────────────────

    #[test]
    fn parse_path_enforces_the_frozen_grammar() {
        assert_eq!(
            parse_path("gateway.port").unwrap(),
            vec![
                PathStep::Key("gateway".into()),
                PathStep::Key("port".into())
            ]
        );
        assert_eq!(
            parse_path("providers[0].models[]").unwrap(),
            vec![
                PathStep::Key("providers".into()),
                PathStep::Index(0),
                PathStep::Key("models".into()),
                PathStep::Element,
            ]
        );
        assert_eq!(
            parse_path("_a1").unwrap(),
            vec![PathStep::Key("_a1".into())]
        );
        assert_eq!(parse_path("").unwrap(), Vec::new());
        for bad in [
            "a.",
            ".a",
            "a..b",
            "a[",
            "a]",
            "a[x]",
            "a[0",
            "a[0]x",
            "a[]]",
            "a[+1]",
            "a[-1]",
            "1a",
            "a-b",
            "a[999999999999999999999999]",
        ] {
            assert!(parse_path(bad).is_none(), "{bad} 必须判非法");
        }
    }

    #[test]
    fn node_at_follows_the_frozen_path_grammar() {
        let root = schema().root;

        // 具体下标 / 模板下标 / 多级 / 嵌套对象
        assert_eq!(root.node_at("gateway.port").unwrap().kind, SettingKind::Int);
        assert_eq!(
            root.node_at("gateway.auth.keys[1].key").unwrap().path,
            "gateway.auth.keys[].key"
        );
        assert_eq!(
            root.node_at("providers[3].name").unwrap().path,
            "providers[].name"
        );
        assert_eq!(
            root.node_at("gateway.auth.keys[].name").unwrap().path,
            "gateway.auth.keys[].name"
        );
        // 根名是可选的路径前缀（P2：根节点的 key == path == "config"，两种写法都得能寻址）
        assert_eq!(
            root.node_at("config.gateway.port").unwrap().path,
            "gateway.port"
        );
        assert_eq!(
            root.node_at("gateway.port").unwrap().path,
            "gateway.port",
            "无前缀同样能寻址"
        );
        assert_eq!(root.node_at("config").unwrap().key, "config");
        // 空路径 = 根自己
        assert_eq!(root.node_at("").unwrap().key, "config");

        // 不存在的段 / 下标挂在非列表上
        assert!(root.node_at("gateway.nope").is_none());
        assert!(root.node_at("nope").is_none());
        assert!(root.node_at("gateway[0]").is_none());
        assert!(root.node_at("providers[0].nope").is_none());

        // variants 列表的元素形态无法由 catalog 单独判定 → None（用 select_variant）
        assert!(root.node_at("providers[0].models[0]").is_none());
        assert!(root.node_at("providers[0].models[0].id").is_none());
        assert!(root.node_at("providers[].models[]").is_none());

        // 下标不查界：catalog 是模板，不知道文档有多长
        assert_eq!(
            root.node_at("providers[9].api_key").unwrap().path,
            "providers[].api_key"
        );
    }

    #[test]
    fn select_variant_picks_the_shape_in_declaration_order() {
        let root = schema().root;
        let models = root.node_at("providers[0].models").unwrap();

        let bare = models.select_variant(&json!("ds-flash")).unwrap();
        assert_eq!(bare.kind, SettingKind::Str);
        let object = models.select_variant(&json!({"id": "ds-flash"})).unwrap();
        assert_eq!(object.kind, SettingKind::Object);
        // 两种形态都不像的值 / 单一元素形态的列表 → 选不出来
        assert!(models.select_variant(&json!([1, 2])).is_none());
        assert!(
            root.node_at("gateway.auth.keys")
                .unwrap()
                .select_variant(&json!({}))
                .is_none(),
            "element 列表没有 variants"
        );
    }

    #[test]
    fn display_label_and_is_structural_are_declaration_level_views() {
        let root = schema().root;
        assert_eq!(root.display_label(), "Wing 配置");
        assert_eq!(
            root.node_at("providers").unwrap().display_label(),
            "Providers"
        );
        assert!(root.is_structural());
        assert!(root.node_at("providers").unwrap().is_structural());
        assert!(!root.node_at("gateway.port").unwrap().is_structural());

        // 空 title / 纯空白 title 回落 key；未知 kind 不是结构节点
        let mut odd: SettingNode = serde_json::from_str(
            r#"{"key": "k", "path": "k", "title": "", "doc": "d", "kind": "quantum"}"#,
        )
        .unwrap();
        assert_eq!(odd.display_label(), "k");
        assert!(!odd.is_structural());
        odd.title = "   ".into();
        assert_eq!(odd.display_label(), "k");
        odd.title = "真标签".into();
        assert_eq!(odd.display_label(), "真标签");
    }

    #[test]
    fn leaves_are_depth_first_and_descend_into_elements_and_variants() {
        let node: SettingNode = serde_json::from_str(
            r#"{
                "key": "root", "path": "", "title": "R", "doc": "D", "kind": "object",
                "children": [
                    {"key": "a", "path": "a", "title": "A", "doc": "D", "kind": "str"},
                    {"key": "items", "path": "items", "title": "I", "doc": "D", "kind": "list",
                     "element": {"key": "[]", "path": "items[]", "title": "E", "doc": "D",
                                 "kind": "object",
                                 "children": [{"key": "flag", "path": "items[].flag",
                                               "title": "F", "doc": "D", "kind": "bool"}]}},
                    {"key": "mixed", "path": "mixed", "title": "M", "doc": "D", "kind": "list",
                     "variants": [
                         {"key": "[]", "path": "mixed[]", "title": "S", "doc": "D", "kind": "str"},
                         {"key": "[]", "path": "mixed[]", "title": "O", "doc": "D",
                          "kind": "object",
                          "children": [{"key": "n", "path": "mixed[].n", "title": "N",
                                        "doc": "D", "kind": "int"}]}]},
                    {"key": "empty_obj", "path": "empty_obj", "title": "EO", "doc": "D",
                     "kind": "object"},
                    {"key": "degenerate_list", "path": "degenerate_list", "title": "DL",
                     "doc": "D", "kind": "list"}
                ]
            }"#,
        )
        .unwrap();

        let paths: Vec<&str> = node.leaves().map(|leaf| leaf.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "a",
                "items[].flag",
                "mixed[]",
                "mixed[].n",
                "empty_obj",
                "degenerate_list",
            ]
        );
        assert_eq!(node.leaves().count(), 6);

        // 目录 fixture 的叶子数：providers 下的标量（name/protocol/api_key/
        // timeout_first_chunk/extra_body）+ models 两个 variant 的叶子 + gateway 下的
        // （host/port/enabled/keys 两项）+ log 下的两项。
        assert_eq!(schema().root.leaves().count(), 16);
    }

    // ── 响应 / 请求形状 ───────────────────────────────────────

    /// AD13：`warnings` 是**后加字段**，两种响应都要能解码 —— 带它的（新网关
    /// 修好了坏文件）与不带它的（老网关 / 一切正常）在 Rust 侧都是合法形状。
    #[test]
    fn settings_set_response_decodes_with_and_without_warnings() {
        let with_warnings = r#"{
            "ok": true,
            "fingerprint": "sha256:new",
            "problems": [],
            "changed": ["providers[0].api_key"],
            "restart_required": [],
            "reload": {"ok": true, "results": []},
            "setup_mode_exited": true,
            "backup_path": "/home/u/.wing/core/config.yaml.bak",
            "warnings": ["原配置文件无法解析，其中的密钥无法保留，请重新填写"]
        }"#;
        let resp: SettingsSetResponse = serde_json::from_str(with_warnings).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.warnings.len(), 1);
        assert!(resp.warnings[0].contains("密钥无法保留"));
        assert_eq!(
            resp.backup_path.as_deref(),
            Some("/home/u/.wing/core/config.yaml.bak"),
            "坏文件照常备份（AD13）"
        );

        let without = r#"{
            "ok": false,
            "fingerprint": "sha256:cur",
            "problems": [{"path": "gateway.port", "kind": "invalid_value",
                          "message": "端口被占用", "hint": null}]
        }"#;
        let resp: SettingsSetResponse = serde_json::from_str(without).unwrap();
        assert!(!resp.ok);
        assert!(resp.warnings.is_empty(), "缺字段 = 没有 warning");
        assert!(resp.changed.is_empty());
        assert!(resp.reload.is_none());
    }

    #[test]
    fn settings_get_response_masks_secrets_and_keeps_the_null_marker() {
        let json = r#"{
            "values": {
                "providers": [{"name": "default", "api_key": null, "models": ["ds-flash"]}],
                "gateway": {"port": 32523}
            },
            "secrets": {
                "providers[0].api_key": {"state": "set", "hint": "ab12"},
                "providers[1].api_key": {"state": "empty", "hint": null},
                "providers[2].api_key": {"state": "absent"}
            },
            "fingerprint": "sha256:1111",
            "problems": [{"path": "providers[0].models", "kind": "empty_list",
                          "message": "providers[0].models 不得为空",
                          "hint": "至少声明一个模型"}],
            "setup_mode": false,
            "config_path": "/home/u/.wing/core/config.yaml"
        }"#;
        let resp: SettingsGetResponse = serde_json::from_str(json).unwrap();

        assert_eq!(resp.fingerprint, "sha256:1111");
        assert!(!resp.setup_mode);
        assert_eq!(resp.config_path, "/home/u/.wing/core/config.yaml");
        // 密文叶子是 null —— "原样回传 null = 保留磁盘现值"这条契约的可判定形态
        assert!(resp.values["providers"][0]["api_key"].is_null());
        assert_eq!(resp.values["gateway"]["port"], json!(32523));
        assert_eq!(resp.secrets.len(), 3);
        assert_eq!(
            resp.secrets["providers[0].api_key"].state,
            SecretPresence::Set
        );
        assert_eq!(
            resp.secrets["providers[0].api_key"].hint.as_deref(),
            Some("ab12")
        );
        assert_eq!(
            resp.secrets["providers[1].api_key"].state,
            SecretPresence::Empty
        );
        assert_eq!(resp.secrets["providers[1].api_key"].hint, None);
        assert_eq!(
            resp.secrets["providers[2].api_key"].state,
            SecretPresence::Absent
        );
        assert_eq!(resp.problems.len(), 1);
        assert_eq!(
            resp.problems[0].path.as_deref(),
            Some("providers[0].models")
        );
        assert_eq!(resp.problems[0].kind, "empty_list");
        assert_eq!(resp.problems[0].hint.as_deref(), Some("至少声明一个模型"));
    }

    #[test]
    fn status_and_set_responses_tolerate_the_minimal_shapes() {
        // status：只有必填键；fingerprint 缺席 = None（新网关才有指纹）
        let status: SettingsStatusResponse =
            serde_json::from_str(r#"{"valid": false, "setup_mode": true}"#).unwrap();
        assert!(!status.valid && status.setup_mode);
        assert!(status.problems.is_empty());
        assert!(status.fingerprint.is_none());

        // 保存失败：HTTP 200 + ok=false + problems（D16）；文档级问题的 path 为 null
        let resp: SettingsSetResponse = serde_json::from_str(
            r#"{
                "ok": false,
                "fingerprint": "sha256:2222",
                "problems": [{"path": null, "kind": "parse_error", "message": "YAML 语法错"}]
            }"#,
        )
        .unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.problems[0].path, None);
        assert_eq!(resp.problems[0].hint, None);
        assert!(resp.changed.is_empty() && resp.restart_required.is_empty());
        assert!(resp.reload.is_none() && !resp.setup_mode_exited && resp.backup_path.is_none());

        // 保存成功：reload 复用既有 ReloadResponse 形状（name/ok/detail 逐项）
        let resp: SettingsSetResponse = serde_json::from_str(
            r#"{
                "ok": true,
                "fingerprint": "sha256:3333",
                "changed": ["gateway.port", "providers[0].api_key"],
                "restart_required": ["gateway.port"],
                "reload": {"ok": true, "results": [
                    {"name": "config.yaml", "ok": true, "detail": "已写入"},
                    {"name": "hooks", "ok": true, "detail": null}]},
                "setup_mode_exited": false,
                "backup_path": "/home/u/.wing/core/config.yaml.bak"
            }"#,
        )
        .unwrap();
        assert!(resp.ok && !resp.setup_mode_exited);
        assert_eq!(
            resp.changed,
            vec![
                "gateway.port".to_string(),
                "providers[0].api_key".to_string()
            ]
        );
        assert_eq!(resp.restart_required, vec!["gateway.port".to_string()]);
        let reload = resp.reload.expect("reload 必须复用 ReloadResponse");
        assert!(reload.ok);
        assert_eq!(reload.results[0].name, "config.yaml");
        assert_eq!(reload.results[1].detail, None);
        assert_eq!(
            resp.backup_path.as_deref(),
            Some("/home/u/.wing/core/config.yaml.bak")
        );
    }

    #[test]
    fn settings_set_request_serializes_base_explicitly() {
        let req = SettingsSetRequest {
            base: Some("sha256:abc".into()),
            document: json!({"gateway": {"port": 32523}}),
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"base": "sha256:abc", "document": {"gateway": {"port": 32523}}})
        );

        // base=None 必须显式写成 null（省键会被 §9 的 `base: str | None` 判 422）
        let forced = SettingsSetRequest {
            base: None,
            document: json!({}),
        };
        assert_eq!(
            serde_json::to_string(&forced).unwrap(),
            r#"{"base":null,"document":{}}"#
        );
    }
}
