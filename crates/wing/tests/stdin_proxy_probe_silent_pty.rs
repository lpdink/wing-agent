//! 探针**静默**路径的端到端证据（pty 级；单独进程 = fd 0/1、crossterm 事件源与环境快照都是进程级全局）：
//!
//! 1. 本地档静默（不实现 DECRQM 的 tmux / Terminal.app 形态）：判定阴性 → 就地拆接驳；
//! 2. SSH 档静默：**保留接驳**（迟到应答必须仍被分流层吸收），但一个 `?5522h` 都不发；
//!    随后迟到的应答（含被拆开的半截）不得毒化 crossterm 的按键解析——键盘照常。
//!
//! 环境变量在同一进程内注入（测试各自独立进程、注入发生在任何线程读它之前；edition 2024
//! 的 `set_var` / `remove_var` 因此是 unsafe 的、但此处无并发读者），这样本地 / SSH 两档
//! 的判定与预算**与跑测试的机器环境无关**。

use std::os::fd::RawFd;
use std::time::Duration;
use std::time::Instant;

const SSH_ENV: [&str; 4] = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"];

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
fn a_silent_probe_dismantles_locally_and_keeps_the_splice_over_ssh() {
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
    // 本地档：清掉继承来的 SSH 环境（注入对后面的单测进程无影响——本文件只有一个测试）。
    for key in SSH_ENV {
        unsafe { std::env::remove_var(key) };
    }

    // ── 本地档静默：判定阴性、接驳就地拆除。 ──
    wing::tui::stdin_proxy::install().expect("install stdin proxy");
    assert_eq!(
        wing::tui::stdin_proxy::settle_paste_channel(),
        wing::tui::stdin_proxy::PasteChannel::Unsupported
    );
    assert!(
        !wing::tui::stdin_proxy::is_installed(),
        "本地静默（不实现 DECRQM 的终端）必须拆接驳"
    );

    // ── SSH 档静默：保留接驳、不开 mode。 ──
    unsafe { std::env::set_var("SSH_CONNECTION", "10.0.0.1 51000 10.0.0.2 22") };
    wing::tui::stdin_proxy::install().expect("re-install stdin proxy");
    assert!(wing::tui::stdin_proxy::is_installed());
    let settle_started = Instant::now();
    assert_eq!(
        wing::tui::stdin_proxy::settle_paste_channel(),
        wing::tui::stdin_proxy::PasteChannel::Unsupported
    );
    assert!(
        wing::tui::stdin_proxy::is_installed(),
        "SSH 静默必须保留接驳（迟到应答还要靠它吸收）"
    );
    assert!(
        settle_started.elapsed() >= Duration::from_millis(500),
        "远端档的静默判定必须走长预算（600ms）——否则迟到应答窗口还没关就下判定：{:?}",
        settle_started.elapsed()
    );

    // 事件流：判定阴性，不开 `?5522h`。
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
        "静默终端不许收到 ?5522h，实收 {written:?}"
    );

    // 迟到的应答（先半截、后被拆开补齐）必须被分流层吞掉：半截到点丢弃、完整条吸收，
    // 都不能落进 crossterm——落了就会永久挂住按键解析，下面的 `k` 永远到不了。
    write_all(outer_master, b"\x1b[?5522;0$");
    std::thread::sleep(Duration::from_millis(150)); // 超过 SSH 档 100ms 的候选持有上限
    write_all(outer_master, b"y");
    std::thread::sleep(Duration::from_millis(50));
    write_all(outer_master, b"k");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got_key = false;
    while Instant::now() < deadline && !got_key {
        match rx.try_recv() {
            Ok(event) => got_key = format!("{event:?}").contains("Char('k')"),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(got_key, "迟到应答必须被吸收：键盘在它之后仍要照常工作");

    wing::tui::end_tui_session().expect("end tui session");
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
