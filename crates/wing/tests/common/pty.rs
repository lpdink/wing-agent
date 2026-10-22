//! pty 原语：集成测试自己开一对伪终端，模拟「用户终端」这一侧。
//!
//! 单独一个 pty 就是单独一个进程里的 fd 0 / 事件源，所以这些 helper 只碰原始 fd，
//! 不持有任何全局状态——调用方负责 dup2 / restore。

use std::os::fd::RawFd;
use std::time::Duration;
use std::time::Instant;

/// 开一对 pty，返回 `(master, slave)`。
pub fn openpty_pair() -> (RawFd, RawFd) {
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

/// 把 fd 切成 raw 模式（无回显、无行缓冲、无信号键）。
pub fn make_raw(fd: RawFd) {
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(fd, &mut term), 0);
        libc::cfmakeraw(&mut term);
        assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &term), 0);
    }
}

/// 一次写完 `bytes`（pty 上短写即视为故障）。
pub fn write_all(fd: RawFd, bytes: &[u8]) {
    let n = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
    assert_eq!(n as usize, bytes.len(), "pty write");
}

/// 从 master 读一段（模拟终端看应用写了什么）：`want` 满足即提前停，`budget` 到点返回已读。
pub fn read_written(fd: RawFd, budget: Duration, want: impl Fn(&[u8]) -> bool) -> Vec<u8> {
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
