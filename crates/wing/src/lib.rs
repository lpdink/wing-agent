//! wing — AI agent CLI. Gateway mode, WebSocket communication.

// Forbid accidental stdout/stderr in the library.
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]

pub mod app;
pub mod cmd;
pub mod config;
pub mod gateway;
pub mod protocol;
pub mod render;
pub mod tui;
pub mod ui;
pub mod util;
