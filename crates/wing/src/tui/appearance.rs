//! 终端底色探测 —— `colors.preset: auto` 的唯一输入。
//!
//! 一问一答：写 `ESC]11;?BEL`，读终端回的 `ESC]11;rgb:RRRR/GGGG/BBBB`，按 WCAG 相对亮度
//! 判明暗（> 0.5 为亮底）。查询点由 [`crate::tui`] 的会话开启序列钉死：raw mode 已开
//! （应答要非规范模式读，见下面的 `tty`）、stdin 接驳尚未安装（fd 0 还是真实 tty）。答案进
//! 进程级静态（[`probe_once`]），第二段 TUI 与设置面重算直接复用，不再发查询。
//!
//! **代价**：探测窗口（≤ [`DETECT_TIMEOUT`]）内键入的按键会被本模块读走。与图像能力探测
//! （`ImageSupport::detect`）同款取舍 —— 启动期一次性、有界，且这是 fd 0 还没换手的唯一
//! 时点；事件源一旦起读，再读 stdin 就是抢键盘。
//!
//! **失败面**：非 tty、终端不答（多数复用器 / 部分 SSH）、应答不是 OSC 11、超时，一律
//! `None` —— 调用方回退 `Wing`（现状），不猜。

use std::io::IsTerminal;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use crate::config::Appearance;
use crate::config::ColorPreset;

/// 等应答的预算。也是探测窗口内最多丢掉的键入时长。
pub const DETECT_TIMEOUT: Duration = Duration::from_millis(150);

/// `ESC ] 11 ; ? BEL` —— 问终端「你的默认背景色是什么」。
const QUERY: &[u8] = b"\x1b]11;?\x07";

/// 应答头。OSC 11 的应答参数就是那个颜色。
const REPLY_HEAD: &[u8] = b"\x1b]11;";

/// 累积上限：正常应答不到 30 字节，超了说明进来的不是应答（窗口内的键入），放弃。
const REPLY_CAP: usize = 128;

/// 进程内唯一答案。`None`（探过但没有答案）与「还没探过」在读取侧不作区分：两者都
/// 意味着「回退 `Wing`」。
static DETECTED: OnceLock<Option<Appearance>> = OnceLock::new();

/// 问一次终端：背景是亮还是暗。
///
/// 只在 fd 0 是 tty 时尝试（否则查询字节会写进管道、应答永远不来）。写完查询后最多等
/// `timeout`；期间的键入会被读走（见模块文档）。
pub fn detect(timeout: Duration) -> Option<Appearance> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let deadline = Instant::now() + timeout;
    // 读应答需要非规范模式（BEL 不是行分隔符）；Drop 里把 termios 原样写回。
    let _window = tty::Window::open()?;
    let mut stdout = std::io::stdout();
    stdout.write_all(QUERY).ok()?;
    stdout.flush().ok()?;
    collect(&mut tty::read_before, deadline)
}

/// 进程级一次：按 `preset` 决定要不要问（显式 preset 零查询、零开销），答案记下来给
/// [`detected_appearance`] 复用。第二次调用不会再碰终端。
pub fn probe_once(preset: ColorPreset) -> Option<Appearance> {
    cached(&DETECTED, || match preset {
        ColorPreset::Auto => {
            let answer = detect(DETECT_TIMEOUT);
            // 「浅色终端还是暗色」的排障入口：日志里要看得出是没探到、还是探到了暗底。
            tracing::debug!(?answer, "terminal background probe");
            answer
        }
        _ => None,
    })
}

/// 已知答案。没有答案（没探过 / 探测失败 / 非 auto）返回 `None`。
pub fn detected_appearance() -> Option<Appearance> {
    DETECTED.get().copied().flatten()
}

/// [`OnceLock`] 的「只问一次」语义，抽出来是为了不碰全局静态也能测。
fn cached(
    cell: &OnceLock<Option<Appearance>>,
    probe: impl FnOnce() -> Option<Appearance>,
) -> Option<Appearance> {
    *cell.get_or_init(probe)
}

/// 纯解析：一段终端应答字节 → 底色明暗。
///
/// 应答前后的杂字节（窗口内键入的按键）不影响判定：OSC 11 应答按 `ESC]11;` 定位。没有
/// 终止符（BEL / ST）时返回 `None` —— 调用方继续读，直到超时。
pub(crate) fn parse_osc11(bytes: &[u8]) -> Option<Appearance> {
    let head = find(bytes, REPLY_HEAD)? + REPLY_HEAD.len();
    let rest = bytes.get(head..)?;
    let body = rest.get(..terminator(rest)?)?;
    let channels = parse_channels(body.strip_prefix(b"rgb:".as_slice())?)?;
    Some(verdict(channels))
}

/// 读者的注入点：反复读直到解析出答案、读完预算、或攒够 [`REPLY_CAP`]。
///
/// `read` 一次只读一个 chunk，返回 `Ok(0)` 表示「预算内没有更多字节」（超时 / 挂断）。
fn collect(
    read: &mut impl FnMut(&mut [u8], Instant) -> std::io::Result<usize>,
    deadline: Instant,
) -> Option<Appearance> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 32];
    loop {
        if let Some(appearance) = parse_osc11(&buf) {
            return Some(appearance);
        }
        if buf.len() >= REPLY_CAP || Instant::now() >= deadline {
            return None;
        }
        match read(&mut chunk, deadline) {
            Ok(0) | Err(_) => return None,
            Ok(n) if n <= chunk.len() => buf.extend_from_slice(&chunk[..n]),
            Ok(_) => return None,
        }
    }
}

/// 应答体 `RRRR/GGGG/BBBB` → 三个 0.0..=1.0 的通道值。每通道 1~4 位 hex。
fn parse_channels(body: &[u8]) -> Option<[f32; 3]> {
    let mut parts = body.split(|&b| b == b'/');
    let mut channels = [0.0f32; 3];
    for channel in &mut channels {
        *channel = parse_channel(parts.next()?)?;
    }
    // 四段以上：不是 X11 颜色，判不了。
    if parts.next().is_some() {
        return None;
    }
    Some(channels)
}

fn parse_channel(part: &[u8]) -> Option<f32> {
    let digits = part.len();
    if digits == 0 || digits > 4 {
        return None;
    }
    let value = part.iter().try_fold(0u32, |acc, &byte| {
        // 只认 ASCII hex（`u8 as char` 对 >0x7f 的字节给不出数字）。
        Some(acc * 16 + (byte as char).to_digit(16)?)
    })?;
    // 1 位 hex 已含满量程（X11 的 `f` = `ffff`）：按位宽归一。
    let max = (1u32 << (4 * digits as u32)) - 1;
    Some(value as f32 / max as f32)
}

/// WCAG 相对亮度 → 明暗。阈值 0.5：`#808080` 这类中灰落在暗侧（0.216），是被 sRGB 的
/// 伽马曲线拉下去的 —— 按通道平均值判会得出相反的结论。
fn verdict(channels: [f32; 3]) -> Appearance {
    let [r, g, b] = channels.map(linearize);
    if 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5 {
        Appearance::Light
    } else {
        Appearance::Dark
    }
}

fn linearize(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

/// 应答体的终止位置：BEL，或 ST（`ESC \`）。
fn terminator(bytes: &[u8]) -> Option<usize> {
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            0x07 => return Some(index),
            0x1b => return (bytes.get(index + 1) == Some(&b'\\')).then_some(index),
            _ => {}
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// fd 0 的读写原语。非 Unix 没有这一层（该平台本就不承诺 OSC 11 应答）：没有查询窗口，
/// [`detect`] 直接返回 `None`。
#[cfg(unix)]
mod tty {
    use std::time::Instant;

    /// 一次查询期间的 fd 0 形态：关掉行缓冲与回显，读立即返回。Drop 原样写回。
    ///
    /// 只动这两项、留 `ISIG`：探测窗口里的 Ctrl+C 仍然是中断，不会被当成输入吞掉。
    pub(super) struct Window {
        saved: libc::termios,
    }

    impl Window {
        pub(super) fn open() -> Option<Self> {
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
                return None;
            }
            let mut window = saved;
            // 只关规范模式与回显：探测需要「读到就有、不等整行」。
            // 注意 TUI 路径上 `enable_raw_mode()` 已把输入设成 `cfmakeraw`
            // （ISIG 也是关的），所以窗口内的按键——**包括 Ctrl+C**——会被
            // 读走并丢弃；这是启动期 ≤150ms 的既有取舍（与图像能力探测同款）。
            window.c_lflag &= !(libc::ICANON | libc::ECHO);
            window.c_cc[libc::VMIN] = 0;
            window.c_cc[libc::VTIME] = 0;
            if unsafe { libc::tcsetattr(0, libc::TCSANOW, &window) } != 0 {
                return None;
            }
            Some(Self { saved })
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
        }
    }

    /// 等到 `deadline` 之前 fd 0 可读，最多读一个 chunk。`Ok(0)` = 预算内没有字节
    /// （超时 / 挂断）—— 与 EOF 同一处理。
    pub(super) fn read_before(buf: &mut [u8], deadline: Instant) -> std::io::Result<usize> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
            let mut polled = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut polled, 1, timeout_ms) } {
                0 => return Ok(0),
                rc if rc < 0 => {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                _ => {}
            }
            if polled.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Ok(0);
            }
            let read = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            return match read {
                0 => Ok(0),
                n if n > 0 => Ok(n as usize),
                _ => match std::io::Error::last_os_error().kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock => Ok(0),
                    _ => Err(std::io::Error::last_os_error()),
                },
            };
        }
    }
}

#[cfg(not(unix))]
mod tty {
    use std::time::Instant;

    /// 非 Unix：没有查询窗口 —— [`super::detect`] 因此直接返回 `None`。
    pub(super) struct Window;

    impl Window {
        pub(super) fn open() -> Option<Self> {
            None
        }
    }

    pub(super) fn read_before(_buf: &mut [u8], _deadline: Instant) -> std::io::Result<usize> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bel(body: &str) -> Vec<u8> {
        format!("\x1b]11;{body}\x07").into_bytes()
    }

    fn st(body: &str) -> Vec<u8> {
        format!("\x1b]11;{body}\x1b\\").into_bytes()
    }

    #[test]
    fn bel_and_st_terminators_both_parse() {
        assert_eq!(
            parse_osc11(&bel("rgb:ffff/ffff/ffff")),
            Some(Appearance::Light)
        );
        assert_eq!(
            parse_osc11(&st("rgb:ffff/ffff/ffff")),
            Some(Appearance::Light)
        );
        assert_eq!(
            parse_osc11(&bel("rgb:0000/0000/0000")),
            Some(Appearance::Dark)
        );
        assert_eq!(
            parse_osc11(&st("rgb:0000/0000/0000")),
            Some(Appearance::Dark)
        );
    }

    /// X11 允许每通道 1~4 位 hex；`f` 与 `ffff` 是同一个满量程。
    #[test]
    fn every_hex_width_normalizes_to_the_same_extremes() {
        for body in [
            "rgb:f/f/f",
            "rgb:ff/ff/ff",
            "rgb:fff/fff/fff",
            "rgb:ffff/ffff/ffff",
        ] {
            assert_eq!(parse_osc11(&bel(body)), Some(Appearance::Light), "{body}");
        }
        for body in [
            "rgb:0/0/0",
            "rgb:00/00/00",
            "rgb:000/000/000",
            "rgb:0000/0000/0000",
        ] {
            assert_eq!(parse_osc11(&bel(body)), Some(Appearance::Dark), "{body}");
        }
    }

    /// 中灰在 WCAG 亮度上是暗底（0.216）—— 通道平均值判法会给出相反答案。
    #[test]
    fn mid_gray_is_dark_by_wcag_luminance() {
        assert_eq!(
            parse_osc11(&bel("rgb:8080/8080/8080")),
            Some(Appearance::Dark)
        );
        // 阈值附近的两位 hex：`#bc` 刚过 0.5，`#bb` 还没到。
        assert_eq!(parse_osc11(&bel("rgb:bb/bb/bb")), Some(Appearance::Dark));
        assert_eq!(parse_osc11(&bel("rgb:bc/bc/bc")), Some(Appearance::Light));
    }

    /// 亮度按通道加权：纯绿（0.715）是亮底，纯红（0.213）不是 —— 平均值判法两者都错。
    #[test]
    fn luminance_weights_the_channels() {
        assert_eq!(
            parse_osc11(&bel("rgb:0000/ffff/0000")),
            Some(Appearance::Light)
        );
        assert_eq!(
            parse_osc11(&bel("rgb:ffff/0000/0000")),
            Some(Appearance::Dark)
        );
        assert_eq!(
            parse_osc11(&bel("rgb:0000/0000/ffff")),
            Some(Appearance::Dark)
        );
    }

    /// 窗口内键入的按键会混进同一次读里：应答按 `ESC]11;` 定位，前后杂字节不影响。
    #[test]
    fn stray_bytes_around_the_reply_do_not_hide_it() {
        let mut bytes = b"ab\x1b[A".to_vec();
        bytes.extend(bel("rgb:ffff/ffff/ffff"));
        bytes.extend(b"c");
        assert_eq!(parse_osc11(&bytes), Some(Appearance::Light));
    }

    #[test]
    fn malformed_replies_are_rejected() {
        for body in [
            "rgb:0000/0000",           // 缺通道
            "rgb:0000/0000/0000/0000", // 多通道
            "rgb:zz/00/00",            // 非 hex
            "rgb:/00/00",              // 空通道
            "#ffffff",                 // 不是 rgb: 形式
            "rgb:00000/0000/0000",     // 通道超过 4 位
            "hello",
            "",
        ] {
            assert_eq!(parse_osc11(&bel(body)), None, "{body}");
        }
        assert_eq!(parse_osc11(b""), None);
        assert_eq!(parse_osc11(b"plain text, no escape"), None);
        // 打开但没终结：读者还要继续等。
        assert_eq!(parse_osc11(b"\x1b]11;rgb:ffff/ffff/ffff"), None);
        assert_eq!(parse_osc11(b"\x1b]11;rgb:ffff/ffff/ffff\x1b"), None);
        // 别的 OSC 应答不算（例如 OSC 4 的调色板查询）。
        assert_eq!(parse_osc11(b"\x1b]4;0;rgb:ffff/ffff/ffff\x07"), None);
    }

    /// 跨 read 分片：读者拿到半截应答时继续读，拼完整再判。
    #[test]
    fn a_reply_split_across_reads_is_parsed() {
        let whole = bel("rgb:ffff/ffff/ffff");
        let (first, second) = whole.split_at(10);
        // 栈顶先给前半截（`pop` 从后往前取）。
        let mut pieces = vec![second.to_vec(), first.to_vec()];
        let mut read = |buf: &mut [u8], _deadline: Instant| -> std::io::Result<usize> {
            let Some(piece) = pieces.pop() else {
                return Ok(0);
            };
            buf[..piece.len()].copy_from_slice(&piece);
            Ok(piece.len())
        };
        assert_eq!(
            collect(&mut read, Instant::now() + DETECT_TIMEOUT),
            Some(Appearance::Light)
        );
    }

    /// 终端不答：读者报告「没有更多字节」→ `None`，不会卡到调用方。
    #[test]
    fn silence_times_out_to_none() {
        let mut read = |_buf: &mut [u8], _deadline: Instant| Ok(0);
        assert_eq!(collect(&mut read, Instant::now() + DETECT_TIMEOUT), None);
    }

    /// 应答之外的噪声（例如窗口内的长键入）有上界：攒满 [`REPLY_CAP`] 就放弃。
    #[test]
    fn garbage_is_bounded_by_the_buffer_cap() {
        let mut calls = 0;
        let mut read = |buf: &mut [u8], _deadline: Instant| -> std::io::Result<usize> {
            calls += 1;
            buf[0] = b'x';
            Ok(1)
        };
        assert_eq!(
            collect(&mut read, Instant::now() + DETECT_TIMEOUT),
            None,
            "噪声里解析不出应答"
        );
        assert_eq!(calls, REPLY_CAP, "攒够上限就停，不等预算耗尽");
    }

    /// 一次探测、多次读取：第二段 TUI 与设置面重算复用第一段的答案。
    #[test]
    fn the_answer_is_asked_for_at_most_once() {
        let cell: OnceLock<Option<Appearance>> = OnceLock::new();
        let probes = std::cell::Cell::new(0);
        let probe = |answer| {
            probes.set(probes.get() + 1);
            answer
        };
        assert_eq!(
            cached(&cell, || probe(Some(Appearance::Light))),
            Some(Appearance::Light)
        );
        assert_eq!(
            cached(&cell, || probe(Some(Appearance::Dark))),
            Some(Appearance::Light),
            "已有答案就不再问"
        );
        assert_eq!(probes.get(), 1, "第二次调用不该再碰终端");
    }
}
