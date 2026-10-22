//! 负数基线（spike 证据）：**没有** stdin 预处理层时，crossterm 0.29 会把终端发来的
//! `OSC 5522` 序列解析成一串按键（`Alt+]` + 逐字符）。
//!
//! 这不是产品路径，是**回归证据**：本层（`tui::stdin_proxy`）的全部理由建立在这个事实上，
//! 所以它必须被钉住。若将来 crossterm 升级后本测试开始失败（即它不再误解析），说明预处理
//! 层的存在价值需要重新评估——那正是这个测试要提示的。
//!
//! 单独一个集成测试文件 = 单独一个进程：fd 0 与 crossterm 的事件源都是进程级全局，
//! 不能和「装上接驳」的那份（`stdin_proxy_pty.rs`）混在同一个进程里跑。

use std::time::Duration;

mod common;

use common::pty::{make_raw, openpty_pair, write_all};

#[test]
fn crossterm_turns_a_raw_osc_5522_sequence_into_keystrokes() {
    let (master, slave) = openpty_pair();
    make_raw(slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(unsafe { libc::dup2(slave, 0) }, 0, "fd 0 → pty slave");
    unsafe { libc::close(slave) };

    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        // 终端在 5522 模式下发的一条 unsolicited paste event（OK + listing + DONE）。
        let event = concat!(
            "\x1b]5522;type=read:status=OK:pw=b3Rw\x1b\\",
            "\x1b]5522;type=read:status=DATA:mime=Lg==:pw=b3Rw;dGV4dC9wbGFpbg==\x1b\\",
            "\x1b]5522;type=read:status=DONE:pw=b3Rw\x1b\\",
        );
        // RawFd 是 Copy：线程结束不会关掉它，fd 留给测试进程。
        write_all(master, event.as_bytes());
    });

    // 收集一段时间内的全部事件：先按「误解析」形态断言，出现别的形态就是 crossterm 变了。
    let deadline = std::time::Instant::now() + Duration::from_millis(1200);
    let mut events = Vec::new();
    while std::time::Instant::now() < deadline {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => events.push(crossterm::event::read().expect("read event")),
            Ok(false) => {}
            Err(e) => panic!("poll: {e}"),
        }
    }
    writer.join().expect("writer");

    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;
    assert!(
        !events.is_empty(),
        "crossterm 应该收到东西（这正是问题所在）"
    );
    // 首事件：`ESC ]` 被当成 `Alt+]`。
    match &events[0] {
        Event::Key(key) => {
            assert_eq!(key.code, KeyCode::Char(']'));
            assert!(
                key.modifiers.contains(KeyModifiers::ALT),
                "ESC 引入的字节被当成 Alt 修饰：{key:?}"
            );
        }
        other => panic!("期望一个按键事件，得到 {other:?}"),
    }
    // 其后是逐字符按键（协议数据的裸奔）。
    let chars: String = events
        .iter()
        .filter_map(|e| match e {
            Event::Key(key) => match key.code {
                KeyCode::Char(c) => Some(c),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        chars.contains("5522") && chars.contains("status") && chars.contains("type=read"),
        "协议数据被逐字符吐成按键：{chars:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Paste(_))),
        "这条序列不是 bracketed paste"
    );

    // 归还 fd 0（本进程后续没有用到，但别留下被替换的 stdin）。
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
    }
}
