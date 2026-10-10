//! End-to-end: a non-UTF-8 argument is a diagnostic, not a panic.
//!
//! `std::env::args()` panicked when any argument was not valid Unicode, so a
//! single stray byte (a mis-encoded path, a latin-1 shell variable) took the
//! whole CLI down with a Rust backtrace — before clap, before any subcommand,
//! and without naming the argument at fault. The unit tests in `cmd::argv` pin
//! the decoder; these spawn the **real binary** with raw bytes
//! (`OsString::from_vec`) and pin what the caller actually sees: a readable
//! stderr line naming the argument, exit code 2, empty stdout, no panic — on
//! the plain path and on the stdio path, where those bytes would otherwise be
//! handed to the unknown-flag filter.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// The exit code wing documents for a command line it cannot decode (clap's
/// usage-error code, so "you called me wrong" stays one class of failure).
const USAGE_EXIT_CODE: i32 = 2;

/// `--tag=<invalid byte>` — a value that is not text under any encoding.
fn undecodable_flag() -> OsString {
    OsString::from_vec(b"--tag=\xff".to_vec())
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

/// Scratch `$WING_HOME`: the intake check runs before wing touches the
/// environment, but a regression must fail here rather than against whatever
/// gateway the developer has running.
fn scratch_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wing-argv-non-utf8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("core")).unwrap();
    dir
}

/// Spawn the real binary with `args` and wait for it to exit.
///
/// A timeout is part of the contract: the diagnostic must be *fail-fast*. A
/// regression that let the bytes through would instead enter TUI / stdio mode
/// and hang here, so the test fails loudly instead of blocking the suite.
async fn run(args: &[OsString]) -> Output {
    let home = scratch_home();
    let child = Command::new(env!("CARGO_BIN_EXE_wing"))
        .args(args)
        .env("WING_HOME", &home)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn wing");
    let output = match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await
    {
        Ok(result) => result.expect("wait for wing"),
        Err(_) => panic!(
            "`wing` did not exit on a non-UTF-8 argument — the command line must be rejected \
             before any frontend starts"
        ),
    };
    let _ = std::fs::remove_dir_all(&home);
    output
}

/// The contract for an undecodable command line: usage exit code, stderr naming
/// the argument and its bytes, nothing on stdout, no panic anywhere.
fn assert_rejected(output: &Output, position: usize) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(USAGE_EXIT_CODE),
        "expected the usage exit code, got {:?}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stderr.contains(&format!("argument #{position} is not valid UTF-8")),
        "stderr must name the offending argument by position: {stderr}"
    );
    assert!(
        stderr.contains("\\xff"),
        "stderr must show the offending bytes escaped, so they survive any terminal: {stderr}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
        "the diagnostic must replace the panic, not sit next to one: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout stays clean (it is the protocol stream in stdio mode): {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn plain_mode_reports_the_offending_argument() {
    // No subcommand, no `-p`: the path that would end up in the TUI.
    let output = run(&[undecodable_flag()]).await;
    assert_rejected(&output, 1);
}

#[tokio::test]
async fn stdio_mode_reports_the_offending_argument() {
    // `-p` selects stdio mode, whose filter drops SDK-injected flags before
    // clap parses: it must never be handed bytes wing cannot decode.
    let output = run(&[os("-p"), os("hi"), undecodable_flag()]).await;
    assert_rejected(&output, 3);
}

#[tokio::test]
async fn valid_multibyte_arguments_are_not_rejected() {
    // Only *undecodable* bytes are refused — non-ASCII UTF-8 is ordinary input.
    // (`--tag=中文` is afterwards refused by the misplaced-flag gate, so this
    // exits non-zero too; what matters is which message comes out.)
    let output = run(&[os("--tag=中文")]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("not valid UTF-8"),
        "multi-byte UTF-8 must not be reported as undecodable: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "it must reach the ordinary misplaced-flag gate instead: {stderr}"
    );
}
