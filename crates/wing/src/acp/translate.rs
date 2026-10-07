//! `WingEvent` → ACP v1 `session/update` 的映射（纯函数 + 会话级工具卡片状态）。
//!
//! 本模块**不认识网络也不认识连接**：输入是一个已反序列化的 wing 事件，输出是一串
//! 待包装成 `SessionNotification` 的 [`SessionUpdate`]。这样映射规则可以逐行单测
//! （真实事件 JSON fixture），不需要网关或 ACP 客户端。
//!
//! 两类入口：
//!
//! - [`ToolCards::apply`]：**有状态**映射（状态跨轮次存活于会话上）。工具卡片的
//!   「只创建一次」去重、`diff_content` 的锚定、content 的累积都要它。
//! - [`turn_end`] / [`flatten_prompt`]：无状态纯函数。
//!
//! Ask 事件在 `ask` 模块分流（permission / elicitation / 回退），本模块对它**不产帧**。
//!
//! 映射规格（与 01 步任务书的表格逐行对应）：
//!
//! | wing 事件 | ACP 输出 |
//! |-----------|----------|
//! | `text` | `AgentMessageChunk(TextContent)` |
//! | `reasoning` | `AgentThoughtChunk(TextContent)` |
//! | `tool_call_stream` | `ToolCall`（创建，status=pending；同 id 只创建一次） |
//! | `tool_call` | `ToolCall`（创建）或 `ToolCallUpdate`（title/kind/locations/rawInput，in_progress） |
//! | `tool_call_result` | `ToolCallUpdate`（completed/failed；content=[text]；rawOutput） |
//! | `diff_content` | `ToolCallUpdate`（content 追加 `Diff`，锚定 tool_call_id） |
//! | `session_state_changed{title}` | `SessionInfoUpdate` |
//! | `context_stats` | `UsageUpdate`（窗口 <=0 跳过） |
//! | `turn_result` / `interrupted` / `error` | 轮次终态（end_turn / cancelled / JSON-RPC error） |
//! | 其余 | 零帧（debug 日志） |
//!
//! 子模块 [`replay`]：`session/load` 的历史回放投影（`sync_session` 快照 → update
//! 序列）。它与实时映射共用同一份 [`ToolCards`] 记忆与 title/kind/locations 规则——
//! 回放建好的卡片，后续实时 `diff_content` / `tool_call_result` 继续锚定。

pub mod replay;

use std::collections::HashMap;
use std::collections::VecDeque;

use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::ContentChunk;
use agent_client_protocol::schema::v1::Diff;
use agent_client_protocol::schema::v1::SessionInfoUpdate;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::ToolCall;
use agent_client_protocol::schema::v1::ToolCallContent;
use agent_client_protocol::schema::v1::ToolCallId;
use agent_client_protocol::schema::v1::ToolCallLocation;
use agent_client_protocol::schema::v1::ToolCallStatus;
use agent_client_protocol::schema::v1::ToolCallUpdate;
use agent_client_protocol::schema::v1::ToolCallUpdateFields;
use agent_client_protocol::schema::v1::ToolKind;
use agent_client_protocol::schema::v1::UsageUpdate;

use crate::protocol::WingEvent;

// ============================================================
// 常量
// ============================================================

/// 工具结果转发给客户端的字符上限（超出截断并在尾部注明）。
///
/// 这是**展示**上限，不影响模型看到的内容：ACP 客户端把 content 渲染成卡片，
/// 几十 KB 的输出会把卡片刻爆。
pub const TOOL_RESULT_MAX_CHARS: usize = 8 * 1024;

/// Bash 标题（命令首行）的字符上限。
const BASH_TITLE_MAX_CHARS: usize = 80;

/// 会话级工具卡片记忆的容量上限（超出按插入顺序淘汰最旧）。
///
/// 一次会话的工具调用数可以很多，而记忆只需要覆盖「客户端还没消费完的卡片」；
/// 淘汰最旧条目后针对它的 `diff_content` 会被丢弃（见 [`ToolCards`]），而不是生成
/// 客户端侧的 “Tool call not found” 假卡片。
const MAX_TRACKED_CALLS: usize = 512;

/// 单张工具卡片的 diff 条数上限（超出丢弃后到的，见 [`Card::diffs`]）。
///
/// `replace_all` 这类工具「每个匹配位置一条 `diff_content`」，而 ACP 的 content 是
/// 整表替换语义 → 每条 diff 都要重发一份完整列表，流量与内存都是 O(N²)/O(N)。
/// 上限取 64（远大于人工可读的规模），到顶后列表冻结——稳定前缀比「追赶最新」更省
/// 流量，也不会有内容抖动。
const MAX_DIFFS_PER_CARD: usize = 64;

// ============================================================
// 工具卡片（会话级状态）
// ============================================================

/// 一张工具卡片的记忆。
#[derive(Debug, Default, Clone)]
struct Card {
    /// 是否已向客户端创建过卡片（`tool_call_stream` / `tool_call` 只创建一次）。
    created: bool,
    /// `tool_call_result` 的文本（已截断的展示副本）。
    result: Option<String>,
    /// `diff_content` 送来的文件改动（按到达顺序，最多 [`MAX_DIFFS_PER_CARD`] 条）。
    diffs: Vec<Diff>,
}

impl Card {
    /// 客户端应看到的完整 content 列表。
    ///
    /// **必须是完整列表**：ACP 的 `ToolCallUpdateFields.content` 是整表替换语义
    /// （Zed `acp_thread::apply_patch` 按位置对齐后 `truncate`），只发增量会把之前的
    /// content 抹掉。顺序与 TUI 的视觉一致：diff 在前，结果行在后。
    fn content(&self) -> Vec<ToolCallContent> {
        let mut content: Vec<ToolCallContent> = self
            .diffs
            .iter()
            .cloned()
            .map(ToolCallContent::Diff)
            .collect();
        if let Some(result) = &self.result {
            content.push(ToolCallContent::Content(
                agent_client_protocol::schema::v1::Content::new(result.clone()),
            ));
        }
        content
    }
}

/// 会话级工具卡片记忆（`HashMap` + 插入顺序队列，容量有界）。
///
/// 生命周期跟随**会话**：`tool_call_stream` 的创建去重、`diff_content` 的锚点判断
/// 都跨轮次有效（同一会话里前一轮创建的卡片在后一轮仍然算「已创建」）。
#[derive(Debug, Default)]
pub struct ToolCards {
    cards: HashMap<String, Card>,
    order: VecDeque<String>,
}

impl ToolCards {
    /// 把一个 wing 事件映射成待发送的 ACP update 列表；零帧 = 该事件不进 UI。
    ///
    /// 有状态的三类事件（工具卡片 / 结果 / diff）在这里落地；其余事件走无状态映射，
    /// 且**不会**改动任何状态。
    pub fn apply(&mut self, event: &WingEvent) -> Vec<SessionUpdate> {
        match event {
            WingEvent::ToolCallStream {
                tool_call_id,
                tool_name,
                ..
            } => self.apply_stream(tool_call_id, tool_name),
            WingEvent::ToolCall {
                tool_call_id,
                tool_name,
                tool_args,
                ..
            } => self.apply_tool_call(tool_call_id, tool_name, tool_args),
            WingEvent::ToolCallResult {
                tool_call_id,
                tool_result,
                tool_success,
                ..
            } => self.apply_result(tool_call_id, tool_result, *tool_success),
            WingEvent::DiffContent {
                path,
                old_text,
                new_text,
                tool_call_id,
                ..
            } => self.apply_diff(path, old_text.as_deref(), new_text, tool_call_id),
            other => stateless_updates(other),
        }
    }

    /// `tool_call_stream`：LLM 仍在生成参数阶段的早期信号——只创建卡片（status=pending，
    /// title=工具名），同 id 只创建一次。
    fn apply_stream(&mut self, tool_call_id: &str, tool_name: &str) -> Vec<SessionUpdate> {
        if tool_call_id.is_empty() {
            tracing::debug!("acp: tool_call_stream without tool_call_id; dropped");
            return Vec::new();
        }
        if self.card(tool_call_id).is_some_and(|card| card.created) {
            return Vec::new();
        }
        self.card_mut(tool_call_id).created = true;
        vec![SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new(tool_call_id), tool_name)
                .name(tool_name)
                .kind(tool_kind(tool_name))
                .status(ToolCallStatus::Pending),
        )]
    }

    /// `tool_call`：权威参数到达（工具即将执行）。已创建过卡片 → update；否则一次创建。
    fn apply_tool_call(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        tool_args: &serde_json::Value,
    ) -> Vec<SessionUpdate> {
        self.upsert_call(tool_call_id, tool_name, Some(tool_args.clone()))
    }

    /// 工具卡片的「创建或补齐」：实时 `tool_call` 与历史回放（[`replay`]）共用——
    /// title / kind / locations 与「已创建 → update、未创建 → create」只此一份实现。
    ///
    /// `args`：`None` = 参数缺席（历史消息的 `arguments` 可以缺席）→ `rawInput` 字段
    /// 不发；`Some(Value::Null)` 是显式 null，照发（与线格式保真）。
    fn upsert_call(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        args: Option<serde_json::Value>,
    ) -> Vec<SessionUpdate> {
        if tool_call_id.is_empty() {
            tracing::debug!("acp: tool_call without tool_call_id; dropped");
            return Vec::new();
        }
        // 标题 / 定位用的形状：参数缺席时按空对象处理（退回工具名，不发 rawInput）。
        let shape = args.as_ref().unwrap_or(&serde_json::Value::Null);
        let title = tool_title(tool_name, shape);
        let kind = tool_kind(tool_name);
        let locations = tool_locations(tool_name, shape);
        let created = self.card(tool_call_id).is_some_and(|card| card.created);
        self.card_mut(tool_call_id).created = true;

        if created {
            let mut fields = ToolCallUpdateFields::new()
                .title(title)
                .name(tool_name)
                .kind(kind)
                .locations(locations)
                .status(ToolCallStatus::InProgress);
            if let Some(args) = args {
                fields = fields.raw_input(args);
            }
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id),
                fields,
            ))]
        } else {
            let mut call = ToolCall::new(ToolCallId::new(tool_call_id), title)
                .name(tool_name)
                .kind(kind)
                .locations(locations)
                .status(ToolCallStatus::InProgress);
            if let Some(args) = args {
                call = call.raw_input(args);
            }
            vec![SessionUpdate::ToolCall(call)]
        }
    }

    /// `tool_call_result`：轮次里那张卡片的收口（status + content + rawOutput）。
    fn apply_result(
        &mut self,
        tool_call_id: &str,
        tool_result: &str,
        tool_success: bool,
    ) -> Vec<SessionUpdate> {
        if tool_call_id.is_empty() {
            tracing::debug!("acp: tool_call_result without tool_call_id; dropped");
            return Vec::new();
        }
        let shown = truncate_result(tool_result);
        if !self.card(tool_call_id).is_some_and(|card| card.created) {
            // 客户端没见过这张卡片（订阅晚于轮次开始 / 记忆已被容量淘汰）：仍然发
            // update——状态与内容是主信号，客户端会自行补一张卡片。
            tracing::debug!(tool_call_id, "acp: tool_call_result for an unseen card");
        }
        let card = self.card_mut(tool_call_id);
        card.created = true;
        card.result = Some(shown.clone());
        let content = card.content();
        let fields = ToolCallUpdateFields::new()
            .status(if tool_success {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            })
            .content(content)
            .raw_output(serde_json::Value::String(shown));
        vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(tool_call_id),
            fields,
        ))]
    }

    /// `diff_content`：把文件改动追加到已存在的卡片上（content 重新整表发送）。
    ///
    /// 锚定规则：`tool_call_id` 为空（旧网关）或从未见过该卡片 → 丢弃 + debug 日志。
    /// 客户端对未知 id 的 update 会补一张 “Tool call not found” 假卡片——宁可丢一次
    /// diff，也不要污染 UI。
    fn apply_diff(
        &mut self,
        path: &str,
        old_text: Option<&str>,
        new_text: &str,
        tool_call_id: &str,
    ) -> Vec<SessionUpdate> {
        if tool_call_id.is_empty() {
            tracing::debug!(
                path,
                "acp: diff_content without tool_call_id; dropped (no anchor)"
            );
            return Vec::new();
        }
        if self.card(tool_call_id).is_none() {
            tracing::debug!(
                path,
                tool_call_id,
                "acp: diff_content for an untracked tool call; dropped"
            );
            return Vec::new();
        }
        let mut diff = Diff::new(path, new_text);
        if let Some(old) = old_text {
            diff = diff.old_text(old);
        }
        let card = self.card_mut(tool_call_id);
        if card.diffs.len() >= MAX_DIFFS_PER_CARD {
            // 到顶即冻结：列表没变化，不发 update（也不重发列表）。
            if card.diffs.len() == MAX_DIFFS_PER_CARD {
                tracing::warn!(
                    tool_call_id,
                    cap = MAX_DIFFS_PER_CARD,
                    "acp: tool card diff cap reached; later diffs are dropped"
                );
            }
            return Vec::new();
        }
        card.diffs.push(diff);
        let content = card.content();
        vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(tool_call_id),
            ToolCallUpdateFields::new().content(content),
        ))]
    }

    // ---- 容量有界的存取 ----

    fn card(&self, id: &str) -> Option<&Card> {
        self.cards.get(id)
    }

    /// 取（必要时建）卡片；超过容量时淘汰最旧的条目。
    fn card_mut(&mut self, id: &str) -> &mut Card {
        if !self.cards.contains_key(id) {
            while self.order.len() >= MAX_TRACKED_CALLS {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                self.cards.remove(&oldest);
            }
            self.cards.insert(id.to_string(), Card::default());
            self.order.push_back(id.to_string());
        }
        self.cards
            .get_mut(id)
            .expect("card was inserted immediately above")
    }
}

// ============================================================
// 无状态映射
// ============================================================

/// 不需要工具卡片记忆的事件映射。
fn stateless_updates(event: &WingEvent) -> Vec<SessionUpdate> {
    match event {
        WingEvent::Text { content, .. } => text_update(content, false),
        WingEvent::Reasoning { content, .. } => text_update(content, true),
        WingEvent::SessionStateChanged {
            title: Some(title), ..
        } => vec![SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title.clone()),
        )],
        WingEvent::ContextStats {
            total_tokens,
            context_window_tokens,
            ..
        } => usage_update(*total_tokens, *context_window_tokens),
        other => {
            // 其余事件（delivered / notice / done / sync_session /
            // user_message_accepted / turn_started / llm_call_metrics / compact_done /
            // branch_targets / assistant_turn / tool_result_turn / session_init /
            // unknown …）不产生 ACP 帧。
            //
            // 后续步骤的追加点：
            // - 04：`SessionStateChanged{model, model_display_name, …}` →
            //   `ConfigOptionUpdate`（模型 config option 回执）；
            // - 05：`SyncSession` 的素材 → 历史回放 update 序列（`session/load` 用），
            //   届时在 `translate` 里加独立函数，仍不依赖连接。
            tracing::debug!(event_type = other.event_type(), "acp: event not mapped");
            Vec::new()
        }
    }
}

/// 文本 / 思考分片 → `agent_message_chunk` / `agent_thought_chunk`。
fn text_update(content: &str, thinking: bool) -> Vec<SessionUpdate> {
    if content.is_empty() {
        return Vec::new();
    }
    let chunk = ContentChunk::new(ContentBlock::from(content.to_string()));
    vec![if thinking {
        SessionUpdate::AgentThoughtChunk(chunk)
    } else {
        SessionUpdate::AgentMessageChunk(chunk)
    }]
}

/// `context_stats` → `usage_update`；窗口未知（<=0）时跳过（协议要求 size 合法）。
fn usage_update(total_tokens: i64, context_window_tokens: i64) -> Vec<SessionUpdate> {
    if context_window_tokens <= 0 {
        tracing::debug!(
            total_tokens,
            context_window_tokens,
            "acp: context_stats without a valid window; usage_update skipped"
        );
        return Vec::new();
    }
    let used = u64::try_from(total_tokens).unwrap_or(0);
    let size = u64::try_from(context_window_tokens).unwrap_or(0);
    vec![SessionUpdate::UsageUpdate(UsageUpdate::new(used, size))]
}

/// 会话的标题与上下文用量（`session/load` / `session/resume` 的收尾帧，
/// 数据来自 `GET /api/session/info`）。
///
/// - `name` 非空（trim 后）→ `session_info_update{title}`；否则不发；
/// - 用量沿用 [`usage_update`] 的规则（窗口 <=0 跳过）。
pub fn session_info_updates(
    name: Option<&str>,
    total_tokens: i64,
    context_window_tokens: i64,
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    if let Some(title) = name.map(str::trim).filter(|title| !title.is_empty()) {
        updates.push(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title.to_string()),
        ));
    }
    updates.extend(usage_update(total_tokens, context_window_tokens));
    updates
}

// ============================================================
// 轮次终态
// ============================================================

/// 一轮 prompt 的终态（由 `turn_result` / `interrupted` / `error` 触发）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// `turn_result`（正常/失败收口都是它；`is_error=true` 只记日志）。
    EndTurn,
    /// `interrupted`（`session/cancel` 的结果）→ `stopReason: "cancelled"`。
    Cancelled,
    /// `error` → JSON-RPC error（与 stdio 前端「error 终结轮次」同语义）。
    Failed(String),
}

/// 从事件里取轮次终态；`None` = 本轮继续。
pub fn turn_end(event: &WingEvent) -> Option<TurnEnd> {
    match event {
        WingEvent::TurnResult { is_error, .. } => {
            if *is_error {
                tracing::warn!("acp: turn_result carries is_error=true; ending as end_turn");
            }
            Some(TurnEnd::EndTurn)
        }
        WingEvent::Interrupted { .. } => Some(TurnEnd::Cancelled),
        WingEvent::Error { message, .. } => Some(TurnEnd::Failed(message.clone())),
        _ => None,
    }
}

// ============================================================
// prompt 拍平
// ============================================================

/// 把 ACP 的 `prompt: Vec<ContentBlock>` 拍平成给模型的纯文本。
///
/// 规则（01 步任务书）：
///
/// - `Text` 原样拼接（**不**插分隔符：Zed 会把一段输入按 mention 切成多个块，硬插
///   换行会改变原文）；
/// - `ResourceLink` 追加一行路径（`file://` 前缀去掉、百分号编码还原），仅用于让
///   模型看到路径；前一行不是换行结尾时先补一个换行；
/// - `Image` / `Audio` / `Resource`（本步未广告的能力）记 warn 跳过；
/// - 拍平后 trim 为空 → `None`（调用方回 invalid params）。
pub fn flatten_prompt(prompt: &[ContentBlock]) -> Option<String> {
    let mut out = String::new();
    for block in prompt {
        match block {
            ContentBlock::Text(text) => out.push_str(&text.text),
            ContentBlock::ResourceLink(link) => match file_uri_to_path(&link.uri) {
                Some(path) => {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&path);
                    out.push('\n');
                }
                // 客户端内部 uri（`zed://…` 之类）：追加进 prompt 只会给模型一行
                // 无意义文本，跳过（`file://` 主路径不受影响）。
                None => tracing::debug!(
                    uri = %link.uri,
                    "acp: non-file resource link skipped"
                ),
            },
            other => {
                tracing::warn!(
                    block = content_block_name(other),
                    "acp: prompt block not advertised; skipped"
                );
            }
        }
    }
    let trimmed = out.trim_end();
    if trimmed.trim().is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `file://` URI → 本地路径；`None` = 不是本地文件链接（调用方跳过不追加）。
fn file_uri_to_path(uri: &str) -> Option<String> {
    let stripped = uri.strip_prefix("file://")?;
    Some(percent_decode_minimal(stripped))
}

/// ACP URI 里最常见的是百分号编码的空格 / 中文；只解一层 `%XX`，不做全量 URI 解码。
fn percent_decode_minimal(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn content_block_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "text",
        ContentBlock::Image(_) => "image",
        ContentBlock::Audio(_) => "audio",
        ContentBlock::ResourceLink(_) => "resource_link",
        ContentBlock::Resource(_) => "resource",
        _ => "unknown",
    }
}

// ============================================================
// 工具 title / kind / locations 规则
// ============================================================

/// 工具类别（按工具名，大小写不敏感）。未知工具 = `other`。
fn tool_kind(tool_name: &str) -> ToolKind {
    match tool_name.to_ascii_lowercase().as_str() {
        "bash" => ToolKind::Execute,
        "read" | "readimage" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "glob" | "grep" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// 工具卡片标题（未知工具 = 工具名；参数缺席时退回工具名）。
fn tool_title(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name.to_ascii_lowercase().as_str() {
        "bash" => {
            let command = str_arg(args, "command").unwrap_or_default();
            let first_line = command.lines().next().unwrap_or("").trim();
            let line = truncate_chars(first_line, BASH_TITLE_MAX_CHARS);
            if line.is_empty() {
                tool_name.to_string()
            } else {
                line
            }
        }
        "read" | "readimage" | "write" | "edit" => {
            str_arg(args, "path").unwrap_or_else(|| tool_name.to_string())
        }
        "glob" | "grep" => {
            let pattern = str_arg(args, "pattern").unwrap_or_default();
            match str_arg(args, "path") {
                Some(path) if path != "." => format!("{pattern} @ {path}"),
                _ => {
                    if pattern.is_empty() {
                        tool_name.to_string()
                    } else {
                        pattern
                    }
                }
            }
        }
        "todowrite" => "Update todos".to_string(),
        "askuserquestion" => "Ask user".to_string(),
        _ => tool_name.to_string(),
    }
}

/// 工具影响的文件（客户端据此做 follow-along 跳转）。
fn tool_locations(tool_name: &str, args: &serde_json::Value) -> Vec<ToolCallLocation> {
    match tool_name.to_ascii_lowercase().as_str() {
        "read" | "readimage" | "write" | "edit" => match str_arg(args, "path") {
            Some(path) => vec![ToolCallLocation::new(path)],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// 取某个字符串参数（缺席 / 非字符串 / trim 后为空都视作缺席）。
fn str_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    let value = args.get(key)?.as_str()?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 按字符（非字节）截断，避免切碎多字节字符。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}…")
}

/// 工具结果截断（超出上限时在尾部注明总量）。
fn truncate_result(result: &str) -> String {
    let total = result.chars().count();
    if total <= TOOL_RESULT_MAX_CHARS {
        return result.to_string();
    }
    let kept: String = result.chars().take(TOOL_RESULT_MAX_CHARS).collect();
    format!("{kept}\n… [truncated: {total} chars total]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ContentBlock;
    use agent_client_protocol::schema::v1::ResourceLink;
    use serde_json::json;

    /// 从事件 JSON 解码（fixture = 真实网关线上的形状）。
    fn event(value: serde_json::Value) -> WingEvent {
        let rendered = value.clone();
        serde_json::from_value(value)
            .unwrap_or_else(|err| panic!("fixture must decode: {err}\n{rendered}"))
    }

    fn text_event(content: &str) -> WingEvent {
        event(json!({
            "type": "text",
            "content": content,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-1",
        }))
    }

    fn tool_call_stream(id: &str, name: &str) -> WingEvent {
        event(json!({
            "type": "tool_call_stream",
            "tool_call_id": id,
            "tool_name": name,
            "args_fragment": "{\"comm",
            "is_final": false,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-2",
        }))
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> WingEvent {
        event(json!({
            "type": "tool_call",
            "tool_name": name,
            "tool_args": args,
            "tool_call_id": id,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-3",
        }))
    }

    fn tool_call_result(id: &str, name: &str, result: &str, success: bool) -> WingEvent {
        event(json!({
            "type": "tool_call_result",
            "tool_name": name,
            "tool_args": {},
            "tool_call_id": id,
            "tool_result": result,
            "tool_success": success,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-4",
        }))
    }

    fn diff_event(id: &str, path: &str, old: Option<&str>, new: &str) -> WingEvent {
        event(json!({
            "type": "diff_content",
            "path": path,
            "old_text": old,
            "new_text": new,
            "tool_call_id": id,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-5",
        }))
    }

    fn single(updates: Vec<SessionUpdate>) -> SessionUpdate {
        assert_eq!(updates.len(), 1, "expected exactly one frame");
        updates.into_iter().next().expect("checked len")
    }

    // ---- text / reasoning ----

    #[test]
    fn text_maps_to_agent_message_chunk() {
        let mut cards = ToolCards::default();
        match single(cards.apply(&text_event("hello"))) {
            SessionUpdate::AgentMessageChunk(chunk) => {
                assert_eq!(chunk.content, ContentBlock::from("hello".to_string()));
            }
            other => panic!("expected agent_message_chunk, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_maps_to_agent_thought_chunk() {
        let mut cards = ToolCards::default();
        let ev = event(json!({
            "type": "reasoning",
            "content": "thinking…",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-6",
        }));
        match single(cards.apply(&ev)) {
            SessionUpdate::AgentThoughtChunk(chunk) => {
                assert_eq!(chunk.content, ContentBlock::from("thinking…".to_string()));
            }
            other => panic!("expected agent_thought_chunk, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_produces_no_frame() {
        let mut cards = ToolCards::default();
        assert!(cards.apply(&text_event("")).is_empty());
    }

    // ---- tool_call_stream ----

    #[test]
    fn tool_call_stream_creates_pending_card_once() {
        let mut cards = ToolCards::default();
        match single(cards.apply(&tool_call_stream("tc1", "Bash"))) {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id.to_string(), "tc1");
                assert_eq!(call.title, "Bash");
                assert_eq!(call.name.as_deref(), Some("Bash"));
                assert_eq!(call.kind, ToolKind::Execute);
                assert_eq!(call.status, ToolCallStatus::Pending);
                assert!(call.raw_input.is_none(), "参数未定，rawInput 不发");
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
        // 同一 id 的第二片 args 不再创建（本地半截 JSON 不进 UI）。
        assert!(
            cards.apply(&tool_call_stream("tc1", "Bash")).is_empty(),
            "a second stream fragment for the same id must not create again"
        );
    }

    // ---- tool_call ----

    #[test]
    fn tool_call_without_stream_creates_in_progress_card() {
        let mut cards = ToolCards::default();
        match single(cards.apply(&tool_call("tc1", "Bash", json!({"command": "ls -la"})))) {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.title, "ls -la");
                assert_eq!(call.kind, ToolKind::Execute);
                assert_eq!(call.status, ToolCallStatus::InProgress);
                assert_eq!(call.raw_input, Some(json!({"command": "ls -la"})));
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_after_stream_updates_existing_card() {
        let mut cards = ToolCards::default();
        cards.apply(&tool_call_stream("tc1", "Bash"));
        match single(cards.apply(&tool_call("tc1", "Bash", json!({"command": "ls -la"})))) {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.tool_call_id.to_string(), "tc1");
                assert_eq!(update.fields.title.as_deref(), Some("ls -la"));
                assert_eq!(update.fields.status, Some(ToolCallStatus::InProgress));
                assert_eq!(update.fields.raw_input, Some(json!({"command": "ls -la"})));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    // ---- title / kind / locations 规则 ----

    #[test]
    fn bash_title_is_first_line_truncated() {
        let long = format!("echo {}", "x".repeat(200));
        let args = json!({"command": format!("{long}\nsecond line")});
        let title = tool_title("Bash", &args);
        assert!(title.starts_with("echo "));
        assert!(
            title.ends_with('…'),
            "long command must be truncated: {title}"
        );
        assert_eq!(title.chars().count(), BASH_TITLE_MAX_CHARS + 1);
        assert!(
            !title.contains("second line"),
            "only the first line is used"
        );
        // 大小写不敏感。
        assert_eq!(tool_title("bash", &json!({"command": "ls"})), "ls");
        // 命令缺席 → 退回工具名。
        assert_eq!(tool_title("Bash", &json!({})), "Bash");
    }

    #[test]
    fn read_and_read_image_are_read_kind_with_location() {
        assert_eq!(tool_kind("Read"), ToolKind::Read);
        assert_eq!(tool_kind("read"), ToolKind::Read);
        assert_eq!(tool_kind("ReadImage"), ToolKind::Read);
        let args = json!({"path": "/tmp/a.rs"});
        assert_eq!(tool_title("Read", &args), "/tmp/a.rs");
        let locations = tool_locations("Read", &args);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path.to_string_lossy(), "/tmp/a.rs");
        assert_eq!(tool_locations("ReadImage", &args).len(), 1);
    }

    #[test]
    fn write_and_edit_are_edit_kind_with_location() {
        let args = json!({"path": "src/main.rs", "content": "x"});
        assert_eq!(tool_kind("Write"), ToolKind::Edit);
        assert_eq!(tool_kind("Edit"), ToolKind::Edit);
        assert_eq!(tool_title("Write", &args), "src/main.rs");
        assert_eq!(tool_locations("Edit", &args).len(), 1);
    }

    #[test]
    fn glob_and_grep_are_search_kind() {
        assert_eq!(tool_kind("Glob"), ToolKind::Search);
        assert_eq!(tool_kind("Grep"), ToolKind::Search);
        assert_eq!(
            tool_title("Glob", &json!({"pattern": "**/*.py"})),
            "**/*.py"
        );
        assert_eq!(
            tool_title("Grep", &json!({"pattern": "fn main", "path": "."})),
            "fn main",
            "默认 path 不进标题"
        );
        assert_eq!(
            tool_title("Grep", &json!({"pattern": "fn main", "path": "crates"})),
            "fn main @ crates"
        );
        assert!(tool_locations("Glob", &json!({"pattern": "x"})).is_empty());
    }

    #[test]
    fn todo_and_ask_titles() {
        assert_eq!(tool_kind("TodoWrite"), ToolKind::Other);
        assert_eq!(tool_title("TodoWrite", &json!({})), "Update todos");
        assert_eq!(tool_kind("AskUserQuestion"), ToolKind::Other);
        assert_eq!(tool_title("AskUserQuestion", &json!({})), "Ask user");
    }

    #[test]
    fn unknown_tool_falls_back_to_name_and_other_kind() {
        assert_eq!(tool_kind("SomeFutureTool"), ToolKind::Other);
        assert_eq!(
            tool_title("SomeFutureTool", &json!({"x": 1})),
            "SomeFutureTool"
        );
        assert!(tool_locations("SomeFutureTool", &json!({"path": "/a"})).is_empty());
    }

    // ---- tool_call_result ----

    #[test]
    fn tool_call_result_sets_status_and_content() {
        let mut cards = ToolCards::default();
        cards.apply(&tool_call("tc1", "Bash", json!({"command": "ls"})));
        match single(cards.apply(&tool_call_result("tc1", "Bash", "file.txt\n", true))) {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
                assert_eq!(
                    update.fields.content,
                    Some(vec![ToolCallContent::from("file.txt\n".to_string())])
                );
                assert_eq!(
                    update.fields.raw_output,
                    Some(json!("file.txt\n")),
                    "rawOutput 带原文"
                );
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }

        // 失败 → status=failed。
        let mut cards = ToolCards::default();
        cards.apply(&tool_call("tc2", "Bash", json!({"command": "false"})));
        match single(cards.apply(&tool_call_result("tc2", "Bash", "boom", false))) {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_result_for_an_unseen_card_still_reports_status() {
        // 客户端没见过这张卡（订阅晚于轮次开始 / 记忆被容量淘汰）：状态与内容仍是
        // 主信号，必须发 update——否则卡片永远停在 pending。
        let mut cards = ToolCards::default();
        match single(cards.apply(&tool_call_result("ghost", "Bash", "boom", false))) {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.tool_call_id.to_string(), "ghost");
                assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
                assert_eq!(
                    update.fields.content,
                    Some(vec![ToolCallContent::from("boom".to_string())])
                );
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
        // 之后同一 id 的 diff 也能锚定（结果已经把它记下来了）。
        assert_eq!(
            cards.apply(&diff_event("ghost", "a.rs", None, "n")).len(),
            1
        );
    }

    // ---- N5：空 tool_call_id 的丢弃分支 ----

    #[test]
    fn events_without_tool_call_id_are_dropped() {
        let mut cards = ToolCards::default();
        assert!(
            cards.apply(&tool_call_stream("", "Bash")).is_empty(),
            "tool_call_stream without an id must not create a card"
        );
        assert!(
            cards
                .apply(&tool_call("", "Bash", json!({"command": "ls"})))
                .is_empty(),
            "tool_call without an id must not create a card"
        );
        assert!(
            cards
                .apply(&tool_call_result("", "Bash", "out", true))
                .is_empty(),
            "tool_call_result without an id must not close/report anything"
        );
        // 没有卡片被创建（后续 diff 也锚不上）。
        assert!(cards.apply(&diff_event("", "a.rs", None, "n")).is_empty());
    }

    #[test]
    fn long_tool_result_is_truncated_with_a_note() {
        let mut cards = ToolCards::default();
        cards.apply(&tool_call("tc1", "Bash", json!({"command": "rg ."})));
        let huge = "x".repeat(TOOL_RESULT_MAX_CHARS + 100);
        let update = match single(cards.apply(&tool_call_result("tc1", "Bash", &huge, true))) {
            SessionUpdate::ToolCallUpdate(update) => update,
            other => panic!("expected tool_call_update, got {other:?}"),
        };
        let raw = update.fields.raw_output.expect("rawOutput").to_string();
        assert!(raw.contains("truncated"), "{raw}");
        assert!(raw.len() < huge.len());
        assert!(
            raw.contains(&format!("{} chars total", huge.chars().count())),
            "{raw}"
        );
    }

    // ---- diff_content ----

    #[test]
    fn diff_appends_to_existing_card_and_keeps_result() {
        let mut cards = ToolCards::default();
        cards.apply(&tool_call_stream("tc1", "Edit"));
        cards.apply(&tool_call("tc1", "Edit", json!({"path": "a.rs"})));

        // diff 先到：content = [diff]
        match single(cards.apply(&diff_event("tc1", "a.rs", Some("old"), "new"))) {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.content.as_ref().map(Vec::len), Some(1));
                assert!(matches!(
                    update.fields.content.as_ref().expect("content")[0],
                    ToolCallContent::Diff(_)
                ));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }

        // 结果后到：content = [diff, text]（整表替换，diff 必须在）
        match single(cards.apply(&tool_call_result("tc1", "Edit", "edited", true))) {
            SessionUpdate::ToolCallUpdate(update) => {
                let content = update.fields.content.expect("content");
                assert_eq!(content.len(), 2);
                assert!(matches!(content[0], ToolCallContent::Diff(_)));
                assert!(matches!(content[1], ToolCallContent::Content(_)));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }

        // 再来一个 diff：content = [diff, diff, text]
        match single(cards.apply(&diff_event("tc1", "a.rs", None, "newer"))) {
            SessionUpdate::ToolCallUpdate(update) => {
                let content = update.fields.content.expect("content");
                assert_eq!(content.len(), 3);
                match &content[1] {
                    ToolCallContent::Diff(diff) => {
                        assert_eq!(diff.path.to_string_lossy(), "a.rs");
                        assert_eq!(diff.old_text, None, "新文件没有 old_text");
                        assert_eq!(diff.new_text, "newer");
                    }
                    other => panic!("expected diff, got {other:?}"),
                }
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn diff_without_anchor_is_dropped() {
        let mut cards = ToolCards::default();
        // 空 tool_call_id（旧网关）。
        assert!(cards.apply(&diff_event("", "a.rs", None, "new")).is_empty());
        // 未知 tool_call_id（卡片从未创建过）——宁可丢 diff 也不制造假卡片。
        assert!(
            cards
                .apply(&diff_event("ghost", "a.rs", None, "new"))
                .is_empty()
        );
        // 卡片创建后可以锚定。
        cards.apply(&tool_call_stream("tc1", "Write"));
        assert_eq!(
            cards.apply(&diff_event("tc1", "a.rs", None, "new")).len(),
            1
        );
    }

    // ---- session_state_changed / context_stats ----

    #[test]
    fn session_state_changed_title_maps_to_session_info_update() {
        let mut cards = ToolCards::default();
        let ev = event(json!({
            "type": "session_state_changed",
            "title": "修 ACP 前端",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-7",
        }));
        match single(cards.apply(&ev)) {
            SessionUpdate::SessionInfoUpdate(update) => {
                assert_eq!(
                    update.title.value().map(String::as_str),
                    Some("修 ACP 前端")
                );
            }
            other => panic!("expected session_info_update, got {other:?}"),
        }
    }

    #[test]
    fn session_state_changed_other_fields_produce_no_frame() {
        let mut cards = ToolCards::default();
        let ev = event(json!({
            "type": "session_state_changed",
            "model": "gpt-4o",
            "thinking": true,
            "yolo": false,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-8",
        }));
        assert!(cards.apply(&ev).is_empty());
    }

    #[test]
    fn context_stats_maps_to_usage_update() {
        let mut cards = ToolCards::default();
        let ev = event(json!({
            "type": "context_stats",
            "message_count": 12,
            "total_tokens": 5000,
            "context_window_tokens": 80000,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-9",
        }));
        match single(cards.apply(&ev)) {
            SessionUpdate::UsageUpdate(update) => {
                assert_eq!(update.used, 5000);
                assert_eq!(update.size, 80000);
            }
            other => panic!("expected usage_update, got {other:?}"),
        }
    }

    #[test]
    fn context_stats_without_window_is_skipped() {
        let mut cards = ToolCards::default();
        for window in [0, -1] {
            let ev = event(json!({
                "type": "context_stats",
                "message_count": 12,
                "total_tokens": 5000,
                "context_window_tokens": window,
                "created_at": "2026-01-01T00:00:00+00:00",
                "session_id": "s1",
                "request_id": "req-10",
            }));
            assert!(
                cards.apply(&ev).is_empty(),
                "window {window} must not produce a usage_update"
            );
        }
        // 负的 used 归零（u64 不能表示负数）。
        let ev = event(json!({
            "type": "context_stats",
            "message_count": 1,
            "total_tokens": -5,
            "context_window_tokens": 100,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-11",
        }));
        match single(cards.apply(&ev)) {
            SessionUpdate::UsageUpdate(update) => assert_eq!(update.used, 0),
            other => panic!("expected usage_update, got {other:?}"),
        }
    }

    // ---- 轮次终态 ----

    #[test]
    fn turn_end_covers_result_interrupt_and_error() {
        let result = event(json!({
            "type": "turn_result",
            "subtype": "success",
            "is_error": false,
            "num_turns": 3,
            "duration_ms": 1200,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-12",
        }));
        assert_eq!(turn_end(&result), Some(TurnEnd::EndTurn));

        let failed_turn = event(json!({
            "type": "turn_result",
            "is_error": true,
            "errors": ["boom"],
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-13",
        }));
        assert_eq!(
            turn_end(&failed_turn),
            Some(TurnEnd::EndTurn),
            "is_error 仍收 end_turn（只记日志）"
        );

        let interrupted = event(json!({
            "type": "interrupted",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-14",
        }));
        assert_eq!(turn_end(&interrupted), Some(TurnEnd::Cancelled));

        let error = event(json!({
            "type": "error",
            "message": "provider exploded",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-15",
        }));
        assert_eq!(
            turn_end(&error),
            Some(TurnEnd::Failed("provider exploded".into()))
        );

        // 非终态事件。
        assert_eq!(turn_end(&text_event("hi")), None);
    }

    #[test]
    fn ignored_events_produce_no_frames() {
        let mut cards = ToolCards::default();
        let ignored = [
            json!({"type": "delivered", "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "done", "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "turn_started", "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "user_message_accepted", "content": "x", "origin_request_id": "r",
                   "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "notice", "message": "retrying", "created_at": "c",
                   "session_id": "s1", "request_id": "r"}),
            json!({"type": "llm_call_metrics", "prompt_tokens": 1, "completion_tokens": 1,
                   "cached_tokens": 0, "first_chunk_rt_ms": 1.0, "tokens_per_sec": 1.0,
                   "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "session_init", "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "sync_session", "session_id": "s1", "status": "idle", "messages": [],
                   "created_at": "c", "request_id": "r"}),
            json!({"type": "compact_done", "original_tokens": 100, "compressed_tokens": 10,
                   "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "branch_targets", "targets": [], "created_at": "c",
                   "session_id": "s1", "request_id": "r"}),
            json!({"type": "assistant_turn", "content_blocks": [], "created_at": "c",
                   "session_id": "s1", "request_id": "r"}),
            json!({"type": "tool_result_turn", "tool_use_id": "t1", "tool_name": "Bash",
                   "content": "x", "created_at": "c", "session_id": "s1", "request_id": "r"}),
            // ask 本身不产帧：分流与应答在 `ask` 模块（permission / elicitation / 回退）。
            json!({"type": "ask", "tool_call_id": "tc_ask", "question": "?", "required": true,
                   "created_at": "c", "session_id": "s1", "request_id": "r"}),
            json!({"type": "from_the_future", "created_at": "c", "session_id": "s1",
                   "request_id": "r"}),
        ];
        for value in ignored {
            let kind = value["type"].clone();
            assert!(
                cards.apply(&event(value)).is_empty(),
                "event {kind} must not produce frames"
            );
        }
    }

    // ---- 容量有界 ----

    #[test]
    fn per_card_diffs_are_capped() {
        let mut cards = ToolCards::default();
        cards.apply(&tool_call_stream("tc1", "Edit"));
        cards.apply(&tool_call("tc1", "Edit", json!({"path": "a.rs"})));

        for i in 0..MAX_DIFFS_PER_CARD {
            let updates = cards.apply(&diff_event("tc1", "a.rs", None, &format!("v{i}")));
            assert_eq!(updates.len(), 1, "第 {i} 条 diff 应当发 update");
        }
        // 到顶之后：不再追加、不再发 update（列表冻结，无内容抖动）。
        let extra = cards.apply(&diff_event("tc1", "a.rs", None, "overflow"));
        assert!(extra.is_empty(), "超出上限的 diff 必须被丢弃且不产帧");

        // 结果行仍会把冻结后的完整列表发一次（整表替换语义不变）。
        match single(cards.apply(&tool_call_result("tc1", "Edit", "edited", true))) {
            SessionUpdate::ToolCallUpdate(update) => {
                let content = update.fields.content.expect("content");
                assert_eq!(
                    content.len(),
                    MAX_DIFFS_PER_CARD + 1,
                    "64 条 diff + 1 条结果"
                );
                assert!(matches!(content[0], ToolCallContent::Diff(_)));
                assert!(matches!(
                    content[MAX_DIFFS_PER_CARD],
                    ToolCallContent::Content(_)
                ));
                match &content[MAX_DIFFS_PER_CARD - 1] {
                    ToolCallContent::Diff(diff) => {
                        assert_eq!(
                            diff.new_text,
                            format!("v{}", MAX_DIFFS_PER_CARD - 1),
                            "保留的是最早的 64 条"
                        );
                    }
                    other => panic!("expected diff, got {other:?}"),
                }
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn tool_card_memory_is_bounded() {
        let mut cards = ToolCards::default();
        for i in 0..(MAX_TRACKED_CALLS + 5) {
            cards.apply(&tool_call_stream(&format!("tc{i}"), "Read"));
        }
        assert_eq!(cards.order.len(), MAX_TRACKED_CALLS);
        assert_eq!(cards.cards.len(), MAX_TRACKED_CALLS);
        // 最旧的已被淘汰：针对它的 diff 丢弃（而不是制造假卡片）。
        assert!(
            cards
                .apply(&diff_event("tc0", "a.rs", None, "n"))
                .is_empty()
        );
        // 最新的仍在。
        let newest = format!("tc{}", MAX_TRACKED_CALLS + 4);
        assert_eq!(
            cards.apply(&diff_event(&newest, "a.rs", None, "n")).len(),
            1
        );
    }

    // ---- prompt 拍平 ----

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::from(text.to_string())
    }

    fn link(uri: &str) -> ContentBlock {
        ContentBlock::ResourceLink(ResourceLink::new("f", uri))
    }

    #[test]
    fn flatten_prompt_keeps_text_and_appends_link_paths() {
        let prompt = vec![
            text_block("look at "),
            link("file:///Users/x/a%20b.rs"),
            text_block(" and this"),
        ];
        assert_eq!(
            flatten_prompt(&prompt).as_deref(),
            Some("look at \n/Users/x/a b.rs\n and this")
        );
    }

    #[test]
    fn flatten_prompt_handles_plain_text_only() {
        assert_eq!(
            flatten_prompt(&[text_block("hello "), text_block("world")]).as_deref(),
            Some("hello world"),
            "相邻 text 块之间不插分隔符（Zed 会按 mention 切块）"
        );
    }

    #[test]
    fn flatten_prompt_skips_unadvertised_blocks() {
        use agent_client_protocol::schema::v1::ImageContent;
        let prompt = vec![
            ContentBlock::Image(ImageContent::new("aGk=", "image/png")),
            text_block("描述这张图"),
        ];
        assert_eq!(flatten_prompt(&prompt).as_deref(), Some("描述这张图"));
    }

    #[test]
    fn flatten_prompt_skips_non_file_links() {
        // 客户端内部 uri（zed://…）不追加：模型不该看到一行无意义文本。
        assert_eq!(
            flatten_prompt(&[text_block("hi"), link("zed://thread/42")]).as_deref(),
            Some("hi")
        );
        assert_eq!(flatten_prompt(&[link("zed://thread/42")]), None);
        // `file://` 主路径不受影响。
        assert_eq!(
            flatten_prompt(&[link("file:///tmp/a.rs")]).as_deref(),
            Some("/tmp/a.rs")
        );
    }

    #[test]
    fn flatten_prompt_empty_is_none() {
        assert_eq!(flatten_prompt(&[]), None);
        assert_eq!(flatten_prompt(&[text_block("   ")]), None);
        assert_eq!(
            flatten_prompt(&[link("file:///tmp")]),
            Some("/tmp".to_string())
        );
    }

    // ---- 会话收尾帧（session/load · session/resume） ----

    #[test]
    fn session_info_updates_send_title_and_usage() {
        let updates = session_info_updates(Some("修 ACP 前端"), 5000, 80000);
        assert_eq!(updates.len(), 2);
        match &updates[0] {
            SessionUpdate::SessionInfoUpdate(update) => {
                assert_eq!(
                    update.title.value().map(String::as_str),
                    Some("修 ACP 前端")
                );
            }
            other => panic!("expected session_info_update, got {other:?}"),
        }
        match &updates[1] {
            SessionUpdate::UsageUpdate(update) => {
                assert_eq!(update.used, 5000);
                assert_eq!(update.size, 80000);
            }
            other => panic!("expected usage_update, got {other:?}"),
        }
    }

    #[test]
    fn session_info_updates_skip_blank_titles_and_bad_windows() {
        // 无标题 + 窗口未知：什么都没有。
        assert!(session_info_updates(None, 10, 0).is_empty());
        // 空白标题视同缺席，用量照发。
        let updates = session_info_updates(Some("   "), 10, 100);
        assert_eq!(updates.len(), 1);
        assert!(matches!(updates[0], SessionUpdate::UsageUpdate(_)));
        // 窗口合法但标题缺席。
        let updates = session_info_updates(None, 10, 100);
        assert_eq!(updates.len(), 1);
        assert!(matches!(updates[0], SessionUpdate::UsageUpdate(_)));
    }
}
