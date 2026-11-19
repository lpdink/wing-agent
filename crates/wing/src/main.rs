//! wing — AI agent CLI.
#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use clap::Parser;
use std::process::ExitCode;

use wing::cmd::argv::{EXIT_CODE_USAGE, decode_args};
use wing::cmd::{Cli, dispatch};
use wing::stdio::{filter_unknown_args, is_stdio_mode};

#[tokio::main]
async fn main() -> ExitCode {
    restore_default_sigpipe();

    // Collect raw args as OS strings (`argv[1..]`) before anything else: a
    // non-UTF-8 argument must yield a diagnostic and a documented exit code,
    // not a panic out of `std::env::args()` — and it must be caught here, ahead
    // of both the stdio filter and clap, so every path reports it the same way.
    let raw_args = match decode_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            // stderr, never stdout: in stdio mode stdout is the protocol stream.
            eprintln!("wing error: {err}");
            return ExitCode::from(EXIT_CODE_USAGE);
        }
    };

    // In stdio mode, filter unknown arguments before clap parsing.
    let filtered_args = if is_stdio_mode(&raw_args) {
        filter_unknown_args(raw_args)
    } else {
        raw_args
    };

    let cli = Cli::parse_from(std::iter::once("wing".to_string()).chain(filtered_args));
    dispatch(cli).await
}

/// A downstream that stops reading (`wing ps | head`, `| grep -q`, an orchestrator
/// closing the pipe) kills this process with SIGPIPE — silently, status 141 as the
/// shell reports it — like `seq` / `yes` / `grep` do.
///
/// Rust's runtime sets `SIGPIPE` to `SIG_IGN` before `main`, so instead every write
/// site (the `print!` / `println!` calls under `cmd`, the stdio frame sink, clap's
/// help output) turns a closed pipe into `failed printing to stdout: Broken pipe`
/// — a panic with a backtrace, exit code 101. That is not a louder failure, it is
/// a wrong one: the only observer left when the read end is gone is the shell, and
/// the shell's vocabulary for "the consumer went away" is 141. Restoring the
/// default disposition lets the kernel kill us inside `write`: no panic, no stderr
/// noise, no half-done cleanup.
///
/// Pipes and FIFOs only: std writes sockets with `MSG_NOSIGNAL` (macOS:
/// `SO_NOSIGPIPE`), so a peer that closed a WS / HTTP connection still yields an
/// ordinary `EPIPE` error.
fn restore_default_sigpipe() {
    #[cfg(unix)]
    {
        // SAFETY: only this process's signal disposition is written, and SIG_DFL
        // installs no user-space handler. Dispositions are process-wide, so the
        // runtime's worker threads are covered as well.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
}
