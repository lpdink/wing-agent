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

use std::time::Duration;
use std::time::Instant;

mod common;

use common::pty::{make_raw, openpty_pair, read_written, write_all};

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
    // 本地档：清掉继承来的 SSH 环境（注入发生在 install 读它之前；本文件只有一个测试）。
    for key in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"] {
        unsafe { std::env::remove_var(key) };
    }

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

    // 阴性拆解是进程级结论：向导段跑完后主 TUI 的第二次 `init_terminal()` 会再走一遍
    // `install()`——它必须是个空操作。先模拟向导段的第一次 poll（事件源单例在这里注册，
    // 此时 fd 0 已是原 tty），再调 install：任何重装都会把 fd 0 从单例底下换走（B1 形态：
    // 泵吃掉就绪、键盘永久哑掉）。
    assert!(
        !crossterm::event::poll(Duration::from_millis(1)).expect("wizard poll"),
        "没有输入的 1ms poll 必须超时"
    );
    wing::tui::stdin_proxy::install().expect("second install must be a no-op");
    assert!(
        !wing::tui::stdin_proxy::is_installed(),
        "探针判定之后的 install 不许重装接驳（事件源单例已注册，fd 0 不能再换）"
    );

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
