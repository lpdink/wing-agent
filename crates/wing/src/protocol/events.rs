//! WingEvent enum and all associated types.
//!
//! Mirrors:
//!   - wing/event/base.py
//!   - wing/event/react.py
//!   - wing/event/state_change.py
//!   - wing/event/query_response.py

use serde::Deserialize;
use serde::Serialize;

// ============================================================
// Shared / nested types
// ============================================================

/// Common metadata present on every event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMeta {
    /// UTC ISO-8601 timestamp string.
    pub created_at: String,
    /// Session this event belongs to. `#[serde(default)]` makes the tolerance
    /// explicit: the wire serializer strips null fields, so global events (no
    /// session) arrive without this key — serde already maps a missing
    /// `Option<T>` to `None`, this documents the intent.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Unique request correlation id.
    pub request_id: String,
}

/// `#[serde(default = …)]` for the diff window's absolute start lines: an
/// absent field means "starts at line 1" — which is exactly what the
/// pre-windowing payloads (whole file, both sides from line 1) carried.
pub(crate) fn first_line() -> usize {
    1
}

/// Routing target injected by EventBus — the TUI can ignore this but must
/// tolerate it in the JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTarget {
    pub scope: String,
    #[serde(default)]
    pub client_ids: Vec<String>,
}

/// Session runtime status — mirror of `wing/event/base.py::SessionStatus`.
///
/// The *authoritative* answer to "is a turn in flight" — carried by the
/// `sync_session` snapshot. Never infer that from uncommitted content: an LLM
/// call in flight (before its first finalized block) projects no content while
/// the turn is very much running.
///
/// Strict on purpose: the gateway sends this vocabulary and nothing else (CLI
/// and gateway ship as one version), so an unknown value is a version mismatch
/// and fails the decode instead of being papered over.
///
/// The same vocabulary describes `/api/session/list` rows ([`Self::parse`] —
/// the picker renders it); this is the single definition for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Loaded nowhere — session-list vocabulary, never seen in a sync payload.
    Inactive,
    /// Loaded, no turn in flight.
    Idle,
    /// A turn is in flight.
    Working,
    /// A turn is in flight, blocked on user input (pending ask).
    Waiting,
}

impl SessionStatus {
    /// Parse the session-list row's status string.
    ///
    /// `None` for a value this build does not know: the list is a display-only
    /// surface, so how to show it is the caller's (rendering) decision — the
    /// sync payload's `status`, by contrast, is typed strictly.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "inactive" => Some(Self::Inactive),
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            "waiting" => Some(Self::Waiting),
            _ => None,
        }
    }

    /// Whether a turn is in flight — the only question a mid-join subscriber
    /// asks of the snapshot. `waiting` counts: the turn runs, blocked on a
    /// pending ask (the live path keeps the spinner up for it too).
    pub fn turn_in_flight(self) -> bool {
        matches!(self, Self::Working | Self::Waiting)
    }
}

/// Agent configuration snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub model_name: String,
    /// 模型的**引用词**（∈ 配置声明的 id 空间）；不可用时缺席（serde → None）。
    /// 前端的选择态 / 匹配以它为准（展示回落 `model_display_name`‖`model_name`）。
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
    /// Active provider name; absent on old gateways (serde → None).
    #[serde(default)]
    pub provider_name: Option<String>,
    /// Display label declared for `model_name` (gateway config); absent on old
    /// gateways / undeclared models (serde → None). Display-only — identity is
    /// `model_id`.
    #[serde(default)]
    pub model_display_name: Option<String>,
}

/// A selectable option in an Ask question: label + optional description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// A single question in a multi-question Ask event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    /// Very short label rendered as the question tab (falls back to `id`).
    #[serde(default)]
    pub header: String,
    /// true = the user may toggle several options.
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
    /// Selectable options; empty = free-form only.
    #[serde(default)]
    pub options: Vec<AskOption>,
    /// Legacy plain-string choices (old gateway records) — normalized into
    /// `options` by `AskPanel::new`, kept for replay compatibility.
    #[serde(default)]
    pub choices: Vec<String>,
}

impl AskQuestion {
    /// Effective tab label: header, falling back to id.
    pub fn tab_label(&self) -> &str {
        if self.header.trim().is_empty() {
            &self.id
        } else {
            &self.header
        }
    }
}

/// Magic command metadata for command list.
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

/// Session summary info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub created_at: Option<String>,
    pub template_name: Option<String>,
    pub workspace: Option<String>,
    pub last_interaction: Option<String>,
}

/// Branch target for /rewind and /fork.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTargetInfo {
    pub uuid: String,
    pub content: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "user".into()
}

/// 一条媒体引用（`tool_call_result` 追加的 `tool_media` 项）——镜像
/// Python `wing.schema.MediaRef` 的投影。
///
/// 放在 `protocol/events.rs` 而非 `protocol/history.rs`：唯一消费点是 WS
/// 事件镜像；`SessionMessage`（history 投影）本期保持原样，改动面收敛在
/// 单文件内。
///
/// 容忍策略：整个 `tool_media` 键在旧网关上缺席；`name` 还可能缺键、也可能
/// 为 `null`（wire 只剥离顶层 null，嵌套 null 原样在线）。条目级容错见
/// `deserialize_tool_media`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMediaRef {
    /// 图片字节的 sha256 hex——即存储 id。
    pub id: String,
    /// `image/png` | `image/jpeg` | `image/webp` | `image/gif`。
    pub mime: String,
    /// 原始字节数。
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    /// 展示名（basename，不含目录）；旧载荷上缺席。
    #[serde(default)]
    pub name: Option<String>,
}

/// `tool_media` 的宽容解码，两层：
///
/// - `null` / 缺席 / 非数组形状 → 空表（旧网关只发缺键；中间代理可能发 null）；
/// - 数组里单条畸形（缺字段 / 类型不符）→ **只跳过该条**，保留其余合法条目
///   与整帧事件——一条坏数据不应让 `tool_call_result` 整帧消失（卡片会永远
///   停在 Pending、结果行不出现）。
fn deserialize_tool_media<'de, D>(deserializer: D) -> Result<Vec<SessionMediaRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::Array(items)) = raw else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(
            |item| match serde_json::from_value::<SessionMediaRef>(item) {
                Ok(media) => Some(media),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed tool_media entry");
                    None
                }
            },
        )
        .collect())
}

// ============================================================
// WingEvent — the main event enum
// ============================================================

/// All events from wing Gateway, discriminated by the `type` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WingEvent {
    // ---- base ----
    /// Error event.
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default = "default_status_code")]
        status_code: i32,
        error_code: Option<String>,
        detail: Option<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Request delivered confirmation.
    #[serde(rename = "delivered")]
    Delivered {
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// One-shot notice (retry in progress, degradation, …).
    ///
    /// Mirrors `wing/event/base.py::NoticeEvent`. It is **not** an error and
    /// **not** an end-of-turn signal: the TUI renders it as a system message
    /// and keeps the turn running (unlike `Error`, which finishes the turn).
    /// Broadcast-only (`persist=false`) — never replayed from history.
    #[serde(rename = "notice")]
    Notice {
        /// `info` | `warning` | `error` (unknown values degrade to `info`).
        #[serde(default = "default_notice_level")]
        level: String,
        #[serde(default)]
        message: String,
        /// Retry progress (1-based attempt); absent for other notices.
        #[serde(default)]
        attempt: Option<i64>,
        #[serde(default)]
        max_attempts: Option<i64>,
        /// Backoff before the next attempt, in seconds.
        #[serde(default)]
        retry_in_s: Option<f64>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- react ----
    /// Assistant text output (streaming chunks).
    #[serde(rename = "text")]
    Text {
        content: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Reasoning / thinking output.
    #[serde(rename = "reasoning")]
    Reasoning {
        content: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Tool call initiated.
    #[serde(rename = "tool_call")]
    ToolCall {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Streaming tool call args fragment (during LLM generation).
    ///
    /// Carries the incremental raw args text emitted since the last event
    /// for this call (the first event carries the full prefix accumulated
    /// so far). Clients accumulate fragments into a buffer and parse it
    /// locally for rendering — the backend never parses partial args.
    /// `ToolCall` (authoritative parsed args) follows at execution start.
    #[serde(rename = "tool_call_stream")]
    ToolCallStream {
        tool_call_id: String,
        tool_name: String,
        #[serde(default)]
        args_fragment: String,
        #[serde(default)]
        is_final: bool,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Tool call result.
    #[serde(rename = "tool_call_result")]
    ToolCallResult {
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        tool_result: String,
        tool_success: bool,
        #[serde(default)]
        model: String,
        /// 工具产生的媒体引用（如 ReadImage 的图片）——追加属性。
        /// 旧网关不返回该键；null / 单条畸形都容忍（见 `deserialize_tool_media`）。
        #[serde(default, deserialize_with = "deserialize_tool_media")]
        tool_media: Vec<SessionMediaRef>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// LLM call metrics.
    #[serde(rename = "llm_call_metrics")]
    LlmCallMetrics {
        #[serde(default)]
        model: String,
        prompt_tokens: i64,
        completion_tokens: i64,
        cached_tokens: i64,
        first_chunk_rt_ms: f64,
        tokens_per_sec: f64,
        /// Termination cause (end_turn / max_tokens / tool_use / stop / length).
        #[serde(default)]
        stop_reason: Option<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Agent asks the user a question.
    #[serde(rename = "ask")]
    Ask {
        /// Correlation id — echo back via send_message to resolve this ask's
        /// feedback waiter (distinguishes replies under concurrent asks).
        #[serde(default)]
        tool_call_id: String,
        /// Multi-question format (AskUserQuestion tool).
        #[serde(default)]
        questions: Vec<AskQuestion>,
        /// Legacy single-question (Bash dangerous command confirmation).
        #[serde(default)]
        question: String,
        #[serde(default)]
        choices: Vec<String>,
        /// When true, the user must pick from choices (selection menu).
        #[serde(default)]
        required: bool,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Turn completed.
    #[serde(rename = "done")]
    Done {
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Agent turn started — emitted when the agent begins processing a message.
    /// Unlike Delivered (transport ack), this only fires for actual agent turns,
    /// not magic commands.
    #[serde(rename = "turn_started")]
    TurnStarted {
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// User message accepted into the model context — either as the input of
    /// a new turn (before `turn_started`) or as a steer note after tool
    /// execution. The TUI holds sent messages in a pending area and promotes
    /// them into chat history only when this event arrives: a message moves
    /// up exactly when the model actually receives it.
    ///
    /// `origin_request_id` is the request_id the client submitted with, used
    /// to correlate with the local pending queue; `content` carries the
    /// original text. Internal posts (no request_id) never fire this event.
    #[serde(rename = "user_message_accepted")]
    UserMessageAccepted {
        content: String,
        origin_request_id: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Diff content for file edits.
    ///
    /// `tool_call_id` correlates the diff with the tool call that produced
    /// it (Write/Edit) so the TUI can anchor the Diff cell
    /// directly after its ToolCall cell under concurrent (out-of-order)
    /// execution. Empty for older gateways — falls back to append.
    ///
    /// The payload is a **window**: `old_text` / `new_text` carry the changed
    /// region ± context lines (Edit), or the full content (Write /
    /// new files). `old_start_line` / `new_start_line` are the 1-based
    /// absolute line numbers of the window's first line in the old / new
    /// revision — the TUI renders the given rows verbatim and uses these for
    /// the gutter and the `@@` header. Absent (old gateway, old session) →
    /// line 1, which is correct for the pre-windowing full-file payloads.
    #[serde(rename = "diff_content")]
    DiffContent {
        path: String,
        old_text: Option<String>,
        new_text: String,
        #[serde(default = "first_line")]
        old_start_line: usize,
        #[serde(default = "first_line")]
        new_start_line: usize,
        #[serde(default)]
        tool_call_id: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- state_change ----
    /// Full session state sync.
    ///
    /// Carries the turn state (`status`, required) plus four replay groups —
    /// subscribers MUST assemble the groups in the order `messages →
    /// uncommitted → uncommitted_tools → events`, then continue seamlessly with
    /// the live stream. The replay materials are `#[serde(default)]`: a frame
    /// with nothing in flight / nothing anchored (or a null-stripped wire
    /// frame) degrades to "replay committed only".
    #[serde(rename = "sync_session")]
    SyncSession {
        #[serde(default)]
        session_id: String,
        /// Committed Message projections (active chain).
        #[serde(default)]
        messages: Vec<serde_json::Value>,
        /// Single uncommitted assistant Message projection (finalized blocks of
        /// the in-progress turn) — rendered via the same `replay_messages` path
        /// as `messages`. Null when no turn is in progress.
        #[serde(default)]
        uncommitted: Option<serde_json::Value>,
        /// Unfinished tool calls' raw args fragments
        /// (`[{tool_call_id, tool_name, args_fragment}]`) — rendered via the
        /// live `ToolCallStream` branch (client-side partial parse).
        #[serde(default)]
        uncommitted_tools: Vec<serde_json::Value>,
        /// Durable fact-event nodes on the active chain (in chain order) —
        /// replay material for diff views and other message-projection gaps.
        #[serde(default)]
        events: Vec<serde_json::Value>,
        /// Session status at snapshot time — `working` / `waiting` mean a turn
        /// is in flight, `idle` / `inactive` mean none is.
        ///
        /// A mid-join subscriber takes its turn state from here and nowhere
        /// else: it can never hear the already-past `turn_started` (once-only
        /// live event, never replayed), and the uncommitted projections are not
        /// a substitute — between rounds, or while the first LLM call is still
        /// in flight, a turn runs with nothing finalized to project.
        ///
        /// Required: the snapshot MUST state the turn state (CLI and gateway
        /// ship as one version, so it is always on the wire; a missing or
        /// unknown value is a protocol error, not a case to guess around).
        status: SessionStatus,
        /// When the current turn started (UTC ISO-8601) — restores elapsed time
        /// on resume instead of recounting from the resume moment. Null when no
        /// turn is in progress.
        #[serde(default)]
        turn_started_at: Option<String>,
        /// Boxed: the agent snapshot is the largest inline payload of this
        /// variant — boxing keeps `WingEvent` small (clippy large_enum_variant).
        #[serde(default)]
        agent: Option<Box<AgentInfo>>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        draft: Option<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Session state changed — unified event for model/thinking/yolo/title/agent updates.
    #[serde(rename = "session_state_changed")]
    SessionStateChanged {
        model: Option<String>,
        /// 模型的**引用词**（∈ 配置声明的 id 空间）；不可用时省略 / null。
        /// 前端的选择态 / 匹配以它为准（展示可回落 `model`）。
        #[serde(default)]
        model_id: Option<String>,
        /// 当前模型的 provider 名（运行期事实；与 `model` / `model_id` 同刻下发）。
        #[serde(default)]
        provider_name: Option<String>,
        /// Declared display label for `model`; same clock as `model` (absent
        /// when the model is unchanged / has no declaration).
        #[serde(default)]
        model_display_name: Option<String>,
        thinking: Option<bool>,
        reasoning_effort: Option<String>,
        yolo: Option<bool>,
        title: Option<String>,
        agent: Option<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Agent interrupted.
    #[serde(rename = "interrupted")]
    Interrupted {
        /// Pending client requests the interrupt discarded from the agent
        /// inbox (queue order). `None` = legacy gateway that predates the
        /// field — frontends then fall back to discarding every pending
        /// message (the historical behavior); `Some` lists exactly what was
        /// dropped, so messages that arrived while the interrupt was in
        /// flight are kept pending.
        #[serde(default)]
        dropped_request_ids: Option<Vec<String>>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Context compaction completed.
    #[serde(rename = "compact_done")]
    CompactDone {
        original_tokens: i64,
        compressed_tokens: i64,
        #[serde(default)]
        model: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- query_response ----
    /// Context usage statistics.
    #[serde(rename = "context_stats")]
    ContextStats {
        message_count: i64,
        total_tokens: i64,
        #[serde(default)]
        context_window_tokens: i64,
        #[serde(default)]
        system_prompt_parts: Vec<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Branch targets for /rewind and /fork.
    #[serde(rename = "branch_targets")]
    BranchTargets {
        #[serde(default)]
        targets: Vec<BranchTargetInfo>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- turn-level events (for stdio / SDK consumers) ----
    /// Turn-level assistant message (complete, not streaming).
    #[serde(rename = "assistant_turn")]
    AssistantTurn {
        #[serde(default)]
        uuid: String,
        content_blocks: Vec<serde_json::Value>,
        #[serde(default)]
        model: String,
        stop_reason: Option<String>,
        usage: Option<serde_json::Value>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Turn-level tool result.
    #[serde(rename = "tool_result_turn")]
    ToolResultTurn {
        #[serde(default)]
        uuid: String,
        tool_use_id: String,
        tool_name: String,
        content: String,
        #[serde(default)]
        is_error: bool,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Turn-level final result of the entire agent loop.
    #[serde(rename = "turn_result")]
    TurnResult {
        #[serde(default)]
        uuid: String,
        #[serde(default = "default_subtype")]
        subtype: String,
        #[serde(default)]
        is_error: bool,
        result: Option<String>,
        #[serde(default)]
        num_turns: i64,
        #[serde(default)]
        duration_ms: i64,
        usage: Option<serde_json::Value>,
        #[serde(default)]
        errors: Vec<String>,
        #[serde(flatten)]
        meta: EventMeta,
    },

    // ---- session init (for stdio mode) ----
    /// Session initialization event — emitted on subscribe/fork.
    /// Carries authoritative session state for stdio consumers.
    #[serde(rename = "session_init")]
    SessionInit {
        #[serde(default)]
        uuid: String,
        #[serde(default)]
        tools: Vec<String>,
        #[serde(default)]
        model: String,
        #[serde(default = "default_permission_mode")]
        permission_mode: String,
        #[serde(default)]
        cwd: String,
        #[serde(flatten)]
        meta: EventMeta,
    },

    /// Catch-all for unknown event types — prevents deserialization failures
    /// when wing adds new event types.
    #[serde(other)]
    Unknown,
}

fn default_status_code() -> i32 {
    500
}

fn default_subtype() -> String {
    "success".into()
}

fn default_permission_mode() -> String {
    "default".into()
}

/// Missing `level` on a notice degrades to the weakest signal.
fn default_notice_level() -> String {
    "info".into()
}

impl WingEvent {
    /// Decode one session-history event payload (a chain event record).
    ///
    /// Same rules as the live wire decode, plus the tolerance replay needs:
    /// chain payloads always carry the event meta (`created_at` /
    /// `request_id` — `wire_dump` guarantees it) and a `tool_call_id` on the
    /// types that have one, but replay never reads meta, and its renderers
    /// treat a null `tool_call_id` exactly like an absent one (the decoders
    /// this replaced declared these fields `Option<String>`). Absent or null
    /// meta / `tool_call_id` keys are therefore backfilled with empty strings
    /// — which is also the value `#[serde(default)]` would have produced for
    /// a missing key. Unknown `type`s still fall back to
    /// [`WingEvent::Unknown`]; a known `type` whose payload is otherwise
    /// malformed is an error (the caller skips that node).
    pub fn from_history_value(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        let mut value = value.clone();
        if let Some(obj) = value.as_object_mut() {
            for key in ["created_at", "request_id", "tool_call_id"] {
                if obj.get(key).is_none_or(serde_json::Value::is_null) {
                    obj.insert(key.to_string(), serde_json::Value::String(String::new()));
                }
            }
        }
        serde_json::from_value(value)
    }

    /// Returns the `type` discriminator string for this event.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Error { .. } => "error",
            Self::Delivered { .. } => "delivered",
            Self::Notice { .. } => "notice",
            Self::Text { .. } => "text",
            Self::Reasoning { .. } => "reasoning",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolCallStream { .. } => "tool_call_stream",
            Self::ToolCallResult { .. } => "tool_call_result",
            Self::LlmCallMetrics { .. } => "llm_call_metrics",
            Self::Ask { .. } => "ask",
            Self::Done { .. } => "done",
            Self::TurnStarted { .. } => "turn_started",
            Self::UserMessageAccepted { .. } => "user_message_accepted",
            Self::DiffContent { .. } => "diff_content",
            Self::SyncSession { .. } => "sync_session",
            Self::SessionStateChanged { .. } => "session_state_changed",
            Self::Interrupted { .. } => "interrupted",
            Self::CompactDone { .. } => "compact_done",
            Self::ContextStats { .. } => "context_stats",
            Self::BranchTargets { .. } => "branch_targets",
            Self::AssistantTurn { .. } => "assistant_turn",
            Self::ToolResultTurn { .. } => "tool_result_turn",
            Self::TurnResult { .. } => "turn_result",
            Self::SessionInit { .. } => "session_init",
            Self::Unknown => "unknown",
        }
    }

    /// Returns a reference to the event metadata, if this is a known variant.
    pub fn meta(&self) -> Option<&EventMeta> {
        match self {
            Self::Error { meta, .. }
            | Self::Delivered { meta, .. }
            | Self::Notice { meta, .. }
            | Self::Text { meta, .. }
            | Self::Reasoning { meta, .. }
            | Self::ToolCall { meta, .. }
            | Self::ToolCallStream { meta, .. }
            | Self::ToolCallResult { meta, .. }
            | Self::LlmCallMetrics { meta, .. }
            | Self::Ask { meta, .. }
            | Self::Done { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::UserMessageAccepted { meta, .. }
            | Self::DiffContent { meta, .. }
            | Self::SyncSession { meta, .. }
            | Self::SessionStateChanged { meta, .. }
            | Self::Interrupted { meta, .. }
            | Self::CompactDone { meta, .. }
            | Self::ContextStats { meta, .. }
            | Self::BranchTargets { meta, .. }
            | Self::AssistantTurn { meta, .. }
            | Self::ToolResultTurn { meta, .. }
            | Self::TurnResult { meta, .. }
            | Self::SessionInit { meta, .. } => Some(meta),
            Self::Unknown => None,
        }
    }

    /// Returns the session this event belongs to, if present.
    ///
    /// Most variants carry it in [`EventMeta`]; `SyncSession` has its own
    /// **named** `session_id` field instead — serde's flatten only sees the
    /// keys no named field consumed, so its meta never holds the id. Frontends
    /// route the replay snapshot by this value (wing acp's `SessionHub`), so it
    /// must read the named field. An absent key (empty string) counts as none,
    /// matching the meta-based variants.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::SyncSession { session_id, .. } => {
                (!session_id.is_empty()).then_some(session_id.as_str())
            }
            _ => self.meta().and_then(|m| m.session_id.as_deref()),
        }
    }

    /// Returns the request_id from the event meta.
    pub fn request_id(&self) -> Option<&str> {
        self.meta().map(|m| m.request_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_text_event() {
        let json = r#"{
            "type": "text",
            "content": "Hello world",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req1"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::Text { ref content, .. } if content == "Hello world"));
        assert_eq!(event.event_type(), "text");
        assert_eq!(event.session_id(), Some("abc123"));
    }

    #[test]
    fn deserialize_tool_call_event() {
        let json = r#"{
            "type": "tool_call",
            "tool_name": "Bash",
            "tool_args": {"command": "ls"},
            "tool_call_id": "tc_1",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req2"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::ToolCall { ref tool_name, .. } if tool_name == "Bash"));
    }

    #[test]
    fn deserialize_interrupted_event() {
        // 旧网关：不带 dropped_request_ids → None（前端回落"全部丢弃"）。
        let json = r#"{
            "type": "interrupted",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req5"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Interrupted {
                dropped_request_ids,
                ..
            } => assert_eq!(dropped_request_ids, None),
            _ => panic!("expected Interrupted"),
        }

        // 新网关：显式列出被放弃的 request_id。
        let json = r#"{
            "type": "interrupted",
            "dropped_request_ids": ["req-a", "req-b"],
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req6"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Interrupted {
                dropped_request_ids,
                ..
            } => assert_eq!(
                dropped_request_ids,
                Some(vec!["req-a".to_string(), "req-b".to_string()])
            ),
            _ => panic!("expected Interrupted"),
        }
    }

    #[test]
    fn deserialize_session_state_changed_event() {
        let json = r#"{
            "type": "session_state_changed",
            "model": "gpt-4o",
            "thinking": true,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req3"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::SessionStateChanged {
                model,
                model_id,
                provider_name,
                model_display_name,
                thinking,
                yolo,
                title,
                agent,
                ..
            } => {
                assert_eq!(model, Some("gpt-4o".to_string()));
                assert_eq!(
                    model_id, None,
                    "old gateway payload (no field) must deserialize to None"
                );
                assert_eq!(provider_name, None);
                assert_eq!(
                    model_display_name, None,
                    "old gateway payload (no field) must deserialize to None"
                );
                assert_eq!(thinking, Some(true));
                assert_eq!(yolo, None);
                assert_eq!(title, None);
                assert_eq!(agent, None);
            }
            _ => panic!("expected SessionStateChanged"),
        }
    }

    #[test]
    fn deserialize_session_state_changed_with_model_identity() {
        // New gateway: the reference word, the provider fact and the display
        // label all travel with the model value.
        let json = r#"{
            "type": "session_state_changed",
            "model": "dfmodel-2026",
            "model_id": "ds-flash",
            "provider_name": "qoder",
            "model_display_name": "DeepSeek-Flash",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req4"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::SessionStateChanged {
                model,
                model_id,
                provider_name,
                model_display_name,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("dfmodel-2026"));
                assert_eq!(model_id.as_deref(), Some("ds-flash"));
                assert_eq!(provider_name.as_deref(), Some("qoder"));
                assert_eq!(model_display_name.as_deref(), Some("DeepSeek-Flash"));
            }
            _ => panic!("expected SessionStateChanged"),
        }
    }

    #[test]
    fn deserialize_notice_event() {
        // Full shape (retry notice).
        let json = r#"{
            "type": "notice",
            "level": "warning",
            "message": "generate 调用失败 (1/3): TimeoutError: stalled, 6s 后重试",
            "attempt": 1,
            "max_attempts": 3,
            "retry_in_s": 6.0,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req-notice"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match &event {
            WingEvent::Notice {
                level,
                message,
                attempt,
                max_attempts,
                retry_in_s,
                ..
            } => {
                assert_eq!(level, "warning");
                assert!(message.contains("stalled"));
                assert_eq!(*attempt, Some(1));
                assert_eq!(*max_attempts, Some(3));
                assert_eq!(*retry_in_s, Some(6.0));
            }
            other => panic!("expected Notice, got {other:?}"),
        }
        assert_eq!(event.event_type(), "notice");
        assert_eq!(event.session_id(), Some("abc123"));
    }

    #[test]
    fn deserialize_notice_event_minimal() {
        // Only the level/message matter — every other field must be optional
        // (older/newer gateways, null-stripped wire frames).
        let json = r#"{
            "type": "notice",
            "message": "degraded",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req5"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match &event {
            WingEvent::Notice {
                level,
                attempt,
                max_attempts,
                retry_in_s,
                ..
            } => {
                assert_eq!(level, "info", "missing level degrades to info");
                assert_eq!(*attempt, None);
                assert_eq!(*max_attempts, None);
                assert_eq!(*retry_in_s, None);
            }
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_unknown_event() {
        let json = r#"{
            "type": "some_future_event",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req4"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::Unknown));
        assert_eq!(event.event_type(), "unknown");
    }

    #[test]
    fn sync_session_agent_provider_name_tolerated_and_roundtripped() {
        // An agent payload without provider_name / model_display_name → None, no error.
        let legacy = r#"{
            "type": "sync_session",
            "session_id": "s1",
            "status": "idle",
            "messages": [],
            "uncommitted": null,
            "uncommitted_tools": [],
            "events": [],
            "agent": {
                "model_name": "gpt-4",
                "system_prompt": null,
                "tools": [],
                "skills": [],
                "rules": [],
                "workspace": null
            },
            "created_at": "2026-01-01T00:00:00+00:00",
            "request_id": "req-agent-1"
        }"#;
        let event: WingEvent = serde_json::from_str(legacy).unwrap();
        match event {
            WingEvent::SyncSession { agent, .. } => {
                let agent = agent.expect("agent snapshot");
                assert_eq!(
                    agent.provider_name, None,
                    "missing provider_name must deserialize to None"
                );
                assert_eq!(
                    agent.model_display_name, None,
                    "missing model_display_name must deserialize to None"
                );
            }
            _ => panic!("expected SyncSession"),
        }

        // New gateway: provider_name / model_display_name survive a
        // serialize → deserialize round trip.
        let info = AgentInfo {
            model_name: "gpt-4".into(),
            model_id: None,
            system_prompt: None,
            tools: vec![],
            skills: vec![],
            rules: vec![],
            workspace: None,
            provider_name: Some("dashscope-openai".into()),
            model_display_name: Some("DeepSeek-Flash".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: AgentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider_name.as_deref(), Some("dashscope-openai"));
        assert_eq!(back.model_display_name.as_deref(), Some("DeepSeek-Flash"));
    }

    #[test]
    fn deserialize_delivered_event() {
        let json = r#"{
            "type": "delivered",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req5"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, WingEvent::Delivered { .. }));
    }

    #[test]
    fn deserialize_error_event_default_status() {
        let json = r#"{
            "type": "error",
            "message": "something broke",
            "created_at": "2025-01-01T00:00:00",
            "session_id": null,
            "request_id": "req6"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Error {
                status_code,
                message,
                ..
            } => {
                assert_eq!(status_code, 500);
                assert_eq!(message, "something broke");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn deserialize_diff_content_event() {
        let json = r#"{
            "type": "diff_content",
            "path": "src/main.rs",
            "old_text": "fn old() {}",
            "new_text": "fn new() {}",
            "old_start_line": 42,
            "new_start_line": 43,
            "tool_call_id": "call_edit_1",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req8"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::DiffContent {
                path,
                old_text,
                new_text,
                old_start_line,
                new_start_line,
                tool_call_id,
                ..
            } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(old_text.unwrap(), "fn old() {}");
                assert_eq!(new_text, "fn new() {}");
                assert_eq!(old_start_line, 42);
                assert_eq!(new_start_line, 43);
                assert_eq!(tool_call_id, "call_edit_1");
            }
            _ => panic!("expected DiffContent"),
        }
    }

    #[test]
    fn deserialize_diff_content_window_lines_default_to_one() {
        // Pre-windowing payloads (and old gateways) omit the start lines —
        // the whole file started at line 1, so 1 is the correct default.
        let json = r#"{
            "type": "diff_content",
            "path": "src/main.rs",
            "old_text": "fn old() {}",
            "new_text": "fn new() {}",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req8"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::DiffContent {
                old_start_line,
                new_start_line,
                ..
            } => {
                assert_eq!(old_start_line, 1);
                assert_eq!(new_start_line, 1);
            }
            _ => panic!("expected DiffContent"),
        }
    }

    #[test]
    fn deserialize_diff_content_event_tool_call_id_defaults_empty() {
        // Older gateways omit tool_call_id — must default to "" (append fallback).
        let json = r#"{
            "type": "diff_content",
            "path": "src/main.rs",
            "old_text": null,
            "new_text": "fn new() {}",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req8"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::DiffContent { tool_call_id, .. } => {
                assert_eq!(tool_call_id, "");
            }
            _ => panic!("expected DiffContent"),
        }
    }

    #[test]
    fn deserialize_context_stats_event() {
        let json = r#"{
            "type": "context_stats",
            "message_count": 10,
            "total_tokens": 5000,
            "context_window_tokens": 80000,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req9"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::ContextStats {
                total_tokens,
                context_window_tokens,
                ..
            } => {
                assert_eq!(total_tokens, 5000);
                assert_eq!(context_window_tokens, 80000);
            }
            _ => panic!("expected ContextStats"),
        }
    }

    #[test]
    fn deserialize_user_message_accepted_event() {
        let json = r#"{
            "type": "user_message_accepted",
            "content": "hello while busy",
            "origin_request_id": "req-42",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req-turn"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match &event {
            WingEvent::UserMessageAccepted {
                content,
                origin_request_id,
                ..
            } => {
                assert_eq!(content, "hello while busy");
                assert_eq!(origin_request_id, "req-42");
            }
            _ => panic!("expected UserMessageAccepted"),
        }
        assert_eq!(event.event_type(), "user_message_accepted");
    }

    #[test]
    fn deserialize_tool_call_stream_event() {
        let json = r#"{
            "type": "tool_call_stream",
            "tool_call_id": "tc_stream_1",
            "tool_name": "Bash",
            "args_fragment": "{\"command\": \"ls\"}",
            "is_final": false,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req10"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), "tool_call_stream");
        match event {
            WingEvent::ToolCallStream {
                tool_call_id,
                tool_name,
                args_fragment,
                is_final,
                ..
            } => {
                assert_eq!(tool_call_id, "tc_stream_1");
                assert_eq!(tool_name, "Bash");
                assert_eq!(args_fragment, "{\"command\": \"ls\"}");
                assert!(!is_final);
            }
            _ => panic!("expected ToolCallStream"),
        }
    }

    #[test]
    fn deserialize_tool_call_stream_defaults() {
        let json = r#"{
            "type": "tool_call_stream",
            "tool_call_id": "tc_2",
            "tool_name": "Read",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req11"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::ToolCallStream {
                args_fragment,
                is_final,
                ..
            } => {
                assert_eq!(args_fragment, "");
                assert!(!is_final);
            }
            _ => panic!("expected ToolCallStream"),
        }
    }

    // ── tool_call_result 追加字段 tool_media（读图） ─────────────

    #[test]
    fn deserialize_tool_call_result_with_media() {
        let json = r#"{
            "type": "tool_call_result",
            "tool_name": "ReadImage",
            "tool_args": {"path": "/tmp/shot.png"},
            "tool_call_id": "tc_img_1",
            "tool_result": "[image: /tmp/shot.png | png 2880x1800 | 2.4 MB | id 9f3c1a2b | mtime 1789000000]",
            "tool_success": true,
            "tool_media": [
                {"id": "9f3c1a2bde44", "mime": "image/png", "bytes": 123456,
                 "width": 2880, "height": 1800, "name": "shot.png"}
            ],
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc123",
            "request_id": "req20"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), "tool_call_result");
        match event {
            WingEvent::ToolCallResult {
                tool_name,
                tool_media,
                ..
            } => {
                assert_eq!(tool_name, "ReadImage");
                assert_eq!(tool_media.len(), 1);
                let media = &tool_media[0];
                assert_eq!(media.id, "9f3c1a2bde44");
                assert_eq!(media.mime, "image/png");
                assert_eq!(media.bytes, 123456);
                assert_eq!(media.width, 2880);
                assert_eq!(media.height, 1800);
                assert_eq!(media.name.as_deref(), Some("shot.png"));
            }
            _ => panic!("expected ToolCallResult"),
        }
    }

    #[test]
    fn deserialize_tool_call_result_tolerates_missing_or_null_media_fields() {
        // 旧网关：整个 tool_media 键缺席 → 空表，其余字段照旧可用。
        let legacy = r#"{
            "type": "tool_call_result",
            "tool_name": "Bash",
            "tool_args": {},
            "tool_call_id": "tc_old",
            "tool_result": "ok",
            "tool_success": true,
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req21"
        }"#;
        match serde_json::from_str::<WingEvent>(legacy).unwrap() {
            WingEvent::ToolCallResult {
                tool_result,
                tool_media,
                ..
            } => {
                assert_eq!(tool_result, "ok");
                assert!(tool_media.is_empty(), "missing key decodes to no media");
            }
            other => panic!("expected ToolCallResult, got {other:?}"),
        }

        // 新网关：name 为 null / 缺席都要容忍（wire 只剥离顶层 null，嵌套 null 原样在线）。
        let null_name = r#"{
            "type": "tool_call_result",
            "tool_name": "ReadImage",
            "tool_args": {},
            "tool_call_id": "tc_img_2",
            "tool_result": "…",
            "tool_success": true,
            "tool_media": [
                {"id": "aa", "mime": "image/gif", "bytes": 7,
                 "width": 1, "height": 2, "name": null},
                {"id": "bb", "mime": "image/webp", "bytes": 8,
                 "width": 3, "height": 4}
            ],
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req22"
        }"#;
        match serde_json::from_str::<WingEvent>(null_name).unwrap() {
            WingEvent::ToolCallResult { tool_media, .. } => {
                assert_eq!(tool_media.len(), 2);
                assert_eq!(tool_media[0].name, None, "explicit null degrades to None");
                assert_eq!(tool_media[1].name, None, "absent key degrades to None");
                assert_eq!(tool_media[1].mime, "image/webp");
            }
            other => panic!("expected ToolCallResult, got {other:?}"),
        }
    }

    /// `tool_media: null` / 非数组形状 → 空表；整帧照常解析。
    #[test]
    fn deserialize_tool_media_null_and_non_array_decode_as_empty() {
        for media in ["null", "42", "{}", r#""nope""#] {
            let json = format!(
                r#"{{
                    "type": "tool_call_result",
                    "tool_name": "ReadImage",
                    "tool_args": {{}},
                    "tool_call_id": "tc_img_3",
                    "tool_result": "…",
                    "tool_success": true,
                    "tool_media": {media},
                    "created_at": "2025-01-01T00:00:00",
                    "session_id": "abc",
                    "request_id": "req23"
                }}"#
            );
            match serde_json::from_str::<WingEvent>(&json).unwrap() {
                WingEvent::ToolCallResult { tool_media, .. } => {
                    assert!(tool_media.is_empty(), "tool_media={media}");
                }
                other => panic!("expected ToolCallResult, got {other:?}"),
            }
        }
    }

    /// 单条畸形只跳过该条：合法条目与整帧都必须保留。
    #[test]
    fn deserialize_tool_media_skips_only_malformed_entries() {
        let json = r#"{
            "type": "tool_call_result",
            "tool_name": "ReadImage",
            "tool_args": {},
            "tool_call_id": "tc_img_4",
            "tool_result": "[image: ok]",
            "tool_success": true,
            "tool_media": [
                {"id": "good", "mime": "image/png", "bytes": 10,
                 "width": 20, "height": 30, "name": "ok.png"},
                {"id": "no-width", "mime": "image/png", "bytes": 1, "height": 3},
                {"id": "bytes-str", "mime": "image/png", "bytes": "1",
                 "width": 2, "height": 3},
                {"id": "width-overflow", "mime": "image/png", "bytes": 1,
                 "width": 4294967296, "height": 3},
                7,
                {"id": "last", "mime": "image/gif", "bytes": 5,
                 "width": 6, "height": 7, "name": null}
            ],
            "created_at": "2025-01-01T00:00:00",
            "session_id": "abc",
            "request_id": "req24"
        }"#;
        match serde_json::from_str::<WingEvent>(json).unwrap() {
            WingEvent::ToolCallResult {
                tool_result,
                tool_media,
                ..
            } => {
                assert_eq!(tool_result, "[image: ok]", "结果行不能被吞掉");
                assert_eq!(tool_media.len(), 2, "只保留两条合法条目");
                assert_eq!(tool_media[0].id, "good");
                assert_eq!(tool_media[0].name.as_deref(), Some("ok.png"));
                assert_eq!(tool_media[1].id, "last");
                assert_eq!(tool_media[1].name, None);
            }
            other => panic!("expected ToolCallResult, got {other:?}"),
        }
    }

    // ── Wire frames after null/storage-field stripping (lean-event-log) ──
    //
    // The backend `wire_dump` strips storage-only fields (role / parent_uuid /
    // unzip_last_uuid / target / persist) and every null-valued field. These
    // lock that the Rust mirror still parses such frames — missing `Option<T>`
    // fields fall back to None, and SyncSession's replay materials default.

    #[test]
    fn deserialize_stripped_text_frame() {
        // No session_id (global / stripped null), no role/parent_uuid/persist.
        let json = r#"{
            "type": "text",
            "content": "hello",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req1"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Text { content, meta } => {
                assert_eq!(content, "hello");
                assert!(meta.session_id.is_none(), "absent session_id → None");
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn deserialize_stripped_diff_frame_missing_old_text() {
        // old_text=None (new file) is stripped → must parse as None.
        let json = r#"{
            "type": "diff_content",
            "path": "new.txt",
            "new_text": "content",
            "created_at": "2025-01-01T00:00:00",
            "session_id": "s1",
            "request_id": "req2"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::DiffContent {
                path,
                old_text,
                new_text,
                tool_call_id,
                ..
            } => {
                assert_eq!(path, "new.txt");
                assert!(old_text.is_none(), "absent old_text → None (new file)");
                assert_eq!(new_text, "content");
                assert_eq!(tool_call_id, "", "absent tool_call_id → default empty");
            }
            _ => panic!("expected DiffContent"),
        }
    }

    #[test]
    fn deserialize_stripped_error_frame() {
        // error_code / detail are None → stripped → must parse as None.
        let json = r#"{
            "type": "error",
            "message": "boom",
            "status_code": 500,
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req3"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Error {
                message,
                error_code,
                detail,
                ..
            } => {
                assert_eq!(message, "boom");
                assert!(error_code.is_none());
                assert!(detail.is_none());
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn deserialize_sync_session_with_all_replay_materials_defaulted() {
        // The replay materials are all optional within one version: a frame
        // carrying only committed messages (nothing in flight, nothing
        // anchored) parses and degrades to "replay committed only". `status` is
        // not part of that tolerance — it is required (see
        // `sync_session_status_is_required_and_strict`).
        let json = r#"{
            "type": "sync_session",
            "session_id": "s1",
            "status": "idle",
            "messages": [{"role": "user", "content": "hi"}],
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req4"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::SyncSession {
                session_id,
                messages,
                uncommitted,
                uncommitted_tools,
                events,
                turn_started_at,
                ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(messages.len(), 1);
                assert!(uncommitted.is_none());
                assert!(uncommitted_tools.is_empty());
                assert!(events.is_empty());
                assert!(turn_started_at.is_none());
            }
            _ => panic!("expected SyncSession"),
        }
    }

    #[test]
    fn sync_session_reports_its_own_session_id() {
        // `SyncSession` carries `session_id` as a **named** field: serde's
        // flatten only sees the leftover keys, so the flattened meta never
        // holds this id. `session_id()` must read the named field — frontends
        // route the replay snapshot by it (wing acp's `SessionHub::dispatch`).
        let json = r#"{
            "type": "sync_session",
            "session_id": "s1",
            "status": "idle",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req4"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.session_id(), Some("s1"));

        // 缺 key → 与 meta 口径一致：None（不是空串）。
        let bare = r#"{
            "type": "sync_session",
            "status": "idle",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req4"
        }"#;
        let event: WingEvent = serde_json::from_str(bare).unwrap();
        assert_eq!(event.session_id(), None);
    }

    #[test]
    fn deserialize_sync_session_status() {
        // The working-state carrier: every status decodes to its variant and
        // answers `turn_in_flight` (working / waiting are a turn in flight).
        for (raw, expected, in_flight) in [
            ("idle", SessionStatus::Idle, false),
            ("inactive", SessionStatus::Inactive, false),
            ("working", SessionStatus::Working, true),
            ("waiting", SessionStatus::Waiting, true),
        ] {
            let json = format!(
                r#"{{"type": "sync_session", "session_id": "s1", "status": "{raw}",
                     "created_at": "2026-01-01T00:00:00", "request_id": "req"}}"#
            );
            let event: WingEvent = serde_json::from_str(&json).unwrap();
            match event {
                WingEvent::SyncSession { status, .. } => {
                    assert_eq!(status, expected, "raw {raw:?}");
                    assert_eq!(status.turn_in_flight(), in_flight);
                }
                _ => panic!("expected SyncSession"),
            }
        }

        // The list endpoint's string vocabulary parses through `parse` (that
        // surface is display-only, so an unknown value is the caller's call).
        assert_eq!(
            SessionStatus::parse("working"),
            Some(SessionStatus::Working)
        );
        assert_eq!(SessionStatus::parse("compacting"), None);
    }

    #[test]
    fn sync_session_status_is_required_and_strict() {
        // The snapshot must *state* the turn state: a payload without the field,
        // or with a value this build does not know, is a protocol error (CLI and
        // gateway ship as one version) — it must not silently decode to "idle",
        // which is exactly the inference this field exists to remove.
        let missing = r#"{
            "type": "sync_session",
            "session_id": "s1",
            "messages": [],
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req"
        }"#;
        assert!(
            serde_json::from_str::<WingEvent>(missing).is_err(),
            "a sync_session without status must fail to decode"
        );

        let unknown = missing.replace(
            "\"messages\": []",
            "\"messages\": [], \"status\": \"compacting\"",
        );
        assert!(
            serde_json::from_str::<WingEvent>(&unknown).is_err(),
            "an unknown status must fail to decode"
        );
    }

    #[test]
    fn deserialize_sync_session_with_uncommitted() {
        let json = r#"{
            "type": "sync_session",
            "session_id": "s1",
            "messages": [],
            "uncommitted": {"role": "assistant", "content": "partial"},
            "uncommitted_tools": [
                {"tool_call_id": "tc1", "tool_name": "Bash", "args_fragment": "{\"c"}
            ],
            "events": [{"type": "diff_content", "path": "f", "new_text": "x"}],
            "status": "working",
            "turn_started_at": "2026-01-01T00:00:00+00:00",
            "created_at": "2026-01-01T00:00:00+00:00",
            "request_id": "req5"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::SyncSession {
                uncommitted,
                uncommitted_tools,
                turn_started_at,
                ..
            } => {
                assert!(uncommitted.is_some());
                assert_eq!(uncommitted_tools.len(), 1);
                assert_eq!(uncommitted_tools[0]["tool_call_id"].as_str(), Some("tc1"));
                assert_eq!(
                    turn_started_at.as_deref(),
                    Some("2026-01-01T00:00:00+00:00")
                );
            }
            _ => panic!("expected SyncSession"),
        }
    }

    #[test]
    fn deserialize_ask_event_with_panel_questions() {
        // Wire contract with the Python AskUserQuestion tool
        // (AskQuestion.model_dump(by_alias=True)): header + multiSelect +
        // options[{label, description}].
        let json = r#"{
            "type": "ask",
            "tool_call_id": "tc_ask",
            "questions": [
                {
                    "id": "features",
                    "header": "测试项",
                    "question": "测哪些？",
                    "multiSelect": true,
                    "options": [
                        {"label": "多选交互", "description": "测试 multiSelect"},
                        {"label": "代码预览", "description": ""}
                    ]
                }
            ],
            "created_at": "2026-01-01T00:00:00+00:00",
            "request_id": "req6"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Ask { questions, .. } => {
                let q = &questions[0];
                assert_eq!(q.id, "features");
                assert_eq!(q.header, "测试项");
                assert!(q.multi_select);
                assert_eq!(q.tab_label(), "测试项");
                assert_eq!(q.options.len(), 2);
                assert_eq!(q.options[0].label, "多选交互");
                assert_eq!(q.options[0].description, "测试 multiSelect");
                assert_eq!(q.options[1].description, "");
            }
            _ => panic!("expected Ask"),
        }
    }

    #[test]
    fn deserialize_legacy_ask_question_falls_back_to_choices() {
        // Old gateway records carry plain-string choices and no header —
        // serde defaults must keep them parseable; AskPanel normalizes them.
        let json = r#"{
            "type": "ask",
            "tool_call_id": "tc_old",
            "questions": [{"id": "q1", "question": "proceed?", "choices": ["y", "n"]}],
            "created_at": "2026-01-01T00:00:00+00:00",
            "request_id": "req7"
        }"#;
        let event: WingEvent = serde_json::from_str(json).unwrap();
        match event {
            WingEvent::Ask { questions, .. } => {
                let q = &questions[0];
                assert!(!q.multi_select);
                assert!(q.options.is_empty());
                assert_eq!(q.choices, vec!["y".to_string(), "n".to_string()]);
                assert_eq!(q.tab_label(), "q1", "missing header falls back to id");
            }
            _ => panic!("expected Ask"),
        }
    }

    // ── from_history_value: chain-record decode (replay) ─────────────

    #[test]
    fn history_value_decodes_payload_without_meta() {
        // Chain records from `wire_dump` always carry meta, but replay never
        // reads it — payload-only records (hand-written fixtures) must decode.
        let value = serde_json::json!({
            "type": "diff_content",
            "path": "main.rs",
            "old_text": null,
            "new_text": "fn main() {}",
            "tool_call_id": "tc-1",
        });
        match WingEvent::from_history_value(&value).unwrap() {
            WingEvent::DiffContent { path, new_text, .. } => {
                assert_eq!(path, "main.rs");
                assert_eq!(new_text, "fn main() {}");
            }
            other => panic!("expected DiffContent, got {other:?}"),
        }
    }

    #[test]
    fn history_value_backfills_null_meta() {
        // Explicit null meta degrades the same way as a missing key.
        let value = serde_json::json!({
            "type": "ask",
            "tool_call_id": "ask-1",
            "created_at": null,
            "request_id": null,
        });
        let event = WingEvent::from_history_value(&value).unwrap();
        assert_eq!(event.event_type(), "ask");
        assert_eq!(event.request_id(), Some(""));
    }

    #[test]
    fn history_value_null_tool_call_id_reads_as_absent() {
        // The renderers treat null `tool_call_id` exactly like an absent one
        // (the replaced decoders declared it `Option<String>`): both decode to
        // "" — diff renders with the append fallback, ask keeps an empty id.
        for event_type in ["diff_content", "ask"] {
            let mut payload = serde_json::json!({
                "type": event_type,
                "tool_call_id": null,
            });
            if event_type == "diff_content" {
                payload["path"] = "f".into();
                payload["new_text"] = "n".into();
            }
            let from_null = WingEvent::from_history_value(&payload).unwrap();
            payload.as_object_mut().unwrap().remove("tool_call_id");
            let from_absent = WingEvent::from_history_value(&payload).unwrap();

            let id = |ev: &WingEvent| match ev {
                WingEvent::DiffContent { tool_call_id, .. }
                | WingEvent::Ask { tool_call_id, .. } => tool_call_id.clone(),
                other => panic!("expected diff/ask, got {other:?}"),
            };
            assert_eq!(id(&from_null), "");
            assert_eq!(id(&from_absent), "");
            assert_eq!(id(&from_null), id(&from_absent), "{event_type}");
        }
    }

    #[test]
    fn history_value_unknown_type_is_unknown() {
        let value = serde_json::json!({"type": "from_the_future", "anything": true});
        assert!(matches!(
            WingEvent::from_history_value(&value).unwrap(),
            WingEvent::Unknown
        ));
    }

    #[test]
    fn history_value_malformed_payloads_fail() {
        // Known type missing a required payload field → error (caller skips).
        assert!(
            WingEvent::from_history_value(&serde_json::json!({
                "type": "diff_content",
                "path": "f",
            }))
            .is_err()
        );
        // No discriminator at all.
        assert!(WingEvent::from_history_value(&serde_json::json!({})).is_err());
        assert!(WingEvent::from_history_value(&serde_json::json!({"type": 5})).is_err());
    }
}
