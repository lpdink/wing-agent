//! 负数基线（spike 证据之二）：**没有** stdin 预处理层时，一条 DECRPM 应答（探针
//! `CSI ? 5522 $ p` 的回复）会让 crossterm 0.29 的解析器永久扣住缓冲，之后到达的按键都被
//! 并进这个「半截序列」——一个公开事件都出不来。
//!
//! 比「没有事件」更糟的是事件任务连**超时都回不来**：crossterm 的 `try_read` 在 poll 报
//! 就绪后进入「读 → 无事件 → 再读」的内循环，而对 tty 的 read 是阻塞的——半截序列把
//! 解析器卡死之后，任务就停在那次 `read(fd 0)` 里，直到碰巧出现 `u` / `c` 这类 final。
//! 本测试把这两段（毒化 + `u` 才恢复）都钉下来。
//!
//! 这是 `tui::stdin_proxy` 必须摘走 `ESC[? … $y` 的**直接理由**：阴性终端在拆接驳前、
//! SSH 静默时保留接驳，都属于「应答仍可能到达」的场景；不摘，应答一到整条输入链就哑掉。
//!
//! 单独一个集成测试文件 = 单独一个进程：fd 0 与 crossterm 的事件源都是进程级全局，
//! 不能和任何「装上接驳」的文件混在同一个进程里跑。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

mod common;

use common::pty::{make_raw, openpty_pair, write_all};

#[test]
fn crossterm_swallows_every_key_after_a_decrpm_reply() {
    let (master, slave) = openpty_pair();
    make_raw(slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(unsafe { libc::dup2(slave, 0) }, 0, "fd 0 → pty slave");
    unsafe { libc::close(slave) };

    // 事件循环（单独线程：毒化后 poll 会卡在阻塞读里，主线程还要喂字节）。
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let events = Arc::clone(&events);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match crossterm::event::poll(Duration::from_millis(50)) {
                    Ok(true) => events
                        .lock()
                        .unwrap()
                        .push(format!("{:?}", crossterm::event::read().expect("read"))),
                    Ok(false) => {}
                    Err(e) => panic!("poll: {e}"),
                }
            }
        })
    };

    // 假终端：先回一条「不支持」的探针应答，再按一个普通键。
    std::thread::sleep(Duration::from_millis(150));
    write_all(master, b"\x1b[?5522;0$y");
    std::thread::sleep(Duration::from_millis(50));
    write_all(master, b"a");

    // 没有任何东西把它摘走：`ESC[?…y` 进解析缓冲后永远等不到 `u`/`c`，`a` 也被并进去
    // —— 公开事件一个都出不来（而且 poll 连超时都回不来，任务停在 read 里）。
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        events.lock().unwrap().is_empty(),
        "毒化形态下不该有任何事件（有的话 crossterm 行为变了）：{:?}",
        events.lock().unwrap()
    );

    // 恢复：`u` 充当半截序列的 final（解析成键盘增强应答、被内部过滤器丢弃并清空缓冲），
    // 卡住的 poll 随之返回；其后的按键重新可见。这也证明「哑掉」≠「字节被吃」。
    write_all(master, b"u");
    std::thread::sleep(Duration::from_millis(200));
    write_all(master, b"b");
    let deadline = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < deadline
        && !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.contains("Char('b')"))
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::SeqCst);
    poller.join().expect("poller");
    let events = events.lock().unwrap().clone();
    assert!(
        events.iter().any(|event| event.contains("Char('b')")),
        "`u` 清掉解析器之后按键必须重新可见（这里钉恢复形态）：{events:?}"
    );

    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
    }
}
