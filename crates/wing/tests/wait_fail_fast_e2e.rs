//! End-to-end: `wing wait` against a fake gateway.
//!
//! Regression test for the fail-fast contract (2026-09-13 incident):
//!
//! - `wing wait` used to `warn!` and keep looping when `recv_event()` returned
//!   `None`. A closed tokio channel returns `None` instantly, so the process
//!   burned 100% CPU until `--timeout` and never told anyone *why*.
//! - The CLI also never initialized logging outside TUI / stdio, so the read
//!   task's real error (`Space limit exceeded: Message too long: … > 16777216`)
//!   went nowhere.
//!
//! These tests drive the real binary against a minimal in-process HTTP + WS
//! server: one stream dies with an oversized frame (exactly what a >16 MiB
//! `sync_session` payload does), the other delivers a normal `TurnResult`.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::SinkExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

/// The client's per-frame receive limit (tungstenite default).
const FRAME_LIMIT: usize = 16 * 1024 * 1024;

// A fake session that is "working" until the outcome is delivered over WS.
const SESSION_ID: &str = "20260101-000000-testtest";

/// What the fake gateway does once the client is subscribed.
#[derive(Clone, Copy)]
enum Scenario {
    /// Kill the client's read task with a frame one byte over the limit.
    OversizedFrame,
    /// Deliver a normal turn completion.
    TurnResult,
}

// ============================================================
// Fake gateway: minimal HTTP/1.1 + WS upgrade
// ============================================================

async fn serve(listener: TcpListener, scenario: Scenario) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(handle_conn(stream, scenario));
    }
}

async fn handle_conn(mut stream: TcpStream, scenario: Scenario) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, _body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        // Request target including the query string (e.g. `/api/session/info?…`).
        let target = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        let path = target.split('?').next().unwrap_or("").to_string();

        let is_upgrade = header_value(&head, "upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        if path == "/ws" && is_upgrade {
            let key = header_value(&head, "sec-websocket-key").unwrap_or_default();
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {}\r\n\r\n",
                derive_accept_key(key.as_bytes())
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                return;
            }
            let ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
            drive_ws(ws, scenario).await;
            return;
        }

        let body = match path.as_str() {
            "/api/health" => {
                r#"{"service":"wing-gateway","status":"ok","version":"test","uptime":1}"#
                    .to_string()
            }
            "/api/session/info" => {
                // "working" keeps the session pending — the wait must not
                // finish on its own; only an event (or the dead stream) ends it.
                r#"{"model":"test","api_url":"http://x","tools":[],"total_tokens":0,"context_window_tokens":1,"thinking":false,"reasoning_effort":null,"yolo":false,"session_name":null,"status":"working","context_stats":{"message_count":0,"total_tokens":0}}"#
                    .to_string()
            }
            _ => "{}".to_string(),
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

/// Complete the client handshake, then play out the scenario.
async fn drive_ws(mut ws: WebSocketStream<TcpStream>, scenario: Scenario) {
    let _ = ws
        .send(Message::Text(
            r#"{"type":"connected","client_id":"fake"}"#.into(),
        ))
        .await;
    // Give `wing wait` time to subscribe and enter its wait loop.
    tokio::time::sleep(Duration::from_millis(300)).await;

    match scenario {
        Scenario::OversizedFrame => {
            let _ = ws
                .send(Message::Text("x".repeat(FRAME_LIMIT + 1).into()))
                .await;
            // Keep the socket open so the client observes a frame error (not EOF).
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Scenario::TurnResult => {
            let event = format!(
                r#"{{"type":"turn_result","subtype":"success","is_error":false,"result":"all good","num_turns":1,"duration_ms":12,"session_id":"{SESSION_ID}","created_at":"2026-01-01T00:00:00+00:00","request_id":"r"}}"#
            );
            let _ = ws.send(Message::Text(event.into())).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Read one HTTP request head (+ body) from `stream`, buffering leftovers.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    loop {
        if let Some(pos) = find(buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let content_length = header_value(&head, "content-length")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = pos + 4;
            if buf.len() >= body_start + content_length {
                let body = buf[body_start..body_start + content_length].to_vec();
                buf.drain(..body_start + content_length);
                return Some((head, body));
            }
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Case-insensitive header lookup over the raw request head (header *values*
/// are case-sensitive — e.g. the base64 `Sec-WebSocket-Key`).
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

// ============================================================
// Test harness
// ============================================================

fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wing-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("core")).unwrap();
    dir
}

/// Start the fake gateway and point a fresh `$WING_HOME` at it.
async fn start_gateway(scenario: Scenario, tag: &str) -> (PathBuf, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, scenario));

    // The CLI reads its gateway endpoint from $WING_HOME/core/config.yaml.
    let home = scratch_home(tag);
    std::fs::write(
        home.join("core").join("config.yaml"),
        format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
    )
    .unwrap();
    (home, port)
}

#[tokio::test]
async fn wait_fails_fast_with_reason_and_logs_when_stream_dies() {
    let (home, _port) = start_gateway(Scenario::OversizedFrame, "wait-fail-fast").await;

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(["wait", SESSION_ID, "--timeout", "300"])
        .env("WING_HOME", &home)
        .env_remove("RUST_LOG")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();

    // A 300s timeout would mean "spun until the deadline"; the process must
    // exit on its own long before that.
    let output = match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await
    {
        Ok(result) => result.unwrap(),
        Err(_) => panic!(
            "`wing wait` did not exit after the event stream died — expected fail-fast, \
             got a spinning / timing-out process"
        ),
    };

    // 1. Non-zero exit.
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "expected failure exit code, got {:?}\nstderr: {stderr}",
        output.status,
    );

    // 2. stderr names the cause (frame limit) and the pending session.
    assert!(
        stderr.contains("gateway event stream closed"),
        "stderr must explain the dead stream: {stderr}"
    );
    assert!(
        stderr.contains(&FRAME_LIMIT.to_string()),
        "stderr must carry the frame-limit numbers: {stderr}"
    );
    assert!(
        stderr.contains(SESSION_ID),
        "stderr must name the sessions still pending: {stderr}"
    );

    // 3. The subcommand initialized logging — the read-task failure landed in
    //    the very same file the TUI / stdio use.
    let logs_dir = home.join("tui").join("logs");
    let files: Vec<PathBuf> = std::fs::read_dir(&logs_dir)
        .unwrap_or_else(|e| {
            panic!(
                "no log dir at {}: {e}\nstderr: {stderr}",
                logs_dir.display()
            )
        })
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert_eq!(files.len(), 1, "expected one log file, got {files:?}");
    let log_name = files[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(
        log_name.starts_with("wing_") && log_name.ends_with(".log"),
        "log naming must match the TUI convention: {log_name}"
    );
    let log = std::fs::read_to_string(&files[0]).unwrap();
    // The default filter is `wing=warn`, so INFO startup lines are absent by
    // design — the read-task failure below is exactly what used to vanish.
    assert!(
        log.contains("WebSocket read error"),
        "log must capture the read-task failure: {log}"
    );
    assert!(
        log.contains(&FRAME_LIMIT.to_string()),
        "log must carry the frame-limit numbers: {log}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// Non-regression: a normal `TurnResult` still completes the wait successfully.
#[tokio::test]
async fn wait_succeeds_when_turn_result_arrives() {
    let (home, _port) = start_gateway(Scenario::TurnResult, "wait-success").await;

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(["wait", SESSION_ID, "--timeout", "30", "--json"])
        .env("WING_HOME", &home)
        .env_remove("RUST_LOG")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("`wing wait` must exit once the session is done")
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "expected success, got {:?}\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("\"subtype\":\"success\""),
        "wait output must carry the turn result: {stdout}"
    );
    assert!(
        stdout.contains(SESSION_ID),
        "wait output must name the session: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
