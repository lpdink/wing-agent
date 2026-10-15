//! 「事件通道被灌满」时的收尾回归：任务停在 `tx.send(...).await` 上（唤醒字节够不到它），
//! 有界等待必须走满上限、记 warn、继续收尾；随后放行一个槽位，任务回到循环顶看到
//! EVENT_STOP 才退出——收尾不依赖「唤醒一定打断 send」。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 0、crossterm 事件源与 EVENT_* 都是进程级全局）。

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
fn teardown_survives_an_event_task_parked_on_a_full_channel() {
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
    let mut rx = wing::tui::spawn_event_stream();
    drop(entered);

    // 灌爆事件通道（容量 128）：SGR 鼠标一条只有 9 字节，200 条足够把第 129 次 send 挂起。
    const MOUSE: &[u8] = b"\x1b[<0;1;1M";
    let burst: Vec<u8> = MOUSE
        .iter()
        .cycle()
        .take(MOUSE.len() * 200)
        .copied()
        .collect();
    for chunk in burst.chunks(1024) {
        write_all(outer_master, chunk);
    }
    // 给任务跑完 128 次 send、停在 #129 的时间。
    std::thread::sleep(Duration::from_millis(150));

    // 收尾：任务停在 send 上，唤醒字节无处着力 → 有界等待走满 200ms 上限后才继续。
    let started = Instant::now();
    wing::tui::end_tui_session().expect("end tui session");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(150),
        "任务停在 send 上时必须走满有界等待（实测 {elapsed:?}）"
    );
    assert!(
        elapsed < Duration::from_millis(600),
        "收尾必须有界：{elapsed:?}"
    );

    // 放行一个槽位：挂起的 send 完成，任务回到循环顶 → 看到 EVENT_STOP → 退出。
    let _ = rx.blocking_recv();

    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let closer = std::thread::spawn(move || {
        drop(rt);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("runtime teardown timed out: the event task never left the parked send");
    closer.join().expect("closer thread");

    assert_eq!(unsafe { libc::isatty(0) }, 1, "fd 0 必须还是 tty");
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
        libc::close(outer_master);
        libc::close(outer_slave);
    }
}
