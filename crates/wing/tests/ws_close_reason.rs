//! `GatewayClient::close_reason()` — the event stream must be able to say
//! *why* it ended.
//!
//! Regression tests for the failure mode where a dead read task was
//! indistinguishable from an idle connection: `recv_event()` returned `None`
//! and consumers either spun (`wing wait`) or reconnected without ever knowing
//! the cause (frame limit / close frame / IO error).
//!
//! Each test drives a real in-process WebSocket server; no gateway required.

use std::time::Duration;

use futures_util::SinkExt;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use wing::gateway::{CloseReason, GatewayClient};

#[allow(dead_code)]
#[path = "common/e2e.rs"]
mod e2e;

use e2e::{start_ws_server, wait_for_ws_disconnect};

/// The client's per-frame receive limit (tungstenite default).
const FRAME_LIMIT: usize = 16 * 1024 * 1024;

async fn assert_stream_closed(client: &mut GatewayClient) {
    let event = tokio::time::timeout(Duration::from_secs(5), client.recv_event())
        .await
        .expect("event stream must close promptly");
    assert!(event.is_none(), "stream must be over");
}

#[tokio::test]
async fn close_frame_is_reported_with_code_and_reason() {
    let (allow_close, can_close) = oneshot::channel();
    let (url, _) = start_ws_server(|mut ws| async move {
        can_close.await.unwrap();
        ws.send(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "bye".into(),
        })))
        .await
        .unwrap();
        wait_for_ws_disconnect(ws).await;
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert_eq!(client.close_reason(), None);
    allow_close.send(()).unwrap();
    assert_stream_closed(&mut client).await;
    match client.close_reason() {
        Some(CloseReason::CloseFrame { code, reason }) => {
            assert_eq!(*code, 1000);
            assert_eq!(reason, "bye");
        }
        other => panic!("expected CloseFrame, got {other:?}"),
    }
    // Stable across calls — the first reason wins.
    assert_eq!(client.close_reason(), client.close_reason());
}

#[tokio::test]
async fn oversized_frame_is_reported_as_frame_too_large() {
    let head = r#"{"type":"sync_session","session_id":"s1","status":"idle","created_at":"2026-09-13T00:00:00+00:00","request_id":"req-1","messages":[{"role":"assistant","content":""#;
    let tail = r#""}]}"#;
    let mut payload = String::with_capacity(FRAME_LIMIT + 1);
    payload.push_str(head);
    payload.extend(std::iter::repeat_n(
        'x',
        FRAME_LIMIT + 1 - head.len() - tail.len(),
    ));
    payload.push_str(tail);

    let (url, _) = start_ws_server(move |mut ws| async move {
        // The client may reject the frame as soon as it reads the length.
        let _ = ws.send(Message::Text(payload.into())).await;
        wait_for_ws_disconnect(ws).await;
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert_stream_closed(&mut client).await;
    match client.close_reason() {
        Some(CloseReason::FrameTooLarge { size, max_size }) => {
            assert_eq!(*max_size, FRAME_LIMIT);
            assert_eq!(*size, FRAME_LIMIT + 1);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
    // The message must carry the numbers the operator needs.
    let text = client.close_reason().unwrap().describe();
    assert!(text.contains(&(FRAME_LIMIT + 1).to_string()), "{text}");
    assert!(text.contains(&FRAME_LIMIT.to_string()), "{text}");
}

#[tokio::test]
async fn abrupt_disconnect_is_reported_as_read_error() {
    // Drop the TCP connection without a close handshake.
    let (url, _) = start_ws_server(|_| async {}).await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert_stream_closed(&mut client).await;
    match client.close_reason() {
        Some(CloseReason::ReadError { detail }) => {
            assert!(!detail.is_empty(), "read errors must not lose their detail");
        }
        // A clean EOF (no error) is also an acceptable classification — what
        // matters is that the cause is recorded and not silently swallowed.
        Some(CloseReason::StreamEnded) => {}
        other => panic!("expected ReadError/StreamEnded, got {other:?}"),
    }
}

#[tokio::test]
async fn healthy_connection_has_no_close_reason() {
    let (url, _) = start_ws_server(wait_for_ws_disconnect).await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client.recv_event())
            .await
            .is_err(),
        "an idle connection must keep waiting for events"
    );
    assert_eq!(
        client.close_reason(),
        None,
        "connected client must not report a close reason"
    );
}
