//! OSC 5522（kitty clipboard protocol）协议机。
//!
//! 通道与字节流分流在 [`super::stdin_proxy`]；这里只有协议本身：解析终端发来的
//! unsolicited paste event、构造带一次性密码的 follow-up read 请求、重组 DATA 分片、
//! 超时与失败降级。
//!
//! # 线上格式（以 Ghostty 实现为准，本机 `~/ws/extern/ghostty` 源码核对）
//!
//! ```text
//! 终端 → 应用（用户 Cmd+V；targets listing 只列 MIME，不含数据）:
//!   ESC]5522;type=read:status=OK:pw=<b64 OTP>ST
//!   ESC]5522;type=read:status=DATA:mime=Lg==:pw=<b64 OTP>;<b64 "text/plain image/png\n">ST
//!   ESC]5522;type=read:status=DONE:pw=<b64 OTP>ST
//!
//! 应用 → 终端（带 OTP 的 follow-up read，负载是 base64(空格分隔的 MIME 列表)）:
//!   ESC]5522;type=read:name=<b64 程序名>:id=<id>:pw=<b64 OTP>;<b64 "image/png">ST
//!
//! 终端 → 应用（响应回显 id；DATA 每片 ≤4096 原始字节、各自独立 base64）:
//!   ESC]5522;type=read:status=OK:id=<id>ST
//!   ESC]5522;type=read:status=DATA:id=<id>:mime=<b64 MIME>;<b64 分片>ST   （可多片）
//!   ESC]5522;type=read:status=DONE:id=<id>ST
//! ```
//!
//! 两条容易踩的事实：
//!
//! - **`name` 不是装饰**：Ghostty 的 `Metadata.parse` 规定「没有 name 的 pw 等于没有 pw」
//!   （`stream_terminal.zig`：`const pw = if (meta.name.len > 0) meta.pw else ""`）。请求里
//!   漏了 name，一次性密码不参与授权判定 → 用户被弹确认框（`clipboard-read=ask`）或被拒。
//! - **event 的 OK 包没有 `id`，我们的 read 响应回显 `id`**：这是区分「新事件」与「响应」
//!   的唯一依据（`pw` 两边都有；listing 的 DATA 与数据的 DATA 靠 `mime` 区分）。

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::TermEvent;
use super::stdin_proxy::Sequence;

/// 一条 read 事务 / 一次 event 收集的空闲上限：正常终端毫秒级回完整串，超了就是没指望。
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// 单次读回的字节上限（超出即失败，防止坏终端把内存吃光）。
pub(crate) const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// 阅读器身份：`name` 参与授权判定（见模块文档），也是终端权限提示里显示的字符串。
const REQUESTER_NAME: &str = "wing";

/// 我们发出的 read 请求的 id（响应回显；用于与 unsolicited event 区分）。
const REQUEST_ID: &str = "wing-paste";

/// 一次事件最多排队几个（连续快速粘贴时后面的等前一个读完）。
const MAX_QUEUED_EVENTS: usize = 4;

/// 剪贴板读回的载荷（App 边界，冻结给 05）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPayload {
    /// 文本表示（`text/plain`）。
    Text(String),
    /// 图片表示；`name` 本步骤恒为 `None`（协议不带文件名）。
    Image {
        bytes: Vec<u8>,
        mime: String,
        name: Option<String>,
    },
}

/// 读失败的原因（App 侧转 toast；文案由 [`ClipboardFailure::message`] 统一给）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardFailure {
    /// 剪贴板既没有可用图片，也没有文本表示。
    NoRepresentation,
    /// 终端拒绝（EPERM）：`clipboard-read` 策略或授权未通过。
    Denied,
    /// 终端不支持（ENOSYS）。
    Unsupported,
    /// 终端一直没回（或回了一半就停）。
    Timeout,
    /// 超过 [`MAX_PAYLOAD`]。
    TooLarge,
    /// 数据不可解析（坏 base64 / 坏包）。
    Malformed,
    /// 其它协议状态码（原样带上）。
    Status(String),
}

impl ClipboardFailure {
    /// 用户可见的一句话（toast 文案）。
    pub fn message(&self) -> String {
        match self {
            Self::NoRepresentation => "Clipboard has no text or image".to_string(),
            Self::Denied => "Clipboard read was denied by the terminal".to_string(),
            Self::Unsupported => "This terminal cannot serve clipboard reads".to_string(),
            Self::Timeout => "Clipboard read timed out".to_string(),
            Self::TooLarge => "Clipboard content is too large to paste".to_string(),
            Self::Malformed => "Clipboard response was malformed".to_string(),
            Self::Status(code) => format!("Clipboard read failed ({code})"),
        }
    }
}

// ---------------------------------------------------------------------------
// base64（RFC 4648 标准字母表 + padding；自己实现，不为此引依赖）
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        let n = u32::from_be_bytes([0, a, b, c]);
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 严格解码：只认标准字母表与末尾 padding；非字母字符、`%4 == 1` 的残缺、
/// padding 数量对不上都判失败（坏数据走降级路径，绝不 panic）。
pub(crate) fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut body = text.as_bytes();
    let mut pad = 0usize;
    while let [head @ .., b'='] = body {
        body = head;
        pad += 1;
        if pad > 2 {
            return None;
        }
    }
    if body.len() % 4 == 1 {
        return None;
    }
    if pad > 0 && !(body.len() + pad).is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 3);
    for chunk in body.chunks(4) {
        let mut acc = 0u32;
        for &c in chunk {
            acc = (acc << 6) | b64_value(c)? as u32;
        }
        // 每组的有效位是 6×字符数；输出第 i 个字节取从高位数起的那一段。
        let bits = 6 * chunk.len() as u32;
        for i in 0..chunk.len() - 1 {
            out.push((acc >> (bits - 8 * (i as u32 + 1))) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// 包解析
// ---------------------------------------------------------------------------

/// 一条 OSC 5522 包（`ESC]5522;` 与终止符之间的内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Packet {
    pub op: String,
    pub status: String,
    /// 终端回显的请求 id；unsolicited paste event 没有。
    pub id: Option<String>,
    /// `loc=primary`（只有 event 的 OK 包会带）。
    pub primary: bool,
    /// base64 解码后的 MIME（DATA 包）。
    pub mime: Option<String>,
    /// base64 解码后的一次性密码（event 包）。
    pub pw: Option<String>,
    /// base64 解码后的负载字节。
    pub payload: Vec<u8>,
}

impl Packet {
    /// 解析一条包。`None` = 结构坏掉（缺 `=`、非 UTF-8、坏 base64）——业务上按
    /// 「读中途收到坏包 = Malformed」处理。
    pub(crate) fn parse(raw: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(raw).ok()?;
        let (meta, payload) = match text.split_once(';') {
            Some((meta, payload)) => (meta, Some(payload)),
            None => (text, None),
        };
        let mut packet = Packet {
            op: String::new(),
            status: String::new(),
            id: None,
            primary: false,
            mime: None,
            pw: None,
            payload: Vec::new(),
        };
        for record in meta.split(':') {
            let (key, value) = record.split_once('=')?;
            match key {
                "type" => packet.op = value.to_string(),
                "status" => packet.status = value.to_string(),
                "id" => packet.id = Some(value.to_string()),
                "loc" => packet.primary = value == "primary",
                "mime" => packet.mime = Some(String::from_utf8(b64_decode(value)?).ok()?),
                "pw" => packet.pw = Some(String::from_utf8(b64_decode(value)?).ok()?),
                // 未知键（含 kitty 的 `list`、`name`）忽略：协议允许扩展。
                _ => {}
            }
        }
        if let Some(payload) = payload {
            packet.payload = b64_decode(payload)?;
        }
        Some(packet)
    }
}

/// targets listing 的负载：空格 / 换行分隔的 MIME 串（Ghostty 会用 `\n` 结尾）。
fn parse_listing(payload: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(payload)
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// MIME 选择与请求构造
// ---------------------------------------------------------------------------

/// 图片表示的优先级（先精确命中，再任意 `image/*` 兜底）。
const IMAGE_PRIORITY: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// 从 listing 里挑一个要读的 MIME（纯函数）。
///
/// 规则：图片优先（按 [`IMAGE_PRIORITY`]，其余 `image/*` 按 listing 顺序兜底），否则
/// `text/plain`；都没有 → `None`（不读，直接降级）。比较前会剥掉 `;charset=…` 之类的
/// 参数并忽略大小写，以容纳各终端列出的名字差异。
pub(crate) fn choose_mime(available: &[String]) -> Option<String> {
    let bases: Vec<String> = available
        .iter()
        .map(|m| m.split(';').next().unwrap_or(m).trim().to_ascii_lowercase())
        .collect();
    for want in IMAGE_PRIORITY {
        if bases.iter().any(|b| b == want) {
            return Some(want.to_string());
        }
    }
    if let Some(other) = bases.iter().find(|b| b.starts_with("image/")) {
        return Some(other.clone());
    }
    // 文本统一按规范名 `text/plain` 请求：终端（Ghostty 的 `isTextMime`）会把各种文本别名
    // 都算成同一个表示，请求规范名最稳。
    if bases
        .iter()
        .any(|b| b == "text/plain" || b.starts_with("text/plain"))
    {
        return Some("text/plain".to_string());
    }
    None
}

/// 构造带 OTP 的 follow-up read 请求（`ESC]5522;…ST`）。
pub(crate) fn read_request(pw: &str, mime: &str, primary: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(mime.len() + pw.len() + REQUESTER_NAME.len() + 64);
    out.extend_from_slice(b"\x1b]5522;type=read");
    if primary {
        out.extend_from_slice(b":loc=primary");
    }
    out.extend_from_slice(b":name=");
    out.extend_from_slice(b64_encode(REQUESTER_NAME.as_bytes()).as_bytes());
    out.extend_from_slice(b":id=");
    out.extend_from_slice(REQUEST_ID.as_bytes());
    out.extend_from_slice(b":pw=");
    out.extend_from_slice(b64_encode(pw.as_bytes()).as_bytes());
    out.push(b';');
    out.extend_from_slice(b64_encode(mime.as_bytes()).as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

// ---------------------------------------------------------------------------
// 状态机
// ---------------------------------------------------------------------------

/// 状态机产出的动作，由驱动执行。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// 发给终端：read 请求字节。
    Send(Vec<u8>),
    /// 读回完成：交付 App。
    Deliver(ClipboardPayload),
    /// 失败：交付 App（toast）。
    Fail(ClipboardFailure),
}

/// 一次已完成的 paste event（listing 与 DONE 都到齐了）。
#[derive(Debug)]
struct PendingEvent {
    pw: String,
    primary: bool,
    mimes: Vec<String>,
}

/// 正在收集的 event（OK 已到，等 listing 与 DONE）。
#[derive(Debug)]
struct EventCollector {
    pw: String,
    primary: bool,
    mimes: Vec<String>,
    deadline: Instant,
}

/// 在途的 read。
#[derive(Debug)]
struct ReadState {
    mime: String,
    data: Vec<u8>,
    deadline: Instant,
}

/// 协议状态机：纯逻辑（时钟注入），驱动的 I/O 在外面。
#[derive(Debug, Default)]
pub(crate) struct Session {
    event: Option<EventCollector>,
    read: Option<ReadState>,
    queue: VecDeque<PendingEvent>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// 下一次需要 [`Session::on_timeout`] 的时刻（无在途 = 无需唤醒）。
    pub fn deadline(&self) -> Option<Instant> {
        let event = self.event.as_ref().map(|e| e.deadline);
        let read = self.read.as_ref().map(|r| r.deadline);
        match (event, read) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    /// 喂一条分流层提取出来的包。
    pub fn on_sequence(&mut self, raw: &[u8], now: Instant) -> Vec<Effect> {
        let Some(packet) = Packet::parse(raw) else {
            // 坏包：读中途算致命（响应不可信），其余忽略（可能是别人的协议流量）。
            if self.read.is_some() {
                self.read = None;
                tracing::debug!("clipboard: malformed packet during read");
                return vec![Effect::Fail(ClipboardFailure::Malformed)];
            }
            tracing::debug!("clipboard: ignoring malformed OSC 5522 packet");
            return Vec::new();
        };
        if packet.op != "read" {
            return Vec::new();
        }
        let ours = packet.id.as_deref() == Some(REQUEST_ID);
        match packet.status.as_str() {
            "OK" => {
                if ours {
                    if let Some(read) = &mut self.read {
                        read.deadline = now + IDLE_TIMEOUT;
                    }
                } else if let Some(pw) = packet.pw.clone() {
                    if self.event.is_some() {
                        tracing::debug!("clipboard: superseding an unfinished paste event");
                    }
                    self.event = Some(EventCollector {
                        pw,
                        primary: packet.primary,
                        mimes: Vec::new(),
                        deadline: now + IDLE_TIMEOUT,
                    });
                }
            }
            "DATA" => {
                if ours {
                    if let Some(read) = &mut self.read
                        && packet.mime.as_deref() == Some(read.mime.as_str())
                    {
                        if read.data.len() + packet.payload.len() > MAX_PAYLOAD {
                            self.read = None;
                            return vec![Effect::Fail(ClipboardFailure::TooLarge)];
                        }
                        read.data.extend_from_slice(&packet.payload);
                        read.deadline = now + IDLE_TIMEOUT;
                    }
                } else if let Some(event) = &mut self.event
                    && packet.mime.as_deref() == Some(".")
                {
                    event.mimes = parse_listing(&packet.payload);
                    event.deadline = now + IDLE_TIMEOUT;
                }
            }
            "DONE" => {
                if ours {
                    return self.finish_read(now);
                }
                if let Some(event) = self.event.take() {
                    let pending = PendingEvent {
                        pw: event.pw,
                        primary: event.primary,
                        mimes: event.mimes,
                    };
                    if self.read.is_some() {
                        if self.queue.len() < MAX_QUEUED_EVENTS {
                            self.queue.push_back(pending);
                        } else {
                            tracing::warn!("clipboard: paste event dropped (queue full)");
                        }
                        return Vec::new();
                    }
                    return self.start(pending, now);
                }
            }
            other if ours => {
                self.read = None;
                return vec![Effect::Fail(failure_for(other))];
            }
            _ => {}
        }
        Vec::new()
    }

    /// 推进超时。
    pub fn on_timeout(&mut self, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(event) = &self.event
            && now >= event.deadline
        {
            tracing::debug!("clipboard: paste event incomplete, giving up");
            self.event = None;
        }
        if let Some(read) = &self.read
            && now >= read.deadline
        {
            self.read = None;
            effects.push(Effect::Fail(ClipboardFailure::Timeout));
        }
        effects.extend(self.pump_queue(now));
        effects
    }

    fn finish_read(&mut self, now: Instant) -> Vec<Effect> {
        let Some(read) = self.read.take() else {
            return Vec::new();
        };
        let payload = if read.mime.starts_with("image/") {
            ClipboardPayload::Image {
                bytes: read.data,
                mime: read.mime.clone(),
                name: None,
            }
        } else {
            ClipboardPayload::Text(String::from_utf8_lossy(&read.data).into_owned())
        };
        let mut effects = vec![Effect::Deliver(payload)];
        effects.extend(self.pump_queue(now));
        effects
    }

    /// 队列里下一个事件（读空出来了就开始）。
    fn pump_queue(&mut self, now: Instant) -> Vec<Effect> {
        if self.read.is_some() {
            return Vec::new();
        }
        match self.queue.pop_front() {
            Some(next) => self.start(next, now),
            None => Vec::new(),
        }
    }

    fn start(&mut self, event: PendingEvent, now: Instant) -> Vec<Effect> {
        let Some(mime) = choose_mime(&event.mimes) else {
            tracing::debug!(mimes = ?event.mimes, "clipboard: no usable representation");
            return vec![Effect::Fail(ClipboardFailure::NoRepresentation)];
        };
        self.read = Some(ReadState {
            mime: mime.clone(),
            data: Vec::new(),
            deadline: now + IDLE_TIMEOUT,
        });
        vec![Effect::Send(read_request(&event.pw, &mime, event.primary))]
    }
}

fn failure_for(status: &str) -> ClipboardFailure {
    match status {
        "EPERM" => ClipboardFailure::Denied,
        "ENOSYS" => ClipboardFailure::Unsupported,
        "EFBIG" => ClipboardFailure::TooLarge,
        other => ClipboardFailure::Status(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// 驱动：tokio 任务（超时、写请求、投事件）
// ---------------------------------------------------------------------------

/// 起一个协议驱动任务：消费分流层的序列通道，把读回的载荷投进终端事件通道。
///
/// 独立任务的意义：超时 / 等待 / 写出都不占事件循环（UI 永远不会因为终端不回而卡住）。
pub(crate) fn spawn(sequences: mpsc::Receiver<Sequence>, events: mpsc::Sender<TermEvent>) {
    spawn_with(sequences, events, write_request);
}

/// [`spawn`] 的测试缝：写出口可注入（单测不该往真 stdout 上写转义序列）。
fn spawn_with<W>(mut sequences: mpsc::Receiver<Sequence>, events: mpsc::Sender<TermEvent>, write: W)
where
    W: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let mut session = Session::new();
        loop {
            tokio::select! {
                incoming = sequences.recv() => {
                    let Some(raw) = incoming else {
                        // 分流层没了（进程收尾）：静默退场。
                        break;
                    };
                    for effect in session.on_sequence(&raw, Instant::now()) {
                        apply(effect, &events, &write).await;
                    }
                }
                _ = sleep_until(session.deadline()) => {
                    for effect in session.on_timeout(Instant::now()) {
                        apply(effect, &events, &write).await;
                    }
                }
            }
        }
    });
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending::<()>().await,
    }
}

async fn apply<W>(effect: Effect, events: &mpsc::Sender<TermEvent>, write: &W)
where
    W: Fn(&[u8]) -> io::Result<()>,
{
    match effect {
        Effect::Send(bytes) => {
            if let Err(e) = write(&bytes) {
                // 终端没了：让随后的超时把这次读收尾，不在这里改状态。
                tracing::warn!("clipboard: failed to send read request: {e}");
            }
        }
        Effect::Deliver(payload) => {
            let _ = events.send(TermEvent::Clipboard(Ok(payload))).await;
        }
        Effect::Fail(failure) => {
            let _ = events.send(TermEvent::Clipboard(Err(failure))).await;
        }
    }
}

/// 请求走 stdout：与 ratatui / crossterm 共用同一个全局缓冲与锁，不会与帧数据在 Rust
/// 层交错；单条转义序列落在流的哪个位置都是自解释的。
fn write_request(bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    /// 一条包的线上字节（不含 `ESC]5522;` 前缀与终止符）。
    fn packet(meta: &str, payload: Option<&[u8]>) -> Vec<u8> {
        let mut out = meta.as_bytes().to_vec();
        if let Some(payload) = payload {
            out.push(b';');
            out.extend_from_slice(b64_encode(payload).as_bytes());
        }
        out
    }

    /// 终端发来的 unsolicited paste event（OK + listing + DONE）。
    fn event_triplet(pw: &str, mimes: &[&str], at_ms: u64, s: &mut Session) -> Vec<Effect> {
        let mut effects = s.on_sequence(
            &packet(
                &format!("type=read:status=OK:pw={}", b64_encode(pw.as_bytes())),
                None,
            ),
            at(at_ms),
        );
        let listing = if mimes.is_empty() {
            String::new()
        } else {
            format!("{}\n", mimes.join(" "))
        };
        effects.extend(s.on_sequence(
            &packet(
                &format!(
                    "type=read:status=DATA:mime={}:pw={}",
                    b64_encode(b"."),
                    b64_encode(pw.as_bytes())
                ),
                Some(listing.as_bytes()),
            ),
            at(at_ms + 1),
        ));
        effects.extend(s.on_sequence(
            &packet(
                &format!("type=read:status=DONE:pw={}", b64_encode(pw.as_bytes())),
                None,
            ),
            at(at_ms + 2),
        ));
        effects
    }

    fn response(meta: &str, payload: Option<&[u8]>) -> Vec<u8> {
        packet(meta, payload)
    }

    // ── base64 ────────────────────────────────────────────────────────────

    #[test]
    fn base64_round_trip() {
        let cases: [&[u8]; 7] = [
            b"",
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"hello world",
            &[0, 1, 2, 250, 251, 252, 253, 254, 255],
        ];
        for case in cases {
            let encoded = b64_encode(case);
            assert_eq!(b64_decode(&encoded).as_deref(), Some(case), "{case:?}");
        }
        // 与标准实现（RFC 4648 例子）对齐。
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_decode("Zm9vYmFy").unwrap(), b"foobar");
        // 缺 padding 也收（终端实现差异的宽容面）。
        assert_eq!(b64_decode("Zm9vYg").unwrap(), b"foob");
    }

    #[test]
    fn base64_decode_rejects_garbage() {
        for bad in ["!!!!", "Zm9vYmFy===", "Z", "Zm9vY$==", "====="] {
            assert_eq!(b64_decode(bad), None, "{bad:?} 必须判失败");
        }
        // 嵌在别处的 `=` 不是 padding。
        assert_eq!(b64_decode("Z=9v"), None);
    }

    // ── 包解析 ────────────────────────────────────────────────────────────

    #[test]
    fn packet_parses_paste_event_ok() {
        let raw = packet("type=read:status=OK:pw=b3Rw", None);
        let p = Packet::parse(&raw).expect("parse");
        assert_eq!(p.op, "read");
        assert_eq!(p.status, "OK");
        assert_eq!(p.pw.as_deref(), Some("otp"));
        assert_eq!(p.id, None);
        assert!(!p.primary);
        assert!(p.payload.is_empty());
    }

    #[test]
    fn packet_parses_listing_with_primary_and_payload() {
        let raw = packet(
            "type=read:status=DATA:loc=primary:mime=Lg==:pw=b3Rw",
            Some(b"text/plain image/png\n"),
        );
        let p = Packet::parse(&raw).expect("parse");
        assert!(p.primary);
        assert_eq!(p.mime.as_deref(), Some("."));
        assert_eq!(parse_listing(&p.payload), vec!["text/plain", "image/png"]);
    }

    #[test]
    fn packet_tolerates_field_order_and_unknown_keys() {
        let raw = packet("status=DONE:type=read:list=true:id=wing-paste:bobr=1", None);
        let p = Packet::parse(&raw).expect("parse");
        assert_eq!(p.status, "DONE");
        assert_eq!(p.id.as_deref(), Some("wing-paste"));
    }

    #[test]
    fn packet_rejects_structural_garbage() {
        for bad in [
            &b"type=read:status"[..],           // 缺 '='
            &b"type=read:status=OK:pw=!!!"[..], // 坏 base64
            &b"\xff\xfe"[..],                   // 不是 UTF-8
        ] {
            assert_eq!(Packet::parse(bad), None, "{bad:?}");
        }
    }

    // ── MIME 选择 ─────────────────────────────────────────────────────────

    #[test]
    fn choose_mime_prefers_the_priority_list() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            choose_mime(&s(&["text/plain", "image/png"])).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            choose_mime(&s(&["image/gif", "image/jpeg", "text/plain"])).as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            choose_mime(&s(&["image/webp", "image/gif"])).as_deref(),
            Some("image/webp")
        );
    }

    #[test]
    fn choose_mime_falls_back_to_any_image_then_text() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // 未知的 image/* 也认（真机 MIME 名称待验收，宽容比精确重要）。
        assert_eq!(
            choose_mime(&s(&["image/tiff"])).as_deref(),
            Some("image/tiff")
        );
        assert_eq!(
            choose_mime(&s(&["text/plain"])).as_deref(),
            Some("text/plain")
        );
        // 参数形态（`text/plain;charset=utf-8`）：请求规范名。
        assert_eq!(
            choose_mime(&s(&["text/plain;charset=utf-8"])).as_deref(),
            Some("text/plain")
        );
        // 大小写不敏感。
        assert_eq!(
            choose_mime(&s(&["IMAGE/PNG"])).as_deref(),
            Some("image/png")
        );
        // 只有不认识的东西 → 不读。
        assert_eq!(choose_mime(&s(&["text/html", "application/pdf"])), None);
        assert_eq!(choose_mime(&[]), None);
    }

    // ── 请求构造 ──────────────────────────────────────────────────────────

    #[test]
    fn read_request_is_exact_bytes() {
        let bytes = read_request("otpxyz", "image/png", false);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b]5522;type=read:name=d2luZw==:id=wing-paste:pw=b3RweHl6;aW1hZ2UvcG5n\x1b\\"
        );
        // primary 带上 loc；请求里的 name / pw / mime 都是 base64（协议要求）。
        let bytes = read_request("otp", "text/plain", true);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b]5522;type=read:loc=primary:name=d2luZw==:id=wing-paste:pw=b3Rw;dGV4dC9wbGFpbg==\x1b\\"
        );
    }

    /// `name` 不是装饰：Ghostty 的授权判定要求它非空（见模块文档）。
    #[test]
    fn read_request_always_carries_a_name() {
        assert!(read_request("x", "text/plain", false).starts_with(b"\x1b]5522;type=read:name="));
    }

    // ── 状态机 ────────────────────────────────────────────────────────────

    #[test]
    fn paste_event_starts_a_read_with_the_granted_password() {
        let mut s = Session::new();
        let effects = event_triplet("otp123", &["text/plain", "image/png"], 0, &mut s);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::Send(bytes) => {
                assert_eq!(
                    String::from_utf8(bytes.clone()).unwrap(),
                    "\x1b]5522;type=read:name=d2luZw==:id=wing-paste:pw=b3RwMTIz;aW1hZ2UvcG5n\x1b\\"
                );
            }
            other => panic!("期望发出 read 请求，得到 {other:?}"),
        }
        assert!(s.deadline().is_some(), "在途读要有超时");
    }

    #[test]
    fn text_read_delivers_text() {
        let mut s = Session::new();
        event_triplet("otp", &["text/plain"], 0, &mut s);
        assert!(
            s.on_sequence(&response("type=read:status=OK:id=wing-paste", None), at(10))
                .is_empty()
        );
        assert!(
            s.on_sequence(
                &response(
                    "type=read:status=DATA:id=wing-paste:mime=dGV4dC9wbGFpbg==",
                    Some("hello 世界".as_bytes()),
                ),
                at(11),
            )
            .is_empty()
        );
        let effects = s.on_sequence(
            &response("type=read:status=DONE:id=wing-paste", None),
            at(12),
        );
        assert_eq!(
            effects,
            vec![Effect::Deliver(ClipboardPayload::Text("hello 世界".into()))]
        );
        assert!(s.deadline().is_none(), "读完了 = 不需要唤醒");
    }

    #[test]
    fn image_read_reassembles_chunks_independently_base64_encoded() {
        let mut s = Session::new();
        event_triplet("otp", &["image/png"], 0, &mut s);
        let mut effects = Vec::new();
        // 三片，每片独立 base64（Ghostty 的 4096 字节分片语义）。
        for chunk in [&b"\x89PNG"[..], &[0u8; 8192], &b"tail"[..]] {
            effects.extend(s.on_sequence(
                &response(
                    "type=read:status=DATA:id=wing-paste:mime=aW1hZ2UvcG5n",
                    Some(chunk),
                ),
                at(20),
            ));
        }
        effects.extend(s.on_sequence(
            &response("type=read:status=DONE:id=wing-paste", None),
            at(21),
        ));
        let mut expected = b"\x89PNG".to_vec();
        expected.extend_from_slice(&[0u8; 8192]);
        expected.extend_from_slice(b"tail");
        assert_eq!(
            effects,
            vec![Effect::Deliver(ClipboardPayload::Image {
                bytes: expected,
                mime: "image/png".into(),
                name: None,
            })]
        );
    }

    #[test]
    fn denial_status_maps_to_failure() {
        for (status, expected) in [
            ("EPERM", ClipboardFailure::Denied),
            ("ENOSYS", ClipboardFailure::Unsupported),
            ("EFBIG", ClipboardFailure::TooLarge),
            ("EIO", ClipboardFailure::Status("EIO".into())),
        ] {
            let mut s = Session::new();
            event_triplet("otp", &["image/png"], 0, &mut s);
            let effects = s.on_sequence(
                &response(&format!("type=read:status={status}:id=wing-paste"), None),
                at(5),
            );
            assert_eq!(effects, vec![Effect::Fail(expected)], "{status}");
        }
    }

    #[test]
    fn read_stalls_out() {
        let mut s = Session::new();
        event_triplet("otp", &["image/png"], 0, &mut s);
        assert!(s.on_timeout(at(1000)).is_empty(), "没到点");
        assert_eq!(
            s.on_timeout(at(IDLE_TIMEOUT.as_millis() as u64 + 100)),
            vec![Effect::Fail(ClipboardFailure::Timeout)]
        );
        assert!(s.deadline().is_none());
    }

    #[test]
    fn incomplete_event_is_dropped_without_effects() {
        let mut s = Session::new();
        // 只有 OK：listing / DONE 都没来。
        s.on_sequence(&packet("type=read:status=OK:pw=b3Rw", None), at(0));
        assert!(s.on_timeout(at(100)).is_empty());
        assert!(s.on_timeout(at(6000)).is_empty(), "收不全的 event 只是丢掉");
        assert!(s.deadline().is_none());
    }

    #[test]
    fn no_usable_representation_fails_fast() {
        let mut s = Session::new();
        let effects = event_triplet("otp", &["text/html"], 0, &mut s);
        assert_eq!(
            effects,
            vec![Effect::Fail(ClipboardFailure::NoRepresentation)]
        );
    }

    #[test]
    fn oversized_read_aborts() {
        let mut s = Session::new();
        event_triplet("otp", &["image/png"], 0, &mut s);
        let big = vec![0u8; MAX_PAYLOAD];
        let mut effects = s.on_sequence(
            &response(
                "type=read:status=DATA:id=wing-paste:mime=aW1hZ2UvcG5n",
                Some(&big),
            ),
            at(1),
        );
        effects.extend(s.on_sequence(
            &response(
                "type=read:status=DATA:id=wing-paste:mime=aW1hZ2UvcG5n",
                Some(b"x"),
            ),
            at(2),
        ));
        assert_eq!(effects, vec![Effect::Fail(ClipboardFailure::TooLarge)]);
        assert!(s.deadline().is_none());
    }

    #[test]
    fn malformed_packet_during_read_is_fatal() {
        let mut s = Session::new();
        event_triplet("otp", &["text/plain"], 0, &mut s);
        let effects = s.on_sequence(b"type=read:status=DATA:pw=!!!", at(1));
        assert_eq!(effects, vec![Effect::Fail(ClipboardFailure::Malformed)]);
    }

    /// 别的协议流量（write 事务、别人的 id）不能影响我们的读。
    #[test]
    fn unrelated_packets_are_ignored() {
        let mut s = Session::new();
        event_triplet("otp", &["text/plain"], 0, &mut s);
        assert!(
            s.on_sequence(&packet("type=write:status=DONE:id=42", None), at(1))
                .is_empty()
        );
        assert!(
            s.on_sequence(
                &response(
                    "type=read:status=DATA:id=other:mime=dGV4dA==",
                    Some(b"nope")
                ),
                at(2)
            )
            .is_empty()
        );
        assert!(
            s.on_sequence(&response("type=read:status=DONE:id=other", None), at(3))
                .is_empty()
        );
        // 我们的读仍然活着。
        assert_eq!(
            s.on_sequence(
                &response("type=read:status=DONE:id=wing-paste", None),
                at(4)
            ),
            vec![Effect::Deliver(ClipboardPayload::Text(String::new()))]
        );
    }

    #[test]
    fn second_paste_event_is_queued_and_served_after_the_first() {
        let mut s = Session::new();
        event_triplet("one", &["text/plain"], 0, &mut s);
        // 第一条还在读，第二条 event 到达：不丢，排队。
        let effects = event_triplet("two", &["text/plain"], 1, &mut s);
        assert!(effects.is_empty(), "排队 = 先不发请求");
        // 第一条读完：第二条的请求立刻发出。
        s.on_sequence(&response("type=read:status=OK:id=wing-paste", None), at(2));
        let effects = s.on_sequence(
            &response(
                "type=read:status=DATA:id=wing-paste:mime=dGV4dC9wbGFpbg==",
                Some(b"first"),
            ),
            at(3),
        );
        assert!(effects.is_empty());
        let effects = s.on_sequence(
            &response("type=read:status=DONE:id=wing-paste", None),
            at(4),
        );
        assert_eq!(
            effects.len(),
            2,
            "先交付第一条，再发第二条请求：{effects:?}"
        );
        assert_eq!(
            effects[0],
            Effect::Deliver(ClipboardPayload::Text("first".into()))
        );
        match &effects[1] {
            Effect::Send(bytes) => {
                let text = String::from_utf8(bytes.clone()).unwrap();
                assert!(text.contains(&b64_encode(b"two")), "{text}");
            }
            other => panic!("期望第二条的 read 请求，得到 {other:?}"),
        }
    }

    #[test]
    fn queue_is_bounded() {
        let mut s = Session::new();
        event_triplet("first", &["text/plain"], 0, &mut s);
        for i in 0..(MAX_QUEUED_EVENTS + 3) {
            event_triplet(&format!("pw{i}"), &["text/plain"], 1 + i as u64, &mut s);
        }
        assert_eq!(s.queue.len(), MAX_QUEUED_EVENTS, "队列有上限，多余的丢掉");
    }

    // ── 驱动（tokio 任务）─────────────────────────────────────────────────

    /// 端到端：通道收包 → 发出 read 请求（可注入的写出口捕获）→ 收响应 → 交付事件。
    #[tokio::test]
    async fn driver_writes_the_read_request_and_delivers_the_payload() {
        use std::sync::Arc;
        use std::sync::Mutex;

        let (seq_tx, seq_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let written = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&written);
        spawn_with(seq_rx, ev_tx, move |bytes| {
            sink.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        });

        // 终端发来 paste event 三连（text/plain）。
        seq_tx
            .send(packet("type=read:status=OK:pw=b3Rw", None))
            .await
            .unwrap();
        seq_tx
            .send(packet(
                &format!("type=read:status=DATA:mime={}:pw=b3Rw", b64_encode(b".")),
                Some(b"text/plain\n"),
            ))
            .await
            .unwrap();
        seq_tx
            .send(packet("type=read:status=DONE:pw=b3Rw", None))
            .await
            .unwrap();

        // 驱动应该把 read 请求写出去（等它一会儿）。
        let deadline = Instant::now() + Duration::from_secs(2);
        while written.lock().unwrap().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let request = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        assert_eq!(
            request,
            "\x1b]5522;type=read:name=d2luZw==:id=wing-paste:pw=b3Rw;dGV4dC9wbGFpbg==\x1b\\"
        );

        // 终端回 OK + DATA + DONE。
        seq_tx
            .send(packet("type=read:status=OK:id=wing-paste", None))
            .await
            .unwrap();
        seq_tx
            .send(packet(
                "type=read:status=DATA:id=wing-paste:mime=dGV4dC9wbGFpbg==",
                Some(b"pasted text"),
            ))
            .await
            .unwrap();
        seq_tx
            .send(packet("type=read:status=DONE:id=wing-paste", None))
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("交付不超时")
            .expect("事件通道还开着");
        match event {
            TermEvent::Clipboard(Ok(ClipboardPayload::Text(text))) => {
                assert_eq!(text, "pasted text");
            }
            other => panic!("期望一条文本粘贴事件，得到 {other:?}"),
        }
    }

    /// 终端拒绝（EPERM）在读中途到达：驱动把它投成失败事件（不 panic、不挂死）。
    #[tokio::test]
    async fn driver_reports_failures_as_events() {
        let (seq_tx, seq_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        spawn_with(seq_rx, ev_tx, |_| Ok(()));

        // 完整的 event 三连 → 驱动进入「读在途」。
        seq_tx
            .send(packet("type=read:status=OK:pw=b3Rw", None))
            .await
            .unwrap();
        seq_tx
            .send(packet(
                &format!("type=read:status=DATA:mime={}:pw=b3Rw", b64_encode(b".")),
                Some(b"image/png\n"),
            ))
            .await
            .unwrap();
        seq_tx
            .send(packet("type=read:status=DONE:pw=b3Rw", None))
            .await
            .unwrap();
        // 读请求已发出（等一小会儿），然后终端拒绝。
        tokio::time::sleep(Duration::from_millis(20)).await;
        seq_tx
            .send(packet("type=read:status=EPERM:id=wing-paste", None))
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("失败事件必须到达")
            .expect("通道还开着");
        assert_eq!(event_failure(&event), Some(ClipboardFailure::Denied));
    }

    fn event_failure(event: &TermEvent) -> Option<ClipboardFailure> {
        match event {
            TermEvent::Clipboard(Err(f)) => Some(f.clone()),
            _ => None,
        }
    }

    #[test]
    fn failure_messages_are_non_empty() {
        for failure in [
            ClipboardFailure::NoRepresentation,
            ClipboardFailure::Denied,
            ClipboardFailure::Unsupported,
            ClipboardFailure::Timeout,
            ClipboardFailure::TooLarge,
            ClipboardFailure::Malformed,
            ClipboardFailure::Status("EIO".into()),
        ] {
            assert!(!failure.message().is_empty(), "{failure:?}");
        }
    }
}
