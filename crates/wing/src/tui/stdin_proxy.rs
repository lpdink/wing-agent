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
//! epoll 的 interest 绑定的是**注册时刻的文件描述**，`dup2` 之后 fd 0 与它就绪源不再对应。
//! 换成 **pty slave** 就不一样：它本身是 tty，`isatty(0)` 仍为真，crossterm 照旧读 fd 0
//! （spike 实证，见 `crates/wing/tests/stdin_proxy_pty.rs` 与
//! `crates/wing/tests/crossterm_osc5522_misparse.rs`）。
//!
//! ```text
//!  真实 tty ──read──► 泵线程（Splitter）──write──► pty master ⇄ pty slave（fd 0）──► crossterm
//!                                    └─ 完整 OSC 5522 序列 ─► 协议机（clipboard_protocol）
//! ```
//!
//! # 生命周期（顺序是硬约束）
//!
//! - [`install`] 必须晚于 `crossterm::terminal::enable_raw_mode()`（raw 打在真实 tty 上）、
//!   早于 crossterm 事件源的首次创建（`spawn_event_stream` 的第一次 `poll`）。
//! - [`restore`] 必须早于 `crossterm::terminal::disable_raw_mode()`（它把 termios 写回
//!   fd 0，彼时 fd 0 必须已经是真实 tty）。
//! - 失败不致命：[`install`] 任何一步出错都先在返回前还原 fd 0，调用方降级即可；但**没有
//!   装上就不要开 mode 5522**（[`is_installed`] 是 `enter_sequence` 的判据）——否则协议
//!   数据会直接喂给 crossterm，正是本层要避免的事。

use std::time::{Duration, Instant};

/// 一条完整的 OSC 5522 负载：`ESC]5522;` 之后、终止符（BEL / ST）之前的原始字节。
pub type Sequence = Vec<u8>;

/// 前缀候选的持有上限：25ms 内没等到后续字节就冲回透传流。
///
/// `Esc` 键的形态就是「一个孤零零的 `ESC`」——不能无限期扣着（否则 Esc 会在下一次按键
/// 到达时才生效，还会被 crossterm 读成 `Alt+某键`）。
const PREFIX_HOLD: Duration = Duration::from_millis(25);

/// 序列体的空闲上限：正常终端毫秒级闭合（Ghostty 一条包一次 write），超过即视为坏数据。
const SEQUENCE_IDLE: Duration = Duration::from_secs(5);

/// 泵线程里「待写进内层 pty」的字节上限。到顶就暂时不读源（也让真实 tty 的环形缓冲
/// 把背压顶回终端，而不是在我们这里无限吃内存）——poll 的 POLLIN 只能撤掉，否则
/// level-triggered 的就绪会变成空转。
#[cfg(unix)]
const PUMP_PENDING_HIGH_WATER: usize = 64 * 1024;

/// `ESC]5522;`——OSC 5522（kitty clipboard protocol）的前缀。
const SEQUENCE_PREFIX: &[u8] = b"\x1b]5522;";

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
}

/// 超时产生的结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// 候选前缀等不到后续：原样冲回（`Esc` 键的延迟上限）。
    Flush(Vec<u8>),
    /// 序列体等不到终止符：整段丢弃（调用方记日志）。
    Drop,
}

#[derive(Debug, Default)]
enum Hold {
    #[default]
    None,
    /// `ESC]5522;` 或 `ESC[200~` 的**部分前缀**（含开头的 ESC）。
    Candidate { bytes: Vec<u8>, since: Instant },
    /// 已确认 `ESC]5522;` 开头，累积到终止符为止的负载。
    Body { payload: Vec<u8>, since: Instant },
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
    Candidate::No
}

/// 字节流分流器。**不丢用户数据**是唯一红线：除「已确认的 5522 序列」与「等不到终止符的
/// 5522 序列体」外，所有字节按原顺序从 [`Feed::passthrough`] 出来。
#[derive(Debug, Default)]
pub(crate) struct Splitter {
    hold: Hold,
}

impl Splitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一次 read 的字节。
    ///
    /// `queue_drained`：本次 `read` 返回的字节数小于缓冲区（内核队列已空）。据此决定
    /// **尾部半截候选前缀**是立刻冲回还是继续持有——`Esc` 键（单字节 `ESC`）落在这一条上，
    /// 它是零延迟的；只有「缓冲读满 + 尾部长这样」的罕见形态才吃 [`PREFIX_HOLD`]。
    pub fn feed(&mut self, chunk: &[u8], queue_drained: bool, now: Instant) -> Feed {
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
                            };
                        }
                        Candidate::PasteOpen => {
                            // 开界本身是 crossterm 要看的控制序列：原样放行。
                            out.passthrough.append(bytes);
                            self.hold = Hold::Paste { tail: Vec::new() };
                        }
                        Candidate::No => {
                            // 失配：持有的字节全部原样冲回（`Alt+]` 之类语义一字不差）。
                            out.passthrough.append(bytes);
                            self.hold = Hold::None;
                        }
                    }
                }
                // 序列体：累积到 BEL 或 ST。
                Hold::Body { payload, since } => {
                    let b = chunk[i];
                    i += 1;
                    if b == BEL {
                        out.sequences.push(std::mem::take(payload));
                        self.hold = Hold::None;
                        continue;
                    }
                    payload.push(b);
                    *since = now;
                    // `ends_with` 天然覆盖 ST 被 read 边界切开的情况（base64 负载里不含 ESC）。
                    if payload.ends_with(b"\x1b\\") {
                        payload.truncate(payload.len() - 2);
                        out.sequences.push(std::mem::take(payload));
                        self.hold = Hold::None;
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

        // 队列已空 + 尾部半截候选：留着只可能把 `Esc` 键扣住（它就是一个孤立的 ESC 字节）。
        if queue_drained && let Hold::Candidate { bytes, .. } = &mut self.hold {
            out.passthrough.append(bytes);
            self.hold = Hold::None;
        }
        out
    }

    /// 推进超时。调用方在 [`Splitter::deadline`] 到点（或更晚）时调用。
    pub fn expire(&mut self, now: Instant) -> Option<Expiry> {
        match &self.hold {
            Hold::Candidate { bytes, since } if now.duration_since(*since) >= PREFIX_HOLD => {
                let bytes = bytes.clone();
                self.hold = Hold::None;
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
            Hold::Candidate { since, .. } => Some(*since + PREFIX_HOLD),
            Hold::Body { since, .. } => Some(*since + SEQUENCE_IDLE),
            Hold::None | Hold::Paste { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// fd 接驳（Unix）
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io;
    use std::os::fd::RawFd;
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use tokio::sync::mpsc;

    struct Proxy {
        /// 真实 tty 的副本：`restore` 用它还原 fd 0。泵线程另有一份自己的 dup。
        restore_fd: RawFd,
        stop_w: RawFd,
        join: Option<JoinHandle<()>>,
    }

    #[derive(Default)]
    struct Shared {
        proxy: Option<Proxy>,
        seq_rx: Option<mpsc::Receiver<Sequence>>,
    }

    /// 全局句柄（安装 / 卸载各只有一次；`is_installed` 每次进序列读一次）。
    static SHARED: Mutex<Shared> = Mutex::new(Shared {
        proxy: None,
        seq_rx: None,
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
    fn abort(restore_fd: RawFd, master: RawFd, stop_r: RawFd, stop_w: RawFd) {
        unsafe { libc::dup2(restore_fd, 0) };
        for fd in [restore_fd, master, stop_r, stop_w] {
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
                None => {}
            }

            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let n =
                    unsafe { libc::read(src, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n > 0 {
                    let n = n as usize;
                    let part = splitter.feed(&buf[..n], n < buf.len(), Instant::now());
                    feed.passthrough.extend_from_slice(&part.passthrough);
                    feed.sequences.extend(part.sequences);
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

    pub fn install() -> io::Result<()> {
        let mut shared = lock();
        if shared.proxy.is_some() {
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
        if unsafe { libc::dup2(slave, 0) } < 0 {
            let e = io_err();
            close(slave);
            close(master);
            close(src);
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
            return Err(e);
        }
        let mut pipe_fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            let e = io_err();
            abort(restore_fd, master, -1, -1);
            close(src);
            return Err(e);
        }
        let (stop_r, stop_w) = (pipe_fds[0], pipe_fds[1]);

        let (tx, rx) = mpsc::channel(16);
        let spawned = std::thread::Builder::new()
            .name("wing-stdin-proxy".into())
            .spawn(move || pump(src, master, stop_r, tx));
        match spawned {
            Ok(join) => {
                shared.proxy = Some(Proxy {
                    restore_fd,
                    stop_w,
                    join: Some(join),
                });
                shared.seq_rx = Some(rx);
                Ok(())
            }
            Err(e) => {
                abort(restore_fd, master, stop_r, stop_w);
                close(src);
                Err(e)
            }
        }
    }

    pub fn restore() {
        let proxy = lock().proxy.take();
        let Some(mut proxy) = proxy else { return };
        // 唤醒泵线程（写 stop pipe），等它自己退出——没有孤儿读者去抢 shell 的输入。
        unsafe { libc::write(proxy.stop_w, b"x".as_ptr() as *const libc::c_void, 1) };
        if let Some(join) = proxy.join.take() {
            let _ = join.join();
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

    pub fn is_installed() -> bool {
        false
    }

    pub fn take_sequences() -> Option<tokio::sync::mpsc::Receiver<Sequence>> {
        None
    }
}

pub use imp::install;
pub use imp::is_installed;
pub use imp::restore;
pub use imp::take_sequences;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        // 相对基准的确定时刻：单测不依赖 wall clock。
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    /// 一次「完整」的读（内核队列已空）：单测里就是常态。
    fn feed(s: &mut Splitter, chunk: &[u8], ms: u64) -> Feed {
        s.feed(chunk, true, at(ms))
    }

    fn seq(inner: &str) -> Vec<u8> {
        let mut out = SEQUENCE_PREFIX.to_vec();
        out.extend_from_slice(inner.as_bytes());
        out.extend_from_slice(b"\x1b\\");
        out
    }

    #[test]
    fn plain_bytes_pass_through_byte_for_byte() {
        let mut s = Splitter::new();
        let out = feed(&mut s, b"hello \xe4\xb8\x96\xe7\x95\x8c", 0);
        assert_eq!(out.passthrough, "hello 世界".as_bytes());
        assert!(out.sequences.is_empty());
        // 分块喂同样一致。
        let mut s = Splitter::new();
        let mut acc = Vec::new();
        for chunk in [&b"he"[..], &b"ll"[..], &b"o"[..]] {
            acc.extend(feed(&mut s, chunk, 0).passthrough);
        }
        assert_eq!(acc, b"hello");
    }

    #[test]
    fn complete_sequence_is_extracted_not_passed_through() {
        let mut s = Splitter::new();
        let out = feed(&mut s, &seq("type=read:status=OK:pw=b3Rw"), 0);
        assert_eq!(out.passthrough, b"");
        assert_eq!(out.sequences, vec![b"type=read:status=OK:pw=b3Rw".to_vec()]);
    }

    #[test]
    fn bel_terminated_sequence_is_extracted_too() {
        let mut s = Splitter::new();
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
            let mut s = Splitter::new();
            // 两段（每一段都不是 drained 的尾部，模拟终端一次 write 被两次 read 切开的形态）
            let a = s.feed(&raw[..cut], false, at(0));
            let b = s.feed(&raw[cut..], true, at(1));
            let mut seqs = a.sequences;
            seqs.extend(b.sequences);
            assert_eq!(seqs, vec![expected.clone()], "cut at {cut}");
            assert_eq!(a.passthrough.len() + b.passthrough.len(), 0, "cut at {cut}");
        }
    }

    #[test]
    fn st_terminator_split_across_reads() {
        let mut s = Splitter::new();
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
        let mut s = Splitter::new();
        let out = feed(&mut s, &raw, 0);
        assert_eq!(out.passthrough, b"abc");
        assert_eq!(out.sequences, vec![b"one".to_vec(), b"two".to_vec()]);
    }

    #[test]
    fn alt_bracket_is_passed_through_when_it_turns_out_not_to_be_a_sequence() {
        // `Alt+]`（crossterm 会解析成 Alt+']'）：前缀候选失配后必须原样冲回。
        let mut s = Splitter::new();
        let out = feed(&mut s, b"\x1b]x", 0);
        assert_eq!(out.passthrough, b"\x1b]x");
        assert!(out.sequences.is_empty());
    }

    #[test]
    fn lone_esc_at_a_drained_read_is_flushed_immediately() {
        // Esc 键的形态：一个孤立的 ESC 字节，队列已空 → 零延迟放行。
        let mut s = Splitter::new();
        let out = feed(&mut s, b"\x1b", 0);
        assert_eq!(out.passthrough, b"\x1b");
        assert!(s.deadline().is_none(), "不持有 = 不需要唤醒");
    }

    #[test]
    fn lone_esc_at_a_full_read_is_held_then_flushed_on_timeout() {
        // 满缓冲读的尾部：可能还有后续（序列前缀被切开），持有到 25ms 上限。
        let mut s = Splitter::new();
        let out = s.feed(b"\x1b", false, at(0));
        assert_eq!(out.passthrough, b"", "先持有");
        assert_eq!(s.deadline(), Some(at(0) + PREFIX_HOLD));
        assert_eq!(s.expire(at(10)), None, "还没到点");
        assert_eq!(s.expire(at(30)), Some(Expiry::Flush(b"\x1b".to_vec())));
        assert_eq!(s.deadline(), None);
    }

    #[test]
    fn held_prefix_resolves_with_the_next_read() {
        // `ESC` 与 `[A` 分两次读到：拼起来是方向键，按原序透传。
        let mut s = Splitter::new();
        assert_eq!(s.feed(b"\x1b", false, at(0)).passthrough, b"");
        let out = s.feed(b"[A", true, at(1));
        assert_eq!(out.passthrough, b"\x1b[A");
        assert!(out.sequences.is_empty());
    }

    #[test]
    fn held_prefix_split_inside_the_sequence_prefix_still_matches() {
        let mut s = Splitter::new();
        assert_eq!(s.feed(b"\x1b]55", false, at(0)).passthrough, b"");
        let out = s.feed(b"22;hello\x1b\\", true, at(1));
        assert_eq!(out.passthrough, b"");
        assert_eq!(out.sequences, vec![b"hello".to_vec()]);
    }

    #[test]
    fn unterminated_sequence_is_dropped_after_the_idle_timeout() {
        let mut s = Splitter::new();
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
        let mut s = Splitter::new();
        let mut raw = SEQUENCE_PREFIX.to_vec();
        raw.extend_from_slice(b"1234");
        feed(&mut s, &raw, 0);
        assert_eq!(s.deadline(), Some(at(0) + SEQUENCE_IDLE));
        // 4 秒后又来一个字节：空闲计时重置（大包分片之间不该被误杀）。
        feed(&mut s, b"5", 4000);
        assert_eq!(s.deadline(), Some(at(4000) + SEQUENCE_IDLE));
    }

    #[test]
    fn bracketed_paste_content_keeps_a_literal_5522_intact() {
        // 保护区的意义：非 5522 终端上，粘贴内容里的字面 OSC 5522 是用户数据，不能吃。
        let mut raw = b"\x1b[200~".to_vec();
        raw.extend(seq("literal"));
        raw.extend(b" tail\x1b[201~");
        let mut s = Splitter::new();
        let out = feed(&mut s, &raw, 0);
        assert!(out.sequences.is_empty(), "保护区内的序列不当协议数据");
        assert_eq!(out.passthrough, raw, "含开闭界的每个字节都原样放行");
    }

    #[test]
    fn bracketed_paste_terminator_split_across_reads() {
        let mut s = Splitter::new();
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
        let mut s = Splitter::new();
        assert_eq!(feed(&mut s, b"\x1b[200~", 0).passthrough, b"\x1b[200~");
        assert_eq!(feed(&mut s, b"abc\x1b", 1).passthrough, b"abc");
        assert_eq!(feed(&mut s, b"Z", 2).passthrough, b"\x1bZ");
    }

    #[test]
    fn other_osc_sequences_pass_through_untouched() {
        let mut s = Splitter::new();
        let raw = b"\x1b]52;c;aGVsbG8=\x07";
        assert_eq!(feed(&mut s, raw, 0).passthrough, raw);
        let raw = b"\x1b]0;title\x1b\\";
        assert_eq!(feed(&mut s, raw, 1).passthrough, raw);
    }

    /// 不变式：**透传 + 提取出的序列（补回前缀与 ST）= 输入**，一字节不增不减。
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

        for seed in 0..64u64 {
            let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut next = move || {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as usize
            };
            let mut s = Splitter::new();
            let mut passthrough = Vec::new();
            let mut sequences = Vec::new();
            let mut i = 0;
            while i < stream.len() {
                let take = (next() % 7 + 1).min(stream.len() - i);
                let last = i + take == stream.len();
                // 段的末尾故意报「队列未空」：模拟一次 write 被多次 read 切开的形态。
                let out = s.feed(&stream[i..i + take], last, at(i as u64));
                passthrough.extend_from_slice(&out.passthrough);
                sequences.extend(out.sequences);
                i += take;
            }
            if let Some(expiry) = s.expire(at(10_000)) {
                match expiry {
                    Expiry::Flush(bytes) => passthrough.extend_from_slice(&bytes),
                    Expiry::Drop => panic!("seed {seed}: unterminated sequence"),
                }
            }
            assert_eq!(sequences, expected_sequences, "seed {seed}");
            assert_eq!(passthrough, expected_passthrough, "seed {seed}");
            // 长度恒等：透传 + Σ(前缀 + 负载 + ST) == 输入总长。
            let rebuilt: usize = passthrough.len()
                + sequences
                    .iter()
                    .map(|s| SEQUENCE_PREFIX.len() + s.len() + 2)
                    .sum::<usize>();
            assert_eq!(rebuilt, stream.len(), "seed {seed}: 字节不增不减");
        }
    }
}
