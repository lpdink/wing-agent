//! 探针阴性路径的端到端证据（pty 级；单独进程 = fd 0/1 与 crossterm 事件源都是进程级全局）：
//!
//! 1. 假终端对启动探针（DECRQM）明确应答 `Ps=0`（不支持）；
//! 2. `settle_paste_channel` 判定阴性并**就地拆掉接驳**：`is_installed()==false`、
//!    fd 0 还原成原 tty（字节能直接读到）；
//! 3. `spawn_event_stream` 不开 mode 5522（stdout 上没有 `?5522h`）——阴性终端回到
//!    #221 之前的纯输入路径，键盘事件照常。
//!
//! stdout 被重定向到外层 pty 的 slave，所以应用写出的控制序列（探针查询 / `?5522h`）
//! 能被测试从 master 侧读到；结束前还原 fd 1（测试框架自己的输出不受影响）。

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

/// 从 master 读一段（模拟终端看应用写了什么）：`want` 满足即提前停，`budget` 到点返回已读。
fn read_written(fd: RawFd, budget: Duration, want: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let deadline = Instant::now() + budget;
    let mut acc = Vec::new();
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 20) };
        if rc > 0 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                acc.extend_from_slice(&buf[..n as usize]);
                if want(&acc) {
                    break;
                }
            }
        }
    }
    acc
}

#[test]
fn a_negative_probe_drops_the_splice_and_leaves_the_plain_input_path() {
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved_stdin = unsafe { libc::dup(0) };
    let saved_stdout = unsafe { libc::dup(1) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 1) },
        1,
        "fd 1 → outer slave（读应用写出的控制序列）"
    );

    // 装上接驳：探针查询立即写出（stdout 现在是外层 pty）。
    wing::tui::stdin_proxy::install().expect("install stdin proxy");
    assert!(wing::tui::stdin_proxy::is_installed());
    let written = read_written(outer_master, Duration::from_secs(1), |acc| {
        acc.windows(b"\x1b[?5522$p".len())
            .any(|w| w == b"\x1b[?5522$p")
    });
    assert!(
        written
            .windows(b"\x1b[?5522$p".len())
            .any(|w| w == b"\x1b[?5522$p"),
        "install 必须发出 DECRQM 探针查询，实收 {written:?}"
    );

    // 假终端明确应答「不支持」，随后判定：接驳被就地拆掉；判定幂等（第二次读缓存）。
    write_all(outer_master, b"\x1b[?5522;0$y");
    assert_eq!(
        wing::tui::stdin_proxy::settle_paste_channel(),
        wing::tui::stdin_proxy::PasteChannel::Unsupported
    );
    assert_eq!(
        wing::tui::stdin_proxy::settle_paste_channel(),
        wing::tui::stdin_proxy::PasteChannel::Unsupported
    );
    assert!(
        !wing::tui::stdin_proxy::is_installed(),
        "阴性判定必须拆掉接驳（回到 pre-#221 的输入路径）"
    );

    // fd 0 已还原：写进外层 master 的字节能直接从 fd 0 读到（泵线程退场，无残留读者）。
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
        "拆接驳后 fd 0 应能直接读到外层 pty 的字节"
    );
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    assert_eq!(&buf[..n as usize], b"q");

    // 主 TUI 的事件流：阴性判定下不得开 `?5522h`、不得起协议机；键盘事件照常。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let entered = rt.enter();
    let mut rx = wing::tui::spawn_event_stream();
    drop(entered);
    let written = read_written(outer_master, Duration::from_millis(300), |acc| {
        acc.windows(8).any(|w| w == b"\x1b[?5522h")
    });
    assert!(
        !written.windows(8).any(|w| w == b"\x1b[?5522h"),
        "阴性终端不许收到 ?5522h，实收 {written:?}"
    );

    write_all(outer_master, b"k");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(event) => {
                if format!("{event:?}").contains("Char('k')") {
                    events.push(event);
                    break;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(!events.is_empty(), "拆接驳后键盘必须照常工作");

    wing::tui::end_tui_session().expect("end tui session");
    // 收尾后 fd 0 还是原 tty；runtime 能整个收掉（没有挂在读上的残留任务）。
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let closer = std::thread::spawn(move || {
        drop(rt);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("runtime teardown timed out");
    closer.join().expect("closer thread");

    unsafe {
        libc::dup2(saved_stdin, 0);
        libc::close(saved_stdin);
        libc::dup2(saved_stdout, 1);
        libc::close(saved_stdout);
        libc::close(outer_master);
        libc::close(outer_slave);
    }
}
