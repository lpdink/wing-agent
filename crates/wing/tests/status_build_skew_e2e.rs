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
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The commit the binary under test was built from.
const CLI_COMMIT: &str = env!("WING_COMMIT_HASH");

/// The fake gateway's commit: a hash no build of this repo produces.
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

async fn serve(listener: TcpListener, build: Build) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(handle_conn(stream, build));
    }
}

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

/// Read one HTTP request head (`GET`, no body); the rest stays buffered.
async fn read_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            buf.drain(..pos + 4);
            return Some(head);
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

// ============================================================
// Fixtures
// ============================================================

fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wing-status-skew-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("core")).unwrap();
    dir
}

/// Start the fake gateway and point a fresh `$WING_HOME` at it (the CLI reads
/// its endpoint from `$WING_HOME/core/config.yaml`).
async fn start_gateway(build: Build, tag: &str) -> PathBuf {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, build));

    let home = scratch_home(tag);
    std::fs::write(
        home.join("core").join("config.yaml"),
        format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
    )
    .unwrap();
    home
}

/// Run the real binary and collect (exit code, stdout, stderr).
async fn run_wing(args: &[&str], home: &PathBuf) -> (i32, String, String) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(args)
        .env("WING_HOME", home)
        .env_remove("RUST_LOG")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let output = match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await
    {
        Ok(result) => result.unwrap(),
        Err(_) => panic!("`wing {args:?}` did not exit in 30s"),
    };
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

// ============================================================
// Tests
// ============================================================

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
    assert_eq!(stderr, "", "no warning for a matching build");
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
    assert_eq!(stderr, "", "no warning without a gateway hash");
}
