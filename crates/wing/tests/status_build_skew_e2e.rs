//! End-to-end: `wing status` speaks up when the gateway and this CLI were
//! built from different commits.
//!
//! The unit tests in `cmd::status` pin the decision matrix; what only a real
//! process can show is the wiring around it —
//!
//! 1. the warning lands on **stderr**, one line, while stdout stays the plain
//!    status report (`wing reload`'s notes follow the same split);
//! 2. the CLI's half of the comparison is the commit *this binary* was built
//!    from: `build.rs` hands the same `WING_COMMIT_HASH` to every target of the
//!    package, so the test binary knows the value the CLI under test compares
//!    against.
//!
//! The fake gateway answers `/api/health` and nothing else — that is all
//! `wing status` reaches for.

use std::path::PathBuf;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

mod common;

use common::e2e::{read_head, run_wing, start_fake_gateway};

/// The commit the binary under test was built from.
const CLI_COMMIT: &str = env!("WING_COMMIT_HASH");

/// The fake gateway's commit: a hash no build of this repo produces — fixed, so
/// a failure is readable (`assert_ne!` in the skew test guards the collision a
/// constant cannot rule out on its own).
const GATEWAY_COMMIT: &str = "0badc0de";

/// What the fake gateway reports as its build.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Build {
    /// A gateway that reports this commit.
    Commit(&'static str),
    /// A gateway old enough to send no `commit` field at all.
    Silent,
}

// ============================================================
// Fake gateway: minimal HTTP/1.1, `/api/health` only
// ============================================================
async fn handle_conn(mut stream: TcpStream, build: Build) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some(head) = read_head(&mut stream, &mut buf).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let path = target.split('?').next().unwrap_or("");
        // Anything else is a test bug, not a case to answer with `{}`: dropping
        // the connection makes `wing status` say the gateway is down, which
        // points at the offending path.
        if path != "/api/health" {
            return;
        }

        let body = match build {
            Build::Commit(commit) => format!(
                r#"{{"service":"wing-gateway","status":"ok","version":"test","commit":"{commit}","uptime":61}}"#
            ),
            Build::Silent => {
                r#"{"service":"wing-gateway","status":"ok","version":"test","uptime":61}"#
                    .to_string()
            }
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            body.len(),
            body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}
// ============================================================
// Fixtures
// ============================================================
// ============================================================
// Tests
// ============================================================

/// Bind a port, publish it in a scratch `WING_HOME`, report `build` there.
async fn start_gateway(build: Build, tag: &str) -> PathBuf {
    let (home, _port) = start_fake_gateway("wing-status-skew", tag, build, handle_conn).await;
    home
}

/// A skew earns exactly one warning line, on stderr, naming both builds — the
/// report on stdout has none.
#[tokio::test]
async fn a_skewed_gateway_warns_once_on_stderr() {
    assert_ne!(GATEWAY_COMMIT, CLI_COMMIT, "the fixture must be a skew");

    let home = start_gateway(Build::Commit(GATEWAY_COMMIT), "skew").await;
    let (code, stdout, stderr) = run_wing(&["status"], &home).await;

    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Gateway is running"), "{stdout}");
    assert!(
        !stdout.contains("Warning"),
        "stdout is the report: {stdout}"
    );

    let warnings: Vec<&str> = stderr.lines().filter(|l| l.contains("Warning")).collect();
    // A build without git carries the `unknown` sentinel: it has no hash of its
    // own to compare, and the contract is silence (the unit tests pin that
    // rule) — so the fixture asks for a warning only when this binary has a
    // hash to offer.
    if CLI_COMMIT == "unknown" {
        assert!(warnings.is_empty(), "no hash to compare: {stderr:?}");
        return;
    }
    assert_eq!(warnings.len(), 1, "one warning line, got: {stderr:?}");
    assert!(warnings[0].contains(CLI_COMMIT), "{stderr}");
    assert!(warnings[0].contains(GATEWAY_COMMIT), "{stderr}");
}

/// Two sides from the same commit: the report, and nothing else.
#[tokio::test]
async fn a_matching_gateway_is_silent() {
    let home = start_gateway(Build::Commit(CLI_COMMIT), "match").await;
    let (code, stdout, stderr) = run_wing(&["status"], &home).await;

    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Gateway is running"), "{stdout}");
    assert!(
        !stderr.contains("Warning"),
        "no warning for a matching build: {stderr}"
    );
}

/// A gateway that predates commit injection reports nothing to compare with —
/// "unknown vs hash" is not a mismatch.
#[tokio::test]
async fn a_gateway_without_a_commit_is_silent() {
    let home = start_gateway(Build::Silent, "no-commit").await;
    let (code, stdout, stderr) = run_wing(&["status"], &home).await;

    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("Commit:") && stdout.contains("unknown"),
        "{stdout}"
    );
    assert!(
        !stderr.contains("Warning"),
        "no warning without a gateway hash: {stderr}"
    );
}
