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
//!   的唯一依据（`pw` 两边都有；listing 的 DATA 与数据的 DATA 靠 `mime` 区分）。每次 read
//!   用**唯一 id**（`wing-paste-<n>`），被放弃的 read 残留的包因此不会被记到下一次读上。

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

/// 我们发出的 read 请求的 id 前缀（响应回显；用于与 unsolicited event 区分）。
///
/// 每次 read 追加序号：被放弃的 read（超时 / 超限）之后终端仍会把剩下的包补发完，唯一 id
/// 让这些残留包落不到下一次读上（同 id 时无法区分：同 mime、同 pw）。
/// 字符集必须是 Ghostty `sanitizeId` 的白名单（`[a-zA-Z0-9-_+.]`），`-` 在内 ✓。
const REQUEST_ID_PREFIX: &str = "wing-paste-";

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
    /// 队列满（连续粘贴超出 [`MAX_QUEUED_EVENTS`]）：这一次粘贴被丢弃。
    QueueFull,
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
            // 队列里可能是文本也可能是图片（还没读，无从知道），所以说 "Paste"。
            Self::QueueFull => "Paste dropped: the paste queue is full".to_string(),
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
    /// 一次性密码的**线上原样**（base64 文本，event 包）。回显进 read 请求时逐字使用：
    /// 不解码、也就不要求它是合法 UTF-8（协议只规定是 base64；Ghostty 的 OTP 字母表是
    /// ASCII，但这里没必要依赖这一点）。
    pub pw_b64: Option<String>,
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
            pw_b64: None,
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
                "pw" => packet.pw_b64 = Some(value.to_string()),
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

/// MIME 的归一形态：剥掉 `;charset=…` 之类的参数、去首尾空白、转小写。
///
/// **请求侧与响应侧必须用同一份归一**（[`choose_mime`] 挑表示、read DATA 的 mime 归属
/// 比对）：终端在 listing 里列的名字与它自己 DATA 回显的名字不必逐字一致（参数 / 大小写
/// 都是允许的差异），两侧口径一旦分叉，DATA 会被静默丢弃——读停在空数据上，最后以
/// 「没有可用表示」或超时收场，而终端明明把字节发过来了。
pub(crate) fn normalize_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
}

/// 从 listing 里挑一个要读的 MIME（纯函数）。
///
/// 规则：图片优先（按 [`IMAGE_PRIORITY`]，其余 `image/*` 按 listing 顺序兜底），否则
/// `text/plain`；都没有 → `None`（不读，直接降级）。比较前会剥掉 `;charset=…` 之类的
/// 参数并忽略大小写（[`normalize_mime`]），以容纳各终端列出的名字差异。
pub(crate) fn choose_mime(available: &[String]) -> Option<String> {
    let bases: Vec<String> = available.iter().map(|m| normalize_mime(m)).collect();
    for want in IMAGE_PRIORITY {
        if bases.iter().any(|b| b == want) {
            return Some(want.to_string());
        }
    }
    if let Some(other) = bases.iter().find(|b| b.starts_with("image/")) {
        return Some(other.clone());
    }
    // 文本统一按规范名 `text/plain` 请求：终端（Ghostty 的 `isTextMime`）会把各种文本别名
    // （含 `text/plain;charset=utf-8`）都算成同一个表示，请求规范名最稳。`starts_with`
    // 同时覆盖等值形态。
    if bases.iter().any(|b| b.starts_with("text/plain")) {
        return Some("text/plain".to_string());
    }
    None
}

/// 构造带 OTP 的 follow-up read 请求（`ESC]5522;…ST`）。
///
/// `pw_b64` = 事件包里带过来的**原样 base64 文本**（逐字回显，不解码）；`id` 每次唯一。
pub(crate) fn read_request(pw_b64: &str, mime: &str, primary: bool, id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(mime.len() + pw_b64.len() + REQUESTER_NAME.len() + 64);
    out.extend_from_slice(b"\x1b]5522;type=read");
    if primary {
        out.extend_from_slice(b":loc=primary");
    }
    out.extend_from_slice(b":name=");
    out.extend_from_slice(b64_encode(REQUESTER_NAME.as_bytes()).as_bytes());
    out.extend_from_slice(b":id=");
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(b":pw=");
    out.extend_from_slice(pw_b64.as_bytes());
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
    pw_b64: String,
    primary: bool,
    mimes: Vec<String>,
}

/// 正在收集的 event（OK 已到，等 listing 与 DONE）。
#[derive(Debug)]
struct EventCollector {
    pw_b64: String,
    primary: bool,
    mimes: Vec<String>,
    deadline: Instant,
}

/// 在途的 read。`id` 每次唯一：被放弃的 read 残留的包（同 mime、同 pw）必须落不到下一次。
#[derive(Debug)]
struct ReadState {
    id: String,
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
    /// 下一次 read 的序号（唯一 id 的来源）。
    reads_started: u64,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// 下一次需要 [`Session::on_timeout`] 的时刻（无在途 = 无需唤醒）。
    ///
    /// `read` 为空 ⟹ 队列为空：每条让 `read` 变空的路径都在同一次调用里推进队列
    /// （见 [`Session::abort_read`]），待服务的事件不会停在「没有任何 deadline」上。
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
    ///
    /// 归属规则（协议里没有别的手段）：**没有 `id` = 终端自发的事件**（三连包），
    /// `id == 当前 read 的 id` = 我们这次读的响应，其余一律是噪声（例如被放弃的读的残留包）。
    pub fn on_sequence(&mut self, raw: &[u8], now: Instant) -> Vec<Effect> {
        let Some(packet) = Packet::parse(raw) else {
            // 坏包：读中途算致命（响应不可信），其余忽略（可能是别人的协议流量）。
            if self.read.is_some() {
                tracing::debug!("clipboard: malformed packet during read");
                return self.abort_read(ClipboardFailure::Malformed, now);
            }
            tracing::debug!("clipboard: ignoring malformed OSC 5522 packet");
            return Vec::new();
        };
        if packet.op != "read" {
            return Vec::new();
        }
        match packet.id.as_deref() {
            // 终端自发的事件（event 三连不带 id）。
            None => self.on_event_packet(&packet, now),
            Some(id) => {
                let ours = self.read.as_ref().is_some_and(|read| read.id == id);
                if !ours {
                    // 上一次读（已放弃）的残留，或别人的流量：忽略。
                    tracing::debug!("clipboard: ignoring OSC 5522 response for another read");
                    return Vec::new();
                }
                self.on_read_packet(&packet, now)
            }
        }
    }

    fn on_event_packet(&mut self, packet: &Packet, now: Instant) -> Vec<Effect> {
        // event 三连没有 id 可比，就比**一次性密码**：它由终端为每一次粘贴新生成（Ghostty
        // 的 `otp`），三连里每个包都回显。旧事件被放弃后迟到的 listing / DONE 因此落不到
        // 新事件上（read 侧的同款隔离见唯一 id）。
        let same_event = |packet: &Packet, event: &EventCollector| {
            packet.pw_b64.as_deref() == Some(event.pw_b64.as_str())
        };
        match packet.status.as_str() {
            "OK" => {
                let Some(pw_b64) = packet.pw_b64.clone() else {
                    return Vec::new();
                };
                if self.event.is_some() {
                    tracing::debug!("clipboard: superseding an unfinished paste event");
                }
                self.event = Some(EventCollector {
                    pw_b64,
                    primary: packet.primary,
                    mimes: Vec::new(),
                    deadline: now + IDLE_TIMEOUT,
                });
                Vec::new()
            }
            "DATA" => {
                if let Some(event) = &mut self.event
                    && packet.mime.as_deref() == Some(".")
                    && same_event(packet, event)
                {
                    event.mimes = parse_listing(&packet.payload);
                    event.deadline = now + IDLE_TIMEOUT;
                }
                Vec::new()
            }
            "DONE" => {
                if self
                    .event
                    .as_ref()
                    .is_some_and(|event| !same_event(packet, event))
                {
                    tracing::debug!("clipboard: ignoring a stale paste event packet");
                    return Vec::new();
                }
                let Some(event) = self.event.take() else {
                    return Vec::new();
                };
                let pending = PendingEvent {
                    pw_b64: event.pw_b64,
                    primary: event.primary,
                    mimes: event.mimes,
                };
                if self.read.is_some() {
                    // 上一次读还在途：排队，读完再服务它。
                    if self.queue.len() < MAX_QUEUED_EVENTS {
                        self.queue.push_back(pending);
                        Vec::new()
                    } else {
                        // 队列满 = 这次粘贴被丢弃。丢弃也要出声（与读失败同一条红线：
                        // 不出现「粘了但什么都没发生」）——App 侧转成 warning toast。
                        tracing::warn!("clipboard: paste event dropped (queue full)");
                        vec![Effect::Fail(ClipboardFailure::QueueFull)]
                    }
                } else {
                    self.start(pending, now)
                }
            }
            _ => Vec::new(),
        }
    }

    fn on_read_packet(&mut self, packet: &Packet, now: Instant) -> Vec<Effect> {
        match packet.status.as_str() {
            "OK" => {
                if let Some(read) = &mut self.read {
                    read.deadline = now + IDLE_TIMEOUT;
                }
                Vec::new()
            }
            "DATA" => {
                let mut too_large = false;
                // mime 归属按**归一形态**比对（与 `choose_mime` 同一份归一）：终端回显
                // 带参数（`text/plain;charset=utf-8`）或大小写不同是允许的差异，逐字相等
                // 会把 DATA 静默丢掉。
                if let Some(read) = &mut self.read
                    && packet
                        .mime
                        .as_deref()
                        .is_some_and(|mime| normalize_mime(mime) == read.mime)
                {
                    if read.data.len() + packet.payload.len() > MAX_PAYLOAD {
                        too_large = true;
                    } else {
                        read.data.extend_from_slice(&packet.payload);
                        read.deadline = now + IDLE_TIMEOUT;
                    }
                }
                if too_large {
                    return self.abort_read(ClipboardFailure::TooLarge, now);
                }
                Vec::new()
            }
            "DONE" => self.finish_read(now),
            other => self.abort_read(failure_for(other), now),
        }
    }

    /// 推进超时。
    pub fn on_timeout(&mut self, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(event) = &self.event
            && now >= event.deadline
        {
            // 收不全的事件（OK 到了、listing / DONE 没到）也要给用户一个交代：粘贴了却
            // 毫无动静是最难排查的形态。
            tracing::debug!("clipboard: paste event incomplete, giving up");
            self.event = None;
            effects.push(Effect::Fail(ClipboardFailure::Timeout));
        }
        if let Some(read) = &self.read
            && now >= read.deadline
        {
            effects.extend(self.abort_read(ClipboardFailure::Timeout, now));
        }
        effects.extend(self.pump_queue(now));
        effects
    }

    /// 放弃在途的读：交付失败，并把队列推进一格。
    ///
    /// **让 `read` 变空的失败路径必须走这里**（正常收尾走 [`Session::finish_read`]）：
    /// 只置空会让 `deadline()` 变成 `None`，驱动任务的 `select!` 从此不再唤醒——
    /// 队列里已到齐的粘贴事件无反馈卡死，而后续粘贴因 `read.is_none()` 直接开始新读
    /// （插队），被卡的事件要等下一次读完成才被取出（那时 OTP 多半已失效）。
    fn abort_read(&mut self, failure: ClipboardFailure, now: Instant) -> Vec<Effect> {
        self.read = None;
        let mut effects = vec![Effect::Fail(failure)];
        effects.extend(self.pump_queue(now));
        effects
    }

    fn finish_read(&mut self, now: Instant) -> Vec<Effect> {
        let Some(read) = self.read.take() else {
            return Vec::new();
        };
        // 一个 DATA 都没到 = 终端表示「这个表示不可用」（协议正是用它表达不可用），
        // 不是「空粘贴」：空载荷投给 App 只会变成静默无操作或 0 字节的图。
        let mut effects = if read.data.is_empty() {
            vec![Effect::Fail(ClipboardFailure::NoRepresentation)]
        } else {
            let payload = if read.mime.starts_with("image/") {
                ClipboardPayload::Image {
                    bytes: read.data,
                    mime: read.mime.clone(),
                    name: None,
                }
            } else {
                ClipboardPayload::Text(String::from_utf8_lossy(&read.data).into_owned())
            };
            vec![Effect::Deliver(payload)]
        };
        effects.extend(self.pump_queue(now));
        effects
    }

    /// 队列里下一个事件（读空出来了就开始）。
    ///
    /// `start` 自己也会失败（listing 里没有可读的表示 → 直接交付失败）：读仍空着，
    /// 必须继续拿下一个，否则排在他后面的事件们同样卡死。
    fn pump_queue(&mut self, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        while self.read.is_none() {
            let Some(next) = self.queue.pop_front() else {
                return effects;
            };
            effects.extend(self.start(next, now));
        }
        effects
    }

    fn start(&mut self, event: PendingEvent, now: Instant) -> Vec<Effect> {
        let Some(mime) = choose_mime(&event.mimes) else {
            tracing::debug!(mimes = ?event.mimes, "clipboard: no usable representation");
            return vec![Effect::Fail(ClipboardFailure::NoRepresentation)];
        };
        self.reads_started += 1;
        let id = format!("{REQUEST_ID_PREFIX}{}", self.reads_started);
        self.read = Some(ReadState {
            id: id.clone(),
            mime: mime.clone(),
            data: Vec::new(),
            deadline: now + IDLE_TIMEOUT,
        });
        vec![Effect::Send(read_request(
            &event.pw_b64,
            &mime,
            event.primary,
            &id,
        ))]
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
        assert_eq!(
            p.pw_b64.as_deref(),
            Some("b3Rw"),
            "pw 按线上原样（base64 文本）保留"
        );
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
            &b"type=read:status"[..],          // 缺 '='
            &b"type=read:status=OK:x;!!!"[..], // 坏 base64 负载
            &b"\xff\xfe"[..],                  // 不是 UTF-8
        ] {
            assert_eq!(Packet::parse(bad), None, "{bad:?}");
        }
    }

    /// `pw` 按**线上原样**保留：不解码、不要求 UTF-8，回显进请求时逐字使用。
    #[test]
    fn packet_keeps_the_password_in_wire_form() {
        let p = Packet::parse(b"type=read:status=OK:pw=!!not-base64!!").expect("parse");
        assert_eq!(p.pw_b64.as_deref(), Some("!!not-base64!!"));
        // 非 UTF-8 的 base64 也不该让整个包作废（解码成 String 会把它判坏）。
        let p = Packet::parse(b"type=read:status=OK:pw=//8=").expect("parse");
        assert_eq!(p.pw_b64.as_deref(), Some("//8="));
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
        // 相对优先级：两个候选同时在列时也必须按表选（只断言「能识别」会漏掉顺序回归）。
        assert_eq!(
            choose_mime(&s(&["image/jpeg", "image/png", "text/plain"])).as_deref(),
            Some("image/png"),
            "png 优于 jpeg"
        );
        assert_eq!(
            choose_mime(&s(&["image/gif", "image/webp"])).as_deref(),
            Some("image/webp"),
            "webp 优于 gif"
        );
        assert_eq!(
            choose_mime(&s(&["image/tiff", "image/gif"])).as_deref(),
            Some("image/gif"),
            "优先级表内的 gif 优于兜底的任意 image/*"
        );
        assert_eq!(
            choose_mime(&s(&["text/plain", "image/tiff"])).as_deref(),
            Some("image/tiff"),
            "任意 image/* 兜底都优于文本"
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

    /// 终端回显的 mime 带参数 / 大小写不同：DATA 的归属按**归一形态**（与 `choose_mime`
    /// 同一份归一）——逐字相等会把字节静默掉光，读停在空数据上，收场是误导性的
    /// 「没有可用表示」（或超时），而终端明明把字节发来了。
    #[test]
    fn read_data_matches_the_echoed_mime_normalized() {
        for echo in [
            "text/plain",
            "text/plain;charset=utf-8",
            "TEXT/PLAIN",
            " text/plain ",
        ] {
            let mut s = Session::new();
            event_triplet("otp", &["text/plain"], 0, &mut s);
            assert!(
                s.on_sequence(
                    &response(
                        &format!(
                            "type=read:status=DATA:id=wing-paste-1:mime={}",
                            b64_encode(echo.as_bytes())
                        ),
                        Some(b"hello"),
                    ),
                    at(1),
                )
                .is_empty(),
                "{echo:?}"
            );
            assert_eq!(
                s.on_sequence(
                    &response("type=read:status=DONE:id=wing-paste-1", None),
                    at(2)
                ),
                vec![Effect::Deliver(ClipboardPayload::Text("hello".into()))],
                "{echo:?}: 等价的回显必须归属到这次读"
            );
        }
    }

    /// 归一不是「谁的 DATA 都收」：类型真的不同仍然被忽略（否则文本字节会被当成
    /// PNG 交付），大小写 / 参数的等价变体才归属。
    #[test]
    fn read_data_mime_match_stays_strict_about_the_actual_type() {
        let mut s = Session::new();
        event_triplet("otp", &["image/png"], 0, &mut s);
        assert!(
            s.on_sequence(
                &response(
                    &format!(
                        "type=read:status=DATA:id=wing-paste-1:mime={}",
                        b64_encode(b"text/plain")
                    ),
                    Some(b"NOT-PNG"),
                ),
                at(1),
            )
            .is_empty()
        );
        assert!(
            s.on_sequence(
                &response(
                    &format!(
                        "type=read:status=DATA:id=wing-paste-1:mime={}",
                        b64_encode(b"IMAGE/PNG")
                    ),
                    Some(b"\x89PNG"),
                ),
                at(2),
            )
            .is_empty()
        );
        assert_eq!(
            s.on_sequence(
                &response("type=read:status=DONE:id=wing-paste-1", None),
                at(3)
            ),
            vec![Effect::Deliver(ClipboardPayload::Image {
                bytes: b"\x89PNG".to_vec(),
                mime: "image/png".into(),
                name: None,
            })],
            "只有归一后等价的那些片字节进了读"
        );
    }

    #[test]
    fn normalize_mime_strips_parameters_whitespace_and_case() {
        assert_eq!(normalize_mime("text/plain;charset=utf-8"), "text/plain");
        assert_eq!(normalize_mime("IMAGE/PNG"), "image/png");
        assert_eq!(normalize_mime(" image/jpeg "), "image/jpeg");
        assert_eq!(normalize_mime("image/webp; q=0.3"), "image/webp");
        assert_eq!(normalize_mime("application/x-foo"), "application/x-foo");
    }

    // ── 请求构造 ──────────────────────────────────────────────────────────

    #[test]
    fn read_request_is_exact_bytes() {
        let bytes = read_request(&b64_encode(b"otpxyz"), "image/png", false, "wing-paste-7");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b]5522;type=read:name=d2luZw==:id=wing-paste-7:pw=b3RweHl6;aW1hZ2UvcG5n\x1b\\"
        );
        // primary 带上 loc；请求里的 name / mime 是 base64、pw 逐字回显事件里的 base64。
        let bytes = read_request(&b64_encode(b"otp"), "text/plain", true, "wing-paste-8");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b]5522;type=read:loc=primary:name=d2luZw==:id=wing-paste-8:pw=b3Rw;dGV4dC9wbGFpbg==\x1b\\"
        );
    }

    /// `name` 不是装饰：Ghostty 的授权判定要求它非空（见模块文档）。
    #[test]
    fn read_request_always_carries_a_name() {
        assert!(
            read_request("x", "text/plain", false, "wing-paste-1")
                .starts_with(b"\x1b]5522;type=read:name=")
        );
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
                    "\x1b]5522;type=read:name=d2luZw==:id=wing-paste-1:pw=b3RwMTIz;aW1hZ2UvcG5n\x1b\\"
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
            s.on_sequence(
                &response("type=read:status=OK:id=wing-paste-1", None),
                at(10)
            )
            .is_empty()
        );
        assert!(
            s.on_sequence(
                &response(
                    "type=read:status=DATA:id=wing-paste-1:mime=dGV4dC9wbGFpbg==",
                    Some("hello 世界".as_bytes()),
                ),
                at(11),
            )
            .is_empty()
        );
        let effects = s.on_sequence(
            &response("type=read:status=DONE:id=wing-paste-1", None),
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
                    "type=read:status=DATA:id=wing-paste-1:mime=aW1hZ2UvcG5n",
                    Some(chunk),
                ),
                at(20),
            ));
        }
        effects.extend(s.on_sequence(
            &response("type=read:status=DONE:id=wing-paste-1", None),
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
                &response(&format!("type=read:status={status}:id=wing-paste-1"), None),
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

    /// 收不全的事件（OK 到了、listing / DONE 没到）也要给用户反馈：粘贴了却毫无动静
    /// 是最难排查的形态。
    #[test]
    fn incomplete_event_reports_a_failure() {
        let mut s = Session::new();
        // 只有 OK：listing / DONE 都没来。
        s.on_sequence(&packet("type=read:status=OK:pw=b3Rw", None), at(0));
        assert!(s.on_timeout(at(100)).is_empty());
        assert_eq!(
            s.on_timeout(at(6000)),
            vec![Effect::Fail(ClipboardFailure::Timeout)],
            "收不全也要有反馈"
        );
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
                "type=read:status=DATA:id=wing-paste-1:mime=aW1hZ2UvcG5n",
                Some(&big),
            ),
            at(1),
        );
        effects.extend(s.on_sequence(
            &response(
                "type=read:status=DATA:id=wing-paste-1:mime=aW1hZ2UvcG5n",
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
        // 结构坏掉（记录缺 '='）：读中途的响应不可信，直接放弃这次读。
        let effects = s.on_sequence(b"type=read:status", at(1));
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
        // 我们的读仍然活着；零数据 DONE = 该表示不可用（不是「空粘贴」）。
        assert_eq!(
            s.on_sequence(
                &response("type=read:status=DONE:id=wing-paste-1", None),
                at(4)
            ),
            vec![Effect::Fail(ClipboardFailure::NoRepresentation)]
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
        s.on_sequence(
            &response("type=read:status=OK:id=wing-paste-1", None),
            at(2),
        );
        let effects = s.on_sequence(
            &response(
                "type=read:status=DATA:id=wing-paste-1:mime=dGV4dC9wbGFpbg==",
                Some(b"first"),
            ),
            at(3),
        );
        assert!(effects.is_empty());
        let effects = s.on_sequence(
            &response("type=read:status=DONE:id=wing-paste-1", None),
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

    /// [B] 读中途失败的三种形态（终端拒绝 / 数据超限 / 坏包）都必须把队列推进一格。
    ///
    /// 修复前的反例：失败路径只把 `read` 置空（不 pump、不留 deadline）→ 驱动任务的
    /// `select!` 拿 `None` 永远 pending，队列里已到齐的第二个粘贴事件无反馈卡死；
    /// 后续粘贴还会因 `read.is_none()` 直接开始新读（插队），被卡的事件要等下一次读
    /// 完成才被取出（那时它的 OTP 多半已失效 → 迟到错误 + 顺序错乱）。
    #[test]
    fn read_failure_serves_the_queued_paste_instead_of_stranding_it() {
        type Kill = fn(&mut Session) -> Vec<Effect>;
        let kills: [(&str, Kill, ClipboardFailure); 4] = [
            (
                "EPERM",
                |s| {
                    s.on_sequence(
                        &response("type=read:status=EPERM:id=wing-paste-1", None),
                        at(10),
                    )
                },
                ClipboardFailure::Denied,
            ),
            (
                "EFBIG",
                |s| {
                    s.on_sequence(
                        &response("type=read:status=EFBIG:id=wing-paste-1", None),
                        at(10),
                    )
                },
                ClipboardFailure::TooLarge,
            ),
            (
                "malformed",
                |s| s.on_sequence(b"type=read:status", at(10)),
                ClipboardFailure::Malformed,
            ),
            (
                "oversize data",
                |s| {
                    s.on_sequence(
                        &response(
                            &format!(
                                "type=read:status=DATA:id=wing-paste-1:mime={}",
                                b64_encode(b"text/plain")
                            ),
                            Some(&vec![0u8; MAX_PAYLOAD + 1]),
                        ),
                        at(10),
                    )
                },
                ClipboardFailure::TooLarge,
            ),
        ];

        for (label, kill, expected) in kills {
            let mut s = Session::new();
            event_triplet("one", &["text/plain"], 0, &mut s);
            assert!(
                event_triplet("two", &["text/plain"], 1, &mut s).is_empty(),
                "{label}: 第二个事件先排队"
            );

            let effects = kill(&mut s);

            // 失败有反馈，第二个事件在同一批里被服务（既不卡死也不被插队）。
            assert_eq!(
                effects.len(),
                2,
                "{label}: 失败要有反馈、队列要推进：{effects:?}"
            );
            assert_eq!(effects[0], Effect::Fail(expected), "{label}");
            let Effect::Send(bytes) = &effects[1] else {
                panic!("{label}: 期望第二个事件的 read 请求，得到 {:?}", effects[1]);
            };
            let text = String::from_utf8(bytes.clone()).unwrap();
            assert!(
                text.contains("id=wing-paste-2"),
                "{label}: 新读要用下一个序号（旧 id 的残留包才不会串场）：{text}"
            );
            assert!(
                text.contains(&b64_encode(b"two")),
                "{label}: 授权必须来自第二个事件：{text}"
            );

            // 第三次粘贴不插队：read#2 在途 → 排队；read#2 收完才轮到它（FIFO）。
            assert!(
                event_triplet("three", &["text/plain"], 11, &mut s).is_empty(),
                "{label}: 第三次粘贴排队"
            );
            assert!(
                s.on_sequence(
                    &response(
                        "type=read:status=DATA:id=wing-paste-2:mime=dGV4dC9wbGFpbg==",
                        Some(b"second"),
                    ),
                    at(12),
                )
                .is_empty()
            );
            let effects = s.on_sequence(
                &response("type=read:status=DONE:id=wing-paste-2", None),
                at(13),
            );
            assert_eq!(effects.len(), 2, "{label}: {effects:?}");
            assert_eq!(
                effects[0],
                Effect::Deliver(ClipboardPayload::Text("second".into())),
                "{label}: 先交付第二个事件"
            );
            let Effect::Send(bytes) = &effects[1] else {
                panic!("{label}: 期望第三次的 read 请求，得到 {:?}", effects[1]);
            };
            let text = String::from_utf8(bytes.clone()).unwrap();
            assert!(
                text.contains("id=wing-paste-3") && text.contains(&b64_encode(b"three")),
                "{label}: {text}"
            );
        }
    }

    /// 队列推进撞上「没有可读表示」的事件不能停在那里：它自己要有失败反馈，
    /// 排在它后面的事件也必须继续被服务（同一类卡死：`read` 空着、队列非空、
    /// 没有任何 deadline 会再来叫醒 [`Session::on_timeout`]）。
    #[test]
    fn pump_queue_serves_past_an_event_with_no_usable_representation() {
        let mut s = Session::new();
        event_triplet("one", &["text/plain"], 0, &mut s);
        event_triplet("two", &["text/html"], 1, &mut s);
        event_triplet("three", &["text/plain"], 2, &mut s);

        // read#1 零数据 DONE：自己失败，随后 #2 失败、#3 立刻开始。
        let effects = s.on_sequence(
            &response("type=read:status=DONE:id=wing-paste-1", None),
            at(3),
        );
        assert_eq!(effects.len(), 3, "{effects:?}");
        assert_eq!(effects[0], Effect::Fail(ClipboardFailure::NoRepresentation));
        assert_eq!(effects[1], Effect::Fail(ClipboardFailure::NoRepresentation));
        let Effect::Send(bytes) = &effects[2] else {
            panic!("期望第三次的 read 请求，得到 {:?}", effects[2]);
        };
        let text = String::from_utf8(bytes.clone()).unwrap();
        // 序号按**已开始的读**计数（#2 没有可读表示、从未起读，所以这次是 wing-paste-2）。
        assert!(
            text.contains("id=wing-paste-2") && text.contains(&b64_encode(b"three")),
            "{text}"
        );
        assert!(s.deadline().is_some(), "第三次读在途");
    }

    /// 队列有上限：多出来的事件被丢弃——但**丢弃要出声**（红线：不出现「粘了但什么
    /// 都没发生」），每个被丢的事件给一条 [`ClipboardFailure::QueueFull`]。
    #[test]
    fn queue_is_bounded_and_a_drop_is_announced() {
        let mut s = Session::new();
        event_triplet("first", &["text/plain"], 0, &mut s);
        let mut effects = Vec::new();
        for i in 0..(MAX_QUEUED_EVENTS + 3) {
            effects.extend(event_triplet(
                &format!("pw{i}"),
                &["text/plain"],
                1 + i as u64,
                &mut s,
            ));
        }
        assert_eq!(s.queue.len(), MAX_QUEUED_EVENTS, "队列有上限，多余的丢掉");
        assert_eq!(effects.len(), 3, "每个被丢的事件都要有反馈：{effects:?}");
        assert!(
            effects
                .iter()
                .all(|e| *e == Effect::Fail(ClipboardFailure::QueueFull)),
            "{effects:?}"
        );
        // 文案是 App 直接转 toast 的那句（队列里可能是文本也可能是图片，还没读）。
        assert_eq!(
            ClipboardFailure::QueueFull.message(),
            "Paste dropped: the paste queue is full"
        );
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
            "\x1b]5522;type=read:name=d2luZw==:id=wing-paste-1:pw=b3Rw;dGV4dC9wbGFpbg==\x1b\\"
        );

        // 终端回 OK + DATA + DONE。
        seq_tx
            .send(packet("type=read:status=OK:id=wing-paste-1", None))
            .await
            .unwrap();
        seq_tx
            .send(packet(
                "type=read:status=DATA:id=wing-paste-1:mime=dGV4dC9wbGFpbg==",
                Some(b"pasted text"),
            ))
            .await
            .unwrap();
        seq_tx
            .send(packet("type=read:status=DONE:id=wing-paste-1", None))
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
            .send(packet("type=read:status=EPERM:id=wing-paste-1", None))
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("失败事件必须到达")
            .expect("通道还开着");
        assert_eq!(event_failure(&event), Some(ClipboardFailure::Denied));
    }

    /// [B] 驱动面：第一次读以失败收场（EPERM）时，队列里的第二次粘贴必须当场被服务。
    ///
    /// 修复前的形态：失败让 `read` 变空、队列仍非空 → `deadline()` 变 `None`，驱动
    /// 的 `select!` 不再唤醒，第二个请求要等下一次流量路过才被写出（若流量迟迟不来，
    /// 用户看到的是"粘了没反应"）。
    #[tokio::test]
    async fn driver_serves_the_queued_paste_after_a_read_failure() {
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

        // event#1（pw=one）→ 读在途；event#2（pw=two）→ 排队。
        for pw in [b"one".as_slice(), b"two".as_slice()] {
            seq_tx
                .send(packet(
                    &format!("type=read:status=OK:pw={}", b64_encode(pw)),
                    None,
                ))
                .await
                .unwrap();
            seq_tx
                .send(packet(
                    &format!(
                        "type=read:status=DATA:mime={}:pw={}",
                        b64_encode(b"."),
                        b64_encode(pw)
                    ),
                    Some(b"text/plain\n"),
                ))
                .await
                .unwrap();
            seq_tx
                .send(packet(
                    &format!("type=read:status=DONE:pw={}", b64_encode(pw)),
                    None,
                ))
                .await
                .unwrap();
        }
        wait_for("第一个 read 请求写出去", || {
            !written.lock().unwrap().is_empty()
        })
        .await;

        seq_tx
            .send(packet("type=read:status=EPERM:id=wing-paste-1", None))
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("失败事件必须到达")
            .expect("通道还开着");
        assert_eq!(event_failure(&event), Some(ClipboardFailure::Denied));

        wait_for("第二个 read 请求写出去", || {
            String::from_utf8_lossy(&written.lock().unwrap()).contains("id=wing-paste-2")
        })
        .await;
        let text = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains(&format!("pw={}", b64_encode(b"two"))),
            "第二个请求必须带第二个事件的授权：{text}"
        );
    }

    /// 轮询等待驱动任务（独立 tokio 任务）完成某件事；超时即断言失败。
    async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(done(), "等待超时：{what}");
    }

    fn event_failure(event: &TermEvent) -> Option<ClipboardFailure> {
        match event {
            TermEvent::Clipboard(Err(f)) => Some(f.clone()),
            _ => None,
        }
    }

    /// [S2] 上一次读被放弃后，终端仍会把剩下的包补发完。唯一 id 让这些残留落不到下一次
    /// 读上（同 id 时无法区分：同 mime、同 pw）——否则新读会被旧的 DONE 提前终结，投出
    /// 上一次的内容。
    #[test]
    fn stale_residue_from_an_abandoned_read_is_ignored() {
        let mut s = Session::new();
        event_triplet("otp", &["image/png"], 0, &mut s);
        // 第一次读：请求 id = wing-paste-1，然后超时放弃。
        assert_eq!(
            s.on_timeout(at(IDLE_TIMEOUT.as_millis() as u64 + 100)),
            vec![Effect::Fail(ClipboardFailure::Timeout)]
        );
        // 用户又粘了一次 → 第二次读，id 必须换了。
        let effects = event_triplet("otp", &["image/png"], 6000, &mut s);
        match &effects[0] {
            Effect::Send(bytes) => {
                let text = String::from_utf8(bytes.clone()).unwrap();
                assert!(text.contains("id=wing-paste-2"), "{text}");
            }
            other => panic!("期望第二次读的请求，得到 {other:?}"),
        }
        // 终端补发**第一次**读的残留：DATA + DONE（旧 id）。
        assert!(
            s.on_sequence(
                &response(
                    "type=read:status=DATA:id=wing-paste-1:mime=aW1hZ2UvcG5n",
                    Some(b"STALE")
                ),
                at(6010)
            )
            .is_empty(),
            "旧 id 的 DATA 不该进新读"
        );
        assert!(
            s.on_sequence(
                &response("type=read:status=DONE:id=wing-paste-1", None),
                at(6011)
            )
            .is_empty(),
            "旧 id 的 DONE 不该终结新读"
        );
        // 新读自己的响应照常收。
        assert!(
            s.on_sequence(
                &response(
                    "type=read:status=DATA:id=wing-paste-2:mime=aW1hZ2UvcG5n",
                    Some(b"FRESH")
                ),
                at(6020)
            )
            .is_empty()
        );
        assert_eq!(
            s.on_sequence(
                &response("type=read:status=DONE:id=wing-paste-2", None),
                at(6021)
            ),
            vec![Effect::Deliver(ClipboardPayload::Image {
                bytes: b"FRESH".to_vec(),
                mime: "image/png".into(),
                name: None,
            })],
            "只有新读自己的字节"
        );
    }

    /// [N1] event 侧的孪生 S2：旧事件（被放弃）迟到的 listing / DONE 带的是**旧 pw**，
    /// 必须当噪声 —— 否则新事件会被旧 listing 定死 mime、被旧 DONE 提前终结。
    #[test]
    fn stale_event_packets_with_another_password_are_ignored() {
        let mut s = Session::new();
        // event#1（pw=otp1）：OK 到了，listing / DONE 拖到超时被放弃。
        s.on_sequence(&packet("type=read:status=OK:pw=b3RwMQ==", None), at(0));
        assert_eq!(
            s.on_timeout(at(6000)),
            vec![Effect::Fail(ClipboardFailure::Timeout)]
        );
        // 用户又粘一次：event#2（pw=otp2，剪贴板只有文本）。
        s.on_sequence(&packet("type=read:status=OK:pw=b3RwMg==", None), at(6001));
        // event#1 迟到的 listing（image/png）+ DONE：不得记到 event#2 上。
        assert!(
            s.on_sequence(
                &packet(
                    &format!(
                        "type=read:status=DATA:mime={}:pw=b3RwMQ==",
                        b64_encode(b".")
                    ),
                    Some(b"image/png\n"),
                ),
                at(6002),
            )
            .is_empty(),
            "旧 pw 的 listing 不得被采纳"
        );
        assert!(
            s.on_sequence(&packet("type=read:status=DONE:pw=b3RwMQ==", None), at(6003))
                .is_empty(),
            "旧 pw 的 DONE 不得终结新事件"
        );
        // event#2 自己的 listing + DONE 正常：请求用 otp2 的授权、读的是 text/plain。
        s.on_sequence(
            &packet(
                &format!(
                    "type=read:status=DATA:mime={}:pw=b3RwMg==",
                    b64_encode(b".")
                ),
                Some(b"text/plain\n"),
            ),
            at(6004),
        );
        let effects = s.on_sequence(&packet("type=read:status=DONE:pw=b3RwMg==", None), at(6005));
        match &effects[0] {
            Effect::Send(bytes) => {
                let text = String::from_utf8(bytes.clone()).unwrap();
                assert!(text.contains("pw=b3RwMg=="), "授权必须来自 event#2：{text}");
                assert!(
                    text.ends_with(&format!("{}\x1b\\", b64_encode(b"text/plain"))),
                    "读的必须是 event#2 的表示：{text}"
                );
            }
            other => panic!("期望 event#2 的 read 请求，得到 {other:?}"),
        }
    }

    /// 零数据 DONE（协议用它表示「该表示不可用」）走失败路径，而不是投空载荷。
    #[test]
    fn zero_data_read_fails_instead_of_delivering_empty() {
        let mut s = Session::new();
        event_triplet("otp", &["text/plain"], 0, &mut s);
        assert_eq!(
            s.on_sequence(
                &response("type=read:status=DONE:id=wing-paste-1", None),
                at(1)
            ),
            vec![Effect::Fail(ClipboardFailure::NoRepresentation)]
        );
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
            ClipboardFailure::QueueFull,
            ClipboardFailure::Status("EIO".into()),
        ] {
            assert!(!failure.message().is_empty(), "{failure:?}");
        }
    }
}
