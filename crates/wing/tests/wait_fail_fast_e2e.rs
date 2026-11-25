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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::SinkExt;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

mod common;

use common::e2e::{
    RUN_TIMEOUT, header_value, is_websocket_upgrade, read_request, request_path, request_target,
    send_connected, send_json, start_fake_gateway, upgrade_websocket, wait_for_ws_disconnect,
    wing_command,
};

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

struct WaitGateway {
    scenario: Scenario,
    subscription_answered: AtomicBool,
    info_sent: Notify,
}

async fn handle_conn(mut stream: TcpStream, gateway: Arc<WaitGateway>) {
    let mut buf = Vec::new();
    while let Some((head, body)) = read_request(&mut stream, &mut buf).await {
        let path = request_path(&head);
        if path == "/ws" && is_websocket_upgrade(&head) {
            if let Ok(ws) = upgrade_websocket(stream, &head).await {
                drive_ws(ws, gateway).await;
            }
            return;
        }

        let payload = match path {
            "/api/health" => {
                r#"{"service":"wing-gateway","status":"ok","version":"test","uptime":1}"#
            }
            "/api/session/subscribe" => {
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["session_id"], SESSION_ID);
                assert_eq!(header_value(&head, "x-client-id"), Some("fake"));
                "{}"
            }
            "/api/session/info" => {
                assert!(
                    gateway.subscription_answered.load(Ordering::Relaxed),
                    "session info arrived before the subscription response"
                );
                let (_, query) = request_target(&head).split_once('?').unwrap_or_default();
                let session_ids: Vec<_> = url::form_urlencoded::parse(query.as_bytes())
                    .filter_map(|(key, value)| (key == "session_id").then_some(value))
                    .collect();
                assert_eq!(session_ids, [SESSION_ID], "{head}");
                // The session stays pending until the WS outcome arrives.
                r#"{"model":"test","api_url":"http://x","tools":[],"total_tokens":0,"context_window_tokens":1,"thinking":false,"reasoning_effort":null,"yolo":false,"session_name":null,"status":"working","context_stats":{"message_count":0,"total_tokens":0}}"#
            }
            _ => "{}",
        };
        if send_json(&mut stream, payload).await.is_err() {
            return;
        }
        match path {
            "/api/session/subscribe" => {
                gateway.subscription_answered.store(true, Ordering::Relaxed)
            }
            "/api/session/info" => gateway.info_sent.notify_one(),
            _ => {}
        }
    }
}

async fn drive_ws(mut ws: WebSocketStream<TcpStream>, gateway: Arc<WaitGateway>) {
    if send_connected(&mut ws, "fake").await.is_err() {
        return;
    }
    gateway.info_sent.notified().await;

    let frame = match gateway.scenario {
        Scenario::OversizedFrame => "x".repeat(FRAME_LIMIT + 1),
        Scenario::TurnResult => format!(
            r#"{{"type":"turn_result","subtype":"success","is_error":false,"result":"all good","num_turns":1,"duration_ms":12,"session_id":"{SESSION_ID}","created_at":"2026-01-01T00:00:00+00:00","request_id":"r"}}"#
        ),
    };
    let _ = ws.send(Message::Text(frame.into())).await;
    wait_for_ws_disconnect(ws).await;
}

// ============================================================
// Test harness
// ============================================================
/// Bind a port, publish it in a scratch `WING_HOME`, serve `scenario` there.
async fn start_gateway(scenario: Scenario, tag: &str) -> (PathBuf, u16) {
    let gateway = Arc::new(WaitGateway {
        scenario,
        subscription_answered: AtomicBool::new(false),
        info_sent: Notify::new(),
    });
    start_fake_gateway("wing-e2e", tag, gateway, handle_conn).await
}

#[tokio::test]
async fn wait_fails_fast_with_reason_and_logs_when_stream_dies() {
    let (home, _port) = start_gateway(Scenario::OversizedFrame, "wait-fail-fast").await;

    let child = wing_command(&["wait", SESSION_ID, "--timeout", "300"], &home)
        .spawn()
        .unwrap();

    // A 300s timeout would mean "spun until the deadline"; the process must
    // exit on its own long before that.
    let output = match tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output()).await {
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

    let child = wing_command(&["wait", SESSION_ID, "--timeout", "30", "--json"], &home)
        .spawn()
        .unwrap();

    let output = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
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
    let response: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(response["results"][0]["result"], "all good");

    let _ = std::fs::remove_dir_all(&home);
}
