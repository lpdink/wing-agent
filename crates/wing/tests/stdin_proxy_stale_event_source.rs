//! 反面证据（B1）：**事件源先注册、接驳后装** → 输入永久哑掉。
//!
//! crossterm 的事件源是进程级单例：首次 `poll` 时把 fd 0 的**文件描述**注册进就绪源
//! （epoll / kqueue），之后永不重建；而读取永远走「当时的 fd 0」。于是注册与读端一旦
//! 不是同一个文件描述，就凑不出事件：
//!
//! - 单例注册在**真实 tty** 上，接驳随后把 fd 0 换成内层 pty 的 slave；
//! - 泵线程是真实 tty 的唯一读者，它把就绪**吃掉**（level-triggered：被读空就不再上报）；
//! - 单例从此再也不被唤醒，而它要读的 fd 0（内层 slave）里字节明明躺在那儿。
//!
//! 实测（本测试断言的就是这个形态）：Crossterm 收到 0 条事件，字节滞留内层 pty —— 直接
//! `read(fd 0)` 反而读得到，证明转发没问题，坏的是**唤醒源**。
//!
//! 这就是为什么 [`crate::tui`] 的会话把接驳放在**任何一段 TUI 的第一次 `poll` 之前**，
//! 并且段与段之间绝不拆：安装顺序是硬不变量，不是性能偏好。正确顺序的正面证据见
//! `stdin_proxy_pty.rs`（单段）与 `stdin_proxy_two_segments.rs`（跨段）。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 0 与事件源都是进程级全局）。

use std::time::Duration;
use std::time::Instant;
mod common;

use common::pty::{make_raw, openpty_pair, write_all};

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
fn a_source_registered_before_the_splice_never_wakes_again() {
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );

    // ① 危险顺序的第一步：事件源先注册（fd 0 = 外层 pty slave，代表真实 tty）。
    assert!(
        !crossterm::event::poll(Duration::from_millis(1)).expect("poll"),
        "还没有输入"
    );

    // ② 之后才装接驳：fd 0 → 新 pty 的 slave，泵线程开始独占读外层 tty。
    wing::tui::stdin_proxy::install().expect("install");
    write_all(outer_master, b"A");
    std::thread::sleep(Duration::from_millis(300));

    // ③ 单例再也收不到唤醒 —— 尽管字节已经被泵线程搬进了内层 pty。
    let events = drain_events(300);
    assert!(
        events.is_empty(),
        "错误顺序下不该有事件（有的话说明 crossterm 行为变了，本不变量需要重估）：{events:?}"
    );

    // ④ 证据：字节就在 fd 0 的队列里，只是没人被唤醒去读它。
    let mut buf = [0u8; 8];
    let mut fds = [libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    }];
    assert_eq!(
        unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) },
        1,
        "泵线程应当已经把它转发进内层 pty"
    );
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    assert_eq!(&buf[..n as usize], b"A");

    wing::tui::stdin_proxy::restore();
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
    }
}
