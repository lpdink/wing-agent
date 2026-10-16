//! 探针阳性路径的端到端证据（pty 级；单独进程 = fd 0/1 与 crossterm 事件源都是进程级全局）：
//!
//! 1. 假终端对启动探针（DECRQM）应答 `Ps=2`（支持）→ `settle_paste_channel` 判阳性、
//!    接驳保留；
//! 2. `spawn_event_stream` 开出 mode 5522（stdout 上出现 `?5522h`）并接上协议机；
//! 3. 终端发一条 unsolicited paste event（OK + listing + DONE）→ 协议机写下带 id 的
//!    follow-up read → 终端回 OK + DATA + DONE → 事件流交付 `TermEvent::Clipboard`。
//!
//! stdout 被重定向到外层 pty 的 slave，应用写出的控制序列（探针查询 / `?5522h` /
//! read 请求）都能被测试从 master 侧读到；结束前还原 fd 1。

use std::os::fd::RawFd;
use std::time::Duration;
use std::time::Instant;

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 base64（测试里只用来拼协议报文；实现与协议机无关）。
fn b64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (a << 16) | (b << 8) | c;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

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
    let mut buf = [0u8; 4096];
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

/// 一条 5522 包的完整线上字节（前缀 + 负载 + ST）。
fn packet(meta: &str, payload: Option<&[u8]>) -> Vec<u8> {
    let mut out = b"\x1b]5522;".to_vec();
    out.extend_from_slice(meta.as_bytes());
    if let Some(payload) = payload {
        out.push(b';');
        out.extend_from_slice(b64(payload).as_bytes());
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

#[test]
fn a_positive_probe_opens_5522_and_serves_a_clipboard_read() {
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

    // 装上接驳：探针查询立即写出。
    wing::tui::stdin_proxy::install().expect("install stdin proxy");
    let query = b"\x1b[?5522$p";
    // 分流层吸收任何 DECRPM 应答：别的 mode 的、形状合法但 Ps 段坏掉的、以及重复的 5522
    // 应答都只被吞掉（记录 / 忽略），不影响判定，也不许漏成按键垃圾。
    write_all(outer_master, b"\x1b[?2026;2$y");
    write_all(outer_master, b"\x1b[?99;1;2$y");
    write_all(outer_master, b"\x1b[?5522;2$y"); // 假终端的应答：支持
    write_all(outer_master, b"\x1b[?5522;2$y"); // 重复应答：没有新信息
    let written = read_written(outer_master, Duration::from_secs(1), |acc| {
        acc.windows(query.len()).any(|w| w == query)
    });
    assert!(
        written.windows(query.len()).any(|w| w == query),
        "install 必须发出 DECRQM 探针查询，实收 {written:?}"
    );

    // 判定阳性：接驳保留；判定幂等（第二次读缓存，不再等待）。
    assert_eq!(
        wing::tui::stdin_proxy::settle_paste_channel(),
        wing::tui::stdin_proxy::PasteChannel::Supported
    );
    assert_eq!(
        wing::tui::stdin_proxy::settle_paste_channel(),
        wing::tui::stdin_proxy::PasteChannel::Supported
    );
    assert!(
        wing::tui::stdin_proxy::is_installed(),
        "阳性判定必须保留接驳"
    );

    // 主 TUI 事件流：阳性判定开 `?5522h` 并接上协议机。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let entered = rt.enter();
    let mut rx = wing::tui::spawn_event_stream();
    drop(entered);
    let written = read_written(outer_master, Duration::from_secs(1), |acc| {
        acc.windows(8).any(|w| w == b"\x1b[?5522h")
    });
    assert!(
        written.windows(8).any(|w| w == b"\x1b[?5522h"),
        "阳性终端必须收到 ?5522h，实收 {written:?}"
    );

    // 终端：用户 Cmd+V → unsolicited paste event（listing 只有 text/plain）。
    write_all(outer_master, &packet("type=read:status=OK:pw=b3Rw", None));
    write_all(
        outer_master,
        &packet(
            &format!("type=read:status=DATA:mime={}:pw=b3Rw", b64(b".")),
            Some(b"text/plain\n"),
        ),
    );
    write_all(outer_master, &packet("type=read:status=DONE:pw=b3Rw", None));

    // 协议机写下 follow-up read 请求（带唯一 id）→ 解析出 id。
    let request = read_written(outer_master, Duration::from_secs(2), |acc| {
        acc.windows(b"id=wing-paste-".len())
            .any(|w| w == b"id=wing-paste-")
    });
    let text = String::from_utf8_lossy(&request).into_owned();
    let id = text
        .split("id=")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .expect("read 请求必须带 id");
    assert_eq!(id, "wing-paste-1");

    // 终端回 OK + DATA + DONE（同 id）→ 事件流交付文本。
    write_all(
        outer_master,
        &packet(&format!("type=read:status=OK:id={id}"), None),
    );
    let payload = b"hello from the pty";
    write_all(
        outer_master,
        &packet(
            &format!("type=read:status=DATA:id={id}:mime={}", b64(b"text/plain")),
            Some(payload),
        ),
    );
    write_all(
        outer_master,
        &packet(&format!("type=read:status=DONE:id={id}"), None),
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut delivered = None;
    while Instant::now() < deadline && delivered.is_none() {
        match rx.try_recv() {
            Ok(wing::tui::TermEvent::Clipboard(wing::tui::ClipboardEvent::Delivered(
                wing::tui::ClipboardPayload::Text(text),
            ))) => delivered = Some(text),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert_eq!(
        delivered.as_deref(),
        Some("hello from the pty"),
        "阳性通道必须把读回的文本投进事件流"
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
