//! `spawn_event_stream` 的事件臂回归（pty 直喂真实字节）：鼠标 / bracketed paste / focus /
//! resize / 空闲 tick 都要按 `TermEvent` 原样到达。跑的是生产接线全链——stdin 接驳层、
//! crossterm 解析、事件任务转发，不 mock 任何一环。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 0、crossterm 事件源与 EVENT_* 都是进程级全局）。

use std::time::Duration;
use std::time::Instant;

use crossterm::event::MouseButton;
use crossterm::event::MouseEventKind;
use tokio::sync::mpsc;
use wing::tui::TermEvent;

mod common;

use common::pty::{make_raw, openpty_pair, write_all};

/// 取下一个满足 `pred` 的事件（跳过期间的空闲 `Tick`）；超时 / 通道断都算失败。
fn expect_event(
    rt: &tokio::runtime::Runtime,
    rx: &mut mpsc::Receiver<TermEvent>,
    pred: impl Fn(&TermEvent) -> bool,
    what: &str,
) -> TermEvent {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let got = rt
            .block_on(async { tokio::time::timeout(Duration::from_millis(500), rx.recv()).await });
        let ev = got
            .ok()
            .flatten()
            .unwrap_or_else(|| panic!("event stream ended while waiting for {what}"));
        if pred(&ev) {
            return ev;
        }
    }
}

#[test]
fn event_stream_forwards_mouse_paste_focus_resize_and_tick() {
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

    // crossterm 的 resize 尺寸取自 **/dev/tty**（不是 fd 0）：不接管会话就取不到我们设的
    // winsize（会落到 tput 回退——CI 上 TERM 未设时直接报错，把事件任务打断）。这里把
    // 内层 pty 变成本会话的控制终端，让 /dev/tty 指向它；ctty 挂断会 SIGHUP 本进程，先忽略。
    // （本文件只有这一个测试，进程级副作用不出这个测试二进制。）
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        assert!(
            libc::setsid() >= 0,
            "setsid: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            libc::ioctl(0, libc::TIOCSCTTY.into(), 0),
            0,
            "TIOCSCTTY on fd 0"
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let entered = rt.enter();
    let mut rx = wing::tui::spawn_event_stream();
    drop(entered);

    // 鼠标：SGR 编码的左键按下（1-based 10/20 → crossterm 报 0-based）。
    write_all(outer_master, b"\x1b[<0;10;20M");
    match expect_event(&rt, &mut rx, |e| matches!(e, TermEvent::Mouse(_)), "mouse") {
        TermEvent::Mouse(mouse) => {
            assert_eq!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
            assert_eq!((mouse.column, mouse.row), (9, 19), "0-based 坐标");
        }
        other => panic!("expected a mouse event, got {other:?}"),
    }

    // bracketed paste：内容原样到达（接驳层的保护区也在这条链上）。
    write_all(outer_master, b"\x1b[200~hello\x1b[201~");
    match expect_event(&rt, &mut rx, |e| matches!(e, TermEvent::Paste(_)), "paste") {
        TermEvent::Paste(text) => assert_eq!(text, "hello"),
        other => panic!("expected a paste event, got {other:?}"),
    }

    // focus 进 / 出。
    write_all(outer_master, b"\x1b[I");
    match expect_event(
        &rt,
        &mut rx,
        |e| matches!(e, TermEvent::Focus(_)),
        "focus in",
    ) {
        TermEvent::Focus(true) => {}
        other => panic!("expected focus-in, got {other:?}"),
    }
    write_all(outer_master, b"\x1b[O");
    match expect_event(
        &rt,
        &mut rx,
        |e| matches!(e, TermEvent::Focus(_)),
        "focus out",
    ) {
        TermEvent::Focus(false) => {}
        other => panic!("expected focus-out, got {other:?}"),
    }

    // resize：crossterm 只在 SIGWINCH 时读一次尺寸；尺寸取自 fd 0（内层 pty），
    // 所以先在内层 slave 上设 winsize 再发信号。
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 42,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(0, libc::TIOCSWINSZ, &ws) },
        0,
        "set winsize on fd 0"
    );
    unsafe { libc::raise(libc::SIGWINCH) };
    match expect_event(
        &rt,
        &mut rx,
        |e| matches!(e, TermEvent::Resize(..)),
        "resize",
    ) {
        TermEvent::Resize(w, h) => assert_eq!((w, h), (42, 24), "列 × 行"),
        other => panic!("expected a resize event, got {other:?}"),
    }

    // 空闲：poll 超时 → Tick。
    expect_event(&rt, &mut rx, |e| matches!(e, TermEvent::Tick), "tick");

    // 收尾（与其它 teardown 测试同一约定）。
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
    assert_eq!(unsafe { libc::isatty(0) }, 1, "fd 0 必须还是 tty");
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
        libc::close(outer_master);
        libc::close(outer_slave);
    }
}
