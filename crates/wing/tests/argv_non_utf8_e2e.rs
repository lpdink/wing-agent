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

/// Scratch `$WING_HOME`, removed on drop.
///
/// The intake check runs before wing touches the environment, but a regression
/// must fail here rather than against whatever gateway the developer has
/// running. `tag` keeps concurrent tests from sharing (and deleting) one home.
struct ScratchHome {
    path: PathBuf,
}

impl ScratchHome {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("wing-e2e-argv-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("core")).expect("create scratch home");
        Self { path }
    }
}

impl Drop for ScratchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawn the real binary with `args` and wait for it to exit.
///
/// A timeout is part of the contract: the diagnostic must be *fail-fast*. A
/// regression that let the bytes through would instead enter TUI / stdio mode
/// and hang here, so the test fails loudly instead of blocking the suite.
async fn run(home: &ScratchHome, args: &[OsString]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_wing"))
        .args(args)
        .env("WING_HOME", &home.path)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn wing");
    match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await {
        Ok(result) => result.expect("wait for wing"),
        Err(_) => panic!(
            "`wing` did not exit on a non-UTF-8 argument — the command line must be rejected \
             before any frontend starts"
        ),
    }
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
    let home = ScratchHome::new("plain");
    let output = run(&home, &[undecodable_flag()]).await;
    assert_rejected(&output, 1);
}

#[tokio::test]
async fn stdio_mode_reports_the_offending_argument() {
    // `-p` selects stdio mode, whose filter drops SDK-injected flags before
    // clap parses: it must never be handed bytes wing cannot decode.
    let home = ScratchHome::new("stdio");
    let output = run(&home, &[os("-p"), os("hi"), undecodable_flag()]).await;
    assert_rejected(&output, 3);
}

#[tokio::test]
async fn valid_multibyte_arguments_reach_the_parser() {
    // Only *undecodable* bytes are refused — non-ASCII UTF-8 is ordinary input.
    // (`--tag=中文` is then refused by the misplaced-flag gate, whose message is
    // the observable proof that the argument arrived intact and un-mangled.)
    let home = ScratchHome::new("multibyte");
    let output = run(&home, &[os("--tag=中文")]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("not valid UTF-8"),
        "multi-byte UTF-8 must not be reported as undecodable: {stderr}"
    );
    assert!(
        stderr.contains("top-level --tag only applies to stdio mode"),
        "the argument must reach dispatch intact: {stderr}"
    );
}
