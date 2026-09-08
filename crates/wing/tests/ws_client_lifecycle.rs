//! GatewayClient lifecycle tests against an in-process WebSocket server.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use wing::gateway::GatewayClient;

/// Dropping a client must release its WebSocket without needing any traffic.
///
/// Regression test for the reconnect FD leak: the read task used to notice the
/// owner's death only when a message arrived (`event_tx.send` failing). An
/// idle connection — never subscribed, so nothing but pings ever arrives —
/// kept the task, the socket and its fd alive forever, leaking one fd per
/// abandoned reconnect attempt (e.g. resume 404 on a never-persisted session
/// after a gateway restart).
///
/// The server here deliberately sends nothing after the handshake: the client
/// must still close the connection promptly after being dropped.
#[tokio::test]
async fn dropping_idle_client_closes_socket_without_traffic() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // Handshake: the first frame must be a ConnectResponse.
        ws.send(Message::Text(
            r#"{"type":"connected","client_id":"test"}"#.into(),
        ))
        .await
        .unwrap();
        // Then send nothing at all — the drop itself must close the socket.
        let closed = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        assert!(
            closed.is_ok(),
            "server still connected 5s after client drop — socket leaked"
        );
    });

    let url = format!("ws://{addr}/ws");
    let client = GatewayClient::connect(&url, None).await.unwrap();
    assert_eq!(client.client_id(), "test");

    // The leak: drop a healthy, idle (unsubscribed) client. No traffic ever
    // flows after this point.
    drop(client);

    server.await.unwrap();
}
