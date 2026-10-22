//! 回归证据（B1）：**进程级会话**的接驳必须跨 TUI 段存活。
//!
//! 「配置不可用 → setup 向导 → 主 TUI」是同一进程里的两段 TUI。crossterm 的事件源是
//! 进程级单例（首次 `poll` 时按当时的 fd 0 注册，永不重建），因此：
//!
//! 1. 接驳必须在**任何一段**创建事件源之前装上（向导正是最早的机会）；
//! 2. 段与段之间**绝不拆接驳** —— 拆掉再装会让单例的就绪源与读端错配，第二段从此收不到
//!    任何输入（反面证据：`stdin_proxy_stale_event_source.rs`）。
//!
//! 本测试按这两条走一遍：第一段注册事件源并收到按键 → 段边界不拆 → 第二段照旧收到按键。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 0 与事件源都是进程级全局）。

use std::os::fd::RawFd;
use std::time::Duration;
use std::time::Instant;

mod common;

use common::pty::{make_raw, openpty_pair, write_all};

/// 「终端」一侧喂一个字节，然后给事件循环一点时间。
fn feed(master: RawFd, byte: u8) {
    write_all(master, &[byte]);
    std::thread::sleep(Duration::from_millis(150));
}

fn drain_events(ms: u64) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match crossterm::event::poll(Duration::from_millis(50)) {
            Ok(true) => events.push(format!("{:?}", crossterm::event::read().expect("read"))),
            Ok(false) => {}
            Err(e) => panic!("poll: {e}"),
        }
    }
    events
}

#[test]
fn one_splice_serves_both_tui_segments() {
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );

    // 进程级会话：接驳在最前面装上（生产里 = 第一段 `init_terminal` 内部的 begin_session，
    // 也就是向导这一段的开头）。
    wing::tui::stdin_proxy::install().expect("install");
    assert!(wing::tui::stdin_proxy::is_installed());

    // ── 第一段（= 向导）：第一次 poll 在这里把事件源注册到 fd 0（此刻已是内层 slave）。
    feed(outer_master, b'a');
    let first = drain_events(300);
    assert!(
        first.iter().any(|e| e.contains("Char('a')")),
        "第一段应当收到按键：{first:?}"
    );

    // ── 段边界（= 向导结束）：生产里 `restore_terminal` 只写离开序列（备用屏 / 鼠标 /
    // 焦点上报），既不拆接驳也不碰 raw mode —— 这里就把「什么都不拆」这一步写实。
    assert!(
        wing::tui::stdin_proxy::is_installed(),
        "段边界不得拆接驳（拆了单例就与读端错配）"
    );

    // ── 第二段（= 主 TUI）：同一个单例必须继续工作。
    feed(outer_master, b'b');
    let second = drain_events(300);
    assert!(
        second.iter().any(|e| e.contains("Char('b')")),
        "第二段必须照旧收到按键（B1 的红线）：{second:?}"
    );

    // ── 收尾：还原 fd 0（生产里 = `tui::end_tui_session`）。
    wing::tui::stdin_proxy::restore();
    assert!(!wing::tui::stdin_proxy::is_installed());
    write_all(outer_master, b"q");
    let mut buf = [0u8; 8];
    let mut fds = [libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    }];
    assert_eq!(
        unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) },
        1,
        "收尾之后 fd 0 应当能直接读到外层 pty 的字节"
    );
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    assert_eq!(&buf[..n as usize], b"q");
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
    }
}
