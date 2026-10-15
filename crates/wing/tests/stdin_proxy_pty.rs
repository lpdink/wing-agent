//! A′ 接驳的端到端证据（spike 结论）：装上 `tui::stdin_proxy` 之后，
//!
//! 1. crossterm 读到的事件与「没有接驳时」逐个相同（按键 / 方向键 / SGR 鼠标 /
//!    bracketed paste 零影响）；
//! 2. `OSC 5522` 序列一条不漏地进协议通道，且**不再**被解析成按键；
//! 3. 空闲 `poll` 正常超时（pty slave 上没有死锁）；
//! 4. `restore()` 把 fd 0 还给原 tty，泵线程退出。
//!
//! 单独一个集成测试文件 = 单独一个进程：fd 0 与 crossterm 的事件源都是进程级全局。
//! 本测试不碰真实终端（自己开 pty），headless CI 可跑。

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

#[test]
fn splice_extracts_osc_5522_and_leaves_every_other_event_untouched() {
    // 外层 pty = 「用户终端」；进程 fd 0 先指向它的 slave。
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );

    // 装上接驳（真实路径里由 `tui::init_terminal` 在 raw mode 之后调用）。
    assert!(!wing::tui::stdin_proxy::is_installed());
    wing::tui::stdin_proxy::install().expect("install stdin proxy");
    assert!(wing::tui::stdin_proxy::is_installed());
    assert_eq!(
        unsafe { libc::isatty(0) },
        1,
        "换接后 fd 0 必须仍是 tty（crossterm 的 tty_fd 判据）"
    );
    let mut sequences = wing::tui::stdin_proxy::take_sequences().expect("sequence channel");

    // 生产接线的一致性：装上接驳之后，进入序列必须真的带上 `?5522h`（单测用的是注入的
    // 开关，这里查的是真实 `is_installed()` 判据）；卸载之后必须不带。
    let mut enter = Vec::new();
    wing::tui::enter_sequence(&mut enter).expect("write enter sequence");
    assert!(
        String::from_utf8_lossy(&enter).contains("\x1b[?5522h"),
        "装上接驳就该开 5522：{:?}",
        String::from_utf8_lossy(&enter)
    );
    // 向导形态：接驳装着（进程级会话）也不开粘贴事件模式 —— 它自己不驱动协议机。
    let mut wizard = Vec::new();
    wing::tui::enter_sequence_without_paste_events(&mut wizard).expect("write enter sequence");
    assert!(
        !String::from_utf8_lossy(&wizard).contains("?5522"),
        "向导不该开 5522：{:?}",
        String::from_utf8_lossy(&wizard)
    );

    // 「终端」：注入混合流（三个 5522 包**跨 write 边界**）。
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        write_all(outer_master, b"a");
        std::thread::sleep(Duration::from_millis(30));
        write_all(outer_master, b"\x1b[A");
        std::thread::sleep(Duration::from_millis(30));
        write_all(outer_master, b"\x1b[<0;10;20M");
        std::thread::sleep(Duration::from_millis(30));
        write_all(outer_master, b"\x1b[200~hi there\x1b[201~");
        std::thread::sleep(Duration::from_millis(30));
        write_all(outer_master, b"\x1b]5522;type=read:status=OK:pw=b3Rw\x1b\\");
        std::thread::sleep(Duration::from_millis(30));
        write_all(
            outer_master,
            b"\x1b]5522;type=read:status=DATA:mime=Lg==:pw=b3Rw;dGV4dC9wbGFpbg==\x1b\\",
        );
        std::thread::sleep(Duration::from_millis(30));
        write_all(
            outer_master,
            b"\x1b]5522;type=read:status=DONE:pw=b3Rw\x1b\\",
        );
        std::thread::sleep(Duration::from_millis(30));
        write_all(outer_master, b"z");
    });

    // 「应用」：crossterm 事件循环（读的就是被换接到 fd 0 的内层 pty slave）。
    let mut events: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < deadline {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => events.push(format!("{:?}", crossterm::event::read().expect("read"))),
            Ok(false) => {}
            Err(e) => panic!("poll: {e}"),
        }
    }
    writer.join().expect("writer");

    // 1. 事件逐个相同：5522 一个字节都没漏进来。
    assert_eq!(events.len(), 5, "多出来的事件就是污染：{events:#?}");
    assert!(events[0].contains("Char('a')"), "{}", events[0]);
    assert!(events[1].contains("Up"), "{}", events[1]);
    assert!(events[2].contains("Mouse"), "{}", events[2]);
    assert!(events[3].contains("Paste(\"hi there\")"), "{}", events[3]);
    assert!(events[4].contains("Char('z')"), "{}", events[4]);
    let all = events.join("\n");
    assert!(
        !all.contains("Alt") && !all.contains("'5'"),
        "5522 序列不能以按键形态出现：\n{all}"
    );

    // 2. 协议通道拿到三条包（顺序一致，负载完整）。
    let mut got: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(1000);
    while got.len() < 3 && Instant::now() < deadline {
        while let Ok(seq) = sequences.try_recv() {
            got.push(String::from_utf8_lossy(&seq).into_owned());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        got,
        vec![
            "type=read:status=OK:pw=b3Rw".to_string(),
            "type=read:status=DATA:mime=Lg==:pw=b3Rw;dGV4dC9wbGFpbg==".to_string(),
            "type=read:status=DONE:pw=b3Rw".to_string(),
        ]
    );

    // 3. 空闲 poll 正常超时（前面那 2.5 秒的循环里大量 Ok(false) 已经证明；这里再明确一次）。
    let idle = crossterm::event::poll(Duration::from_millis(200)).expect("poll");
    assert!(!idle, "没有输入时 poll 必须超时返回，不能挂死");

    // 4. 卸载：fd 0 还给外层 pty，泵线程退出（之后的字节只能由 fd 0 直接读到）。
    wing::tui::stdin_proxy::restore();
    assert!(!wing::tui::stdin_proxy::is_installed());
    let mut enter = Vec::new();
    wing::tui::enter_sequence(&mut enter).expect("write enter sequence");
    assert!(
        !String::from_utf8_lossy(&enter).contains("?5522"),
        "接驳没了就不该开 5522"
    );
    write_all(outer_master, b"q");
    let mut buf = [0u8; 8];
    let mut fds = [libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    }];
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) };
    assert_eq!(rc, 1, "restore 之后 fd 0 应能直接读到外层 pty 的字节");
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    assert_eq!(&buf[..n as usize], b"q");
    // 再多读一次不该再有东西：泵线程已经不在了（没有第二个读者抢字节）。
    fds[0].revents = 0;
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 150) };
    assert_eq!(rc, 0, "泵线程退场后不该还有人消费输入");
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
    }
}
