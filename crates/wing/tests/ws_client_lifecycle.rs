//! GatewayClient lifecycle tests against an in-process WebSocket server.

use std::time::Duration;

use wing::gateway::GatewayClient;

#[allow(dead_code)]
#[path = "common/e2e.rs"]
mod e2e;

use e2e::{start_ws_server, wait_for_ws_disconnect};

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
    assert_eq!(client.client_id(), "test");

    // The leak: drop a healthy, idle (unsubscribed) client. No traffic ever
    // flows after this point.
    drop(client);

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server still connected 5s after client drop — socket leaked")
        .unwrap();
}
