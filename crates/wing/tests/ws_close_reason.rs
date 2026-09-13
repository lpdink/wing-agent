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
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use wing::gateway::{CloseReason, GatewayClient};

/// The client's per-frame receive limit (tungstenite default).
const FRAME_LIMIT: usize = 16 * 1024 * 1024;

/// Send the handshake frame every client waits for after upgrading.
async fn send_handshake(ws: &mut WebSocketStream<TcpStream>) {
    ws.send(Message::Text(
        r#"{"type":"connected","client_id":"test"}"#.into(),
    ))
    .await
    .unwrap();
}

/// Bind a loopback listener and hand the accepted socket to `handler`.
async fn spawn_server<F>(handler: F) -> String
where
    F: FnOnce(WebSocketStream<TcpStream>) -> futures_util::future::BoxFuture<'static, ()>
        + Send
        + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        handler(ws).await;
    });
    format!("ws://{addr}/ws")
}

#[tokio::test]
async fn close_frame_is_reported_with_code_and_reason() {
    let url = spawn_server(|mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            ws.send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "bye".into(),
            })))
            .await
            .unwrap();
            // Keep the socket alive long enough for the client to read it.
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    // Connected: no reason yet.
    assert_eq!(client.close_reason(), None);

    assert!(client.recv_event().await.is_none(), "stream must be over");
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
    // One byte over the client's limit: the read task dies with
    // `CapacityError::MessageTooLong` — exactly what a >16 MiB sync payload
    // does to `wing wait` / the TUI.
    let payload = "x".repeat(FRAME_LIMIT + 1);

    let url = spawn_server(move |mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            // The write side has no frame-size limit — this arrives as one frame.
            let _ = ws.send(Message::Text(payload.into())).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert!(client.recv_event().await.is_none(), "stream must be over");
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
    let url = spawn_server(|mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            // Drop the TCP connection without a close handshake.
            drop(ws);
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert!(client.recv_event().await.is_none(), "stream must be over");
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
    let url = spawn_server(|mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            // Stay open; nothing else happens.
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    })
    .await;

    let client = GatewayClient::connect(&url, None).await.unwrap();
    // Give the read task a chance to end if it were going to.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        client.close_reason(),
        None,
        "connected client must not report a close reason"
    );
}
