//! `install()` 在 fd 表紧张时的降级与回滚（真实故障注入，不 mock 系统调用）：
//!
//! - `dup(master)` 拿不到 fd → 唤醒副本降级为 `-1`（warn），install 继续；
//! - `pipe()` 拿不到 fd → 整段回滚：fd 0 还回原 tty、已开的 fd 全关、返回 `Err`，
//!   `is_installed()` 保持 false（没有半装的接驳层）。
//!
//! 注入方式：先撑满进程 fd 表，再**精确**释放 3 个槽位——按 install 的分配序
//! （`dup(0)` 1 个 → `openpty` 2 个 → `dup(master)` 失败 → `close(slave)` 放 1 个 →
//! `dup(src)` 用掉 → `pipe()` 需要 2 个）让失败点恰好落在唤醒副本与 pipe 上。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 表与 SHARED 都是进程级全局）。

use std::os::fd::RawFd;

/// 让 `tracing::warn!` 的参数真正求值：没有订阅者时宏连参数都不碰（callsite 的 interest
/// 缓存会是 never），日志行不会被覆盖到。
fn warn_args_evaluated() -> tracing::subscriber::DefaultGuard {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::level_filters::LevelFilter::WARN)
        .with_writer(std::io::sink)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    guard
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

#[test]
fn install_degrades_and_rolls_back_when_the_fd_table_is_tight() {
    // 这条 warn 的格式参数只有在订阅者启用 WARN 时才会被求值（覆盖也要）。
    let _logs = warn_args_evaluated();
    // 外层 pty = 「用户终端」；进程 fd 0 指向它的 slave。
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );

    // 把软上限压到一个确定的小值：撑满的开销与平台默认（CI 可能 64k+）无关。
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    let capped = libc::rlimit {
        rlim_cur: limit.rlim_cur.min(512),
        rlim_max: limit.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &capped) }, 0);

    // 撑满 fd 表（到 EMFILE 为止）。
    let mut held: Vec<RawFd> = Vec::new();
    loop {
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EMFILE),
                "撑满 fd 表"
            );
            break;
        }
        held.push(fd);
    }
    assert!(!held.is_empty(), "fd 表必须真的能被撑满");

    // 精确放出 3 个槽位（计数见文件头）。
    for _ in 0..3 {
        let fd = held.pop().expect("held fd");
        unsafe { libc::close(fd) };
    }

    let err = wing::tui::stdin_proxy::install().expect_err("pipe 拿不到 fd：install 必须失败");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EMFILE),
        "失败点必须落在 pipe（注入计数写死在文件头）：{err}"
    );
    assert!(
        !wing::tui::stdin_proxy::is_installed(),
        "失败的 install 不许留下接驳层"
    );

    // 回滚的契约：fd 0 已经还回原 tty——还能直接读到外层 master 写进去的字节。
    write_all(outer_master, b"q");
    let mut buf = [0u8; 8];
    let mut fds = [libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    }];
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) };
    assert_eq!(rc, 1, "回滚后 fd 0 应能直接读到外层 pty 的字节");
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    assert_eq!(&buf[..n as usize], b"q");

    // 失败路径把 fd 全还了：现在至少能再开一个。
    let probe = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert!(probe >= 0, "install 失败必须把 fd 全还回来");

    unsafe {
        if probe >= 0 {
            libc::close(probe);
        }
        for fd in held {
            libc::close(fd);
        }
        libc::dup2(saved, 0);
        libc::close(saved);
        libc::close(outer_master);
        libc::close(outer_slave);
    }
}
