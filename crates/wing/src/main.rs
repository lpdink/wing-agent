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
