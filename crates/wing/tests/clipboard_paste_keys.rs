//! 粘贴手势的两种解码形态（证据 + 回归）：没开 kitty 键盘协议时 `Ctrl+V` 是控制字节
//! `0x16`，crossterm 给 `Char('v') + CONTROL`；开了协议时 `Cmd+V` 回传 `CSI 118;9u`，
//! crossterm 给 `Char('v') + SUPER`。`app::modal::is_paste_clipboard_key` 的两种形态
//! 判断以这里钉住的事实为据（0x16 是控制字节，终端基本都原样转发；Ghostty 在图片
//! 剪贴板下把 Cmd+V 以 performable 回传形态交给应用）。
//!
//! 独立文件 = 独立进程：crossterm 的事件源是进程级单例，`fd 0` 替换与「首次 poll
//! 注册」的时序约束只在一个进程里成立一次（同 `crossterm_osc5522_misparse.rs` 的说明）。

use std::time::Duration;
mod common;

use common::pty::{make_raw, openpty_pair, write_all};

#[test]
fn crossterm_decodes_both_paste_gestures_as_v_with_the_documented_modifier() {
    let (master, slave) = openpty_pair();
    make_raw(slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(unsafe { libc::dup2(slave, 0) }, 0, "fd 0 → pty slave");
    unsafe { libc::close(slave) };

    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        // 传统编码的 Ctrl+V：裸控制字节。
        write_all(master, b"\x16");
        std::thread::sleep(Duration::from_millis(100));
        // kitty 键盘协议下 Ghostty 回传的 Cmd+V（`CSI 118;9u`；9 = 1 + super 位）。
        write_all(master, b"\x1b[118;9u");
    });

    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    let mut keys = Vec::new();
    while std::time::Instant::now() < deadline && keys.len() < 2 {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => {
                if let Event::Key(key) = crossterm::event::read().expect("read event") {
                    keys.push(key);
                }
            }
            Ok(false) => {}
            Err(e) => panic!("poll: {e}"),
        }
    }
    writer.join().expect("writer");

    assert_eq!(keys.len(), 2, "两种形态都要收到：{keys:?}");
    assert_eq!(keys[0].code, KeyCode::Char('v'));
    assert!(
        keys[0].modifiers.contains(KeyModifiers::CONTROL),
        "0x16 → Char('v') + CONTROL：{:?}",
        keys[0]
    );
    assert!(
        !keys[0].modifiers.contains(KeyModifiers::SHIFT),
        "解码不带 SHIFT：{:?}",
        keys[0]
    );
    assert_eq!(keys[1].code, KeyCode::Char('v'));
    assert!(
        keys[1].modifiers.contains(KeyModifiers::SUPER),
        "CSI 118;9u → Char('v') + SUPER：{:?}",
        keys[1]
    );
    assert!(
        !keys[1].modifiers.contains(KeyModifiers::CONTROL),
        "解码不带 CONTROL：{:?}",
        keys[1]
    );

    // 归还 fd 0（本进程后续没有用到，但别留下被替换的 stdin）。
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
    }
}
