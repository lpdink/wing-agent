//! 分片重组——把 gateway 切分的 `_chunk` 信封在传输层还原成完整事件。
//!
//! 背景：客户端单帧上限是 tungstenite 默认的 16 MiB（不动），而 `sync_session`
//! 载荷可以更大（实测 22,151,988 B）。网关在唯一 wire 出口把超限载荷按 UTF-8
//! 边界切成 N 个信封帧（`type == "_chunk"`）；本模块在**读任务内**把它们拼回
//! 完整 JSON 再交给应用层——应用层只见完整事件，零感知分片。
//!
//! 三条不变量（见 spec `ws-frame-chunking`）：
//!
//! 1. **保序**：窗口打开期间（已收到某事件的首片、未闭合）所有其他帧原样缓冲、
//!    不解析、不投递；闭合时先投递完整事件、再按到达序放行缓冲帧。否则
//!    `sync_session` 与其后的 live delta 交错会让应用的 `chat.clear()` 吞事件。
//! 2. **有界**：帧数上限、缓冲字节上限、不闭合超时；越界即失败（断开 + 原因）。
//! 3. **安全失败**：畸形信封（count 越界 / index 空洞重复 / 换 id）绝不产出
//!    损坏事件，也绝不按 `count` 预分配内存。
//!
//! 本模块是纯状态机（无 I/O）：`on_text` 输入一帧原始文本，输出「投递 / 等待 /
//! 失败」三态；计时与断开由调用方（读任务）负责。

use std::collections::VecDeque;
use std::time::Duration;

use serde::Deserialize;
use serde::Deserializer;
use serde::de::Error as _;
use tokio::time::Instant;

use crate::protocol::WingEvent;

/// 传输层保留的分片信封 `type`；应用事件 MUST NOT 使用 `_` 前缀。
pub const CHUNK_TYPE: &str = "_chunk";

/// 单个事件的帧数上限（防御畸形 `count`：不预分配、不无界累积）。
pub const MAX_CHUNKS: usize = 1024;

/// 重组窗口的字节上限：未闭合分片 + 窗口内缓冲帧之和。
pub const MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;

/// 分片不闭合超时：窗口内没有任何新帧到达超过它即失败（4 个事件帧的
/// 30s 默认值给慢链路留足余量；有进度即重置——静默才是异常信号）。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 可注入的防护限额（生产用 [`Limits::default`]；测试用小值构造边界场景）。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// 单个事件的帧数上限。
    pub max_chunks: usize,
    /// 重组窗口的字节上限。
    pub max_buffered_bytes: usize,
    /// 分片不闭合（无进展）的超时。
    pub idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_chunks: MAX_CHUNKS,
            max_buffered_bytes: MAX_BUFFERED_BYTES,
            idle_timeout: IDLE_TIMEOUT,
        }
    }
}

/// 信封的 `type` 判别子：非 `_chunk` 的值在解析**当刻**被拒绝——窗口内的
/// 普通帧因此只需付出一次廉价的扫描（不做完整解析）。
#[derive(Debug, Clone, Copy)]
struct ChunkTag;

impl<'de> Deserialize<'de> for ChunkTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            CHUNK_TYPE => Ok(ChunkTag),
            _ => Err(D::Error::custom("not a chunk envelope")),
        }
    }
}

/// 分片信封（字段与语义见 `docs/dev/http-api.md`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ChunkEnvelope {
    #[serde(rename = "type")]
    _tag: ChunkTag,
    /// 同一事件的所有帧共享；事件间由网关保证唯一（诊断/去重用）。
    pub id: String,
    /// 0-based 帧序号。
    pub index: usize,
    /// 该事件的总帧数（≥ 2）。
    pub count: usize,
    /// 原始事件的 `type`（诊断用）。
    pub of_type: String,
    /// 原始载荷 JSON 文本的一段。
    pub data: String,
}

impl ChunkEnvelope {
    /// 尝试把一帧文本解析成信封；不是 `_chunk` / 字段不合法 → `None`。
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str::<ChunkEnvelope>(text).ok()
    }
}

/// 一帧文本的处理结果。
#[derive(Debug)]
pub enum Outcome {
    /// 可以投递给应用层的事件（普通帧 1 个；闭合时是完整事件 + 窗口内缓冲帧）。
    Deliver(Vec<WingEvent>),
    /// 分片已累积，窗口尚未闭合——本帧不投递。
    Pending,
    /// 协议错误：调用方 MUST 记下原因并断开连接（由既有重连路径重同步）。
    Fail(String),
}

/// 未闭合的重组窗口。
struct Pending {
    id: String,
    of_type: String,
    count: usize,
    next_index: usize,
    parts: Vec<String>,
    bytes: usize,
    /// 下一次「必须收到新分片」的时刻（收到分片即重置）。
    deadline: Instant,
}

/// 分片重组状态机。
pub struct Reassembler {
    limits: Limits,
    pending: Option<Pending>,
    buffered: VecDeque<String>,
    buffered_bytes: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Reassembler {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            pending: None,
            buffered: VecDeque::new(),
            buffered_bytes: 0,
        }
    }

    /// 是否有未闭合的重组窗口（读任务据此启用超时分支）。
    pub fn is_assembling(&self) -> bool {
        self.pending.is_some()
    }

    /// 窗口的闭合截止时刻（无窗口时 `None`）。
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.deadline)
    }

    /// 超时时的失败原因（读任务的计时分支调用）。
    pub fn timeout_detail(&self) -> String {
        match &self.pending {
            Some(p) => format!(
                "chunked event {:?} (id={}) not completed: {} of {} frames, \
                 no new frame for {}s",
                p.of_type,
                p.id,
                p.next_index,
                p.count,
                self.limits.idle_timeout.as_secs(),
            ),
            None => "chunk reassembly idle timeout".to_string(),
        }
    }

    /// 处理一帧原始文本。
    pub fn on_text(&mut self, text: &str) -> Outcome {
        if self.pending.is_some() {
            if let Some(env) = ChunkEnvelope::parse(text) {
                return self.accept_chunk(env);
            }
            // 窗口内的其他帧：原样缓冲，等闭合后按到达序放行。
            return match self.buffer(text) {
                Ok(()) => Outcome::Pending,
                Err(detail) => Outcome::Fail(detail),
            };
        }

        match serde_json::from_str::<WingEvent>(text) {
            // 未知类型是分片的入口信号（`WingEvent` 有 `#[serde(other)]` 兜底，
            // 未知 type 不会解析失败）；不是信封就按老行为放行。
            Ok(WingEvent::Unknown) => match ChunkEnvelope::parse(text) {
                Some(env) => self.start(env),
                None => Outcome::Deliver(vec![WingEvent::Unknown]),
            },
            Ok(event) => Outcome::Deliver(vec![event]),
            Err(e) => {
                tracing::warn!("failed to parse event: {e}, raw: {}", truncate(text));
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(text)
                    && let Some(t) = val.get("type").and_then(|v| v.as_str())
                {
                    tracing::warn!("unparseable event type: {t}");
                }
                Outcome::Deliver(Vec::new())
            }
        }
    }

    /// 首片：开窗。
    fn start(&mut self, env: ChunkEnvelope) -> Outcome {
        if env.count < 2 || env.count > self.limits.max_chunks {
            return Outcome::Fail(format!(
                "chunked event {:?} (id={}) declares count={} (allowed 2..={})",
                env.of_type, env.id, env.count, self.limits.max_chunks
            ));
        }
        if env.index != 0 {
            return Outcome::Fail(format!(
                "chunked event {:?} (id={}) starts at index {} (expected 0)",
                env.of_type, env.id, env.index
            ));
        }
        let bytes = env.data.len();
        if bytes > self.limits.max_buffered_bytes {
            return Outcome::Fail(format!(
                "chunk reassembly buffer exceeded: {} > {} bytes",
                bytes, self.limits.max_buffered_bytes
            ));
        }
        self.pending = Some(Pending {
            id: env.id,
            of_type: env.of_type,
            count: env.count,
            next_index: 1,
            parts: vec![env.data],
            bytes,
            deadline: Instant::now() + self.limits.idle_timeout,
        });
        Outcome::Pending
    }

    /// 后续分片：连续性校验 + 累积；闭合则拼接还原。
    fn accept_chunk(&mut self, env: ChunkEnvelope) -> Outcome {
        let pending = self.pending.as_mut().expect("caller checked");
        if env.id != pending.id {
            return Outcome::Fail(format!(
                "chunk id changed mid-reassembly: {:?} -> {:?}",
                pending.id, env.id
            ));
        }
        if env.count != pending.count {
            return Outcome::Fail(format!(
                "chunk count changed mid-reassembly (id={}): {} -> {}",
                pending.id, pending.count, env.count
            ));
        }
        if env.index != pending.next_index {
            return Outcome::Fail(format!(
                "chunk index out of sequence (id={}): got {}, expected {}",
                pending.id, env.index, pending.next_index
            ));
        }

        let total = pending.bytes + self.buffered_bytes + env.data.len();
        if total > self.limits.max_buffered_bytes {
            return Outcome::Fail(format!(
                "chunk reassembly buffer exceeded: {} > {} bytes",
                total, self.limits.max_buffered_bytes
            ));
        }

        pending.bytes += env.data.len();
        pending.next_index += 1;
        pending.parts.push(env.data);
        pending.deadline = Instant::now() + self.limits.idle_timeout;

        let closing = pending.next_index == pending.count;
        if !closing {
            return Outcome::Pending;
        }

        let pending = self.pending.take().expect("checked above");
        let joined = pending.parts.concat();
        match serde_json::from_str::<WingEvent>(&joined) {
            Ok(event) => {
                let mut events = vec![event];
                events.extend(self.flush());
                Outcome::Deliver(events)
            }
            Err(e) => Outcome::Fail(format!(
                "reassembled {:?} payload (id={}) is not a valid event: {e}",
                pending.of_type, pending.id
            )),
        }
    }

    /// 把原始帧放进窗口缓冲（超出上限 → 失败）。
    fn buffer(&mut self, text: &str) -> Result<(), String> {
        let pending_bytes = self.pending.as_ref().map(|p| p.bytes).unwrap_or(0);
        let total = pending_bytes + self.buffered_bytes + text.len();
        if total > self.limits.max_buffered_bytes {
            return Err(format!(
                "chunk reassembly buffer exceeded: {} > {} bytes",
                total, self.limits.max_buffered_bytes
            ));
        }
        self.buffered_bytes += text.len();
        self.buffered.push_back(text.to_string());
        Ok(())
    }

    /// 窗口闭合后按到达序放行缓冲帧（解析失败与既有行为一致：warn + 丢弃）。
    fn flush(&mut self) -> Vec<WingEvent> {
        let mut events = Vec::with_capacity(self.buffered.len());
        while let Some(text) = self.buffered.pop_front() {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(text.len());
            match serde_json::from_str::<WingEvent>(&text) {
                Ok(event) => events.push(event),
                Err(e) => {
                    tracing::warn!(
                        "failed to parse buffered event: {e}, raw: {}",
                        truncate(&text)
                    )
                }
            }
        }
        events
    }
}

/// 日志用截断（解析失败的原始帧可能很大，禁止整帧进日志）。
fn truncate(text: &str) -> String {
    const MAX: usize = 200;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const META: (&str, &str) = ("2026-09-13T00:00:00+00:00", "req-1");

    fn envelope(id: &str, index: usize, count: usize, of_type: &str, data: &str) -> String {
        json!({
            "type": CHUNK_TYPE,
            "id": id,
            "index": index,
            "count": count,
            "of_type": of_type,
            "data": data,
        })
        .to_string()
    }

    fn sync_payload(pad: usize) -> String {
        json!({
            "type": "sync_session",
            "session_id": "s1",
            "messages": [{"role": "assistant", "content": "x".repeat(pad)}],
            "created_at": META.0,
            "request_id": META.1,
        })
        .to_string()
    }

    fn text_payload(content: &str) -> String {
        json!({
            "type": "text",
            "content": content,
            "created_at": META.0,
            "request_id": META.1,
        })
        .to_string()
    }

    /// 把载荷按字符边界切成两半（模拟网关的切分，但不必撞软上限）。
    fn split_payload(payload: &str, id: &str) -> (String, String) {
        let mut mid = payload.len() / 2;
        while !payload.is_char_boundary(mid) {
            mid -= 1;
        }
        let (a, b) = payload.split_at(mid);
        (
            envelope(id, 0, 2, "sync_session", a),
            envelope(id, 1, 2, "sync_session", b),
        )
    }

    fn delivered(outcome: Outcome) -> Vec<WingEvent> {
        match outcome {
            Outcome::Deliver(events) => events,
            other => panic!("expected Deliver, got {other:?}"),
        }
    }

    fn failure(outcome: Outcome) -> String {
        match outcome {
            Outcome::Fail(detail) => detail,
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    fn is_pending(outcome: &Outcome) -> bool {
        matches!(outcome, Outcome::Pending)
    }

    fn sync_content(event: &WingEvent) -> Option<&str> {
        match event {
            WingEvent::SyncSession { messages, .. } => messages.first()?.get("content")?.as_str(),
            _ => None,
        }
    }

    #[test]
    fn plain_event_passes_through_untouched() {
        let mut r = Reassembler::default();
        let events = delivered(r.on_text(&text_payload("hi")));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], WingEvent::Text { .. }));
        assert!(!r.is_assembling());
        assert!(r.deadline().is_none());
    }

    #[test]
    fn unknown_event_still_passes_through() {
        // 未知类型不是分片信封 → 必须按老行为放行（未来事件类型不能被吞掉）。
        let mut r = Reassembler::default();
        let raw =
            json!({"type": "future_event", "x": 1, "created_at": META.0, "request_id": META.1})
                .to_string();
        let events = delivered(r.on_text(&raw));
        assert!(matches!(events.as_slice(), [WingEvent::Unknown]));
    }

    #[test]
    fn unknown_frame_that_looks_like_an_envelope_but_is_broken_is_not_a_chunk() {
        // 缺字段的 `_chunk` 帧：既不是合法信封，也当未知事件放行（后续帧的
        // 序号空洞会让重组失败——安全失败，不产出损坏事件）。
        let mut r = Reassembler::default();
        let raw =
            json!({"type": CHUNK_TYPE, "id": "1", "created_at": META.0, "request_id": META.1})
                .to_string();
        let events = delivered(r.on_text(&raw));
        assert!(matches!(events.as_slice(), [WingEvent::Unknown]));
        assert!(!r.is_assembling());
    }

    #[test]
    fn malformed_json_is_dropped_without_delivery() {
        let mut r = Reassembler::default();
        assert!(delivered(r.on_text("{not json")).is_empty());
    }

    #[test]
    fn chunked_payload_is_reassembled_into_one_complete_event() {
        let payload = sync_payload(64);
        let (first, last) = split_payload(&payload, "c1");
        let mut r = Reassembler::default();

        assert!(is_pending(&r.on_text(&first)));
        assert!(r.is_assembling(), "首片之后必须处于重组窗口内");
        assert!(r.deadline().is_some());

        let events = delivered(r.on_text(&last));
        assert_eq!(events.len(), 1, "应用层只见一个完整事件");
        assert_eq!(sync_content(&events[0]), Some("x".repeat(64).as_str()));
        assert!(!r.is_assembling());
    }

    #[test]
    fn multi_byte_payload_survives_reassembly() {
        let payload = sync_payload(0).replace("\"s1\"", "\"会话🙂\"");
        let (first, last) = split_payload(&payload, "c-mb");
        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&first)));
        let events = delivered(r.on_text(&last));
        match &events[0] {
            WingEvent::SyncSession { session_id, .. } => assert_eq!(session_id, "会话🙂"),
            other => panic!("expected SyncSession, got {other:?}"),
        }
    }

    #[test]
    fn frames_arriving_during_reassembly_are_released_in_order_after_it() {
        let payload = sync_payload(32);
        let (first, last) = split_payload(&payload, "c2");
        let mut r = Reassembler::default();

        assert!(is_pending(&r.on_text(&first)));
        // 窗口内的 live 帧：先缓冲，绝不先于 sync 投递（否则 chat.clear() 吞事件）。
        assert!(is_pending(&r.on_text(&text_payload("live-1"))));
        assert!(is_pending(&r.on_text(&text_payload("live-2"))));

        let events = delivered(r.on_text(&last));
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], WingEvent::SyncSession { .. }));
        match (&events[1], &events[2]) {
            (WingEvent::Text { content: a, .. }, WingEvent::Text { content: b, .. }) => {
                assert_eq!((a.as_str(), b.as_str()), ("live-1", "live-2"))
            }
            other => panic!("expected two text events, got {other:?}"),
        }
    }

    #[test]
    fn three_frame_event_closes_only_on_the_last_frame() {
        let payload = sync_payload(32);
        let mut mid = payload.len() / 3;
        while !payload.is_char_boundary(mid) {
            mid -= 1;
        }
        let mut two_thirds = (payload.len() * 2) / 3;
        while !payload.is_char_boundary(two_thirds) {
            two_thirds -= 1;
        }
        let (a, rest) = payload.split_at(mid);
        let (b, c) = rest.split_at(two_thirds - mid);

        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&envelope(
            "c3",
            0,
            3,
            "sync_session",
            a
        ))));
        assert!(is_pending(&r.on_text(&envelope(
            "c3",
            1,
            3,
            "sync_session",
            b
        ))));
        assert!(delivered(r.on_text(&envelope("c3", 2, 3, "sync_session", c))).len() == 1);
    }

    #[test]
    fn first_frame_must_start_at_index_zero() {
        let mut r = Reassembler::default();
        let detail = failure(r.on_text(&envelope("c4", 1, 2, "sync_session", "{}")));
        assert!(detail.contains("expected 0"), "{detail}");
    }

    #[test]
    fn count_out_of_range_fails_without_preallocating() {
        let mut r = Reassembler::default();
        let detail = failure(r.on_text(&envelope("c5", 0, 1, "sync_session", "{}")));
        assert!(detail.contains("count=1"), "{detail}");

        let mut r = Reassembler::default();
        let detail = failure(r.on_text(&envelope("c5", 0, 1_000_000_000, "sync_session", "{}")));
        assert!(detail.contains("1000000000"), "{detail}");

        let mut r = Reassembler::default();
        let detail = failure(r.on_text(&envelope("c5", 0, MAX_CHUNKS + 1, "sync_session", "{}")));
        assert!(
            detail.contains(&format!("allowed 2..={MAX_CHUNKS}")),
            "{detail}"
        );
        assert!(r.deadline().is_none(), "失败不得留下窗口");
    }

    #[test]
    fn index_gap_and_duplicate_fail() {
        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&envelope(
            "c6",
            0,
            3,
            "sync_session",
            "{\"a\":"
        ))));
        let detail = failure(r.on_text(&envelope("c6", 2, 3, "sync_session", "1}")));
        assert!(detail.contains("got 2, expected 1"), "{detail}");

        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&envelope(
            "c6",
            0,
            3,
            "sync_session",
            "{\"a\":"
        ))));
        let detail = failure(r.on_text(&envelope("c6", 0, 3, "sync_session", "1}")));
        assert!(detail.contains("got 0, expected 1"), "{detail}");
    }

    #[test]
    fn id_or_count_change_mid_reassembly_fails() {
        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&envelope(
            "c7",
            0,
            2,
            "sync_session",
            "["
        ))));
        let detail = failure(r.on_text(&envelope("c8", 1, 2, "sync_session", "]")));
        assert!(detail.contains("id changed"), "{detail}");

        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&envelope(
            "c7",
            0,
            2,
            "sync_session",
            "["
        ))));
        let detail = failure(r.on_text(&envelope("c7", 1, 3, "sync_session", "]")));
        assert!(detail.contains("count changed"), "{detail}");
    }

    #[test]
    fn buffer_cap_is_enforced() {
        // 首个分片本身就超上限。
        let tight = Limits {
            max_buffered_bytes: 64,
            ..Limits::default()
        };
        let mut r = Reassembler::new(tight);
        let detail = failure(r.on_text(&envelope("c9", 0, 2, "sync_session", &"x".repeat(65))));
        assert!(detail.contains("buffer exceeded"), "{detail}");

        // 分片 + 窗口内缓冲帧之和超上限。
        let roomy = Limits {
            max_buffered_bytes: 200,
            ..Limits::default()
        };
        let mut r = Reassembler::new(roomy);
        assert!(is_pending(&r.on_text(&envelope(
            "c9",
            0,
            2,
            "sync_session",
            &"x".repeat(40)
        ))));
        assert!(is_pending(&r.on_text(&text_payload(&"y".repeat(30)))));
        let detail = failure(r.on_text(&text_payload(&"z".repeat(30))));
        assert!(detail.contains("buffer exceeded"), "{detail}");
    }

    #[test]
    fn reassembled_payload_must_be_a_valid_event() {
        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&envelope(
            "c10",
            0,
            2,
            "sync_session",
            "{\"type\":\"sync"
        ))));
        let detail = failure(r.on_text(&envelope("c10", 1, 2, "sync_session", "\"broken")));
        assert!(detail.contains("not a valid event"), "{detail}");
    }

    #[test]
    fn timeout_detail_describes_the_open_window() {
        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&envelope(
            "c11",
            0,
            3,
            "sync_session",
            "["
        ))));
        let detail = r.timeout_detail();
        assert!(detail.contains("sync_session"), "{detail}");
        assert!(detail.contains("1 of 3"), "{detail}");
        assert!(
            detail.contains(&format!("{}s", IDLE_TIMEOUT.as_secs())),
            "{detail}"
        );
    }

    #[test]
    fn a_new_event_can_start_after_a_completed_one() {
        let payload = sync_payload(16);
        let (a1, a2) = split_payload(&payload, "c12");
        let (b1, b2) = split_payload(&payload, "c13");
        let mut r = Reassembler::default();
        assert!(is_pending(&r.on_text(&a1)));
        assert_eq!(delivered(r.on_text(&a2)).len(), 1);
        assert!(is_pending(&r.on_text(&b1)));
        assert_eq!(delivered(r.on_text(&b2)).len(), 1);
    }

    #[test]
    fn chunk_envelope_parser_rejects_other_frames_cheaply() {
        assert!(ChunkEnvelope::parse(&envelope("c14", 0, 2, "sync_session", "[]")).is_some());
        assert!(ChunkEnvelope::parse(&text_payload("hi")).is_none());
        assert!(ChunkEnvelope::parse("{}").is_none());
    }
}
