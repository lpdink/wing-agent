//! 「双击 Ctrl+C 挂死」的回归：进程收尾不得等待一个卡在阻塞读上的事件任务。
//!
//! 机制：crossterm 的事件源是进程级单例，就绪注册绑定**注册时刻**的文件描述，而读取
//! 永远走**当时的 fd 号码 0**。`tui::end_tui_session` 里的 `restore()` 会把 fd 0 换回
//! 真实 tty：事件任务若正卡在对旧（内层 pty）slave 的读上——或先被「master 关闭 → slave
//! EOF」唤醒、重读时落到 canonical 的真实 tty 上阻塞——通道虽已断，任务也永远到不了
//! `send` 失败分支，`Runtime::drop` 等 worker 退出，进程挂住（用户现象：再按一次 Enter 才退）。
//!
//! 修复：`end_tui_session` 先 `stop_event_stream()`（置 flag → `stdin_proxy::wake_reader()`
//! 向 fd 0 投 1 字节打断阻塞读 → 有界等待），再做 `restore()` 与 `disable_raw_mode`。
//!
//! 本文件就是这条时序的端到端证据，单独进程（fd 0 与 crossterm 单例都是进程级全局）：
//! 装上接驳 → 在 runtime 里起事件流并让它进入阻塞等待 → drop 接收端（模拟 `run_app`
//! 返回）→ `end_tui_session()` → 断言 runtime 能在超时内整个收掉。修复前该断言超时
//! （reader 卡死在 read(0)），修复后毫秒级通过。

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
fn teardown_wakes_a_blocked_event_reader() {
    // 外层 pty = 「用户终端」；进程 fd 0 先指向它的 slave（脚手架同 stdin_proxy_pty.rs）。
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );

    wing::tui::stdin_proxy::install().expect("install stdin proxy");

    // 主 TUI 的运行期形态：多线程 runtime（生产是 `#[tokio::main]`），事件任务在其上跑。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let entered = rt.enter();
    let rx = wing::tui::spawn_event_stream();
    drop(entered);
    // 给事件任务一拍：crossterm 事件源完成注册、进入对 fd 0 的阻塞等待。
    std::thread::sleep(Duration::from_millis(50));
    // `run_app` 返回：接收端先没了（收尾的第一种形态）。
    drop(rx);

    // 收尾：必须是「先停事件流、再还原 fd 0」——顺序错了任务就会卡在重读上（见文件头）。
    let started = Instant::now();
    wing::tui::end_tui_session().expect("end tui session");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "end_tui_session 必须有界（不等待一个退不出来的任务）：{:?}",
        started.elapsed()
    );

    // runtime 能整个收掉 = 事件任务确实退出了。
    // 修复前：reader 卡在 read(0)，`drop(rt)` 不返回，这一步超时（红）。
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let closer = std::thread::spawn(move || {
        drop(rt);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("runtime teardown timed out: the event reader is still blocked in read(0)");
    closer.join().expect("closer thread");

    // 收尾后 fd 0 已还原成外层 pty（会话收尾的既有契约不变），且没有残留读者：
    // 写进外层 master 的字节只能由 fd 0 读到。
    assert_eq!(unsafe { libc::isatty(0) }, 1, "fd 0 必须还是 tty");
    write_all(outer_master, b"q");
    let mut buf = [0u8; 8];
    let mut fds = [libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    }];
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) };
    assert_eq!(rc, 1, "收尾后 fd 0 应能直接读到外层 pty 的字节");
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    assert_eq!(&buf[..n as usize], b"q", "残留读者会偷走这个字节");

    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
        libc::close(outer_master);
        libc::close(outer_slave);
    }
}
