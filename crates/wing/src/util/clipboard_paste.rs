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
    /// 命令跑完了但不是一次成功的读取；`reason` 是给日志与 toast 的一句话。
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

/// 新建临时文件的唯一路径（`$TMPDIR` 下，进程 id + 计数器）。
fn temp_image_path() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("wing-paste-{}-{n}.png", std::process::id()))
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
    /// `Ok(stdout)` = the helper was spawned **and** exited successfully. A
    /// helper that cannot serve the read (an `osascript` on a text-only
    /// clipboard included) exits non-zero and comes back as `Err`; the callers
    /// decide per command whether that is a normal "no representation" signal
    /// or a failure. `ErrorKind::TimedOut` means the deadline was hit and the
    /// child was killed.
    fn run(&self, spec: &CommandSpec, timeout: Duration) -> io::Result<Vec<u8>>;
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
        let mut child = Command::new(spec.program)
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        // stdout must be drained concurrently: a large payload (pictures run to
        // 64 MiB) fills the pipe buffer and the helper would never exit — the
        // read would collapse into a timeout with the bytes stuck in the pipe.
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = io::Read::read_to_end(&mut stdout, &mut bytes);
            bytes
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} timed out after {} ms",
                        spec.program,
                        timeout.as_millis()
                    ),
                ));
            }
            std::thread::sleep(WAIT_POLL);
        };
        let bytes = reader.join().unwrap_or_default();
        if status.success() {
            Ok(bytes)
        } else {
            Err(io::Error::other(format!(
                "{} exited with {}",
                spec.program, status
            )))
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
/// 图片失败不阻断文本（纯文本剪贴板是常态）；但失败原因会保留 — 文本也为空时
/// 它才是给用户的解释，而不是一个不实的「剪贴板是空的」。
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
        // 超限是硬失败：拒绝 + 删除 + toast（设计 §3.2 的失败模式）。
        Err(e @ PasteFailure::TooLarge) => return Err(e),
        Err(e) => failure = Some(e),
    }
    match read_text(runner, &CommandSpec::stdout("/usr/bin/pbpaste", &[])) {
        Ok(Some(text)) => return Ok(PasteOutcome::Text(text)),
        Ok(None) => {}
        Err(e) => failure = failure.or(Some(e)),
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(PasteOutcome::Empty),
    }
}

/// macOS 图片读取：`osascript` 写临时文件 → 读文件。
///
/// 非零退出（含实测的「纯文本剪贴板 → 0 字节文件 + exit 1」）不是失败：0 字节 /
/// 文件缺失就是「剪贴板没有图片」，照常走文本。只有超时 / 缺命令 / 超限按失败报。
fn read_macos_image(runner: &impl CommandRunner) -> Result<Option<Vec<u8>>, PasteFailure> {
    let path = temp_image_path();
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
    match failure {
        Some(e) => Err(e),
        None => Ok(PasteOutcome::Empty),
    }
}

/// 图片：`wl-paste`（Wayland）→ `xclip`（X11）。
///
/// 「第一个跑成功的工具是权威的」：它列出的类型就是用户会话里真实的剪贴板，
/// 空清单是「这个会话没有图片」，而不是「换一个显示协议再问一遍」（两个显示协议
/// 同时活跃时 Wayland 侧才是主剪贴板）。工具没装 / 连不上（错误退出）才落到下一档。
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
    match failure {
        Some(e) => Err(e),
        None => Ok(None),
    }
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
    let bytes = runner
        .run(&tool.read_spec(&mime), IMAGE_TIMEOUT)
        .map_err(classify_io)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_PAYLOAD {
        return Err(PasteFailure::TooLarge);
    }
    Ok(Some((mime, bytes)))
}

/// 文本：`wl-paste` → `xclip` → `xsel`，第一个跑成功的命令是权威的
/// （它说没有文本就是没有，不再问下一个工具）。
fn read_linux_text(runner: &impl CommandRunner) -> Result<Option<String>, PasteFailure> {
    let mut failure: Option<PasteFailure> = None;
    for spec in linux_text_specs() {
        match read_text(runner, &spec) {
            Ok(found) => return Ok(found),
            Err(e) => {
                tracing::debug!(program = spec.program, error = %e, "text reader unavailable");
                failure = failure.or(Some(e));
            }
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// 跑一条文本命令：空输出 = 没有文本（合法），非空按 UTF-8 宽容解码
/// （与 OSC 5522 通道的文本口径一致）。
fn read_text(
    runner: &impl CommandRunner,
    spec: &CommandSpec,
) -> Result<Option<String>, PasteFailure> {
    match runner.run(spec, TEXT_TIMEOUT) {
        Ok(bytes) if bytes.is_empty() => Ok(None),
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) => Err(classify_io(e)),
    }
}

/// 从类型清单里挑一个已知可解码的图片 MIME（偏好序见 [`KNOWN_IMAGE_MIMES`]）。
///
/// 表外的 `image/*`（avif / heic…）不选：读回来也解不了（与 OSC 5522 通道的
/// `choose_mime` 同一条规则）。
fn choose_image_mime(types: &[String]) -> Option<String> {
    let bases: Vec<String> = types.iter().map(|t| normalize_type(t)).collect();
    KNOWN_IMAGE_MIMES
        .iter()
        .find(|want| bases.iter().any(|base| base == *want))
        .map(|want| (*want).to_string())
}

/// 类型串的归一形态：剥参数、去首尾空白、转小写（X11 target 与 Wayland MIME
/// 都可能带参数 / 大小写差异）。
fn normalize_type(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_ascii_lowercase()
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
/// 事件循环只 await 这一个结果；时长被每条命令的超时封顶（最坏 图片 5 s +
/// 文本 1 s，期间 UI 不冻）。
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
        Err(io::Error::other("osascript exited with exit status: 1"))
    }

    fn not_found(program: &str) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, format!("{program} not installed"))
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
            PasteFailure::Failed("boom".to_string()),
        ];
        for failure in failures {
            assert!(!failure.message().is_empty(), "{failure:?}");
        }
        assert!(PasteFailure::Remote.message().contains("SSH"));
        assert_eq!(PasteFailure::Timeout.message(), "Clipboard read timed out");
        assert!(
            PasteFailure::Failed("boom".to_string())
                .message()
                .contains("boom")
        );
    }

    #[test]
    fn temp_paths_are_unique_and_named_after_the_process() {
        let a = temp_image_path();
        let b = temp_image_path();
        assert_ne!(a, b);
        assert!(
            a.to_string_lossy()
                .contains(&std::process::id().to_string()),
            "the name carries the pid: {}",
            a.display()
        );
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
            "/usr/bin/osascript" => Err(io::Error::new(io::ErrorKind::TimedOut, "wedged")),
            "/usr/bin/pbpaste" => Ok(Vec::new()),
            other => panic!("unexpected program {other}"),
        });
        assert_eq!(
            read_from_clipboard(&runner, Platform::Macos, false),
            Err(PasteFailure::Timeout),
            "空剪贴板不能把一次超时粉饰成 Empty"
        );
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
        assert_eq!(normalize_type(" IMAGE/PNG;charset=utf-8 "), "image/png");
        assert_eq!(normalize_type("image/tiff"), "image/tiff");
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
