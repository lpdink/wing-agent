//! 「reader 卡在半截转义序列上」的收尾回归：唤醒字节必须被**重投**才救得回来。
//!
//! crossterm 0.29 的解析器在 poll 报就绪后进入 `loop { read(fd 0) }`，只有产出事件才
//! 返回。解析器扣着一条**半截 CSI 序列**（`ESC [`）时，read 会重新阻塞；收尾的第一个
//! 唤醒字节会被解析器当成该序列的 final 吃掉（`Err` → 只清缓冲、不产事件，见
//! `event/sys/unix/{mio,parse}.rs`），任务再次阻塞在 read 上。`stop_event_stream` 在有界
//! 等待里周期性重投唤醒字节：第二次落在空解析器上必然产出事件，把任务带回 flag 检查。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 0、crossterm 事件源与 `EVENT_*` 都是进程级
//! 全局）。去掉重投后本测试红：任务卡在 `read(fd 0)`，`drop(rt)` 永不返回；半截 OSC
//! 字符串（无终止符）重投也救不回，属既定「超时后继续收尾」规格，不在这里钉。

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

#[test]
fn teardown_rescues_a_reader_holding_a_partial_csi() {
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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let entered = rt.enter();
    let rx = wing::tui::spawn_event_stream();
    drop(entered);

    // 半截 CSI（`ESC [` 是粘贴开界的前缀，接驳层最多扣 25ms 后冲给 crossterm）：解析器
    // 扣住它、不产事件，reader 进入「阻塞 read → 无事件 → 再 read」的卡死形态。
    write_all(outer_master, b"\x1b[");
    std::thread::sleep(Duration::from_millis(150));
    drop(rx);

    // 收尾：首字节只清解析器缓冲，重投的第二个字节才产出事件、带回 flag 检查。
    let started = Instant::now();
    wing::tui::end_tui_session().expect("end tui session");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "end_tui_session 必须有界（不等待一个退不出来的任务）：{:?}",
        started.elapsed()
    );

    // runtime 能整个收掉 = reader 确实退出了（无重投时它卡在 read(fd 0)，这一步超时）。
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let closer = std::thread::spawn(move || {
        drop(rt);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("runtime teardown timed out: the reader is still blocked behind a partial CSI");
    closer.join().expect("closer thread");

    // 收尾后 fd 0 已还原成外层 pty（会话收尾的既有契约不变）。
    assert_eq!(unsafe { libc::isatty(0) }, 1, "fd 0 必须还是 tty");
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
        libc::close(outer_master);
        libc::close(outer_slave);
    }
}
