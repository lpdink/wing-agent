//! wing — AI agent CLI. Gateway mode, WebSocket communication.

// Forbid accidental stdout/stderr in the library.
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]

pub mod acp;
pub mod app;
pub mod cmd;
pub mod config;
pub mod gateway;
pub mod protocol;
pub mod render;
pub mod shared;
pub mod stdio;
pub mod tui;
pub mod ui;
pub mod util;

// 构建期 commit 的解析：与 `build.rs` 同一份源码，只在测试里编译，让三档顺序
// （env → git → build_commit 文件）可被 `cargo test` 覆盖。
#[cfg(test)]
#[path = "../build_meta.rs"]
mod build_meta;
