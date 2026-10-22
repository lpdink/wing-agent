//! 探针**本地静默**路径的端到端证据（pty 级；单独进程 = fd 0/1、crossterm 事件源与环境快照都是进程级全局）：
//!
//! 不实现 DECRQM 的本地终端（tmux / Terminal.app / 老终端——最常见的降级场景）在预算内不
//! 应答 → 判定阴性 → **首 poll 前就地拆接驳**（回到 pre-#221 纯输入路径），一个 `?5522h`
//! 都不发；fd 0 还原成原 tty、键盘照常。SSH 档静默（保留接驳）见 `stdin_proxy_probe_silent_pty.rs`。
//!
//! 环境变量在同一进程内清除（测试各自独立进程），本档判定与预算与跑测试的机器环境无关。

use std::time::Duration;
use std::time::Instant;
mod common;

use common::pty::{make_raw, openpty_pair, read_written, write_all};

#[test]
fn a_silent_local_probe_drops_the_splice_and_leaves_the_plain_input_path() {
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
    // 本地档：清掉继承来的 SSH 环境（本文件只有一个测试）。
    for key in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"] {
        unsafe { std::env::remove_var(key) };
    }

    // 装上接驳（探针查询立即写出），终端不做任何应答。
    wing::tui::stdin_proxy::install().expect("install stdin proxy");
    assert!(wing::tui::stdin_proxy::is_installed());
    let query = b"\x1b[?5522$p";
    let written = read_written(outer_master, Duration::from_secs(1), |acc| {
        acc.windows(query.len()).any(|w| w == query)
    });
    assert!(
        written.windows(query.len()).any(|w| w == query),
        "install 必须发出 DECRQM 探针查询，实收 {written:?}"
    );

    // 本地档的静默判定走短预算（150ms），随后就地拆接驳。
    let settle_started = Instant::now();
    assert_eq!(
        wing::tui::stdin_proxy::settle_paste_channel(),
        wing::tui::stdin_proxy::PasteChannel::Unsupported
    );
    assert!(
        settle_started.elapsed() >= Duration::from_millis(100),
        "本地档必须等满短预算再下判定（不是看一眼就走）：{:?}",
        settle_started.elapsed()
    );
    assert!(
        !wing::tui::stdin_proxy::is_installed(),
        "本地静默（不实现 DECRQM 的终端）必须拆接驳"
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

    // 事件流：阴性判定下不得开 `?5522h`、不得起协议机；键盘事件照常。
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

    write_all(outer_master, b"k");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got_key = false;
    while Instant::now() < deadline && !got_key {
        match rx.try_recv() {
            Ok(event) => got_key = format!("{event:?}").contains("Char('k')"),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(got_key, "拆接驳后键盘必须照常工作");

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
