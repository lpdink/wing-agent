//! 回归证据：crossterm 的事件源是**进程级单例**（首次 `poll` 时按当时的 fd 0 注册，
//! 之后永不重建），所以「两次安装接驳」之间换掉 fd 0 会让第二段彻底收不到输入。
//!
//! 这就是为什么 setup 向导用 `tui::init_terminal_without_stdin_splice()`：向导与主 TUI
//! 是同一进程里的两段 TUI，第一段不能换接 fd 0。若将来 crossterm 支持重建事件源，
//! 本测试会失败——那正是重新评估这条分支的信号。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 0 与事件源都是进程级全局）。

use std::os::fd::RawFd;
use std::time::Duration;
use std::time::Instant;

fn openpty_pair() -> (RawFd, RawFd) {
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
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    (master, slave)
}

fn make_raw(fd: RawFd) {
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(fd, &mut term), 0);
        libc::cfmakeraw(&mut term);
        assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &term), 0);
    }
}

fn write_all(fd: RawFd, bytes: &[u8]) {
    let n = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
    assert_eq!(n as usize, bytes.len(), "pty write");
}

/// 「终端」一侧：写一个字节并等它被读走。
fn feed(master: RawFd, byte: u8) {
    write_all(master, &[byte]);
    std::thread::sleep(Duration::from_millis(120));
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
fn a_second_splice_cannot_retarget_the_cached_event_source() {
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );

    // 第一段：装接驳 → 第一次 poll（事件源在这一刻注册 fd 0）→ 输入正常。
    wing::tui::stdin_proxy::install().expect("install #1");
    let _sequences = wing::tui::stdin_proxy::take_sequences();
    feed(outer_master, b'a');
    let first = drain_events(300);
    assert!(
        first.iter().any(|e| e.contains("Char('a')")),
        "第一段应当收到按键：{first:?}"
    );

    // 收尾第一段：fd 0 还回外层 tty（此时被注册的那个文件描述随之消失）。
    wing::tui::stdin_proxy::restore();

    // 第二段：重新装接驳（新的 pty），再喂一个字节。
    wing::tui::stdin_proxy::install().expect("install #2");
    let _sequences = wing::tui::stdin_proxy::take_sequences();
    feed(outer_master, b'b');

    // 数据确实到了 fd 0（泵线程转发进了新的 pty slave）……
    let mut buf = [0u8; 8];
    let mut fds = [libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    }];
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) };
    assert_eq!(rc, 1, "泵线程应当把字节转发进新的 pty slave");
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    assert_eq!(&buf[..n as usize], b"b");

    // ……但缓存的事件源看不见它：就绪源还挂在**上一段**那个已经关掉的文件描述上。
    // 这就是「两段 TUI 之间不能换接 fd 0」的事实依据。
    feed(outer_master, b'c');
    let second = drain_events(400);
    assert!(
        second.is_empty(),
        "缓存的事件源不该再看到任何按键（看到就说明 crossterm 变了）：{second:?}"
    );

    wing::tui::stdin_proxy::restore();
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
    }
}
