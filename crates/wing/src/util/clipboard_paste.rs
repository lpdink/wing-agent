//! Local clipboard paste (L1): read the system clipboard through platform commands.
//!
//! The terminal-protocol channel (OSC 5522, `tui::clipboard_protocol`) needs a
//! terminal that implements the protocol; this module is the channel that does
//! not: the app reads the *local* clipboard itself, so `Ctrl+V` — and a
//! `super+v` the terminal forwards — can deliver text and, on macOS, a picture
//! even in terminals that never speak 5522. Over SSH the channel is dead by
//! construction (a local helper would read the *remote* machine's clipboard),
//! so remote sessions are short-circuited before any child process is spawned,
//! exactly like the copy direction in [`super::clipboard`].
//!
//! Order per platform: picture first, then text.
//!
//! - macOS: `osascript` lets AppKit write the clipboard's PNG representation
//!   into a temp file (a TIFF-only clipboard is converted along the way — the
//!   screenshot case; a text-only clipboard exits non-zero and leaves a 0-byte
//!   file, which is the "no picture" signal), then `pbpaste` reads the text.
//! - Linux: `wl-paste` (Wayland) then `xclip` / `xsel` (X11); the picture branch
//!   first reads the advertised types (`--list-types` / `TARGETS`) and picks one
//!   from [`KNOWN_IMAGE_MIMES`].
//!
//! Every child runs with piped stdio and a deadline (text ~1 s, pictures ~5 s)
//! through an injectable runner, so the candidate order, the fall-through and
//! the temp-file lifecycle are unit-testable without touching the real
//! clipboard. Spawning happens on the blocking pool; the event loop only awaits
//! the result.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::util::image_compress::KNOWN_IMAGE_MIMES;
use crate::util::mime::normalize_mime;

/// Environment variables that mark the session as remote (SSH / MOSH).
///
/// Same markers as the copy direction, plus `SSH_TTY`: an SSH session whose
/// connection metadata was not forwarded still has a tty name.
const REMOTE_SESSION_MARKERS: [&str; 4] =
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"];

/// 单次读回的字节上限：与 OSC 5522 通道的 `MAX_PAYLOAD` 同一个数。
pub(crate) const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// 文本读取命令的超时（`pbpaste` 实测 6 ms；1 s 对慢机器也够）。
const TEXT_TIMEOUT: Duration = Duration::from_secs(1);

/// 图片读取命令的超时（`osascript` 4K 图实测约 1.1 s）。
const IMAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// 子进程退出状态的轮询间隔。
const WAIT_POLL: Duration = Duration::from_millis(5);

/// 一次成功读取的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteOutcome {
    /// 文本表示。
    Text(String),
    /// 图片表示（字节 + 来源声明的 MIME）。
    Image { bytes: Vec<u8>, mime: String },
    /// 剪贴板既没有文本也没有图片（合法的空结果，不是失败）。
    Empty,
}

/// 读失败的原因（App 侧转定向 toast；文案由 [`PasteFailure::message`] 给）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteFailure {
    /// 远程会话：本机剪贴板不是用户的剪贴板，短路（没有产生任何子进程）。
    Remote,
    /// 平台不支持，或一个可用的读取命令都没有。
    Unavailable,
    /// 读取命令超时。
    Timeout,
    /// 内容超过 [`MAX_PAYLOAD`]。
    TooLarge,
    /// 读取器报告「这个剪贴板没有该表示」（普通非零退出：`wl-paste` 的空剪贴板 /
    /// 类型不可用都是这种）。不是通道故障 —— 收场由 [`settle`] 归入空结果。
    NoRepresentation,
    /// 真实失败（临时文件读不动、无法预占、读任务 panic 等）。**不是**「没有内容」：
    /// 收场必须上报错误，不能粉饰成空剪贴板。
    Failed(String),
}

impl std::fmt::Display for PasteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote => write!(
                f,
                "Clipboard reading is local-only — over SSH use the terminal's paste key"
            ),
            Self::Unavailable => write!(
                f,
                "No clipboard reader available (pbpaste / osascript, wl-paste / xclip / xsel)"
            ),
            Self::Timeout => write!(f, "Clipboard read timed out"),
            Self::TooLarge => write!(f, "Clipboard content is too large to paste"),
            Self::NoRepresentation => write!(f, "Clipboard has no text or image"),
            Self::Failed(reason) => write!(f, "Clipboard read failed: {reason}"),
        }
    }
}

impl PasteFailure {
    /// 用户可见的一句话（toast 文案），与 OSC 5522 通道的失败面同一种口气。
    pub(crate) fn message(&self) -> String {
        self.to_string()
    }
}

/// 主机平台。显式枚举（而不是 `cfg` 分支散在各处）是因为测试要在任一主机上
/// 走每一条平台的流程 — 同 `super::clipboard` 把三张命令表都声明出来的理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // 非本机平台的变体只被测试构造（`host_platform` 的 cfg 会编掉它们）
enum Platform {
    Macos,
    Linux,
    /// 其余平台（Windows 等）：不承诺本地读取。
    Other,
}

fn host_platform() -> Platform {
    #[cfg(target_os = "macos")]
    {
        Platform::Macos
    }
    #[cfg(target_os = "windows")]
    {
        Platform::Other
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Platform::Linux
    }
}

/// 一条读取命令。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandSpec {
    program: &'static str,
    args: Vec<String>,
    /// macOS 的 `osascript` 把图片写进这个文件；`None` = 从 stdout 读字节。
    image_file: Option<PathBuf>,
}

impl CommandSpec {
    /// 从 stdout 读字节的命令（Linux 的 `wl-paste` / `xclip` / `xsel`，
    /// macOS 的 `pbpaste`）。
    fn stdout(program: &'static str, args: &[&str]) -> Self {
        Self {
            program,
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            image_file: None,
        }
    }
}

/// `osascript`：让 AppKit 把剪贴板的 PNG 表示写进 `path`。
///
/// AppKit 在这个转换里会把 TIFF-only 的剪贴板（macOS 截图的规范表示）转成
/// PNG；纯文本剪贴板则让脚本非零退出、留下 0 字节文件 — 调用方据此判定
/// 「没有图片」。AppleScript 字符串字面量里只有 `"` 与 `\` 需要转义。
fn macos_image_spec(path: &Path) -> CommandSpec {
    let escaped = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    CommandSpec {
        program: "/usr/bin/osascript",
        args: vec![
            "-e".into(),
            format!("set f to open for access POSIX file \"{escaped}\" with write permission"),
            "-e".into(),
            "set eof f to 0".into(),
            "-e".into(),
            "write (the clipboard as «class PNGf») to f".into(),
            "-e".into(),
            "close access f".into(),
        ],
        image_file: Some(path.to_path_buf()),
    }
}

/// 预占候选名的重试上限（名字被抢占 / 上次残留都是极端情况）。
const TEMP_RESERVE_ATTEMPTS: usize = 16;

/// 新建并**预占**临时文件的唯一路径（`$TMPDIR` 下，进程 id + 计数器）。
///
/// `create_new` 先由本进程创建：sticky 目录（`/tmp`）里别人无法替换已存在的文件，
/// 关掉「路径被预换成符号链接 → osascript 覆写链接目标」的面 ——
/// `open for access … with write permission` 自己没有 O_EXCL 语义。名字被占用
/// （上一次残留 / 并行进程）就换一个计数重试。
fn temp_image_path() -> io::Result<PathBuf> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    reserve_temp_file((0..).map(|_| {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("wing-paste-{}-{n}.png", std::process::id()))
    }))
}

/// 逐个尝试候选路径，`create_new` 成功即预占；名字已被占用（`AlreadyExists`）就试
/// 下一个，其它错误立即上报（`$TMPDIR` 不可写之类）。
fn reserve_temp_file(candidates: impl Iterator<Item = PathBuf>) -> io::Result<PathBuf> {
    let mut last = None;
    for path in candidates.take(TEMP_RESERVE_ATTEMPTS) {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free clipboard temp name")))
}

/// Linux 的图片读取工具：Wayland 优先，X11 兜底。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxImageTool {
    WlPaste,
    Xclip,
}

impl LinuxImageTool {
    /// 列出剪贴板声明了哪些类型（每行一个）。
    fn list_spec(self) -> CommandSpec {
        match self {
            Self::WlPaste => CommandSpec::stdout("wl-paste", &["--list-types"]),
            // `TARGETS` 是 X11 规定的特殊 target：请求它得到可用 target 的列表。
            Self::Xclip => {
                CommandSpec::stdout("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])
            }
        }
    }

    /// 取某一 MIME 的字节。
    fn read_spec(self, mime: &str) -> CommandSpec {
        match self {
            // `--no-newline`：默认会在末尾补一个换行，二进制数据不能用它。
            Self::WlPaste => CommandSpec::stdout("wl-paste", &["--no-newline", "--type", mime]),
            Self::Xclip => {
                CommandSpec::stdout("xclip", &["-selection", "clipboard", "-t", mime, "-o"])
            }
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::WlPaste => "wl-paste",
            Self::Xclip => "xclip",
        }
    }
}

/// Linux 的文本读取命令，优先级：Wayland → X11（xclip → xsel）。
fn linux_text_specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::stdout("wl-paste", &["--no-newline"]),
        CommandSpec::stdout("xclip", &["-selection", "clipboard", "-o"]),
        CommandSpec::stdout("xsel", &["--clipboard", "--output"]),
    ]
}

/// Runs one clipboard reader. Injected so the candidate order, the
/// picture-first fall-through and the temp-file lifecycle are unit-testable
/// without touching the real clipboard.
trait CommandRunner {
    /// `Ok(bytes)` = the helper was spawned **and** exited successfully, with at
    /// most [`MAX_PAYLOAD`] bytes (an empty `Ok` is legal). A helper that cannot
    /// serve the read (an `osascript` on a text-only clipboard included) exits
    /// non-zero and comes back as `Err` with `ErrorKind::Unsupported` — the
    /// callers decide per command whether to keep looking (another reader may
    /// answer) or settle for "no representation". Every other `Err` kind is a
    /// real failure.
    ///
    /// `timeout` is a **hard bound on this call**: on expiry the helper's whole
    /// process group is killed and `ErrorKind::TimedOut` comes back without
    /// waiting for the output pipe to close (`wl-paste` forks a `cat` per read
    /// that can hold it). Reading stops at `MAX_PAYLOAD + 1` bytes, reported as
    /// `ErrorKind::FileTooLarge`.
    fn run(&self, spec: &CommandSpec, timeout: Duration) -> io::Result<Vec<u8>>;
}

/// stdout 读取上限：比 [`MAX_PAYLOAD`] 多一个字节 —— 刚好能分辨「在上限内」与
/// 「超了」，又不会把超限内容整个读进内存。
const READ_CAP: u64 = MAX_PAYLOAD as u64 + 1;

/// 一条命令的输出超过上限时给调用方的错误 kind（`classify_io` 认它）。
fn too_large(program: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("{program} produced more than {MAX_PAYLOAD} bytes"),
    )
}

/// 读取器「普通非零退出」的错误 kind：它说的是「这个剪贴板没有该表示」（`wl-paste`
/// 的空剪贴板 / 类型不可用都是 exit 1），`classify_io` 认它并把收场交给 [`settle`]
/// （→ 空结果）；**不是**通道故障，别和 `ErrorKind` 的通用错误混为一谈。
fn exited(program: &str, status: std::process::ExitStatus) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{program} exited with {status}"),
    )
}

/// 杀掉子进程**及其进程组**（子进程以 `process_group(0)` 起，pgid == pid），并
/// reap 直接子进程。
///
/// 只 `kill()` 直接子进程不够：`wl-paste` 每次读都会 fork 一个 `cat` 持有我们的
/// stdout 写端，父进程死了写端照样敞开 —— 读侧的 EOF 永远不来。
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        // 对已经不存在的组返回 ESRCH，忽略即可。
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Production runner: `std::process::Command` with piped stdio and a deadline.
struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, spec: &CommandSpec, timeout: Duration) -> io::Result<Vec<u8>> {
        use std::process::Command;
        use std::process::Stdio;

        // Stdio is piped / null (never inherited): the TUI owns the terminal in
        // raw mode, and the crate denies direct printing. stderr is discarded
        // on purpose — an unread stderr pipe can fill up and stall the helper,
        // and the exit status is what the callers classify.
        let mut command = Command::new(spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // 自己的进程组：超时要杀的是整组（见 `kill_group`）。仓库先例：
        // `cmd::start` 的网关守护进程。
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;

        // stdout must be drained concurrently: a large payload (pictures run to
        // 64 MiB) fills the pipe buffer and the helper would never exit. The
        // drain is bounded (`READ_CAP`) and hands the bytes back over a channel:
        // the timeout path must not join the reader — a grandchild can keep the
        // write end open indefinitely, and `join` would stretch the deadline
        // into a hang (the reader thread then finishes on its own once the
        // process group is killed and the pipe closes).
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut limited = io::Read::take(&mut stdout, READ_CAP);
            let _ = io::Read::read_to_end(&mut limited, &mut bytes);
            // 接收端在超时路径上可能已经走了：发送失败无所谓。
            let _ = tx.send(bytes);
        });

        let deadline = Instant::now() + timeout;
        let mut status = None;
        let mut data: Option<Vec<u8>> = None;
        while status.is_none() || data.is_none() {
            if status.is_none() {
                match child.try_wait() {
                    Ok(found) => status = found,
                    Err(e) => {
                        // 早退也守纪律：先收进程组再返回（deadline 硬上界 + 整组回收
                        // 适用于每一条出口）；读线程随管道关闭自行收尾，不 join。
                        kill_group(&mut child);
                        return Err(e);
                    }
                }
            }
            if data.is_none() {
                data = match rx.try_recv() {
                    Ok(bytes) => Some(bytes),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    // 读线程死了：等价于没有数据（进程退出状态仍照常判定）。
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
                };
            }
            // 数据已经超限：不必再等退出状态（写端多半正堵在满管道上）。
            if matches!(&data, Some(bytes) if bytes.len() > MAX_PAYLOAD) {
                break;
            }
            if status.is_some() && data.is_some() {
                break;
            }
            // 到点即收，不给读线程调度宽限：deadline 是硬上界；代价是「子进程恰在
            // 到点前一瞬退出、reader 还没被调度出 send」的窄窗口里可用数据被丢
            // （微秒~毫秒级，登记为取舍）。
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(WAIT_POLL);
        }
        if matches!(&data, Some(bytes) if bytes.len() > MAX_PAYLOAD) {
            kill_group(&mut child);
            let _ = reader.join();
            return Err(too_large(spec.program));
        }
        let (Some(status), Some(bytes)) = (status, data) else {
            // deadline 到了但还没配齐：超时是硬上界，**不 join** 读线程。
            kill_group(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} timed out after {} ms",
                    spec.program,
                    timeout.as_millis()
                ),
            ));
        };
        // 数据已回传：读线程正在收尾，join 即刻返回（有界）。
        let _ = reader.join();
        if status.success() {
            Ok(bytes)
        } else {
            Err(exited(spec.program, status))
        }
    }
}

/// Whether the process environment marks this as a remote session.
fn is_remote_session() -> bool {
    is_remote_session_with(|key| std::env::var(key).ok())
}

/// Injected lookup for testability — the real one reads the environment.
fn is_remote_session_with(env: impl Fn(&str) -> Option<String>) -> bool {
    REMOTE_SESSION_MARKERS
        .iter()
        .any(|marker| env(marker).is_some_and(|value| !value.is_empty()))
}

/// 把 runner 的 IO 错误归类成用户可见的失败面。
fn classify_io(error: io::Error) -> PasteFailure {
    match error.kind() {
        io::ErrorKind::NotFound => PasteFailure::Unavailable,
        io::ErrorKind::TimedOut => PasteFailure::Timeout,
        io::ErrorKind::FileTooLarge => PasteFailure::TooLarge,
        // 普通非零退出（「没有该表示」）：见 `exited`。
        io::ErrorKind::Unsupported => PasteFailure::NoRepresentation,
        _ => PasteFailure::Failed(error.to_string()),
    }
}

/// 读一次系统剪贴板（远程短路 → 图片 → 文本）。
///
/// 同步函数（子进程 + 等待），生产在阻塞池上跑；`platform` 与 `remote` 都由
/// 调用方注入，所以每一支流程都能在任一主机上测。
fn read_from_clipboard(
    runner: &impl CommandRunner,
    platform: Platform,
    remote: bool,
) -> Result<PasteOutcome, PasteFailure> {
    if remote {
        return Err(PasteFailure::Remote);
    }
    match platform {
        Platform::Macos => read_macos(runner),
        Platform::Linux => read_linux(runner),
        Platform::Other => Err(PasteFailure::Unavailable),
    }
}

/// macOS：`osascript` 图片（PNG）→ `pbpaste` 文本。
///
/// 图片失败不阻断文本（纯文本剪贴板是常态）；超限硬拒绝（文件已删，toast 由
/// 上层给），其余失败保留到最后由 [`settle`] 裁决。
fn read_macos(runner: &impl CommandRunner) -> Result<PasteOutcome, PasteFailure> {
    let mut failure: Option<PasteFailure> = None;
    match read_macos_image(runner) {
        Ok(Some(bytes)) => {
            return Ok(PasteOutcome::Image {
                bytes,
                mime: "image/png".to_string(),
            });
        }
        Ok(None) => {}
        Err(e @ PasteFailure::TooLarge) => return Err(e),
        Err(e) => failure = Some(e),
    }
    match read_text(runner, &CommandSpec::stdout("/usr/bin/pbpaste", &[])) {
        Ok(Some(text)) => return Ok(PasteOutcome::Text(text)),
        Ok(None) => {}
        Err(e) => failure = failure.or(Some(e)),
    }
    settle(failure)?;
    Ok(PasteOutcome::Empty)
}

/// macOS 图片读取：`osascript` 写临时文件 → 读文件。
///
/// 非零退出（含实测的「纯文本剪贴板 → 0 字节文件 + exit 1」）不是失败：0 字节 /
/// 文件缺失就是「剪贴板没有图片」，照常走文本。只有超时 / 缺命令 / 超限，以及临时
/// 路径预占不了，按失败报。
fn read_macos_image(runner: &impl CommandRunner) -> Result<Option<Vec<u8>>, PasteFailure> {
    // 路径先预占（`create_new`，见 `temp_image_path`）；预占不了就没什么可读。
    let path = match temp_image_path() {
        Ok(path) => path,
        Err(e) => return Err(classify_io(e)),
    };
    let spec = macos_image_spec(&path);
    match runner.run(&spec, IMAGE_TIMEOUT) {
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            remove_temp_file(&path);
            Err(PasteFailure::Timeout)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            remove_temp_file(&path);
            Err(PasteFailure::Unavailable)
        }
        Err(e) => {
            tracing::debug!(error = %e, "osascript did not produce a picture");
            read_and_remove(&path)
        }
        Ok(_) => read_and_remove(&path),
    }
}

/// Linux：`wl-paste` / `xclip` 图片 → `wl-paste` / `xclip` / `xsel` 文本。
fn read_linux(runner: &impl CommandRunner) -> Result<PasteOutcome, PasteFailure> {
    let mut failure: Option<PasteFailure> = None;
    match read_linux_image(runner) {
        Ok(Some((mime, bytes))) => return Ok(PasteOutcome::Image { bytes, mime }),
        Ok(None) => {}
        Err(e @ PasteFailure::TooLarge) => return Err(e),
        Err(e) => failure = Some(e),
    }
    match read_linux_text(runner) {
        Ok(Some(text)) => return Ok(PasteOutcome::Text(text)),
        Ok(None) => {}
        Err(e) => failure = failure.or(Some(e)),
    }
    settle(failure)?;
    Ok(PasteOutcome::Empty)
}

/// 图片：`wl-paste`（Wayland）→ `xclip`（X11）。
///
/// 「第一个跑成功的工具是权威的」：它列出的类型就是用户会话里真实的剪贴板，
/// 空清单是「这个会话没有图片」，而不是「换一个显示协议再问一遍」（两个显示协议
/// 同时活跃时 Wayland 侧才是主剪贴板）。工具没装 / 超时才落到下一档并记账；普通
/// 非零退出归「没有图片」（空剪贴板与类型不可用都是这种退出），全部如此收场时就是
/// 干净的 `Ok(None)`，不是失败。
fn read_linux_image(
    runner: &impl CommandRunner,
) -> Result<Option<(String, Vec<u8>)>, PasteFailure> {
    let mut failure: Option<PasteFailure> = None;
    for tool in [LinuxImageTool::WlPaste, LinuxImageTool::Xclip] {
        match image_via_linux_tool(runner, tool) {
            Ok(found) => return Ok(found),
            Err(e @ PasteFailure::TooLarge) => return Err(e),
            Err(e) => {
                tracing::debug!(tool = tool.program(), error = %e, "picture reader unavailable");
                failure = failure.or(Some(e));
            }
        }
    }
    settle(failure)?;
    Ok(None)
}

/// 一个 Linux 图片工具的一趟：列类型 → 选已知 MIME → 取字节。
fn image_via_linux_tool(
    runner: &impl CommandRunner,
    tool: LinuxImageTool,
) -> Result<Option<(String, Vec<u8>)>, PasteFailure> {
    let listing = runner
        .run(&tool.list_spec(), IMAGE_TIMEOUT)
        .map_err(classify_io)?;
    let types: Vec<String> = String::from_utf8_lossy(&listing)
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect();
    let Some(mime) = choose_image_mime(&types) else {
        return Ok(None);
    };
    match runner.run(&tool.read_spec(&mime), IMAGE_TIMEOUT) {
        Ok(bytes) if bytes.is_empty() => Ok(None),
        Ok(bytes) if bytes.len() > MAX_PAYLOAD => Err(PasteFailure::TooLarge),
        Ok(bytes) => Ok(Some((mime, bytes))),
        Err(e) if e.kind() == io::ErrorKind::TimedOut => Err(PasteFailure::Timeout),
        Err(e) if e.kind() == io::ErrorKind::FileTooLarge => Err(PasteFailure::TooLarge),
        // 清单里有、读却失败：最可能是表示在两次调用之间消失（剪贴板易主）——
        // 按「没有图片」处理，文本照常继续。
        Err(e) => {
            tracing::debug!(tool = tool.program(), error = %e, "picture read found no data");
            Ok(None)
        }
    }
}

/// 文本：`wl-paste` → `xclip` → `xsel`，第一个跑成功的命令是权威的
/// （它说没有文本就是没有，不再问下一个工具）；全部只是普通失败时收场也是
/// 「没有文本」（空剪贴板在三个工具上都是一次普通失败），只有缺命令 / 超时才上报。
fn read_linux_text(runner: &impl CommandRunner) -> Result<Option<String>, PasteFailure> {
    let mut failure: Option<PasteFailure> = None;
    for spec in linux_text_specs() {
        match read_text(runner, &spec) {
            Ok(found) => return Ok(found),
            // 超限是硬拒绝（与图片链 `read_linux_image` 对齐）：后面的工具
            // 「成功但空」（`Ok(None)`）不能把已经读到的超限事实掩蔽成 Empty ——
            // 那样用户看到的是一句误导性的「没有文本或图片」。
            Err(e @ PasteFailure::TooLarge) => return Err(e),
            Err(e) => {
                tracing::debug!(program = spec.program, error = %e, "text reader unavailable");
                failure = failure.or(Some(e));
            }
        }
    }
    settle(failure)?;
    Ok(None)
}

/// 跑一条文本命令：空输出 = 没有文本（合法），非空按 UTF-8 宽容解码
/// （与 OSC 5522 通道的文本口径一致）。
///
/// 普通非零退出**不是**通道失败：读取器用它表示「这个剪贴板没有文本表示」
/// （`wl-paste` 在空剪贴板 / 无选中时就是 `Nothing is copied` + exit 1），所以链条
/// 要继续试下一个工具，但也不该把它升级成定向失败 —— 由 [`settle`] 统一收场。
///
/// 上限判在**返回之前**：超限文本一旦返回，上游会整段插进 composer（5522 通道对
/// 同一份内容给的是 `TooLarge` toast，两条通道对同一手势必须一致）。
fn read_text(
    runner: &impl CommandRunner,
    spec: &CommandSpec,
) -> Result<Option<String>, PasteFailure> {
    match runner.run(spec, TEXT_TIMEOUT) {
        Ok(bytes) if bytes.len() > MAX_PAYLOAD => Err(PasteFailure::TooLarge),
        Ok(bytes) if bytes.is_empty() => Ok(None),
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) => Err(classify_io(e)),
    }
}

/// 收场规则：**结构性失败与真实错误**（缺命令 / 超时 / 超限 / `Failed`）都要上报 ——
/// 超限的文案与 5522 通道一字不差，`Failed`（临时文件读不动、读任务 panic 等）是
/// 真实故障，不能被粉饰成「剪贴板是空的」。
///
/// 「没有该表示」（普通非零退出：空剪贴板在各平台读取器上都是一次普通退出）与
/// 「什么都没读到」才归空结果 —— 把读取器的退出码原样抛给用户会得到误导性的
/// 「读取失败」toast。
fn settle(failure: Option<PasteFailure>) -> Result<(), PasteFailure> {
    match failure {
        Some(
            e @ (PasteFailure::Unavailable
            | PasteFailure::Timeout
            | PasteFailure::TooLarge
            | PasteFailure::Failed(_)),
        ) => Err(e),
        _ => Ok(()),
    }
}

/// 从类型清单里挑一个已知可解码的图片 MIME（偏好序见 [`KNOWN_IMAGE_MIMES`]）。
///
/// 表外的 `image/*`（avif / heic…）不选：读回来也解不了（与 OSC 5522 通道的
/// `choose_mime` 同一条规则）。比较前先过 [`normalize_mime`]（X11 target 与
/// Wayland MIME 都可能带参数 / 大小写差异）。
fn choose_image_mime(types: &[String]) -> Option<String> {
    let bases: Vec<String> = types.iter().map(|t| normalize_mime(t)).collect();
    KNOWN_IMAGE_MIMES
        .iter()
        .find(|want| bases.iter().any(|base| base == *want))
        .map(|want| (*want).to_string())
}

/// 读临时文件 → 按上限裁决 → **无论成败都删文件**（on-error 也删，见模块文档）。
fn read_and_remove(path: &Path) -> Result<Option<Vec<u8>>, PasteFailure> {
    let outcome = read_capped(path);
    remove_temp_file(path);
    outcome
}

/// 上限判定放在 `read` 之前（`metadata`）：拒绝一张超限图不必把它整个读进内存。
fn read_capped(path: &Path) -> Result<Option<Vec<u8>>, PasteFailure> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() == 0 => Ok(None),
        Ok(meta) if meta.len() > MAX_PAYLOAD as u64 => Err(PasteFailure::TooLarge),
        Ok(_) => match std::fs::read(path) {
            Ok(bytes) if bytes.is_empty() => Ok(None),
            Ok(bytes) if bytes.len() > MAX_PAYLOAD => Err(PasteFailure::TooLarge),
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) => Err(PasteFailure::Failed(format!(
                "cannot read the clipboard temp file: {e}"
            ))),
        },
        // 文件根本不存在（脚本在 write 之前就失败）＝没有图片。
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PasteFailure::Failed(format!(
            "cannot stat the clipboard temp file: {e}"
        ))),
    }
}

/// 删除临时文件；失败只记日志（调用方正拿着结果返回，不能因清理报错）。
fn remove_temp_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "cannot remove the clipboard temp file");
        }
    }
}

/// 读系统剪贴板：远程短路 → 图片 → 文本，整个同步流程都在阻塞池上。
///
/// 事件循环只 await 这一个结果；每条命令的超时都是**硬上界**（超时即杀进程组并返回，
/// 不 join 读线程，见 [`ProcessCommandRunner`]），所以最坏时长 = 各命令超时之和：
/// macOS 5 s（图片）+ 1 s（文本）；Linux 2 个图片工具 ×（listing 5 s + read 5 s）+
/// 3 个文本工具 × 1 s ≈ 23 s —— 期间 UI 不冻，只是这段时间内不回粘贴结果。
///
/// **读取期间没有「进行中」提示**（取舍：正常路径毫秒级、最坏时长如上已量化，加提示
/// 要多一份跨窗口状态，收益低）；TUI 侧对连击做合并 —— 一次只跑一条读取链，落地
/// （成功或失败）后恢复可读，见 `app::runner` 的在飞闸门。
pub(crate) async fn read_clipboard() -> Result<PasteOutcome, PasteFailure> {
    let remote = is_remote_session();
    let platform = host_platform();
    tokio::task::spawn_blocking(move || {
        read_from_clipboard(&ProcessCommandRunner, platform, remote)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("clipboard read task failed: {e}");
        Err(PasteFailure::Failed(
            "clipboard read task failed".to_string(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 一次命令调用的记录（程序 / 参数 / 超时 / 临时文件出口）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        program: &'static str,
        args: Vec<String>,
        timeout: Duration,
        image_file: Option<PathBuf>,
    }

    /// 注入行为：把一条命令变成它的结果（写文件 / 报错 / 返回字节）。
    type Respond = Box<dyn Fn(&CommandSpec) -> io::Result<Vec<u8>>>;

    /// 注入的 runner：记录调用，行为由闭包决定（写文件 / 报错 / 返回字节）。
    struct FakeRunner {
        respond: Respond,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeRunner {
        fn new(respond: impl Fn(&CommandSpec) -> io::Result<Vec<u8>> + 'static) -> Self {
            Self {
                respond: Box::new(respond),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn called(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|c| c.program.to_string())
                .collect()
        }

        fn call(&self, n: usize) -> Call {
            self.calls.borrow().get(n).cloned().unwrap_or_else(|| {
                panic!("expected a call #{n}; got {:?}", self.calls.borrow());
            })
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, spec: &CommandSpec, timeout: Duration) -> io::Result<Vec<u8>> {
            self.calls.borrow_mut().push(Call {
                program: spec.program,
                args: spec.args.clone(),
                timeout,
                image_file: spec.image_file.clone(),
            });
            (self.respond)(spec)
        }
    }

    /// osascript 的「没有图片」形态：非零退出 + 0 字节文件（实测）。
    fn osascript_without_picture(spec: &CommandSpec) -> io::Result<Vec<u8>> {
        let path = spec.image_file.as_ref().expect("osascript writes a file");
        std::fs::write(path, b"").expect("touch the temp file");
        Err(plain_exit("osascript"))
    }

    fn not_found(program: &str) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, format!("{program} not installed"))
    }

    /// 读取器普通非零退出的真 runner 形态（`ErrorKind::Unsupported`，见 `exited`）。
    fn plain_exit(program: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{program} exited with exit status: 1"),
        )
    }

    #[test]
    fn remote_sessions_short_circuit_before_any_spawn() {
        let runner = FakeRunner::new(|spec| panic!("remote must not spawn {}", spec.program));
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, true),
            Err(PasteFailure::Remote)
        );
        assert!(runner.calls().is_empty(), "no child process over SSH");
    }

    #[test]
    fn remote_session_detection_reads_the_markers() {
        assert!(!is_remote_session_with(|_| None));
        assert!(!is_remote_session_with(|_| Some(String::new())));
        for marker in REMOTE_SESSION_MARKERS {
            assert!(
                is_remote_session_with(
                    |key| (key == marker).then(|| "10.0.0.1 22 10.0.0.2 55".to_string())
                ),
                "{marker} must mark the session as remote"
            );
        }
    }

    #[test]
    fn every_failure_has_a_directed_message() {
        let failures = [
            PasteFailure::Remote,
            PasteFailure::Unavailable,
            PasteFailure::Timeout,
            PasteFailure::TooLarge,
            PasteFailure::NoRepresentation,
            PasteFailure::Failed("boom".to_string()),
        ];
        for failure in failures {
            assert!(!failure.message().is_empty(), "{failure:?}");
        }
        assert!(PasteFailure::Remote.message().contains("SSH"));
        assert_eq!(PasteFailure::Timeout.message(), "Clipboard read timed out");
        assert_eq!(
            PasteFailure::NoRepresentation.message(),
            "Clipboard has no text or image"
        );
        assert!(
            PasteFailure::Failed("boom".to_string())
                .message()
                .contains("boom")
        );
    }

    /// `settle` 只放行「没有该表示」与「什么都没读到」；真实错误（`Failed`）与结构性
    /// 失败都要上报 —— 临时文件读不动不能被粉饰成「剪贴板是空的」。
    #[test]
    fn settle_upgrades_real_failures_but_not_missing_representations() {
        assert!(settle(None).is_ok());
        assert!(settle(Some(PasteFailure::NoRepresentation)).is_ok());
        assert!(
            settle(Some(PasteFailure::Failed(
                "cannot read the clipboard temp file".to_string()
            )))
            .is_err()
        );
        for failure in [
            PasteFailure::Unavailable,
            PasteFailure::Timeout,
            PasteFailure::TooLarge,
        ] {
            assert!(settle(Some(failure.clone())).is_err(), "{failure:?}");
        }
    }

    #[test]
    fn temp_paths_are_unique_and_named_after_the_process() {
        let a = temp_image_path().expect("reserve a");
        let b = temp_image_path().expect("reserve b");
        assert_ne!(a, b);
        assert!(a.exists() && b.exists(), "预占 = 文件真的被创建");
        assert!(
            a.to_string_lossy()
                .contains(&std::process::id().to_string()),
            "the name carries the pid: {}",
            a.display()
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// 路径预占：候选名已被占用（预置的符号链接 / 上次残留）时换下一个，
    /// 不把已有路径交给 osascript 覆写。
    #[test]
    fn a_taken_temp_name_is_skipped_instead_of_reused() {
        let stem = format!("wing-paste-test-{}-", std::process::id());
        let taken = std::env::temp_dir().join(format!("{stem}0.png"));
        let _ = std::fs::remove_file(&taken);
        std::fs::write(&taken, b"occupied").expect("occupy the first candidate");

        let reserved =
            reserve_temp_file((0..4).map(|n| std::env::temp_dir().join(format!("{stem}{n}.png"))))
                .expect("reserve");
        assert!(
            reserved.ends_with(format!("{stem}1.png")),
            "已占用的名字必须被跳过：{}",
            reserved.display()
        );
        assert!(reserved.exists(), "预占 = 文件真的被创建");
        assert_eq!(
            std::fs::read(&taken).expect("the taken path is untouched"),
            b"occupied"
        );

        let _ = std::fs::remove_file(&taken);
        let _ = std::fs::remove_file(&reserved);
    }

    #[test]
    fn macos_image_script_is_pinned() {
        let spec = macos_image_spec(Path::new("/tmp/wing-paste-1-0.png"));
        assert_eq!(spec.program, "/usr/bin/osascript");
        assert_eq!(
            spec.args,
            vec![
                "-e",
                "set f to open for access POSIX file \"/tmp/wing-paste-1-0.png\" with write permission",
                "-e",
                "set eof f to 0",
                "-e",
                "write (the clipboard as «class PNGf») to f",
                "-e",
                "close access f",
            ]
        );
        assert_eq!(
            spec.image_file.as_deref(),
            Some(Path::new("/tmp/wing-paste-1-0.png"))
        );
        // 引号 / 反斜杠在 AppleScript 字面量里必须转义。
        let weird = macos_image_spec(Path::new("/tmp/a\"b\\c.png"));
        assert!(
            weird.args[1].contains("POSIX file \"/tmp/a\\\"b\\\\c.png\""),
            "{:?}",
            weird.args[1]
        );
    }

    #[test]
    fn macos_reads_the_picture_first_and_removes_the_temp_file() {
        // 本模块不解码：字节原样传递（压缩器才嗅探格式）。
        let png = b"\x89PNG\r\n\x1a\nfixture".to_vec();
        let written = png.clone();
        let runner = FakeRunner::new(move |spec| match spec.program {
            "/usr/bin/osascript" => {
                let path = spec.image_file.as_ref().expect("osascript writes a file");
                assert!(path.exists(), "路径先被本进程预占（N4 加固）");
                std::fs::write(path, &written).expect("write the fixture");
                Ok(Vec::new())
            }
            other => panic!("text must not be asked once the picture landed: {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Ok(PasteOutcome::Image {
                bytes: png,
                mime: "image/png".to_string(),
            })
        );
        let call = runner.call(0);
        assert_eq!(call.timeout, IMAGE_TIMEOUT, "图片命令用图片超时");
        let path = call.image_file.clone().expect("temp file");
        assert!(
            call.args
                .iter()
                .any(|arg| arg.contains(&path.display().to_string())),
            "the script must point at the runner's file: {call:?}"
        );
        assert!(!path.exists(), "temp file must be removed after the read");
    }

    #[test]
    fn macos_falls_back_to_text_when_the_clipboard_has_no_picture() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => osascript_without_picture(spec),
            "/usr/bin/pbpaste" => Ok("hello 粘贴".as_bytes().to_vec()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Ok(PasteOutcome::Text("hello 粘贴".to_string()))
        );
        assert_eq!(runner.called(), ["/usr/bin/osascript", "/usr/bin/pbpaste"]);
        assert_eq!(runner.call(1).timeout, TEXT_TIMEOUT, "文本命令用文本超时");
        let path = runner.call(0).image_file.clone().expect("temp file");
        assert!(!path.exists(), "the 0-byte temp file must be removed too");
    }

    #[test]
    fn macos_reports_empty_when_neither_side_has_content() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => osascript_without_picture(spec),
            "/usr/bin/pbpaste" => Ok(Vec::new()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Ok(PasteOutcome::Empty)
        );
    }

    #[test]
    fn a_slow_picture_read_does_not_swallow_the_text() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => Err(io::Error::new(io::ErrorKind::TimedOut, "wedged")),
            "/usr/bin/pbpaste" => Ok(b"still here".to_vec()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Ok(PasteOutcome::Text("still here".to_string()))
        );
    }

    #[test]
    fn macos_reports_the_picture_failure_when_text_is_empty_too() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => {
                // 超时路径也要删临时文件：让脚本「已经写了半截」再超时。
                let path = spec.image_file.as_ref().expect("temp file");
                std::fs::write(path, b"partial").expect("touch the temp file");
                Err(io::Error::new(io::ErrorKind::TimedOut, "wedged"))
            }
            "/usr/bin/pbpaste" => Ok(Vec::new()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Err(PasteFailure::Timeout),
            "空剪贴板不能把一次超时粉饰成 Empty"
        );
        let path = runner.call(0).image_file.clone().expect("temp file");
        assert!(!path.exists(), "超时分支也要把临时文件删掉");
    }

    #[test]
    fn macos_reports_unavailable_only_when_the_text_reader_also_fails() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => Err(not_found("osascript")),
            "/usr/bin/pbpaste" => Err(not_found("pbpaste")),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Err(PasteFailure::Unavailable)
        );

        // 只剩 pbpaste：文本照读（图片那半的失败不进吐司）。
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => Err(not_found("osascript")),
            "/usr/bin/pbpaste" => Ok(b"text".to_vec()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Ok(PasteOutcome::Text("text".to_string()))
        );
    }

    #[test]
    fn macos_rejects_an_oversized_temp_file_and_still_removes_it() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => {
                let path = spec.image_file.as_ref().expect("temp file");
                // 稀疏文件：不真占 64 MiB。
                let file = std::fs::File::create(path).expect("create");
                file.set_len(MAX_PAYLOAD as u64 + 1).expect("set_len");
                Ok(Vec::new())
            }
            other => panic!("an oversized picture must not fall through to text: {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Err(PasteFailure::TooLarge)
        );
        let path = runner.call(0).image_file.clone().expect("temp file");
        assert!(!path.exists(), "the rejected temp file must be removed");
    }

    #[test]
    fn linux_reads_the_picture_through_the_type_listing() {
        let png = b"\x89PNG-linux".to_vec();
        let fixture = png.clone();
        let runner = FakeRunner::new(move |spec| match (spec.program, spec.args.join(" ")) {
            ("wl-paste", ref args) if args == "--list-types" => {
                Ok(b"text/plain\nimage/tiff\nimage/png\n".to_vec())
            }
            ("wl-paste", ref args) if args == "--no-newline --type image/png" => {
                Ok(fixture.clone())
            }
            other => panic!("unexpected command {other:?}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Ok(PasteOutcome::Image {
                bytes: png,
                mime: "image/png".to_string(),
            }),
            "known-set priority picks png over tiff and never reads text"
        );
        assert_eq!(runner.calls().len(), 2, "one listing + one read");
        assert_eq!(runner.call(0).timeout, IMAGE_TIMEOUT);
    }

    #[test]
    fn linux_skips_undecodable_image_types_and_reads_text_instead() {
        let runner = FakeRunner::new(|spec| match (spec.program, spec.args.join(" ")) {
            ("wl-paste", ref args) if args == "--list-types" => {
                Ok(b"image/avif\nimage/heic\ntext/plain\n".to_vec())
            }
            ("wl-paste", ref args) if args == "--no-newline" => Ok(b"hi".to_vec()),
            other => panic!("unexpected command {other:?}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Ok(PasteOutcome::Text("hi".to_string())),
            "an undecodable image/* must not shadow text/plain"
        );
        assert_eq!(runner.called(), ["wl-paste", "wl-paste"]);
        assert_eq!(runner.call(1).timeout, TEXT_TIMEOUT);
    }

    #[test]
    fn linux_falls_back_from_wl_paste_to_xclip() {
        let runner = FakeRunner::new(|spec| match (spec.program, spec.args.join(" ")) {
            ("wl-paste", _) => Err(not_found("wl-paste")),
            ("xclip", ref args) if args == "-selection clipboard -t TARGETS -o" => {
                Ok(b"image/png\n".to_vec())
            }
            ("xclip", ref args) if args == "-selection clipboard -t image/png -o" => {
                Ok(b"PNG-bytes".to_vec())
            }
            other => panic!("unexpected command {other:?}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Ok(PasteOutcome::Image {
                bytes: b"PNG-bytes".to_vec(),
                mime: "image/png".to_string(),
            })
        );
        assert_eq!(runner.called(), ["wl-paste", "xclip", "xclip"]);
    }

    #[test]
    fn linux_text_falls_through_to_xsel() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "wl-paste" => Err(not_found("wl-paste")),
            "xclip" => Err(not_found("xclip")),
            "xsel" => Ok(b"last resort".to_vec()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Ok(PasteOutcome::Text("last resort".to_string()))
        );
        // 图片阶段先问过 wl-paste / xclip 的类型清单，然后才是文本三档。
        assert_eq!(
            runner.called(),
            ["wl-paste", "xclip", "wl-paste", "xclip", "xsel"]
        );
    }

    #[test]
    fn linux_reports_empty_when_all_readers_agree_there_is_nothing() {
        let runner = FakeRunner::new(|spec| match (spec.program, spec.args.join(" ")) {
            ("wl-paste", ref args) if args == "--list-types" => Ok(Vec::new()),
            ("wl-paste", ref args) if args == "--no-newline" => Ok(Vec::new()),
            other => panic!("unexpected command {other:?}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Ok(PasteOutcome::Empty)
        );
    }

    /// 清单承诺了可解码的 MIME、真正读却普通失败（表示在两次调用之间消失）：
    /// 按「没有图片」收场、**不阻断文本**，也不去问下一个图片工具。
    #[test]
    fn linux_falls_back_to_text_when_the_promised_type_cannot_be_read() {
        let runner = FakeRunner::new(|spec| match (spec.program, spec.args.join(" ")) {
            ("wl-paste", ref args) if args == "--list-types" => Ok(b"image/png\n".to_vec()),
            ("wl-paste", ref args) if args == "--no-newline --type image/png" => {
                Err(plain_exit("wl-paste"))
            }
            ("wl-paste", ref args) if args == "--no-newline" => Ok(b"text wins".to_vec()),
            other => panic!("unexpected command {other:?}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Ok(PasteOutcome::Text("text wins".to_string()))
        );
        assert_eq!(
            runner.called(),
            ["wl-paste", "wl-paste", "wl-paste"],
            "list → read（失败）→ 文本；不落到 xclip"
        );
    }

    /// Linux 图片读取超限（runner 的 `FileTooLarge`）是**硬拒绝**：与 macOS 临时文件
    /// 超限同一条失败面，不能静默按「没有图片」处理（否则 >64 MiB 的图会变成
    /// 误导性的「没有文本或图片」）。
    #[test]
    fn an_oversized_linux_picture_is_rejected_not_ignored() {
        let runner = FakeRunner::new(|spec| match (spec.program, spec.args.join(" ")) {
            ("wl-paste", ref args) if args == "--list-types" => Ok(b"image/png\n".to_vec()),
            ("wl-paste", ref args) if args == "--no-newline --type image/png" => {
                Err(io::Error::new(io::ErrorKind::FileTooLarge, "capped"))
            }
            other => panic!("unexpected command {other:?}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Err(PasteFailure::TooLarge)
        );
        assert_eq!(
            runner.called(),
            ["wl-paste", "wl-paste"],
            "超限即收场，不再问文本、也不落到 xclip"
        );
    }

    /// 文本链的超限同样**硬拒绝**：即使后面的工具「成功但空」（`Ok(None)`，空剪贴板
    /// 在某档工具上的形态），也不能把已经读到的超限事实掩蔽成 Empty —— 否则用户看到
    /// 的是一句误导性的「没有文本或图片」，而不是「内容过大」。
    #[test]
    fn linux_text_oversize_is_not_masked_by_a_later_empty_tool() {
        let runner = FakeRunner::new(|spec| match (spec.program, spec.args.join(" ")) {
            ("wl-paste", ref args) if args == "--list-types" => Ok(Vec::new()),
            ("wl-paste", ref args) if args == "--no-newline" => {
                Err(io::Error::new(io::ErrorKind::FileTooLarge, "capped"))
            }
            ("xclip", ref args) if args == "-selection clipboard -o" => Ok(Vec::new()),
            other => panic!("unexpected command {other:?}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Err(PasteFailure::TooLarge)
        );
        assert_eq!(
            runner.called(),
            ["wl-paste", "wl-paste"],
            "超限即收场，下一个工具的空结果不能把它掩蔽成 Empty"
        );
    }

    /// 空剪贴板在 Linux 上是一条**普通非零退出**（`wl-paste`：`Nothing is copied` +
    /// exit 1；X11 工具同类），不是通道失败：工具全按非零退出时收场必须是 Empty。
    #[test]
    fn linux_without_a_selection_is_empty_not_a_failure() {
        let runner = FakeRunner::new(|spec| Err(plain_exit(spec.program)));
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Ok(PasteOutcome::Empty)
        );
        assert_eq!(
            runner.called(),
            ["wl-paste", "xclip", "wl-paste", "xclip", "xsel"],
            "每个工具都问过（图片两档 + 文本三档）"
        );
    }

    /// macOS 的文本读取器普通非零退出同理：空结果（不是 Failed）。
    #[test]
    fn macos_treats_a_plain_text_reader_failure_as_no_text() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => osascript_without_picture(spec),
            "/usr/bin/pbpaste" => Err(plain_exit("pbpaste")),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Ok(PasteOutcome::Empty)
        );
    }

    /// 真实错误（这里是读取器起不来：PermissionDenied）**不是**「没有内容」：
    /// 收场必须是错误 —— 否则用户看到的是误导性的「剪贴板没有文本或图片」。
    #[test]
    fn a_real_reader_failure_is_not_masked_as_an_empty_clipboard() {
        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => osascript_without_picture(spec),
            "/usr/bin/pbpaste" => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pbpaste is not executable",
            )),
            other => panic!("unexpected program {other}"),
        });
        match read_from_clipboard(&runner, Platform::Macos, false) {
            Err(PasteFailure::Failed(reason)) => {
                assert!(reason.contains("not executable"), "{reason}");
            }
            other => panic!("a real failure must stay a failure: {other:?}"),
        }
    }

    /// 文本也受 64 MiB 上限：超限字节**在返回 / 插入之前**被拒绝，文案与
    /// OSC 5522 通道逐字一致 —— 两条通道对同一份内容给同一种结果。
    ///
    /// 两条防线都钉住：读取器（或任何实现）给出超限的 `Ok(bytes)`；以及 runner 用
    /// `ErrorKind::FileTooLarge` 上报超限（真 runner 的上限截断走这一条）。
    #[test]
    fn oversized_text_is_rejected_before_it_can_reach_the_composer() {
        let message = PasteFailure::TooLarge.message();
        assert_eq!(message, "Clipboard content is too large to paste");

        let huge = vec![b'x'; MAX_PAYLOAD + 1];
        let runner = FakeRunner::new(move |spec| match spec.program {
            "/usr/bin/osascript" => osascript_without_picture(spec),
            "/usr/bin/pbpaste" => Ok(huge.clone()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Err(PasteFailure::TooLarge),
            "超限文本不能落进草稿"
        );

        let runner = FakeRunner::new(|spec| match spec.program {
            "/usr/bin/osascript" => osascript_without_picture(spec),
            "/usr/bin/pbpaste" => Err(io::Error::new(io::ErrorKind::FileTooLarge, "capped")),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Err(PasteFailure::TooLarge),
            "runner 的 FileTooLarge 归类为 TooLarge"
        );
    }

    /// 真进程（unix）：成功路径拿回 stdout；普通超时在**有界时间内**返回 TimedOut
    /// 且把进程收干净。
    #[test]
    #[cfg(unix)]
    fn the_real_runner_returns_stdout_and_times_out_a_plain_sleeper() {
        let ok = ProcessCommandRunner.run(
            &CommandSpec::stdout("/bin/echo", &["hi"]),
            Duration::from_secs(2),
        );
        assert_eq!(ok.expect("echo"), b"hi\n");

        let started = Instant::now();
        let timed_out = ProcessCommandRunner.run(
            &CommandSpec::stdout("/bin/sleep", &["10"]),
            Duration::from_millis(200),
        );
        assert!(
            matches!(&timed_out, Err(e) if e.kind() == io::ErrorKind::TimedOut),
            "{timed_out:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "超时是有界返回：{:?}",
            started.elapsed()
        );
    }

    /// 真进程（unix）：直接子进程退出后**孙进程**还持有 stdout 写端时，读侧的 EOF
    /// 不会来 —— 但「每条命令的超时」仍是硬上界：到点杀**进程组**、返回 TimedOut，
    /// 而不是卡在 reader.join 上（`wl-paste` fork 的 `cat` 就是这种形态）。
    /// 孙进程的 pid 由脚本写进文件：断言它在超时后真的不存在（整组被杀，不是只杀
    /// 直接子进程），测试不留活进程。
    #[test]
    #[cfg(unix)]
    fn a_grandchild_holding_stdout_cannot_stretch_the_deadline() {
        let pidfile = std::env::temp_dir().join(format!("wing-pg-test-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let script = format!("sleep 30 & echo $! > {}; ", pidfile.display());

        let started = Instant::now();
        let result = ProcessCommandRunner.run(
            &CommandSpec::stdout("/bin/sh", &["-c", &script]),
            // 给 sh 启动 + 写 pidfile 留足预算（极端负载的 CI 上 300ms 会 flake）；
            // 断言仍要求远小于 sleep 的 30 s。
            Duration::from_secs(1),
        );
        let elapsed = started.elapsed();
        assert!(
            matches!(&result, Err(e) if e.kind() == io::ErrorKind::TimedOut),
            "{result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "硬上界：{elapsed:?}（修复前这里会被 sleep 的写端拖到 30 s）"
        );

        let pid_text = std::fs::read_to_string(&pidfile).expect("the grandchild pid was written");
        let _ = std::fs::remove_file(&pidfile);
        let pid: libc::pid_t = pid_text.trim().parse().expect("pid");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "孙进程 {pid} 在超时后仍活着（killpg 没生效？）"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 真进程（unix）：输出超过上限时读取**在读满 `MAX_PAYLOAD + 1` 处停下**并报
    /// TooLarge，不把超限 payload 整个读进内存（`yes` 会一直写；唯一的出口是截断）。
    #[test]
    #[cfg(unix)]
    fn the_real_runner_stops_at_the_payload_cap() {
        let started = Instant::now();
        let result =
            ProcessCommandRunner.run(&CommandSpec::stdout("yes", &[]), Duration::from_secs(10));
        assert!(
            matches!(&result, Err(e) if e.kind() == io::ErrorKind::FileTooLarge),
            "{result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "必须在读满上限处停下，而不是等超时：{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn linux_without_any_reader_reports_unavailable() {
        let runner = FakeRunner::new(|spec| Err(not_found(spec.program)));
        assert_eq!(
            read_from_clipboard(&runner, Platform::Linux, false),
            Err(PasteFailure::Unavailable)
        );
    }

    #[test]
    fn the_other_platforms_are_unavailable_without_spawning() {
        let runner = FakeRunner::new(|spec| panic!("no helper on this platform: {}", spec.program));
        assert_eq!(
            read_from_clipboard(&runner, Platform::Other, false),
            Err(PasteFailure::Unavailable)
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn linux_command_tables_are_pinned() {
        assert_eq!(LinuxImageTool::WlPaste.list_spec().args, ["--list-types"]);
        assert_eq!(
            LinuxImageTool::WlPaste.read_spec("image/tiff").args,
            ["--no-newline", "--type", "image/tiff"]
        );
        assert_eq!(LinuxImageTool::WlPaste.program(), "wl-paste");
        assert_eq!(
            LinuxImageTool::Xclip.list_spec().args,
            ["-selection", "clipboard", "-t", "TARGETS", "-o"]
        );
        assert_eq!(
            LinuxImageTool::Xclip.read_spec("image/png").args,
            ["-selection", "clipboard", "-t", "image/png", "-o"]
        );
        let texts = linux_text_specs();
        let programs: Vec<&str> = texts.iter().map(|spec| spec.program).collect();
        assert_eq!(programs, ["wl-paste", "xclip", "xsel"]);
        assert_eq!(texts[0].args, ["--no-newline"]);
        assert_eq!(texts[2].args, ["--clipboard", "--output"]);
    }

    #[test]
    fn linux_known_mime_priority_is_png_first() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            choose_image_mime(&s(&["image/tiff", "image/png"])).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            choose_image_mime(&s(&["image/avif", "image/bmp"])).as_deref(),
            Some("image/bmp")
        );
        assert_eq!(choose_image_mime(&s(&["image/avif", "image/heic"])), None);
        assert_eq!(choose_image_mime(&s(&["text/plain"])), None);
    }

    #[test]
    fn types_are_normalized_before_matching() {
        assert_eq!(normalize_mime(" IMAGE/PNG;charset=utf-8 "), "image/png");
        assert_eq!(normalize_mime("image/tiff"), "image/tiff");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn host_platform_follows_the_host() {
        assert_eq!(host_platform(), Platform::Macos);
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn host_platform_follows_the_host() {
        assert_eq!(host_platform(), Platform::Linux);
    }

    /// 真机手工验收（默认 ignore）：**先存后恢复**地动一次真实剪贴板，覆盖
    /// osascript + 临时文件 + pbpaste 整条生产路径 —— CI 无剪贴板，不能自动跑。
    ///
    /// 本机跑（SSH 会话下要显式摘掉远程标记，否则 L1 按设计直接短路为 Remote）：
    ///
    /// ```text
    /// env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY \
    ///     cargo test -p wing --lib macos_real_clipboard_roundtrip_manual_only -- --ignored
    /// ```
    ///
    /// （原剪贴板不是纯文本 / 是图片时无法恢复，跑之前确认内容无所谓。）
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "reads and temporarily rewrites the real system clipboard"]
    async fn macos_real_clipboard_roundtrip_manual_only() {
        use std::process::Command;
        use std::process::Stdio;

        /// 意外 panic 也要把开场读到的文本放回剪贴板（Drop 在 unwind 中会跑）。
        struct RestoreClipboard(Vec<u8>);

        impl Drop for RestoreClipboard {
            fn drop(&mut self) {
                if let Ok(mut child) = Command::new("/usr/bin/pbcopy")
                    .stdin(Stdio::piped())
                    .spawn()
                    && let Some(stdin) = child.stdin.as_mut()
                {
                    let _ = io::Write::write_all(stdin, &self.0);
                    let _ = child.stdin.take();
                    let _ = child.wait();
                }
            }
        }

        assert!(
            !is_remote_session(),
            "SSH 会话下 L1 按设计关闭：按上面的 env -u … 形式摘掉远程标记再跑"
        );

        let pbcopy = |bytes: &[u8]| {
            let mut child = Command::new("/usr/bin/pbcopy")
                .stdin(Stdio::piped())
                .spawn()
                .expect("pbcopy");
            io::Write::write_all(child.stdin.as_mut().expect("stdin"), bytes).expect("write");
            assert!(child.wait().expect("wait").success());
        };

        let saved = Command::new("/usr/bin/pbpaste")
            .output()
            .expect("pbpaste")
            .stdout;
        let _restore = RestoreClipboard(saved);

        // 文本往返。
        pbcopy(b"wing clipboard selftest");
        assert_eq!(
            read_clipboard().await,
            Ok(PasteOutcome::Text("wing clipboard selftest".to_string()))
        );

        // 图片往返：真 PNG 进剪贴板（osascript 读文件），模块再取回来。
        let fixture =
            std::env::temp_dir().join(format!("wing-selftest-{}.png", std::process::id()));
        let buffer = image::RgbImage::from_pixel(4, 3, image::Rgb([200, 30, 30]));
        image::DynamicImage::ImageRgb8(buffer)
            .save_with_format(&fixture, image::ImageFormat::Png)
            .expect("write fixture");
        let script = format!(
            "set the clipboard to (read (POSIX file \"{}\") as «class PNGf»)",
            fixture.display()
        );
        assert!(
            Command::new("/usr/bin/osascript")
                .args(["-e", &script])
                .status()
                .expect("osascript")
                .success()
        );
        match read_clipboard().await {
            Ok(PasteOutcome::Image { bytes, mime }) => {
                assert_eq!(mime, "image/png");
                assert_eq!(&bytes[..4], b"\x89PNG", "the read bytes are a PNG");
            }
            other => panic!("expected the PNG roundtrip, got {other:?}"),
        }
        let _ = std::fs::remove_file(&fixture);
    }
}
