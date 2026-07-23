//! Gateway WebSocket client — connects to wing-gateway and provides
//! an async interface for sending requests and receiving events.

use anyhow::Context;
use anyhow::Result;
use futures_util::SinkExt;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::protocol::ClientRequest;
use crate::protocol::ConnectResponse;
use crate::protocol::WingEvent;

/// Connection info returned after successful Gateway handshake.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub client_id: String,
}

/// Gateway WebSocket client.
pub struct GatewayClient {
    /// Channel for sending requests to the WS write task.
    tx: mpsc::Sender<ClientRequest>,
    /// Channel for receiving events from the WS read task.
    rx: mpsc::Receiver<WingEvent>,
    /// Connection info from the initial handshake.
    info: ConnectionInfo,
}

impl GatewayClient {
    /// Connect to a wing-gateway WebSocket endpoint.
    ///
    /// Performs the handshake: waits for the first `ConnectResponse` message.
    /// If `api_key` is provided and non-empty, the WS handshake carries
    /// an `Authorization: Bearer <key>` header.
    pub async fn connect(url: &str, api_key: Option<&str>) -> Result<Self> {
        tracing::info!("connecting to gateway: {url}");

        // Let tungstenite build the proper WS handshake request from the URL
        // (sets Host, Upgrade, Sec-WebSocket-Key, etc.), then append auth header.
        let mut request = url
            .into_client_request()
            .context("failed to build WS request")?;
        if let Some(key) = api_key
            && !key.is_empty()
        {
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {key}")
                    .parse()
                    .context("API key contains invalid header characters")?,
            );
        }

        let (ws_stream, _resp) = match tokio_tungstenite::connect_async(request).await {
            Ok(pair) => pair,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp))
                if resp.status() == 401 || resp.status() == 403 =>
            {
                anyhow::bail!(
                    "gateway rejected authentication (HTTP {}); \
                     check `api_key` in ~/.wing/tui/config.yaml matches gateway.auth.keys",
                    resp.status()
                );
            }
            Err(e) => {
                return Err(e).context("failed to connect to gateway WebSocket");
            }
        };

        tracing::debug!("WebSocket connection established");

        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        // Read the first message — should be ConnectResponse.
        let first_msg = ws_stream
            .next()
            .await
            .context("gateway closed before sending ConnectResponse")?
            .context("WebSocket read error")?;

        let connect_resp = match first_msg {
            Message::Text(text) => serde_json::from_str::<ConnectResponse>(&text)
                .context("failed to parse ConnectResponse")?,
            other => anyhow::bail!("expected ConnectResponse, got: {other:?}"),
        };

        tracing::info!(
            client_id = %connect_resp.client_id,
            "connected to gateway"
        );

        let info = ConnectionInfo {
            client_id: connect_resp.client_id,
        };

        // Create channels for async communication with the WS tasks.
        let (tx, mut rx) = mpsc::channel::<ClientRequest>(64);
        let (event_tx, event_rx) = mpsc::channel::<WingEvent>(256);

        // Spawn write task.
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                match serde_json::to_string(&req) {
                    Ok(json) => {
                        if let Err(e) = ws_sink.send(Message::Text(json.into())).await {
                            tracing::error!("failed to send to gateway: {e}");
                            break;
                        }
                        tracing::debug!(request_id = %req.request_id, "sent request");
                    }
                    Err(e) => {
                        tracing::error!("failed to serialize request: {e}");
                    }
                }
            }
            tracing::debug!("write task exiting");
        });

        // Spawn read task.
        tokio::spawn(async move {
            while let Some(msg_result) = ws_stream.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<WingEvent>(&text) {
                            Ok(event) => {
                                tracing::debug!(
                                    event_type = %event.event_type(),
                                    "received event"
                                );
                                if event_tx.send(event).await.is_err() {
                                    tracing::debug!("event receiver dropped, stopping read task");
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("failed to parse event: {e}, raw: {text}");
                                // Try to at least extract the type for debugging.
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
                                    && let Some(t) = val.get("type").and_then(|v| v.as_str())
                                {
                                    tracing::warn!("unparseable event type: {t}");
                                }
                            }
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        tracing::info!("gateway sent close frame: {frame:?}");
                        break;
                    }
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                        // Handled by tungstenite internally.
                    }
                    Ok(other) => {
                        tracing::debug!("unexpected message type: {other:?}");
                    }
                    Err(e) => {
                        tracing::error!("WebSocket read error: {e}");
                        break;
                    }
                }
            }
            tracing::debug!("read task exiting");
        });

        Ok(Self {
            tx,
            rx: event_rx,
            info,
        })
    }

    /// Returns the client_id from the initial handshake.
    pub fn client_id(&self) -> &str {
        &self.info.client_id
    }

    /// Send a message to the agent.
    pub async fn send_message(&self, session_id: &str, content: &str) -> Result<()> {
        let req = ClientRequest::message(session_id, content);
        self.tx
            .send(req)
            .await
            .context("gateway write channel closed")?;
        Ok(())
    }

    /// Send a request with a specific session_id.
    pub async fn send_to_session(&self, content: &str, session_id: &str) -> Result<()> {
        let req = ClientRequest {
            request_id: crate::protocol::generate_request_id(),
            session_id: session_id.to_string(),
            content: content.to_string(),
        };
        self.tx
            .send(req)
            .await
            .context("gateway write channel closed")?;
        Ok(())
    }

    /// Receive the next event from the gateway.
    ///
    /// Returns `None` if the connection is closed.
    pub async fn recv_event(&mut self) -> Option<WingEvent> {
        self.rx.recv().await
    }
}
