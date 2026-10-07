//! `--include-partial-messages`：Anthropic SSE 形状的 `stream_event` 状态机。
//!
//! 置位该旗标时，消费方（Claude Agent SDK / CloudCLI 等）期望在每条消息的
//! 快照帧（`assistant`）之前先收到逐条 `stream_event` 帧
//! （`SDKPartialAssistantMessage`：`event` 是 Anthropic Messages API 的
//! `RawMessageStreamEvent`）。wing 的流式素材是三路 `persist=false` 的瞬态
//! 广播——`text` / `reasoning` / `tool_call_stream`（`tool_call_stream` 的
//! `args_fragment` 是自上次事件以来的**增量**原始参数文本）——本模块把它们
//! 翻译成 SSE 事件序列。
//!
//! **id 一致性（硬要求）**：`message_start` 的 `message.id` 必须与同一消息
//! 最终快照帧的 `message.id` 一致（消费方以它做流式/快照去重）。跨帧关联在
//! 本模块内完成：首个增量铸 id，快照帧（`assistant_turn`）收口时把它交给
//! 渲染器复用（方案理由见 `03_streaming/design.md` D1）。
//!
//! **顺序不变量**（由 ReAct 循环保证）：同一消息的 delta 全部先于它的
//! `assistant_turn`（快照），两者之间不会插入另一条消息的 delta；一轮可含
//! 多条消息，轮边界由 `turn_started` 给出。状态机据此维持「至多一条在途消息、
//! 至多一个打开块」，只持有 id / index / kind，delta 逐条透传（不缓存内容）。
//!
//! 帧序（消息收口时）：`…content_block_stop → message_delta → message_stop`，
//! 然后才是快照帧——SDK 的契约就是「最终完整消息仍会作为独立消息到来」。

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

/// 铸一条消息的 id（`msg_` + 32 位十六进制，与后端 `AssistantTurnEvent.uuid`
/// 的 `uuid4().hex` 同形；真 CLI 的 message id 也是 `msg_` 前缀）。
///
/// 不为此引入 uuid 依赖：单进程内「当前纳秒 + 单调计数器」已保证唯一，且与
/// 本进程其它帧 id（`crate::protocol::generate_request_id`）同一思路。
fn mint_message_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("msg_{nanos:016x}{seq:016x}")
}

/// 在途流式消息（`message_start` 已发、`message_stop` 未发）。
struct OpenMessage {
    /// `message.id`——与最终快照帧同一个值（去重键）。
    id: String,
    /// 当前打开的块（`None` = 块已收口 / 还没开）。
    open_block: Option<OpenBlock>,
    /// 下一个 `content_block_start` 的 index（消息内单调，不复用）。
    next_index: usize,
}

/// 一个已打开的 content block。
struct OpenBlock {
    index: usize,
    kind: BlockKind,
}

/// 块种类（切换判据；`ToolUse` 按 tool_call_id 区分不同调用）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse { id: String },
}

/// 流式帧状态机（渲染器持有）。
pub struct StreamState {
    open: Option<OpenMessage>,
    /// 最近已知的模型名——`message_start` 的占位（增量事件不携带模型名；
    /// 真值在快照帧里）。来源：`session_init` / `assistant_turn`。
    model: String,
}

impl StreamState {
    pub fn new() -> Self {
        Self {
            open: None,
            model: String::new(),
        }
    }

    /// 记住最近已知的模型名（`message_start` 占位用）。
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    /// 在途消息的 id（快照帧复用它产出 `message.id`）。`None` = 无在途消息。
    pub fn current_id(&self) -> Option<&str> {
        self.open.as_ref().map(|m| m.id.as_str())
    }

    /// 文本增量 → 要发出的 SSE 事件。
    pub fn text(&mut self, content: &str) -> Vec<Value> {
        let mut events = self.ensure_open();
        events.extend(self.switch_block(BlockKind::Text, json!({"type": "text", "text": ""})));
        let index = self.current_block_index();
        events.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": content},
        }));
        events
    }

    /// reasoning（thinking）增量 → 要发出的 SSE 事件。
    pub fn reasoning(&mut self, content: &str) -> Vec<Value> {
        let mut events = self.ensure_open();
        events.extend(self.switch_block(
            BlockKind::Thinking,
            json!({"type": "thinking", "thinking": ""}),
        ));
        let index = self.current_block_index();
        events.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": content},
        }));
        events
    }

    /// 工具参数增量 → 要发出的 SSE 事件。
    ///
    /// `fragment` 是 provider 侧的**增量**切片（`args_buffer[emitted_len:]`，
    /// 不重发、不重叠），逐字映射 `input_json_delta.partial_json`。`is_final`
    /// （参数文本终结）收口该块——Anthropic 的 `content_block_stop` 语义。
    pub fn tool_call(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        fragment: &str,
        is_final: bool,
    ) -> Vec<Value> {
        // id 未知的增量不开块（provider 在 id 已知前不发事件；防御性跳过——
        // 没有 id 就无法在快照/结果帧里对账这个块）。
        if tool_call_id.is_empty() {
            return Vec::new();
        }
        let mut events = self.ensure_open();
        events.extend(self.switch_block(
            BlockKind::ToolUse {
                id: tool_call_id.to_string(),
            },
            json!({"type": "tool_use", "id": tool_call_id, "name": tool_name, "input": {}}),
        ));
        let index = self.current_block_index();
        if !fragment.is_empty() {
            events.push(json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": fragment},
            }));
        }
        if is_final {
            events.extend(self.close_block());
        }
        events
    }

    /// 收口在途消息：块 stop（若有打开块）→ `message_delta` → `message_stop`。
    ///
    /// `stop_reason` 传快照帧的同值（`Some`，非快照收口传 `None` → `null`）；
    /// `usage` 传快照帧的 usage（取 `output_tokens`，缺省 0）。无在途消息 =
    /// 空列表（幂等）。
    pub fn close_message(
        &mut self,
        stop_reason: Option<&str>,
        usage: Option<&Value>,
    ) -> Vec<Value> {
        let Some(open) = self.open.take() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(block) = open.open_block {
            events.push(json!({"type": "content_block_stop", "index": block.index}));
        }
        events.push(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason.map_or(Value::Null, |s| Value::String(s.to_string())),
                "stop_sequence": null,
            },
            // Anthropic 的 message_delta usage 只报 output_tokens；缺省补 0
            // （形状恒定——消费方按字段读）。
            "usage": {"output_tokens": output_tokens(usage)},
        }));
        events.push(json!({"type": "message_stop"}));
        events
    }

    // ── 内部 ──

    /// 确保有在途消息（首个增量时发 `message_start`）。
    fn ensure_open(&mut self) -> Vec<Value> {
        if self.open.is_some() {
            return Vec::new();
        }
        let id = mint_message_id();
        let start = json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            },
        });
        self.open = Some(OpenMessage {
            id,
            open_block: None,
            next_index: 0,
        });
        vec![start]
    }

    /// 切到目标块：必要时发旧块 `content_block_stop` + 新块 `content_block_start`。
    /// 已是同类块 = 空列表（直接追加 delta）。
    fn switch_block(&mut self, kind: BlockKind, content_block: Value) -> Vec<Value> {
        let open = self
            .open
            .as_mut()
            .expect("switch_block 只在有在途消息时调用");
        if open.open_block.as_ref().is_some_and(|b| b.kind == kind) {
            return Vec::new();
        }
        let mut events = Vec::new();
        if let Some(block) = open.open_block.take() {
            events.push(json!({"type": "content_block_stop", "index": block.index}));
        }
        let index = open.next_index;
        open.next_index += 1;
        open.open_block = Some(OpenBlock { index, kind });
        events.push(json!({
            "type": "content_block_start",
            "index": index,
            "content_block": content_block,
        }));
        events
    }

    /// 收口当前打开块（无打开块 = 空列表）。
    fn close_block(&mut self) -> Vec<Value> {
        let Some(open) = self.open.as_mut() else {
            return Vec::new();
        };
        match open.open_block.take() {
            Some(block) => vec![json!({"type": "content_block_stop", "index": block.index})],
            None => Vec::new(),
        }
    }

    /// 当前打开块的 index（刚开/刚追加时必定有）。
    fn current_block_index(&self) -> usize {
        self.open
            .as_ref()
            .and_then(|m| m.open_block.as_ref())
            .map(|b| b.index)
            .expect("delta 到达时必定已有打开块")
    }
}

impl Default for StreamState {
    fn default() -> Self {
        Self::new()
    }
}

/// 快照 usage 的 `output_tokens`（缺省 0）。
fn output_tokens(usage: Option<&Value>) -> i64 {
    usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    //! 帧形状以 Anthropic SSE 的 `RawMessageStreamEvent` 为准（SDK 类型
    //! `BetaRawMessageStreamEvent`），状态机性质（开/切块、收口顺序、id）
    //! 逐条钉住。

    use super::*;

    /// 事件类型序列（断言帧序的骨架）。
    fn types(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .map(|e| e["type"].as_str().unwrap_or("<missing>"))
            .collect()
    }

    // ---- 逐 event 载荷形状 ----

    #[test]
    fn text_delta_opens_message_and_block_with_the_anthropic_shape() {
        let mut state = StreamState::new();
        state.set_model("test-model");

        let events = state.text("Hel");
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );

        let start = &events[0];
        let message = &start["message"];
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "test-model");
        assert_eq!(message["content"], json!([]));
        assert_eq!(message["stop_reason"], Value::Null);
        assert_eq!(message["stop_sequence"], Value::Null);
        assert_eq!(
            message["usage"],
            json!({"input_tokens": 0, "output_tokens": 0})
        );
        let id = message["id"].as_str().unwrap();
        assert!(id.starts_with("msg_"), "{id}");
        assert_eq!(id.len(), 4 + 32, "msg_ + 32 hex: {id}");
        assert!(
            id[4..].chars().all(|c| c.is_ascii_hexdigit()),
            "id 是 uuid4().hex 形状: {id}"
        );

        assert_eq!(
            events[1],
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}})
        );
        assert_eq!(
            events[2],
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Hel"}})
        );

        // 同一消息的后续增量：不再开消息、不再开块。
        let events = state.text("lo");
        assert_eq!(types(&events), vec!["content_block_delta"]);
        assert_eq!(
            events[0],
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "lo"}})
        );
    }

    #[test]
    fn reasoning_delta_uses_the_thinking_shape() {
        let mut state = StreamState::new();

        let events = state.reasoning("hmm");
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(
            events[1],
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "thinking", "thinking": ""}})
        );
        assert_eq!(
            events[2],
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "hmm"}})
        );
    }

    #[test]
    fn tool_call_delta_streams_input_json_verbatim() {
        let mut state = StreamState::new();

        let events = state.tool_call("call_1", "Bash", "{\"cmd", false);
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(
            events[1],
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}}})
        );
        // 增量原样透传（provider 已切成增量，不重发、不重叠）。
        assert_eq!(
            events[2],
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"cmd"}})
        );
    }

    #[test]
    fn tool_call_is_final_closes_the_block() {
        let mut state = StreamState::new();
        state.tool_call("call_1", "Bash", "{\"cmd\": 1}", false);

        let events = state.tool_call("call_1", "Bash", "", true);
        assert_eq!(
            types(&events),
            vec!["content_block_stop"],
            "is_final + 空片段只收口（不产空 delta）: {events:?}"
        );
        assert_eq!(events[0], json!({"type": "content_block_stop", "index": 0}));
    }

    #[test]
    fn empty_tool_call_id_is_skipped_without_opening_anything() {
        let mut state = StreamState::new();
        assert!(state.tool_call("", "Bash", "{}", false).is_empty());
        assert!(state.current_id().is_none(), "不得开消息");
    }

    // ---- 块状态机 ----

    #[test]
    fn switching_type_closes_the_old_block_and_opens_a_new_index() {
        let mut state = StreamState::new();

        state.reasoning("think");
        let events = state.text("say");

        assert_eq!(
            types(&events),
            vec![
                "content_block_stop",
                "content_block_start",
                "content_block_delta"
            ],
            "thinking → text 必须切块: {events:?}"
        );
        assert_eq!(events[0], json!({"type": "content_block_stop", "index": 0}));
        assert_eq!(
            events[1],
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "text", "text": ""}})
        );
        assert_eq!(events[2]["index"], 1);
    }

    #[test]
    fn switching_back_opens_a_fresh_index() {
        // A → B → A：Anthropic 的 index 是块身份，不复用。
        let mut state = StreamState::new();
        state.text("a");
        state.reasoning("b");
        let events = state.text("c");

        assert_eq!(
            types(&events),
            vec![
                "content_block_stop",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(events[1]["index"], 2, "第三个块: {events:?}");
    }

    #[test]
    fn parallel_tool_calls_get_their_own_blocks() {
        let mut state = StreamState::new();
        state.tool_call("call_1", "Bash", "{}", false);
        let events = state.tool_call("call_2", "Read", "{\"path\": \"x\"}", false);

        assert_eq!(
            types(&events),
            vec![
                "content_block_stop",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(events[1]["index"], 1);
        assert_eq!(events[1]["content_block"]["id"], "call_2");
        assert_eq!(events[1]["content_block"]["name"], "Read");
    }

    #[test]
    fn text_after_a_final_tool_block_opens_a_new_block() {
        let mut state = StreamState::new();
        state.tool_call("call_1", "Bash", "{}", true); // 块被 is_final 收口
        let events = state.text("after");

        assert_eq!(
            types(&events),
            vec!["content_block_start", "content_block_delta"],
            "已收口的块不复用: {events:?}"
        );
        assert_eq!(events[0]["index"], 1);
    }

    /// 长增量流不累积状态：1000 个 delta 只有一对 start/stop（无界缓冲防御）。
    #[test]
    fn many_deltas_do_not_accumulate_blocks() {
        let mut state = StreamState::new();
        let mut starts = 0;
        let mut stops = 0;
        for i in 0..1000 {
            for event in state.text(&format!("x{i}")) {
                match event["type"].as_str().unwrap() {
                    "content_block_start" => starts += 1,
                    "content_block_stop" => stops += 1,
                    _ => {}
                }
            }
        }
        assert_eq!((starts, stops), (1, 0), "只有一个打开块、一次开启");

        let close = state.close_message(None, None);
        assert_eq!(
            types(&close),
            vec!["content_block_stop", "message_delta", "message_stop"]
        );
        let mut starts = 0;
        for event in state.text("next message") {
            if event["type"] == "content_block_start" {
                starts += 1;
            }
        }
        assert_eq!(starts, 1, "新消息重新开块");
    }

    // ---- 收口 ----

    #[test]
    fn close_message_emits_delta_and_stop_with_the_snapshot_values() {
        let mut state = StreamState::new();
        state.text("hi");
        let usage = json!({"input_tokens": 10, "output_tokens": 7, "cached_tokens": 0});

        let events = state.close_message(Some("tool_use"), Some(&usage));
        assert_eq!(
            types(&events),
            vec!["content_block_stop", "message_delta", "message_stop"]
        );
        assert_eq!(events[0], json!({"type": "content_block_stop", "index": 0}));
        assert_eq!(
            events[1],
            json!({"type": "message_delta",
                   "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                   "usage": {"output_tokens": 7}})
        );
        assert_eq!(events[2], json!({"type": "message_stop"}));
    }

    #[test]
    fn close_message_without_snapshot_values_is_still_well_formed() {
        let mut state = StreamState::new();
        state.reasoning("cut");

        let events = state.close_message(None, None);
        assert_eq!(
            events[1],
            json!({"type": "message_delta",
                   "delta": {"stop_reason": null, "stop_sequence": null},
                   "usage": {"output_tokens": 0}})
        );
    }

    #[test]
    fn close_message_is_idempotent_and_closes_nothing_when_no_message() {
        let mut state = StreamState::new();
        assert!(state.close_message(None, None).is_empty());

        state.text("hi");
        assert!(!state.close_message(None, None).is_empty());
        assert!(state.close_message(None, None).is_empty(), "已收口 = no-op");
        assert!(state.current_id().is_none());
    }

    /// 已收口的块不被重复 stop（`is_final` 之后再收口消息只发 delta/stop）。
    #[test]
    fn closing_after_a_final_tool_block_skips_the_block_stop() {
        let mut state = StreamState::new();
        state.tool_call("call_1", "Bash", "{}", true);

        let events = state.close_message(Some("tool_use"), None);
        assert_eq!(
            types(&events),
            vec!["message_delta", "message_stop"],
            "块已由 is_final 收口: {events:?}"
        );
    }

    // ---- id 铸造与复用 ----

    #[test]
    fn each_message_gets_a_fresh_id() {
        let mut state = StreamState::new();
        state.text("first");
        let first = state.current_id().unwrap().to_string();

        state.close_message(None, None);
        state.text("second");
        let second = state.current_id().unwrap().to_string();

        assert_ne!(first, second, "两条消息两个 id");
        assert!(first.starts_with("msg_") && second.starts_with("msg_"));
    }

    #[test]
    fn id_is_stable_for_the_whole_message() {
        let mut state = StreamState::new();
        state.text("a");
        let id = state.current_id().unwrap().to_string();
        state.reasoning("b");
        state.tool_call("call_1", "Bash", "{}", false);
        assert_eq!(state.current_id(), Some(id.as_str()));
    }

    /// `message_start` 的 id 与 `current_id()`（快照帧复用值）逐字一致。
    #[test]
    fn message_start_id_matches_current_id() {
        let mut state = StreamState::new();
        let events = state.text("hi");
        let started = events[0]["message"]["id"].as_str().unwrap();
        assert_eq!(Some(started), state.current_id());
    }
}
