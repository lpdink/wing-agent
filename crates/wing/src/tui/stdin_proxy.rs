//! stdin 预处理层：在 crossterm 看到输入之前，把 OSC 5522 序列拦下来。
//!
//! 为什么需要这一层：crossterm 0.29（最新版）没有「原始转义序列」事件，`ESC]5522;…`
//! 会被它按 `Alt+]` + 逐字符解析成一串按键（`event/sys/unix/parse.rs` 的
//! `ESC` + 未知第二字节分支），终端发来的协议数据会污染成用户输入。本模块把进程
//! stdin 的字节流改道：**`OSC 5522` 完整序列送协议机，其余字节逐字节透传**。
//!
//! # 为什么是 pty 而不是 pipe
//!
//! crossterm 的输入 fd 由 `terminal::sys::file_descriptor::tty_fd()` 决定：`isatty(stdin)`
//! 为真用 fd 0，否则打开 `/dev/tty`。若把 fd 0 换成 pipe，`isatty` 变假 → crossterm 改开
//! `/dev/tty`，绕过本层、且与泵线程抢同一个 tty 的字节；就算预热注册过事件源也没用——
//! 事件源的就绪注册（epoll / kqueue）绑定的是**注册时刻的文件描述**，`dup2` 之后 fd 0
//! 与它就绪源不再对应。换成 **pty slave** 就不一样：它本身是 tty，`isatty(0)` 仍为真，
//! crossterm 照旧读 fd 0（端到端证据见 `tests/stdin_proxy_pty.rs` 与
//! `tests/crossterm_osc5522_misparse.rs`）。
//!
//! ```text
//!  真实 tty ──read──► 泵线程（Splitter）──write──► pty master ⇄ pty slave（fd 0）──► crossterm
//!                                    ├─ 完整 OSC 5522 序列 ─► 协议机（clipboard_protocol）
//!                                    └─ DECRPM 应答（ESC[? … $y）─► 吸收（探针判定 / 丢弃）
//! ```
//!
//! # 生命周期（顺序是硬约束）
//!
//! - [`install`] 必须晚于 `crossterm::terminal::enable_raw_mode()`（raw 打在真实 tty 上）、
//!   早于 crossterm 事件源的首次创建 —— 后者是**进程级单例**，首次 `poll` 时把当时的
//!   fd 0 注册进就绪源、之后永不重建；而「`poll` 可能在同进程的第一段 TUI（setup 向导）
//!   就发生」，所以安装提升为**进程级一次性**（`tui::init_terminal` 的第一段负责开、
//!   `tui::end_tui_session` 负责关，段与段之间绝不拆）。反面证据：
//!   `tests/stdin_proxy_stale_event_source.rs`。
//! - [`restore`] 必须早于 `crossterm::terminal::disable_raw_mode()`（它把 termios 写回
//!   fd 0，彼时 fd 0 必须已经是真实 tty）。
//! - 收尾时若要打断卡在 fd 0 阻塞读里的读者，用 [`wake_reader`]：字节走 master→slave
//!   的内核路径，**必须在 [`restore`] 之前**调用（之后 master 已关、fd 0 也已换回真实 tty）。
//! - [`settle_paste_channel`] 必须早于本进程任何一次 crossterm `poll`（事件源注册之后 fd 0
//!   不能再换）；阴性判定会在模块内部直接 [`restore`]（首 poll 前的第二次拆解机会）。
//! - 失败不致命：[`install`] 任何一步出错都先在返回前还原 fd 0，调用方降级即可；但**没有
//!   装上就不要开 mode 5522**（唯一发送点在协议机启动处，见 `tui::spawn_event_stream`）
//!   ——否则协议数据会直接喂给 crossterm，正是本层要避免的事。
//!
//! # 启动探针：只在真支持 5522 的终端上留着接驳
//!
//! 接驳不是免费的：fd 0 被换成 pty slave、泵线程独占真实 tty、退出收尾多一层时序。不实现
//! 5522 的终端（Ghostty stable 明确应答 `0`；iTerm2 / Terminal.app / tmux 静默）从这套机制
//! 里拿不到任何东西，却要背下全部事故面。所以 [`install`] 装好接驳后立刻发 DECRQM 查询
//! `CSI ? 5522 $ p`，[`settle_paste_channel`] 在**第一段 TUI 的第一次 crossterm poll 之前**
//! 收束判定（crossterm 的事件源是进程级单例，注册之后 fd 0 就不能再换——反面证据
//! `tests/stdin_proxy_stale_event_source.rs`）：
//!
//! - 应答 `Ps ∈ {1,2,3}`：保留接驳，`?5522h` 与协议机由 `tui::spawn_event_stream` 启动；
//! - `Ps ∈ {0,4}`（明确阴性）：就地还原 fd 0，不再有任何 5522 流量会到达，接驳全部风险退场；
//! - 本地无应答（超时）：同上（不实现 DECRQM 的终端不会迟到应答）；
//! - SSH 无应答：**保留接驳、不开 mode** —— 远程链路的迟到应答必须仍被这里摘走，
//!   否则 `ESC[?…$y` 落进 crossterm 会永久毒化按键解析（见下）。
//!
//! 判定预算本地短（150ms）、SSH 长（600ms）；查询发出得早（`install` 时），到判定点的
//! 网络往返与图像能力探测通常已覆盖等待，额外延迟只发生在静默终端上、且一次性。
//!
//! ## 为什么应答必须在这里被摘走
//!
//! crossterm 0.29 的解析器对 `ESC [ ? … y` 永远返回 `Ok(None)`（`parse.rs` 的 `b'?'` 分支
//! 只认 `u`/`c` 两种 final），缓冲永不清理：一条迟到的 `CSI ? 5522 ; 0 $ y` 会让之后所有
//! 按键都被吞进这个「半截序列」。所以候选 `ESC[? <params> $y`（任意 DECRPM 应答，不只 5522）
//! 被摘取并吸收；**到点等不到 final 时丢弃而不是冲回**——冲回正是毒化本身。
//!
//! # 已知边界：粘贴内容以字面 `ESC` 结尾
//!
//! bracketed paste 保护区内，只扣「可能是闭界 `ESC[201~` 开头」的尾部字节，粘贴内容末尾的
//! 字面 `ESC` 因此先被当作前缀候选；真闭界到达时它原样冲回，透传流里出现**连续的
//! `ESC` + `ESC[201~`**。本层一字节不丢（「透传 + 序列 == 输入」的不变式钉住，
//! `bracketed_paste_keeps_a_trailing_literal_esc_byte_for_byte` 钉这一形态），但这段流对
//! 任何下游都是两义的：哪个 `ESC` 属于粘贴内容、哪个属于闭界，只有终端知道——协议里除
//! 「闭界紧跟数据」没有别的信息可分。固有歧义 + 极罕见，登记为已知边界，本层不做
//! （也做不了）区分。

use std::time::{Duration, Instant};

/// 一条完整的 OSC 5522 负载：`ESC]5522;` 之后、终止符（BEL / ST）之前的原始字节。
pub type Sequence = Vec<u8>;

/// 触发「远端档」的环境变量：SSH / mosh 的转发会把同一次 `write` 拆到多个 read。
const SSH_ENV: [&str; 4] = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"];

/// DECRQM 查询：`CSI ? 5522 $ p`（kitty clipboard protocol 的 paste events，mode 5522）。
const PROBE_QUERY: &[u8] = b"\x1b[?5522$p";

/// mode 5522 的编号（探针应答的 Pa 字段）。
const PASTE_EVENTS_MODE: u32 = 5522;

/// 探针判定预算：本地短（本地终端毫秒级应答，静默就是真静默）、SSH 长（RTT + 重排余量）。
const PROBE_BUDGET_LOCAL: Duration = Duration::from_millis(150);
const PROBE_BUDGET_SSH: Duration = Duration::from_millis(600);

/// 等待应答的轮询步长：应答由泵线程写入共享状态，没有可等待的事件源，只能轻量轮询。
const PROBE_POLL_STEP: Duration = Duration::from_millis(5);

/// 远端档的候选持有上限：拆包间隔可能到几十 ms，本地档会把 `ESC` 与紧跟的字节判成两个
/// 事件（Alt 组合退化成孤立 ESC → 误触发「打断」）。
const SSH_HOLD: Duration = Duration::from_millis(100);

/// 单 ESC 候选的本地持有上限：正好一个 `ESC` 字节可能是 `Esc` 键、也可能是被拆开的序列
/// 开头（读边界落在哪里无从预知），只能等一小会儿再判定。代价是本地 `Esc` 键多 ≤10ms
/// 判定延迟——有意接受的取舍（与 PI 同款），换来「协议字节绝不在未确认时漏给 crossterm」。
const ESC_HOLD: Duration = Duration::from_millis(10);

/// 多字节候选（`ESC]55…` / `ESC[20…`）的本地持有上限：这些形态不可能是 `Esc` 键，多等
/// 几毫秒能把大多数拆包形态拼回来。代价：`Alt+[…]` 组合键晚 ≤25ms。
const PREFIX_HOLD: Duration = Duration::from_millis(25);

/// 序列体的空闲上限：正常终端毫秒级闭合（Ghostty 一条包一次 write），超过即视为坏数据。
const SEQUENCE_IDLE: Duration = Duration::from_secs(5);

/// 单个序列体的字节上限：正常 5522 包远小于此（DATA 分片 ≤4096 原始字节 → base64
/// ≈5.5KiB），1MiB 已留近 200 倍余量；真正的会话级上限在协议机（`MAX_PAYLOAD` 64MiB）。
/// 超限只可能来自坏流：已累积的负载丢弃，后续字节吞到终止符 / 空闲超时，绝不冲回透传。
const SEQUENCE_BODY_MAX: usize = 1024 * 1024;

/// 泵线程里「待写进内层 pty」的字节上限。到顶就暂时不读源（也让真实 tty 的环形缓冲
/// 把背压顶回终端，而不是在我们这里无限吃内存）——poll 的 POLLIN 只能撤掉，否则
/// level-triggered 的就绪会变成空转。
#[cfg(unix)]
const PUMP_PENDING_HIGH_WATER: usize = 64 * 1024;

/// `ESC]5522;`——OSC 5522（kitty clipboard protocol）的前缀。
const SEQUENCE_PREFIX: &[u8] = b"\x1b]5522;";

/// `ESC[?`——DECRPM 应答（`CSI ? Pa ; Ps $ y`）的固定开头。
const REPORT_OPEN: &[u8] = b"\x1b[?";

/// DECRPM 应答参数段（`Pa;Ps`）的字节上限。真实应答 ≤ 10 字节；到不了 `$y` 的超长参数
/// 是坏流，按失配冲回（与 `?1u` / `?62;c` 这些 crossterm 自己能消化的形态同路）。
const REPORT_PARAM_MAX: usize = 16;

/// `ESC[200~` / `ESC[201~`——bracketed paste 的开闭界。
const PASTE_OPEN: &[u8] = b"\x1b[200~";
const PASTE_CLOSE: &[u8] = b"\x1b[201~";

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

// ---------------------------------------------------------------------------
// 分流状态机（纯逻辑，无 I/O；时钟注入，单测不用 sleep）
// ---------------------------------------------------------------------------

/// [`Splitter::feed`] / [`Splitter::expire`] 的产物。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Feed {
    /// 原样交给 crossterm 的字节（顺序与输入一致）。
    pub passthrough: Vec<u8>,
    /// 提取出的 OSC 5522 负载（可一次多个）。
    pub sequences: Vec<Sequence>,
    /// 提取出的 DECRPM 应答参数段（`Pa;Ps`，如 `5522;2`；可一次多个）。**绝不透传**：
    /// crossterm 对 `ESC[?…y` 会永久挂在半截序列上（见模块文档）。
    pub reports: Vec<Vec<u8>>,
}

/// 超时产生的结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// 候选前缀等不到后续：原样冲回（`Esc` 键的延迟上限）。
    Flush(Vec<u8>),
    /// 序列体等不到终止符：整段丢弃（调用方记日志）。
    Drop,
    /// DECRPM 候选等不到 final：整段丢弃（**不能冲回**——`ESC[?…` 喂给 crossterm 正是
    /// 永久毒化按键解析的那条路径，见模块文档）。
    DropReport,
}

#[derive(Debug, Default)]
enum Hold {
    #[default]
    None,
    /// `ESC]5522;` 或 `ESC[200~` 的**部分前缀**（含开头的 ESC）。
    Candidate { bytes: Vec<u8>, since: Instant },
    /// 已确认 `ESC]5522;` 开头，累积到终止符为止的负载。
    ///
    /// `overflow`：体积超过 [`SEQUENCE_BODY_MAX`] 后置位——负载丢弃、`payload` 降级成
    /// 三字节滑动窗口（ST 判定看最近**两个**字节；留三字节是为了配对跨溢出点时也不丢）。
    Body {
        payload: Vec<u8>,
        since: Instant,
        overflow: bool,
    },
    /// bracketed paste 保护区内：`ESC[201~` 之前一律原样透传，尾部最多 6 字节待判。
    Paste { tail: Vec<u8> },
}

/// 候选前缀的归类。
enum Candidate {
    /// 仍是某个候选序列的前缀，继续持有。
    Partial,
    /// 完整匹配 `ESC]5522;`。
    Sequence,
    /// 完整匹配 `ESC[200~`。
    PasteOpen,
    /// 完整匹配一条 DECRPM 应答（`ESC[?` + 参数 + `$y`）。
    Report,
    /// 不是任何候选序列（含超长），立即冲回。
    No,
}

fn classify(buf: &[u8]) -> Candidate {
    fn partial(seq: &[u8], buf: &[u8]) -> bool {
        buf.len() <= seq.len() && seq[..buf.len()] == *buf
    }
    if buf.first() != Some(&ESC) {
        return Candidate::No;
    }
    if partial(SEQUENCE_PREFIX, buf) {
        return if buf.len() == SEQUENCE_PREFIX.len() {
            Candidate::Sequence
        } else {
            Candidate::Partial
        };
    }
    if partial(PASTE_OPEN, buf) {
        return if buf.len() == PASTE_OPEN.len() {
            Candidate::PasteOpen
        } else {
            Candidate::Partial
        };
    }
    if REPORT_OPEN.starts_with(buf) {
        return Candidate::Partial; // `ESC` / `ESC[` / `ESC[?` 都还可能长成应答
    }
    if buf.starts_with(REPORT_OPEN) {
        return classify_report_params(&buf[REPORT_OPEN.len()..]);
    }
    Candidate::No
}

/// `ESC[?` 之后的部分：参数段 `[0-9;]*` + intermediate `$` + final `y`。
fn classify_report_params(rest: &[u8]) -> Candidate {
    let params = rest
        .iter()
        .take_while(|b| b.is_ascii_digit() || **b == b';')
        .count();
    if params > REPORT_PARAM_MAX {
        return Candidate::No;
    }
    match &rest[params..] {
        b"" | b"$" => Candidate::Partial,
        b"$y" => Candidate::Report,
        _ => Candidate::No,
    }
}

/// 候选持有的档位（本地终端 / 远端转发）。构造时快照一次，之后不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Holds {
    /// 单 ESC 候选（`Esc` 键的形态）。
    escape: Duration,
    /// 多字节候选前缀。
    prefix: Duration,
}

impl Holds {
    /// 从环境探测档位。`env` 注入是为单测——套件跑在 SSH 会话里时，本地档用例也必须确定。
    fn detect(env: impl Fn(&str) -> Option<String>) -> Self {
        if SSH_ENV.iter().any(|key| env(key).is_some()) {
            Self {
                escape: SSH_HOLD,
                prefix: SSH_HOLD,
            }
        } else {
            Self {
                escape: ESC_HOLD,
                prefix: PREFIX_HOLD,
            }
        }
    }

    /// 该长度的候选用的持有上限（与 [`candidate_kind`] 同一判据）。
    fn hold_for(&self, candidate_len: usize) -> Duration {
        if candidate_len == 1 {
            self.escape
        } else {
            self.prefix
        }
    }
}

/// 候选的形状名（排障日志用）：单 ESC 与多字节前缀是两类现场。
fn candidate_kind(candidate_len: usize) -> &'static str {
    if candidate_len == 1 {
        "lone-esc"
    } else {
        "prefix"
    }
}

impl Default for Holds {
    fn default() -> Self {
        Self::detect(|key| std::env::var(key).ok())
    }
}

/// 字节流分流器。**不丢用户数据**是唯一红线：除「已确认的 5522 序列」与「超限 / 等不到
/// 终止符的 5522 序列体」外，所有字节按原顺序从 [`Feed::passthrough`] 出来。
#[derive(Debug, Default)]
pub(crate) struct Splitter {
    hold: Hold,
    holds: Holds,
}

impl Splitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一次 read 的字节。
    ///
    /// 任何候选（包括孤立的 `ESC`）都不在这里冲回：`read` 返回不代表写方没有后续字节
    /// （pty 的 `write(2)` 允许短写、SSH 转发按传输窗口分片），把半截候选当用户数据冲回，
    /// 整条 5522 包就会漏给 crossterm。冲回统一交给 [`Splitter::expire`] 的持有超时。
    pub fn feed(&mut self, chunk: &[u8], now: Instant) -> Feed {
        let mut out = Feed::default();
        let mut i = 0;
        while i < chunk.len() {
            match &mut self.hold {
                // 地面：只有 ESC 值得停下来看。
                Hold::None => {
                    if chunk[i] == ESC {
                        self.hold = Hold::Candidate {
                            bytes: vec![ESC],
                            since: now,
                        };
                    } else {
                        out.passthrough.push(chunk[i]);
                    }
                    i += 1;
                }
                Hold::Candidate { bytes, since } => {
                    bytes.push(chunk[i]);
                    i += 1;
                    *since = now;
                    match classify(bytes) {
                        Candidate::Partial => {}
                        Candidate::Sequence => {
                            self.hold = Hold::Body {
                                payload: Vec::new(),
                                since: now,
                                overflow: false,
                            };
                        }
                        Candidate::PasteOpen => {
                            // 开界本身是 crossterm 要看的控制序列：原样放行。
                            out.passthrough.append(bytes);
                            self.hold = Hold::Paste { tail: Vec::new() };
                        }
                        Candidate::Report => {
                            // 完整应答：摘走参数段（`ESC[?` 与 `$y` 之间）。
                            out.reports
                                .push(bytes[REPORT_OPEN.len()..bytes.len() - 2].to_vec());
                            self.hold = Hold::None;
                        }
                        Candidate::No => {
                            // 失配：持有的字节全部原样冲回（`Alt+]` 之类语义一字不差）。
                            tracing::debug!(
                                "stdin proxy: {} candidate ({} bytes) mismatch, waving through",
                                candidate_kind(bytes.len()),
                                bytes.len()
                            );
                            out.passthrough.append(bytes);
                            self.hold = Hold::None;
                        }
                    }
                }
                // 序列体：累积到 BEL 或 ST。
                Hold::Body {
                    payload,
                    since,
                    overflow,
                } => {
                    let b = chunk[i];
                    i += 1;
                    if b == BEL {
                        if *overflow {
                            payload.clear(); // 超限包已 warn，这里只把槽位还给下一个候选
                        } else {
                            out.sequences.push(std::mem::take(payload));
                        }
                        self.hold = Hold::None;
                        continue;
                    }
                    *since = now;
                    if *overflow {
                        // 降级窗口：只保留最近 3 字节（最近两字节对总在其中，见 `Hold::Body`）。
                        payload.push(b);
                        let excess = payload.len().saturating_sub(3);
                        if excess > 0 {
                            payload.drain(..excess);
                        }
                    } else {
                        payload.push(b);
                    }
                    // `ends_with` 天然覆盖 ST 被 read 边界切开的情况（base64 负载里不含 ESC）。
                    if payload.ends_with(b"\x1b\\") {
                        if *overflow {
                            payload.clear();
                        } else {
                            payload.truncate(payload.len() - 2);
                            out.sequences.push(std::mem::take(payload));
                        }
                        self.hold = Hold::None;
                    } else if !*overflow && payload.len() > SEQUENCE_BODY_MAX {
                        // 坏流：丢负载，转成「吞到终止符」的降级窗口（见 SEQUENCE_BODY_MAX）；
                        // 转档时也保留最近 3 字节，`ESC\` 配对跨溢出点时不漏检。
                        tracing::warn!(
                            "stdin proxy: OSC 5522 body over {SEQUENCE_BODY_MAX} bytes, dropping it"
                        );
                        *overflow = true;
                        let keep = payload.len().saturating_sub(3);
                        payload.drain(..keep);
                    }
                }
                // 保护区：粘贴内容里的字面 `ESC]5522;` 属于用户数据，必须保全。
                // 只扣「可能是终止符开头」的那几个字节（`ESC` / `ESC[` / … / `ESC[201`），
                // 其余立即放行——坏了形状的粘贴也不能把字节无限期扣住。
                Hold::Paste { tail } => {
                    tail.push(chunk[i]);
                    i += 1;
                    loop {
                        if *tail == PASTE_CLOSE {
                            out.passthrough.append(tail);
                            self.hold = Hold::None;
                            break;
                        }
                        if PASTE_CLOSE.starts_with(tail) {
                            break; // 仍是终止符前缀：继续等
                        }
                        if tail.is_empty() {
                            break;
                        }
                        out.passthrough.push(tail.remove(0));
                    }
                }
            }
        }
        out
    }

    /// 推进超时。调用方在 [`Splitter::deadline`] 到点（或更晚）时调用。
    pub fn expire(&mut self, now: Instant) -> Option<Expiry> {
        match &self.hold {
            Hold::Candidate { bytes, since }
                if now.duration_since(*since) >= self.holds.hold_for(bytes.len()) =>
            {
                let bytes = bytes.clone();
                self.hold = Hold::None;
                if bytes.starts_with(REPORT_OPEN) {
                    // 已在 `ESC[?` 的形态里：冲回就是毒化 crossterm 的那条路径，只能丢。
                    return Some(Expiry::DropReport);
                }
                tracing::debug!(
                    "stdin proxy: {} candidate hold expired ({} bytes), waving through",
                    candidate_kind(bytes.len()),
                    bytes.len()
                );
                Some(Expiry::Flush(bytes))
            }
            Hold::Body { since, .. } if now.duration_since(*since) >= SEQUENCE_IDLE => {
                self.hold = Hold::None;
                Some(Expiry::Drop)
            }
            _ => None,
        }
    }

    /// 下一次需要 [`Splitter::expire`] 的时刻（无持有 = 无需唤醒）。
    pub fn deadline(&self) -> Option<Instant> {
        match &self.hold {
            Hold::Candidate { bytes, since } => Some(*since + self.holds.hold_for(bytes.len())),
            Hold::Body { since, .. } => Some(*since + SEQUENCE_IDLE),
            Hold::None | Hold::Paste { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 探针判定（纯逻辑）
//
// 非 Unix 上 fd 接驳整体不存在（`imp` 的 cfg），这几个判定只有单测用得到——
// 按仓库约定对「声明的平台」放行 dead_code，而不是让它们只在 Unix 编译（与
// `util/clipboard.rs` 的候选表同一约定）。
// ---------------------------------------------------------------------------

/// 粘贴通道的启用判定（[`settle_paste_channel`] 的产物；也是协议机的启动门控）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteChannel {
    /// 探针阳性：接驳保留、`?5522h` 该发、协议机该起。
    Supported,
    /// 阴性（明确 `0/4`、本地静默或 SSH 静默）：不开 mode、不起协议机。
    /// 接驳是否还在走 [`is_installed`] 单独看——SSH 静默时它保留（吸收迟到应答）。
    Unsupported,
}

/// DECRPM 应答的语义：`Ps ∈ {1,2,3}`（已设 / 未设 / 永久设）才是真支持；`{0,4}` 与
/// 未应答（含非法 `Ps`）都不启用。
#[allow(dead_code)]
pub(crate) fn probe_verdict(ps: Option<u8>) -> PasteChannel {
    match ps {
        Some(1..=3) => PasteChannel::Supported,
        _ => PasteChannel::Unsupported,
    }
}

/// 判定阴性后是否就地拆接驳：阳性不拆；明确阴性（有任何应答，含非法 `Ps`）无论档位都拆；
/// 静默只拆本地——SSH 静默保留是为了兜住迟到应答（拆掉后它会直接落进 crossterm，
/// 永久毒化按键解析）。
#[allow(dead_code)]
pub(crate) fn should_dismantle(ps: Option<u8>, remote: bool) -> bool {
    match probe_verdict(ps) {
        PasteChannel::Supported => false,
        PasteChannel::Unsupported => ps.is_some() || !remote,
    }
}

/// 解析 DECRPM 应答的参数段（`Pa;Ps`，如 `5522;2`）；形状不对（缺 `Ps` / 溢出 / 多段）
/// 返回 `None`——分流层只负责摘取，语义在这里。
#[allow(dead_code)]
pub(crate) fn parse_decrmp(params: &[u8]) -> Option<(u32, u8)> {
    let text = std::str::from_utf8(params).ok()?;
    let (mode, ps) = text.split_once(';')?;
    Some((mode.parse().ok()?, ps.parse().ok()?))
}

// ---------------------------------------------------------------------------
// fd 接驳（Unix）
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io;
    use std::io::Write;
    use std::os::fd::RawFd;
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use tokio::sync::mpsc;

    struct Proxy {
        /// 真实 tty 的副本：`restore` 用它还原 fd 0。泵线程另有一份自己的 dup。
        restore_fd: RawFd,
        stop_w: RawFd,
        /// `dup(master)` 的唤醒副本：泵线程收工时关掉 master，要打断 crossterm 阻塞读的
        /// 收尾方必须自己留一份（向它写 1 字节，字节走 master→slave 的内核路径）。`-1` = 降级。
        wake_fd: RawFd,
        join: Option<JoinHandle<()>>,
    }

    /// 启动探针的进行态：`install` 登记（查询发出后开始计时），泵线程记录应答。
    struct Probe {
        /// 判定截止（`查询时刻 + 预算`）；应答先到就提前结束等待。
        deadline: Instant,
        /// 远端档（静默时决定拆 / 留，也决定预算）。
        remote: bool,
        /// 5522 的 DECRPM 应答（`Ps`）。
        ps: Option<u8>,
    }

    #[derive(Default)]
    struct Shared {
        proxy: Option<Proxy>,
        seq_rx: Option<mpsc::Receiver<Sequence>>,
        probe: Option<Probe>,
        /// 判定缓存（`settle_paste_channel` 幂等；`paste_channel` 读这里）。
        settled: Option<PasteChannel>,
    }

    /// 全局句柄（安装 / 卸载各只有一次；`is_installed` 每次进序列读一次）。
    static SHARED: Mutex<Shared> = Mutex::new(Shared {
        proxy: None,
        seq_rx: None,
        probe: None,
        settled: None,
    });

    /// 锁 poisoned 也继续（panic hook 会在别的线程 panic 之后走到这里）。
    fn lock() -> std::sync::MutexGuard<'static, Shared> {
        SHARED.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn io_err() -> io::Error {
        io::Error::last_os_error()
    }

    fn close(fd: RawFd) {
        unsafe { libc::close(fd) };
    }

    /// 按 crossterm 的同一判据取「它本会读的那个 fd」：stdin 是 tty 就 dup 0，否则开
    /// `/dev/tty`（读写都试，退回只读）。
    fn open_source_fd() -> io::Result<RawFd> {
        if unsafe { libc::isatty(0) } == 1 {
            let fd = unsafe { libc::dup(0) };
            return if fd < 0 { Err(io_err()) } else { Ok(fd) };
        }
        let path = c"/dev/tty";
        let mut fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if fd < 0 {
            fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NOCTTY) };
        }
        if fd < 0 { Err(io_err()) } else { Ok(fd) }
    }

    fn open_pty() -> io::Result<(RawFd, RawFd)> {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(io_err());
        }
        Ok((master, slave))
    }

    /// 把新 pty 的 slave 打成 raw：注入字节不经过行规程（不被 ICANON 扣住、不回显），
    /// crossterm 的读取语义与真实 tty 一致。
    fn make_raw(fd: RawFd) -> io::Result<()> {
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut term) != 0 {
                return Err(io_err());
            }
            libc::cfmakeraw(&mut term);
            if libc::tcsetattr(fd, libc::TCSANOW, &term) != 0 {
                return Err(io_err());
            }
        }
        Ok(())
    }

    /// `install` 走到一半失败的兜底：把 fd 0 还回原 tty 并关掉已开的 fd（`-1` 忽略）。
    fn abort(restore_fd: RawFd, master: RawFd, stop_r: RawFd, stop_w: RawFd, wake_fd: RawFd) {
        unsafe { libc::dup2(restore_fd, 0) };
        for fd in [restore_fd, master, stop_r, stop_w, wake_fd] {
            if fd >= 0 {
                close(fd);
            }
        }
    }

    /// 泵线程主体：读真实 tty → 分流 → 透传字节写进 pty master、序列投给协议机。
    fn pump(src: RawFd, master: RawFd, stop_r: RawFd, tx: mpsc::Sender<Sequence>) {
        let mut splitter = Splitter::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];

        // master 非阻塞：pty 缓冲满时写不完的部分留在 `pending`（背压停读源）。
        unsafe {
            let flags = libc::fcntl(master, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        loop {
            let now = Instant::now();
            let timeout_ms = match splitter.deadline() {
                Some(deadline) if deadline > now => {
                    (deadline - now).as_millis().min(1000) as libc::c_int
                }
                Some(_) => 0,
                None => -1,
            };
            let mut fds = [
                libc::pollfd {
                    fd: src,
                    // 内层 pty 积压到上限就先不读源（背压，见 PUMP_PENDING_HIGH_WATER）。
                    events: if pending.len() < PUMP_PENDING_HIGH_WATER {
                        libc::POLLIN
                    } else {
                        0
                    },
                    revents: 0,
                },
                libc::pollfd {
                    fd: master,
                    // 只在有待写字节时才等可写：master 几乎总是可写，常开就是空转。
                    events: if pending.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop_r,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 3, timeout_ms) };
            if rc < 0 {
                if io_err().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::warn!("stdin proxy: poll failed: {}", io_err());
                break;
            }
            if fds[2].revents != 0 {
                break; // 收工
            }

            // 超时（或这一轮凑巧有别的就绪）都推进一次持有超时：代价可忽略，杜绝
            // 「只有 master 可写」这种 rc≥1 的轮次把到期的持有拖住。
            let mut feed = Feed::default();
            match splitter.expire(Instant::now()) {
                Some(Expiry::Flush(bytes)) => feed.passthrough.extend_from_slice(&bytes),
                Some(Expiry::Drop) => {
                    tracing::warn!("stdin proxy: dropping unterminated OSC 5522 sequence")
                }
                Some(Expiry::DropReport) => {
                    tracing::warn!("stdin proxy: dropping unterminated DECRPM reply candidate")
                }
                None => {}
            }

            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let n =
                    unsafe { libc::read(src, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n > 0 {
                    let n = n as usize;
                    let part = splitter.feed(&buf[..n], Instant::now());
                    feed.passthrough.extend_from_slice(&part.passthrough);
                    feed.sequences.extend(part.sequences);
                    for report in &part.reports {
                        record_report(report);
                    }
                } else if n == 0 {
                    tracing::warn!("stdin proxy: input stream closed");
                    break;
                } else {
                    match io_err().raw_os_error() {
                        Some(libc::EAGAIN) | Some(libc::EINTR) => {}
                        Some(libc::EIO) => break, // tty 挂断
                        _ => {
                            tracing::warn!("stdin proxy: read failed: {}", io_err());
                            break;
                        }
                    }
                }
            }

            pending.extend_from_slice(&feed.passthrough);
            for seq in feed.sequences {
                match tx.try_send(seq) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!("stdin proxy: clipboard sequence dropped (channel full)")
                    }
                    // 接收端没了（进程收尾）：静默。
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }

            if !pending.is_empty() && fds[1].revents & libc::POLLOUT != 0 {
                let n = unsafe {
                    libc::write(
                        master,
                        pending.as_ptr() as *const libc::c_void,
                        pending.len(),
                    )
                };
                if n > 0 {
                    pending.drain(..n as usize);
                } else if n < 0
                    && !matches!(
                        io_err().raw_os_error(),
                        Some(libc::EAGAIN) | Some(libc::EINTR)
                    )
                {
                    tracing::warn!("stdin proxy: write failed: {}", io_err());
                    break;
                }
            }
        }

        // 线程自己的 fd 自己收：真实 tty 由 restore 的副本还原 fd 0。
        close(stop_r);
        close(master);
        close(src);
    }

    /// 本进程是否在远端转发里（SSH / mosh）——决定探针预算与静默时的拆 / 留。
    fn is_remote() -> bool {
        SSH_ENV.iter().any(|key| std::env::var(key).is_ok())
    }

    /// 泵线程每次读到一条 DECRPM 应答时登记（只认 5522 的、只认第一条）。
    fn record_report(params: &[u8]) {
        let Some((mode, ps)) = parse_decrmp(params) else {
            tracing::debug!("stdin proxy: swallowed a malformed DECRPM-shaped reply");
            return;
        };
        if mode != PASTE_EVENTS_MODE {
            tracing::debug!("stdin proxy: DECRPM reply for mode {mode} (status {ps})");
            return;
        }
        let mut shared = lock();
        match shared.probe.as_mut() {
            Some(probe) if probe.ps.is_none() => probe.ps = Some(ps),
            // 判定已收束 / 重复应答：没有新信息。
            _ => {}
        }
    }

    /// 写出探针查询。失败只是没有应答（判定走阴性），不影响接驳本身。
    fn write_probe_query() {
        let mut out = io::stdout();
        if let Err(e) = out.write_all(PROBE_QUERY).and_then(|()| out.flush()) {
            tracing::warn!("stdin proxy: cannot send the 5522 DECRQM probe: {e}");
        }
    }

    pub fn install() -> io::Result<()> {
        let mut shared = lock();
        if shared.proxy.is_some() {
            // 幂等（第二段 TUI）：查询不重发，探针状态原样保留。
            return Ok(());
        }

        let src = open_source_fd()?;
        let (master, slave) = match open_pty() {
            Ok(pair) => pair,
            Err(e) => {
                close(src);
                return Err(e);
            }
        };
        if let Err(e) = make_raw(slave) {
            close(slave);
            close(master);
            close(src);
            return Err(e);
        }
        // 唤醒副本：泵线程收工时关掉 master，`wake_reader` 必须自己留一份（失败降级为 -1）。
        let wake_fd = unsafe { libc::dup(master) };
        if wake_fd < 0 {
            tracing::warn!(
                "stdin proxy: cannot duplicate master for reader wake-up: {}",
                io_err()
            );
        }
        if unsafe { libc::dup2(slave, 0) } < 0 {
            let e = io_err();
            close(slave);
            close(master);
            close(src);
            if wake_fd >= 0 {
                close(wake_fd);
            }
            return Err(e);
        }
        close(slave);

        // 从这一刻起 fd 0 已经是 pty slave，任何失败都必须在返回前还原。
        let restore_fd = unsafe { libc::dup(src) };
        if restore_fd < 0 {
            // 连 dup 都失败（EMFILE 之类）：用 src 本身还回去（泵线程还没起，没人用它）。
            let e = io_err();
            unsafe { libc::dup2(src, 0) };
            close(master);
            close(src);
            if wake_fd >= 0 {
                close(wake_fd);
            }
            return Err(e);
        }
        let mut pipe_fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            let e = io_err();
            abort(restore_fd, master, -1, -1, wake_fd);
            close(src);
            return Err(e);
        }
        let (stop_r, stop_w) = (pipe_fds[0], pipe_fds[1]);

        let (tx, rx) = mpsc::channel(16);
        let remote = is_remote();
        let spawned = std::thread::Builder::new()
            .name("wing-stdin-proxy".into())
            .spawn(move || pump(src, master, stop_r, tx));
        match spawned {
            Ok(join) => {
                shared.proxy = Some(Proxy {
                    restore_fd,
                    stop_w,
                    wake_fd,
                    join: Some(join),
                });
                shared.seq_rx = Some(rx);
                // 探针状态先于查询置好（应答可能快到与查询同拍）；重装 = 重新探测。
                shared.settled = None;
                shared.probe = Some(Probe {
                    deadline: Instant::now() + probe_budget(remote),
                    remote,
                    ps: None,
                });
                drop(shared);
                write_probe_query();
                Ok(())
            }
            Err(e) => {
                abort(restore_fd, master, stop_r, stop_w, wake_fd);
                close(src);
                Err(e)
            }
        }
    }

    /// 探针预算：远端档长（RTT + 重排余量），本地短。
    fn probe_budget(remote: bool) -> Duration {
        if remote {
            PROBE_BUDGET_SSH
        } else {
            PROBE_BUDGET_LOCAL
        }
    }

    /// 等应答（到预算截止或第一条 5522 应答到达为止）；没有接驳 / 没发过查询立即返回。
    fn wait_for_probe_reply() {
        loop {
            let deadline = {
                let shared = lock();
                match shared.probe.as_ref() {
                    Some(probe) if probe.ps.is_none() => probe.deadline,
                    _ => return,
                }
            };
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            std::thread::sleep((deadline - now).min(PROBE_POLL_STEP));
        }
    }

    /// 首 poll 前的判定点（幂等）：收束探针结果，阴性就地拆接驳。
    ///
    /// - 阳性（`Ps ∈ {1,2,3}`）：保留接驳，协议机由 `tui::spawn_event_stream` 启动；
    /// - 明确阴性（`0/4`）：任何档位都拆——终端自己说了不支持，不会再有 5522 流量；
    /// - 静默：本地拆（不实现 DECRQM 的终端不会迟到应答）；SSH 保留（迟到应答必须仍被
    ///   分流层摘走，见模块文档）。
    pub fn settle_paste_channel() -> PasteChannel {
        if let Some(verdict) = lock().settled {
            return verdict;
        }
        wait_for_probe_reply();
        let (ps, remote, installed) = {
            let shared = lock();
            let probe = shared.probe.as_ref();
            (
                probe.and_then(|probe| probe.ps),
                probe.is_some_and(|probe| probe.remote),
                shared.proxy.is_some(),
            )
        };
        let verdict = probe_verdict(ps);
        if installed && should_dismantle(ps, remote) {
            tracing::debug!("stdin proxy: 5522 unsupported, dropping the stdin splice");
            restore();
        }
        lock().settled = Some(verdict);
        verdict
    }

    /// 判定结果的读口（未判定 = [`PasteChannel::Unsupported`]）。
    pub fn paste_channel() -> PasteChannel {
        lock().settled.unwrap_or(PasteChannel::Unsupported)
    }

    /// 向「读 fd 0」的一方投递 1 字节唤醒（收尾时打断 crossterm 的阻塞读）。
    ///
    /// 字节走 master→slave 的内核路径，泵线程不参与；**必须在 [`restore`] 之前调用**
    /// （那时 fd 0 还是内层 slave、master 还没关）。解析器扣着半截转义序列时，这个字节
    /// 可能被当成序列的 final 吃掉（不产事件）——收尾方按需重复投递，别只投一次。
    /// 没装上接驳 / 唤醒副本创建失败时是空操作——调用方另有有界等待兜底。
    pub fn wake_reader() {
        let shared = lock();
        let Some(proxy) = shared.proxy.as_ref() else {
            return;
        };
        if proxy.wake_fd >= 0 {
            unsafe {
                libc::write(proxy.wake_fd, b"x".as_ptr() as *const libc::c_void, 1);
            }
        }
    }

    pub fn restore() {
        let proxy = {
            let mut shared = lock();
            // seq_rx 一并清掉：拆接驳后 `take_sequences()` 不许再给出一个残端
            // （阴性判定后协议机不许起）。
            shared.seq_rx = None;
            shared.proxy.take()
        };
        let Some(mut proxy) = proxy else { return };
        // 唤醒泵线程（写 stop pipe），等它自己退出——没有孤儿读者去抢 shell 的输入。
        unsafe { libc::write(proxy.stop_w, b"x".as_ptr() as *const libc::c_void, 1) };
        if let Some(join) = proxy.join.take() {
            let _ = join.join();
        }
        // wake_fd 是 master 的最后一个副本，必须在 `dup2` **之前**送掉：macOS 上「对端还
        // 活着的 pty slave 上有人卡在 read」会让 `dup2`（关闭旧 fd 0）阻塞；最后一个 master
        // 副本一关，卡住的 read 立刻返回，`dup2` 只花微秒级（超时后「继续收尾」才真的走得动）。
        if proxy.wake_fd >= 0 {
            close(proxy.wake_fd);
        }
        unsafe {
            libc::dup2(proxy.restore_fd, 0);
            close(proxy.restore_fd);
            close(proxy.stop_w);
        }
    }

    pub fn is_installed() -> bool {
        lock().proxy.is_some()
    }

    pub fn take_sequences() -> Option<mpsc::Receiver<Sequence>> {
        lock().seq_rx.take()
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;
    use std::io;

    /// 非 Unix：没有 fd 接驳（该平台本就不承诺 5522 通道），永远「未安装」。
    pub fn install() -> io::Result<()> {
        Ok(())
    }

    pub fn restore() {}

    /// 非 Unix：没有接驳层（不会有人读内层 pty），空操作。
    pub fn wake_reader() {}

    pub fn is_installed() -> bool {
        false
    }

    pub fn take_sequences() -> Option<tokio::sync::mpsc::Receiver<Sequence>> {
        None
    }

    /// 非 Unix：没有接驳（也不发探针查询）——通道永不启用。
    pub fn settle_paste_channel() -> PasteChannel {
        PasteChannel::Unsupported
    }

    /// 非 Unix：同 [`settle_paste_channel`]。
    pub fn paste_channel() -> PasteChannel {
        PasteChannel::Unsupported
    }
}

pub use imp::install;
pub use imp::is_installed;
pub use imp::paste_channel;
pub use imp::restore;
pub use imp::settle_paste_channel;
pub use imp::take_sequences;
pub use imp::wake_reader;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        // 相对基准的确定时刻：单测不依赖 wall clock。
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    /// 一次 read 的字节：单测不区分「队列已空」（`Splitter` 也不再据此做零延迟优化）。
    fn feed(s: &mut Splitter, chunk: &[u8], ms: u64) -> Feed {
        s.feed(chunk, at(ms))
    }

    /// 本地档：不读真实环境——套件跑在 SSH 会话里时用例也必须确定。
    fn local() -> Splitter {
        Splitter {
            hold: Hold::None,
            holds: Holds {
                escape: ESC_HOLD,
                prefix: PREFIX_HOLD,
            },
        }
    }

    /// 远端档（注入 `SSH_CONNECTION`）。
    fn remote() -> Splitter {
        Splitter {
            hold: Hold::None,
            holds: Holds::detect(|key| (key == "SSH_CONNECTION").then(String::new)),
        }
    }

    /// 让 `tracing::debug!` 的参数真正求值：没有订阅者时宏连参数都不碰（callsite 的
    /// interest 缓存先被注册成 never），日志行因此永远「未执行」。thread-local 订阅者
    /// + 显式重建 interest 缓存（与 ACP 侧的日志测试同一套路；缓存不会自愈）。
    fn debug_logs() -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::level_filters::LevelFilter::DEBUG)
            .with_writer(std::io::sink)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        guard
    }

    fn seq(inner: &str) -> Vec<u8> {
        let mut out = SEQUENCE_PREFIX.to_vec();
        out.extend_from_slice(inner.as_bytes());
        out.extend_from_slice(b"\x1b\\");
        out
    }

    #[test]
    fn plain_bytes_pass_through_byte_for_byte() {
        let mut s = local();
        let out = feed(&mut s, b"hello \xe4\xb8\x96\xe7\x95\x8c", 0);
        assert_eq!(out.passthrough, "hello 世界".as_bytes());
        assert!(out.sequences.is_empty());
        // 分块喂同样一致。
        let mut s = local();
        let mut acc = Vec::new();
        for chunk in [&b"he"[..], &b"ll"[..], &b"o"[..]] {
            acc.extend(feed(&mut s, chunk, 0).passthrough);
        }
        assert_eq!(acc, b"hello");
    }

    #[test]
    fn complete_sequence_is_extracted_not_passed_through() {
        let mut s = local();
        let out = feed(&mut s, &seq("type=read:status=OK:pw=b3Rw"), 0);
        assert_eq!(out.passthrough, b"");
        assert_eq!(out.sequences, vec![b"type=read:status=OK:pw=b3Rw".to_vec()]);
    }

    #[test]
    fn bel_terminated_sequence_is_extracted_too() {
        let mut s = local();
        let mut raw = SEQUENCE_PREFIX.to_vec();
        raw.extend_from_slice(b"type=read:status=DONE:pw=b3Rw");
        raw.push(BEL);
        let out = feed(&mut s, &raw, 0);
        assert_eq!(out.passthrough, b"");
        assert_eq!(
            out.sequences,
            vec![b"type=read:status=DONE:pw=b3Rw".to_vec()]
        );
    }

    #[test]
    fn sequence_split_at_every_boundary_is_reassembled() {
        let raw = {
            let mut v = SEQUENCE_PREFIX.to_vec();
            v.extend_from_slice(b"type=read:status=DATA:mime=Lg==;dGV4dA==");
            v.extend_from_slice(b"\x1b\\");
            v
        };
        let expected = b"type=read:status=DATA:mime=Lg==;dGV4dA==".to_vec();
        for cut in 1..raw.len() {
            let mut s = local();
            // 两段各按一次 read 喂：切点落在任何位置（`ESC` / `ESC]` / … / 体内部）时，
            // 第一段留下的**任何**候选都要扣住等后续字节——包括 `cut == 1` 的孤立
            // `ESC`（它可能只是 `Esc` 键，但拆分窗口内不许提前冲回）。
            let a = s.feed(&raw[..cut], at(0));
            let b = s.feed(&raw[cut..], at(1));
            let mut seqs = a.sequences;
            seqs.extend(b.sequences);
            assert_eq!(seqs, vec![expected.clone()], "cut at {cut}");
            assert_eq!(a.passthrough.len() + b.passthrough.len(), 0, "cut at {cut}");
        }
    }

    // ── DECRQM 探针应答（`ESC[? Pa ; Ps $ y`）───────────────────────────

    /// 一条完整应答的原始字节。
    fn report(params: &str) -> Vec<u8> {
        let mut out = REPORT_OPEN.to_vec();
        out.extend_from_slice(params.as_bytes());
        out.extend_from_slice(b"$y");
        out
    }

    #[test]
    fn decrpm_reply_is_extracted_for_every_status() {
        for ps in [0u8, 1, 2, 3, 4] {
            let params = format!("5522;{ps}");
            let mut s = local();
            let out = feed(&mut s, &report(&params), 0);
            assert_eq!(out.passthrough, b"", "ps={ps}: 应答一个字节都不许透传");
            assert_eq!(out.reports, vec![params.into_bytes()], "ps={ps}");
            assert_eq!(s.deadline(), None, "ps={ps}: 完整应答不留持有");
            // 应答吸收后分流恢复：后续字节照常。
            assert_eq!(feed(&mut s, b"x", 1).passthrough, b"x", "ps={ps}");
        }
    }

    /// 任意 DECRPM 应答都吸收（不只 5522）：crossterm 对 `ESC[?…y` 一律挂死，
    /// 「不是 5522 就放行」等于给别的探测应答留一条毒化路径。
    #[test]
    fn decrpm_reply_for_another_mode_is_absorbed_too() {
        let mut s = local();
        let out = feed(&mut s, &report("2026;2"), 0);
        assert_eq!(out.passthrough, b"");
        assert_eq!(out.reports, vec![b"2026;2".to_vec()]);
    }

    /// 分片重组：切点落在任何位置都不能把半截 `ESC[?…` 漏给 crossterm。
    #[test]
    fn decrpm_reply_split_at_every_boundary_is_reassembled() {
        let raw = report("5522;2");
        let expected = b"5522;2".to_vec();
        for cut in 1..raw.len() {
            let mut s = local();
            let a = s.feed(&raw[..cut], at(0));
            let b = s.feed(&raw[cut..], at(1));
            let mut reports = a.reports;
            reports.extend(b.reports);
            assert_eq!(reports, vec![expected.clone()], "cut at {cut}");
            assert_eq!(a.passthrough.len() + b.passthrough.len(), 0, "cut at {cut}");
        }
    }

    /// 形似而**不是** DECRPM 的 `ESC[?…`（键盘协议应答 / DA 应答 / 缺 `$y`）：原样放行，
    /// crossterm 自己能消化（`?…u` / `?…c`）或者本就与 5522 无关。
    #[test]
    fn lookalike_esc_bracket_question_sequences_are_waved_through() {
        for raw in [
            &b"\x1b[?1u"[..],
            b"\x1b[?62;c",
            b"\x1b[?5522;2$z",
            b"\x1b[?25;1R",
        ] {
            let mut s = local();
            let out = feed(&mut s, raw, 0);
            assert_eq!(out.passthrough, raw, "{raw:?} 必须原样放行");
            assert!(out.reports.is_empty(), "{raw:?}");
            assert_eq!(s.deadline(), None, "{raw:?}");
        }
    }

    /// 到点仍不完整：**丢弃**，绝不冲回——`ESC[?…` 喂给 crossterm 正是永久毒化
    /// 按键解析的那条路径（见模块文档）。
    #[test]
    fn decrpm_candidate_stall_is_dropped_not_flushed() {
        let mut s = local();
        assert_eq!(feed(&mut s, b"\x1b[?5522;2$", 0).passthrough, b"");
        assert_eq!(s.deadline(), Some(at(0) + PREFIX_HOLD));
        assert_eq!(s.expire(at(30)), Some(Expiry::DropReport), "冲回就是毒化");
        assert_eq!(s.deadline(), None);
        // 持有清空后分流恢复：迟到的尾巴只是一个普通字节（前缀已被吞掉，不会毒化）。
        assert_eq!(feed(&mut s, b"y", 31).passthrough, b"y");
    }

    /// 超长参数段（坏包）：到不了任何真协议的形状（真应答 ≤ 10 字节），按失配冲回——
    /// 不会无限持有、不会 panic。
    #[test]
    fn overlong_decrpm_params_are_waved_through_not_held() {
        let mut raw = REPORT_OPEN.to_vec();
        raw.extend(std::iter::repeat_n(b'9', REPORT_PARAM_MAX + 1));
        let mut s = local();
        let out = feed(&mut s, &raw, 0);
        assert!(out.reports.is_empty());
        assert_eq!(out.passthrough, raw, "参数超限 = 失配：前面的字节原样冲回");
        assert_eq!(s.deadline(), None);
    }

    /// 探针判定：只有 `Ps ∈ {1,2,3}` 是「真支持」；未应答 / `0/4` / 非法值都不启用。
    #[test]
    fn probe_verdict_only_enables_on_a_supporting_status() {
        for ps in [1u8, 2, 3] {
            assert_eq!(probe_verdict(Some(ps)), PasteChannel::Supported, "Ps={ps}");
        }
        for ps in [0u8, 4, 5, 255] {
            assert_eq!(
                probe_verdict(Some(ps)),
                PasteChannel::Unsupported,
                "Ps={ps}"
            );
        }
        assert_eq!(probe_verdict(None), PasteChannel::Unsupported);
    }

    /// 拆除条件：明确阴性（有任何应答）都拆；静默只拆本地——SSH 静默保留接驳，
    /// 迟到应答必须仍被分流层吸收（拆掉它会落进 crossterm）。
    #[test]
    fn dismantle_rule_keeps_the_splice_for_a_silent_remote_only() {
        assert!(should_dismantle(None, false), "本地静默：拆");
        assert!(!should_dismantle(None, true), "SSH 静默：留");
        for ps in [0u8, 4] {
            assert!(should_dismantle(Some(ps), false), "Ps={ps}");
            assert!(
                should_dismantle(Some(ps), true),
                "Ps={ps} 明确阴性在远端也拆"
            );
        }
        assert!(!should_dismantle(Some(2), false));
        assert!(!should_dismantle(Some(2), true));
    }

    #[test]
    fn decrmp_params_parse_to_mode_and_status() {
        assert_eq!(parse_decrmp(b"5522;2"), Some((5522, 2)));
        assert_eq!(parse_decrmp(b"2026;0"), Some((2026, 0)));
        assert_eq!(parse_decrmp(b"65535;4"), Some((65535, 4)));
        // 缺 Ps / 多段 / 溢出 / 非数字都是坏包。
        assert_eq!(parse_decrmp(b"5522"), None);
        assert_eq!(parse_decrmp(b"5522;2;3"), None);
        assert_eq!(parse_decrmp(b"99999999999;2"), None);
        assert_eq!(parse_decrmp(b"5522;300"), None);
        assert_eq!(parse_decrmp(b""), None);
    }

    /// 短读 + 切点落在包前缀内：`read` 返回短于缓冲只证明「此刻内核队列空」，不证明写方
    /// 没有后续字节。把半截 `ESC]5522;` 前缀当用户数据冲回，整条 5522 包会漏给
    /// crossterm（`Alt+]` + 杂字符灌进草稿），协议机丢包、以超时 toast 收场（归因错误）。
    #[test]
    fn a_short_read_inside_the_sequence_prefix_does_not_leak_to_crossterm() {
        let _logs = debug_logs();
        let mut s = local();
        // 第一次 read：切在 `ESC]`（短读）。
        assert_eq!(feed(&mut s, b"\x1b]", 0).passthrough, b"", "候选不能冲回");
        assert_eq!(
            s.deadline(),
            Some(at(0) + PREFIX_HOLD),
            "扣住的字节要有兜底冲回的时刻（不是无限期）"
        );
        // 后续字节到达：整条包作为**一条序列**提取，零透传。
        let out = feed(&mut s, b"5522;type=read:status=OK:pw=b3Rw\x1b\\", 1);
        assert_eq!(out.passthrough, b"");
        assert_eq!(out.sequences, vec![b"type=read:status=OK:pw=b3Rw".to_vec()]);

        // 反例（切点落在一个不是任何 5522 形态的候选上）：到点仍原样冲回。
        let mut s = local();
        assert_eq!(feed(&mut s, b"\x1b]", 0).passthrough, b"");
        assert_eq!(
            s.expire(at(30)),
            Some(Expiry::Flush(b"\x1b]".to_vec())),
            "25 ms 没等到后续：原样透传（Alt+] 不被吞）"
        );
    }

    #[test]
    fn st_terminator_split_across_reads() {
        let mut s = local();
        let raw = seq("x");
        let (head, tail) = raw.split_at(raw.len() - 1); // 尾部只剩 '\'
        assert!(feed(&mut s, head, 0).sequences.is_empty(), "还没到终止符");
        let out = feed(&mut s, tail, 1);
        assert_eq!(out.sequences, vec![b"x".to_vec()]);
    }

    #[test]
    fn multiple_sequences_in_one_read_and_surrounding_bytes() {
        let mut raw = b"a".to_vec();
        raw.extend(seq("one"));
        raw.extend(b"b");
        raw.extend(seq("two"));
        raw.extend(b"c");
        let mut s = local();
        let out = feed(&mut s, &raw, 0);
        assert_eq!(out.passthrough, b"abc");
        assert_eq!(out.sequences, vec![b"one".to_vec(), b"two".to_vec()]);
    }

    #[test]
    fn alt_bracket_is_passed_through_when_it_turns_out_not_to_be_a_sequence() {
        // `Alt+]`（crossterm 会解析成 Alt+']'）：前缀候选失配后必须原样冲回。
        let _logs = debug_logs();
        let mut s = local();
        let out = feed(&mut s, b"\x1b]x", 0);
        assert_eq!(out.passthrough, b"\x1b]x");
        assert!(out.sequences.is_empty());
    }

    #[test]
    fn lone_esc_is_held_until_the_escape_hold_expires() {
        // Esc 键的形态：一个孤立的 ESC 字节——它也可能是一条被拆开的序列的开头，
        // 按 `ESC_HOLD` 判一次；不立即冲回（协议字节零泄漏的代价）。
        let mut s = local();
        let out = feed(&mut s, b"\x1b", 0);
        assert_eq!(out.passthrough, b"", "不立即冲回");
        assert_eq!(s.deadline(), Some(at(0) + ESC_HOLD));
        assert_eq!(s.expire(at(5)), None, "还没到点");
        assert_eq!(s.expire(at(10)), Some(Expiry::Flush(b"\x1b".to_vec())));
        assert_eq!(s.deadline(), None, "冲回后不再持有");
    }

    /// 超时之前 `]5522;…` 到达：孤立 ESC 归位成整条序列，零透传、零按键污染。
    #[test]
    fn a_lone_esc_tail_is_reattached_when_the_sequence_continues() {
        let mut s = local();
        assert_eq!(feed(&mut s, b"\x1b", 0).passthrough, b"", "先扣住");
        assert_eq!(s.expire(at(5)), None, "还没到判定点");
        let out = feed(&mut s, b"]5522;type=read:status=OK:pw=b3Rw\x1b\\", 6);
        assert_eq!(out.passthrough, b"", "一个字节都不许透传");
        assert_eq!(out.sequences, vec![b"type=read:status=OK:pw=b3Rw".to_vec()]);
        assert_eq!(s.deadline(), None);
    }

    #[test]
    fn ssh_env_degrades_the_holds_to_one_hundred_ms() {
        // 单 ESC：本地 10ms 的判定点不许在远端档生效（分片间隔可能到几十 ms）。
        let mut s = remote();
        assert_eq!(feed(&mut s, b"\x1b", 0).passthrough, b"");
        assert_eq!(s.deadline(), Some(at(0) + SSH_HOLD));
        assert_eq!(s.expire(at(10)), None, "10ms 只在本地档生效");
        assert_eq!(s.expire(at(99)), None);
        assert_eq!(s.expire(at(100)), Some(Expiry::Flush(b"\x1b".to_vec())));

        // 多字节候选：25ms 的本地判定点同样不许在远端档生效。
        let mut s = remote();
        assert_eq!(feed(&mut s, b"\x1b]", 0).passthrough, b"");
        assert_eq!(s.deadline(), Some(at(0) + SSH_HOLD));
        assert_eq!(s.expire(at(30)), None);
        assert_eq!(s.expire(at(100)), Some(Expiry::Flush(b"\x1b]".to_vec())));
    }

    #[test]
    fn holds_detect_the_remote_tier_from_each_env_var() {
        let local_tier = Holds {
            escape: ESC_HOLD,
            prefix: PREFIX_HOLD,
        };
        assert_eq!(Holds::detect(|_| None), local_tier);
        // 任一 SSH / mosh 变量存在（哪怕空串）就是远端档。
        for key in SSH_ENV {
            let holds = Holds::detect(|asked| (asked == key).then(String::new));
            assert_eq!(
                holds,
                Holds {
                    escape: SSH_HOLD,
                    prefix: SSH_HOLD
                },
                "{key} 应该切到远端档"
            );
        }
        // 无关变量不影响档位。
        assert_eq!(
            Holds::detect(|asked| (asked == "TERM").then(|| "xterm-256color".to_string())),
            local_tier
        );
    }

    #[test]
    fn held_prefix_resolves_with_the_next_read() {
        // `ESC` 与 `[A` 分两次读到：拼起来是方向键，按原序透传。
        let mut s = local();
        assert_eq!(s.feed(b"\x1b", at(0)).passthrough, b"");
        let out = s.feed(b"[A", at(1));
        assert_eq!(out.passthrough, b"\x1b[A");
        assert!(out.sequences.is_empty());
    }

    #[test]
    fn held_prefix_split_inside_the_sequence_prefix_still_matches() {
        let mut s = local();
        assert_eq!(s.feed(b"\x1b]55", at(0)).passthrough, b"");
        let out = s.feed(b"22;hello\x1b\\", at(1));
        assert_eq!(out.passthrough, b"");
        assert_eq!(out.sequences, vec![b"hello".to_vec()]);
    }

    #[test]
    fn unterminated_sequence_is_dropped_after_the_idle_timeout() {
        let mut s = local();
        let mut raw = SEQUENCE_PREFIX.to_vec();
        raw.extend_from_slice(b"type=read:status=OK");
        let out = feed(&mut s, &raw, 0);
        assert_eq!(out.passthrough, b"", "进了序列体，不再透传");
        assert_eq!(s.deadline(), Some(at(0) + SEQUENCE_IDLE));
        assert_eq!(s.expire(at(1000)), None);
        assert_eq!(s.expire(at(6000)), Some(Expiry::Drop));
        // 之后的新字节照旧透传（分流恢复）。
        assert_eq!(feed(&mut s, b"z", 6100).passthrough, b"z");
    }

    #[test]
    fn sequence_idle_clock_advances_with_each_byte() {
        let mut s = local();
        let mut raw = SEQUENCE_PREFIX.to_vec();
        raw.extend_from_slice(b"1234");
        feed(&mut s, &raw, 0);
        assert_eq!(s.deadline(), Some(at(0) + SEQUENCE_IDLE));
        // 4 秒后又来一个字节：空闲计时重置（大包分片之间不该被误杀）。
        feed(&mut s, b"5", 4000);
        assert_eq!(s.deadline(), Some(at(4000) + SEQUENCE_IDLE));
    }

    /// 体超限（坏流，没有合法 5522 包会到这个体积）：负载丢弃，后续字节吞到终止符再
    /// 恢复分流——绝不把坏流的尾巴冲回透传（那正是本层要防的按键污染）。
    #[test]
    fn overlong_sequence_body_is_dropped_without_leaking() {
        for terminator in [&b"\x07"[..], b"\x1b\\"] {
            let mut raw = SEQUENCE_PREFIX.to_vec();
            raw.extend(std::iter::repeat_n(b'a', SEQUENCE_BODY_MAX + 1));
            raw.extend_from_slice(terminator);
            let mut s = local();
            let out = feed(&mut s, &raw, 0);
            assert!(out.sequences.is_empty(), "超限的序列体整段丢弃");
            assert!(out.passthrough.is_empty(), "一个字节都不许漏给 crossterm");
            assert_eq!(s.deadline(), None, "终止符到了，持有清空");

            // 终止符之后分流恢复：后续字节照常透传。
            let out = feed(&mut s, b"tail", 1);
            assert_eq!(out.passthrough, b"tail");
            assert!(out.sequences.is_empty());
        }
    }

    /// 超限的坏流一直不来终止符：空闲超时照样清掉（不会把持有没有限地拖住）。
    #[test]
    fn overlong_sequence_body_without_a_terminator_still_expires() {
        let mut raw = SEQUENCE_PREFIX.to_vec();
        raw.extend(std::iter::repeat_n(b'a', SEQUENCE_BODY_MAX + 1));
        let mut s = local();
        let out = feed(&mut s, &raw, 0);
        assert!(out.passthrough.is_empty());
        assert_eq!(s.deadline(), Some(at(0) + SEQUENCE_IDLE));
        assert_eq!(s.expire(at(6000)), Some(Expiry::Drop));
        assert_eq!(feed(&mut s, b"z", 6100).passthrough, b"z");
    }

    /// ST 终止符与溢出点重叠的每一种错位：终止符到齐就必须立刻释放持有并恢复分流——
    /// 降级窗口的字节保留不许把 `ESC\` 配对漏进「吞到下一个终止符」的分支。
    #[test]
    fn overlong_body_terminator_crossing_the_overflow_point_is_honoured() {
        // (ESC 在体里的 0-based 位置, 期望整段丢弃)。
        for (esc_offset, dropped) in [
            (SEQUENCE_BODY_MAX - 2, false), // 未超限：照常提取
            (SEQUENCE_BODY_MAX - 1, false), // `\` 是触发溢出的字节：ST 检查先于降级
            (SEQUENCE_BODY_MAX, true),      // `ESC` 触发溢出：配对落在降级窗口
            (SEQUENCE_BODY_MAX + 1, true),  // 溢出已发生：配对落在降级窗口内
        ] {
            let mut raw = SEQUENCE_PREFIX.to_vec();
            raw.extend(std::iter::repeat_n(b'a', esc_offset));
            raw.extend_from_slice(b"\x1b\\");
            let mut s = local();
            let out = feed(&mut s, &raw, 0);
            assert!(
                out.passthrough.is_empty(),
                "esc_offset={esc_offset}: 终止符之前的字节不许透传"
            );
            if dropped {
                assert!(
                    out.sequences.is_empty(),
                    "esc_offset={esc_offset}: 超限体丢弃"
                );
            } else {
                assert_eq!(
                    out.sequences,
                    vec![vec![b'a'; esc_offset]],
                    "esc_offset={esc_offset}: 未超限体照常提取"
                );
            }
            assert_eq!(
                s.deadline(),
                None,
                "esc_offset={esc_offset}: 终止符到齐后必须立刻释放持有"
            );
            let out = feed(&mut s, b"tail", 1);
            assert_eq!(
                out.passthrough, b"tail",
                "esc_offset={esc_offset}: 分流必须恢复，后续字节不许被吞"
            );
        }
    }

    #[test]
    fn bracketed_paste_content_keeps_a_literal_5522_intact() {
        // 保护区的意义：非 5522 终端上，粘贴内容里的字面 OSC 5522 是用户数据，不能吃。
        let mut raw = b"\x1b[200~".to_vec();
        raw.extend(seq("literal"));
        raw.extend(b" tail\x1b[201~");
        let mut s = local();
        let out = feed(&mut s, &raw, 0);
        assert!(out.sequences.is_empty(), "保护区内的序列不当协议数据");
        assert_eq!(out.passthrough, raw, "含开闭界的每个字节都原样放行");
    }

    #[test]
    fn bracketed_paste_terminator_split_across_reads() {
        let mut s = local();
        let open = b"\x1b[200~hi";
        assert_eq!(feed(&mut s, open, 0).passthrough, open);
        // 闭界被切成两段：`ESC[20` 是终止符前缀（要扣住等下一字节），到齐后整段放行。
        assert_eq!(feed(&mut s, b"\x1b[20", 1).passthrough, b"");
        assert_eq!(feed(&mut s, b"1~", 2).passthrough, b"\x1b[201~");
        // 保护区已退出：后续的 5522 又是协议数据。
        let out = feed(&mut s, &seq("after"), 3);
        assert_eq!(out.sequences, vec![b"after".to_vec()]);
    }

    #[test]
    fn bracketed_paste_region_does_not_stall_non_terminator_bytes() {
        // 保护区内除了「可能是终止符开头」的那几字节，其余都该立刻出去（包括裸 ESC）。
        let mut s = local();
        assert_eq!(feed(&mut s, b"\x1b[200~", 0).passthrough, b"\x1b[200~");
        assert_eq!(feed(&mut s, b"abc\x1b", 1).passthrough, b"abc");
        assert_eq!(feed(&mut s, b"Z", 2).passthrough, b"\x1bZ");
    }

    /// 已知边界（登记，见模块注释）：粘贴内容以字面 ESC 结尾、紧跟闭界时，保护区先把
    /// 这个 ESC 当作闭界前缀候选扣住，闭界到达后再原样冲回——**本层一字节不丢**（字节流
    /// 里出现 `ESC` + `ESC[201~`；哪个 ESC 属于内容只有终端知道，歧义在下游收场）。
    #[test]
    fn bracketed_paste_keeps_a_trailing_literal_esc_byte_for_byte() {
        let mut raw = b"\x1b[200~A\x1b".to_vec();
        raw.extend_from_slice(PASTE_CLOSE);
        raw.extend_from_slice(b"B");
        let mut s = local();
        let out = feed(&mut s, &raw, 0);
        assert_eq!(out.passthrough, raw, "含尾部字面 ESC 的每个字节都原样放行");
        assert!(out.sequences.is_empty());
        // 保护区已退出：后续的 5522 又是协议数据。
        let out = feed(&mut s, &seq("after"), 1);
        assert_eq!(out.sequences, vec![b"after".to_vec()]);
    }

    #[test]
    fn other_osc_sequences_pass_through_untouched() {
        let mut s = local();
        let raw = b"\x1b]52;c;aGVsbG8=\x07";
        assert_eq!(feed(&mut s, raw, 0).passthrough, raw);
        let raw = b"\x1b]0;title\x1b\\";
        assert_eq!(feed(&mut s, raw, 1).passthrough, raw);
    }

    /// 不变式：**透传 + 提取出的序列 / 应答（补回前缀与终结符）= 输入**，一字节不增不减。
    /// 用确定性的 LCG 切块（跨越前缀 / 终止符 / 保护区闭界的所有边界）跑 64 轮。
    #[test]
    fn reconstruction_identity_over_arbitrary_chunking() {
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice("前文 ".as_bytes());
        stream.extend(seq("one"));
        stream.extend_from_slice(b"\x1b[200~paste ");
        stream.extend(seq("inside-paste"));
        stream.extend_from_slice(b"\x1b[201~");
        stream.extend_from_slice("后文 \x1b[A\x1b]0;t\x07".as_bytes());
        stream.extend(report("5522;2"));
        stream.extend(seq("two"));
        stream.extend_from_slice(b"done");

        let mut expected_passthrough: Vec<u8> = Vec::new();
        expected_passthrough.extend_from_slice("前文 ".as_bytes());
        expected_passthrough.extend_from_slice(b"\x1b[200~paste ");
        expected_passthrough.extend(seq("inside-paste"));
        expected_passthrough.extend_from_slice(b"\x1b[201~");
        expected_passthrough.extend_from_slice("后文 \x1b[A\x1b]0;t\x07".as_bytes());
        expected_passthrough.extend_from_slice(b"done");
        let expected_sequences = vec![b"one".to_vec(), b"two".to_vec()];
        let expected_reports = vec![b"5522;2".to_vec()];

        for seed in 0..64u64 {
            let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut next = move || {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as usize
            };
            let mut s = local();
            let mut passthrough = Vec::new();
            let mut sequences = Vec::new();
            let mut reports = Vec::new();
            let mut i = 0;
            while i < stream.len() {
                let take = (next() % 7 + 1).min(stream.len() - i);
                // 任意切块（含「一次 write 被多次 read 切开」的形态）：任何位置的候选都
                // 只能等后续字节或超时，不许提前冲回。
                let out = s.feed(&stream[i..i + take], at(i as u64));
                passthrough.extend_from_slice(&out.passthrough);
                sequences.extend(out.sequences);
                reports.extend(out.reports);
                i += take;
            }
            // 流干净收尾：没有候选 / 序列体残留（有的话下面是 seed 的红）。
            assert!(
                s.expire(at(10_000)).is_none(),
                "seed {seed}: stream left a held candidate behind"
            );
            assert_eq!(sequences, expected_sequences, "seed {seed}");
            assert_eq!(reports, expected_reports, "seed {seed}");
            assert_eq!(passthrough, expected_passthrough, "seed {seed}");
            // 长度恒等：透传 + Σ(前缀 + 负载 + ST) + Σ(`ESC[?` + 参数 + `$y`) == 输入总长。
            let rebuilt: usize = passthrough.len()
                + sequences
                    .iter()
                    .map(|s| SEQUENCE_PREFIX.len() + s.len() + 2)
                    .sum::<usize>()
                + reports
                    .iter()
                    .map(|r| REPORT_OPEN.len() + r.len() + 2)
                    .sum::<usize>();
            assert_eq!(rebuilt, stream.len(), "seed {seed}: 字节不增不减");
        }
    }
}
