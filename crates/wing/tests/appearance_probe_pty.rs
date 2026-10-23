//! 底色探测的端到端证据（pty 级；单独进程 = fd 0 / fd 1 是进程级全局）。
//!
//! 一对伪终端扮演「用户的终端」：应用侧 fd 0 / fd 1 都指向 slave，
//! 测试从 master 侧读它写了什么、往里塞应答。两段场景：
//!
//! 1. **有应答**：读到 `ESC]11;?BEL` → 回 `ESC]11;rgb:ffff/ffff/ffffBEL` → `Light`。
//!    slave 保持默认的规范模式 —— 探测必须自己把 fd 0 切成可读（`tcsetattr`），
//!    否则 `read` 会一直等到换行。
//! 2. **不答**：查询照发，预算内没字节 → `None`，且耗时不越过预算太多（绝不无限等）。
//!
//! fd 1 在测试期间也被重定向，所以断言失败信息可能被吞进 pty —— 收尾靠 [`Tty`] 的
//! Drop，panic 路径同样还原。

use std::time::Duration;
use std::time::Instant;

mod common;

use common::pty::{openpty_pair, read_written, write_all};

/// 查询字节：与生产同一份（这里照抄一遍，测试不该借被测实现自证）。
const QUERY: &[u8] = b"\x1b]11;?\x07";

/// fd 0 / fd 1 → pty slave；Drop 还原。panic 也要还原，否则后面所有测试的输出都没了。
struct Tty {
    saved_stdin: libc::c_int,
    saved_stdout: libc::c_int,
    master: libc::c_int,
    slave: libc::c_int,
}

impl Tty {
    fn open() -> Self {
        let (master, slave) = openpty_pair();
        let saved_stdin = unsafe { libc::dup(0) };
        let saved_stdout = unsafe { libc::dup(1) };
        assert_eq!(
            unsafe { libc::dup2(slave, 0) },
            0,
            "fd 0 → pty slave（探测要看到真 tty）"
        );
        assert_eq!(
            unsafe { libc::dup2(slave, 1) },
            1,
            "fd 1 → pty slave（查询写到终端）"
        );
        Self {
            saved_stdin,
            saved_stdout,
            master,
            slave,
        }
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved_stdin, 0);
            libc::dup2(self.saved_stdout, 1);
            libc::close(self.saved_stdin);
            libc::close(self.saved_stdout);
            libc::close(self.master);
            libc::close(self.slave);
        }
    }
}

#[test]
fn a_terminal_answer_decides_the_palette_and_silence_is_bounded() {
    let tty = Tty::open();

    // ── 场景 1：终端答「我的背景是白的」 ──
    let master = tty.master;
    let terminal = std::thread::spawn(move || {
        let written = read_written(master, Duration::from_secs(2), |acc| {
            acc.windows(QUERY.len()).any(|window| window == QUERY)
        });
        assert!(
            written.windows(QUERY.len()).any(|window| window == QUERY),
            "探测必须写出 OSC 11 查询，实收 {written:?}"
        );
        write_all(master, b"\x1b]11;rgb:ffff/ffff/ffff\x07");
    });

    let answer = wing::tui::appearance::detect(Duration::from_millis(500));
    terminal.join().expect("terminal thread");

    assert_eq!(
        answer,
        Some(wing::config::Appearance::Light),
        "白底应答必须判成亮色"
    );

    // ── 场景 2：终端不答 ──
    let started = Instant::now();
    let silent = wing::tui::appearance::detect(Duration::from_millis(120));
    let elapsed = started.elapsed();

    assert_eq!(silent, None, "没有应答就没有答案（调用方回退 Wing）");
    assert!(
        elapsed >= Duration::from_millis(100),
        "探测该等满预算再放弃，实测 {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "探测不得越过预算阻塞（实测 {elapsed:?}）"
    );
    let written = read_written(master, Duration::from_secs(1), |acc| {
        acc.windows(QUERY.len()).any(|window| window == QUERY)
    });
    assert!(
        written.windows(QUERY.len()).any(|window| window == QUERY),
        "沉默场景也要真的发过查询，实收 {written:?}"
    );
}
