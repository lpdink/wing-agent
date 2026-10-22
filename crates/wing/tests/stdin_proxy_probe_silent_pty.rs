//! 探针**静默**路径的端到端证据（pty 级；单独进程 = fd 0/1、crossterm 事件源与环境快照都是进程级全局）：
//!
//! **SSH 档静默**（远程 tmux / 慢链路）：判定阴性但**保留接驳**（迟到的应答必须仍被分流层
//! 吸收），一个 `?5522h` 都不发；随后迟到的应答（含被拆开的半截）不得毒化 crossterm 的按键
//! 解析——键盘照常。本地档静默（拆接驳）见 `stdin_proxy_probe_local_silent_pty.rs`。
//!
//! 环境变量在同一进程内注入（测试各自独立进程、注入发生在 install 读它之前；edition 2024
//! 的 `set_var` 因此是 unsafe 的、但此处无并发读者），这样远端档的判定与预算**与跑测试的
//! 机器环境无关**。

use std::time::Duration;
use std::time::Instant;

mod common;

use common::pty::{make_raw, openpty_pair, read_written, write_all};

#[test]
fn a_silent_remote_probe_keeps_the_splice_without_enabling_the_channel() {
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
    // 远端档：注入 SSH 环境（任何继承值都先清掉，避免与本地档混档）。
    for key in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"] {
        unsafe { std::env::remove_var(key) };
    }
    unsafe { std::env::set_var("SSH_CONNECTION", "10.0.0.1 51000 10.0.0.2 22") };

    // 装上接驳（发出探针查询），但终端一直不回答。
    wing::tui::stdin_proxy::install().expect("install stdin proxy");
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

    // 迟到的应答被分流层吞掉：先半截 `ESC[?5522;0$`（到点整段丢弃），再补 `y`——丢弃策略下
    // 尾巴按普通字节透传（一个普通按键），但**前缀**绝不能落进 crossterm：落了就会永久挂住
    // 按键解析，后面的 `k` 永远到不了。
    write_all(outer_master, b"\x1b[?5522;0$");
    std::thread::sleep(Duration::from_millis(150)); // 超过 SSH 档 100ms 的候选持有上限
    write_all(outer_master, b"y");
    std::thread::sleep(Duration::from_millis(50));
    write_all(outer_master, b"k");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(event) => {
                let text = format!("{event:?}");
                let done = text.contains("Char('k')");
                events.push(text);
                if done {
                    break;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(
        events.iter().any(|e| e.contains("Char('k')")),
        "迟到应答必须被吸收：键盘在它之后仍要照常工作：{events:?}"
    );
    assert!(
        events.iter().any(|e| e.contains("Char('y')")),
        "半截被丢弃后的尾巴按普通字节透传（登记行为，钉住形态）：{events:?}"
    );

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
