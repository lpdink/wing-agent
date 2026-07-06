//! wing — AI agent CLI.
#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use clap::Parser;
use std::process::ExitCode;

use wing::cmd::{Cli, dispatch};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    dispatch(cli).await
}
