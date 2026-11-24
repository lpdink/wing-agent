//! GatewayClient lifecycle tests against an in-process WebSocket server.

use std::io::ErrorKind;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

use wing::gateway::GatewayClient;

#[allow(dead_code)]
#[path = "common/e2e.rs"]
mod e2e;

use e2e::{WS_TEST_CLIENT_ID, start_ws_server, upgrade_websocket, wait_for_ws_disconnect};

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
    let (url, server) = start_ws_server(wait_for_ws_disconnect).await;
    let client = GatewayClient::connect(&url, None).await.unwrap();
    assert_eq!(client.client_id(), WS_TEST_CLIENT_ID);

    // The leak: drop a healthy, idle (unsubscribed) client. No traffic ever
    // flows after this point.
    drop(client);

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server still connected 5s after client drop — socket leaked")
        .unwrap();
}

#[tokio::test]
async fn upgrade_without_websocket_key_returns_invalid_data() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let head = "GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";

    let error = upgrade_websocket(stream, head).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "missing Sec-WebSocket-Key");
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut [0]))
        .await
        .expect("invalid upgrade must close the socket")
        .unwrap();
    assert_eq!(read, 0, "invalid upgrade must not send a response");
}
