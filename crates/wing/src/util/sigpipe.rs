//! SIGPIPE 处置（进程级，两个开关）。
//!
//! 四种前端形态对「写一个读端已关闭的管道」要相反的语义，所以只有两个开关，
//! 谁在哪装写在这里：
//!
//! - **流式前端**（stdio / ACP / 编排 CLI）：stdout 就是输出流，下游随时可以走
//!   （`wing ps | head`、`| grep -q`）。`main` 在解析参数之前就
//!   [`restore_default`]——读端一走内核在 `write` 里终止进程（shell 看到 141，
//!   与 `seq` / `grep` 一致），`cmd` 下的 `print!` / `println!` 与 stdio 的帧口
//!   都不用各自处理 EPIPE。
//! - **终端 owner**（TUI / 首次运行向导）：stdout 是 tty（tty 只会 `EIO`，不会
//!   EPIPE），进程里会撞上 EPIPE 的是**别的**管道——剪贴板 helper 的 stdin
//!   （[`crate::util::clipboard`]）、stdin 接驳的 stop pipe
//!   （[`crate::tui::stdin_proxy`]）。那几处把 `Err` 当可恢复错误（换下一个 helper
//!   / 落回 OSC52，拷贝失败不该升级成进程死亡），而终端 owner 无论如何都得活着
//!   把 raw 模式与鼠标上报关掉。所以 [`TuiSessionGuard`] 在会话期
//!   [`ignore`]、退出时 [`restore_default`]。
//!
//! [`TuiSessionGuard`]: crate::cmd::TuiSessionGuard
//!
//! 两个开关都只影响**管道 / FIFO**：std 写 socket 走 `MSG_NOSIGNAL`（macOS
//! `SO_NOSIGPIPE`），WS / HTTP 的对端关闭仍是普通的 `EPIPE` 错误。
//!
//! 新增前端照这两条归类：写「输出流」→ 默认处置；自己拿终端 → 会话期 [`ignore`]。

/// 恢复默认处置：写一个读端已关闭的管道 → 内核终止进程（141）。
#[cfg(unix)]
pub fn restore_default() {
    // SAFETY: 只改本进程的信号处置表，SIG_DFL 不安装任何用户态 handler。处置是
    // 进程级的，运行期已起的工作线程同样生效。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// 忽略 SIGPIPE：写失败返回 `EPIPE`，由调用方决定怎么降级。
#[cfg(unix)]
pub fn ignore() {
    // SAFETY: 同上，处置换成 SIG_IGN（Rust 运行期在 `main` 之前装的就是它）。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// Windows 没有 SIGPIPE：写关闭的管道就是普通的 `Err`，两个开关都是空操作。
#[cfg(not(unix))]
pub fn restore_default() {}

#[cfg(not(unix))]
pub fn ignore() {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// 读回当前处置并复原（`signal` 返回**前一个** handler）。
    fn disposition(restore: libc::sighandler_t) -> libc::sighandler_t {
        // SAFETY: 只在本进程装 / 读 SIGPIPE 的处置，结束时由调用方复原。
        unsafe { libc::signal(libc::SIGPIPE, restore) }
    }

    /// 两个开关装的是它们名字说的那个处置（打反了 `141` 与「可恢复错误」就对调了）。
    #[test]
    fn the_two_switches_install_the_dispositions_they_name() {
        let previous = disposition(libc::SIG_IGN);
        assert_eq!(previous, libc::SIG_IGN, "the runtime's default must hold");

        restore_default();
        let installed = disposition(libc::SIG_IGN);
        assert_eq!(
            installed,
            libc::SIG_DFL,
            "restore_default must install SIG_DFL"
        );

        ignore();
        let installed = disposition(previous);
        assert_eq!(installed, libc::SIG_IGN, "ignore must install SIG_IGN");
    }
}
