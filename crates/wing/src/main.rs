//! wing — AI agent CLI.
#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use clap::Parser;
use std::process::ExitCode;

use wing::cmd::{Cli, dispatch};
use wing::stdio::{filter_unknown_args, is_stdio_mode};

#[tokio::main]
async fn main() -> ExitCode {
    // Collect raw args (skip program name).
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    // In stdio mode, filter unknown arguments before clap parsing.
    let filtered_args = if is_stdio_mode(&raw_args) {
        filter_unknown_args(raw_args)
    } else {
        raw_args
    };

    let cli = Cli::parse_from(std::iter::once("wing".to_string()).chain(filtered_args));
    dispatch(cli).await
}
