//! Gateway WebSocket client — connects to wing-gateway and provides
//! an async interface for sending requests and receiving events.

use std::fmt;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use futures_util::SinkExt;
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{CapacityError, ProtocolError};

use crate::gateway::chunk::Limits;
use crate::gateway::chunk::Outcome;
use crate::gateway::chunk::Reassembler;
use crate::protocol::ClientRequest;
use crate::protocol::ConnectResponse;
use crate::protocol::MediaRef;
use crate::protocol::WingEvent;

/// Connection info returned after successful Gateway handshake.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub client_id: String,
}

/// Why the event stream ended.
///
/// Recorded by the read task when it exits (first writer wins) and exposed via
/// [`GatewayClient::close_reason`]. Consumers turn a dead stream into a
/// diagnosable failure instead of silently degrading (see `cmd/wait.rs`), or
/// into a toast that says *why* the TUI is reconnecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// A received frame exceeded the client's per-frame limit — e.g. a
    /// `sync_session` payload bigger than the 16 MiB cap.
    FrameTooLarge {
        /// Actual (rejected) frame size in bytes.
        size: usize,
        /// Configured maximum frame size in bytes.
        max_size: usize,
    },
    /// The gateway sent a WebSocket Close frame.
    CloseFrame {
        /// Close code (RFC 6455 registry value).
        code: u16,
        /// Human-readable close reason (may be empty).
        reason: String,
    },
    /// Transport-level read error (protocol violation, reset, …).
    ReadError {
        /// Underlying error text (never empty).
        detail: String,
    },
    /// The stream ended without a Close frame (gateway process gone).
    StreamEnded,
    /// The local event consumer disappeared (client dropped).
    ChannelClosed,
    /// Chunked frame reassembly failed: malformed envelope, non-contiguous or
    /// duplicated fragments, buffer cap exceeded, or a fragmented event that
    /// never completed. The caller (TUI / `wing wait`) reports it and follows
    /// the normal reconnect + resubscribe path.
    ReassemblyFailed {
        /// Human-readable reason (never empty).
        detail: String,
    },
}

impl CloseReason {
    /// Classify a read-side WebSocket error.
    fn from_read_error(err: &WsError) -> Self {
        match err {
            WsError::Capacity(CapacityError::MessageTooLong { size, max_size }) => {
                CloseReason::FrameTooLarge {
                    size: *size,
                    max_size: *max_size,
                }
            }
            // Normal end of a WebSocket connection without an app-level close
            // frame being surfaced as a message.
            WsError::ConnectionClosed | WsError::AlreadyClosed => CloseReason::StreamEnded,
            other => CloseReason::ReadError {
                detail: other.to_string(),
            },
        }
    }

    /// One-line, human-readable description (stderr / toast / logs).
    pub fn describe(&self) -> String {
        match self {
            Self::FrameTooLarge { size, max_size } => {
                format!("frame exceeds the client receive limit ({size} > {max_size} bytes)")
            }
            Self::CloseFrame { code, reason } if reason.is_empty() => {
                format!("gateway sent close frame (code={code})")
            }
            Self::CloseFrame { code, reason } => {
                format!("gateway sent close frame (code={code}, reason={reason:?})")
            }
            Self::ReadError { detail } => format!("read error: {detail}"),
            Self::StreamEnded => "gateway closed the connection".to_string(),
            Self::ChannelClosed => "local event consumer closed".to_string(),
            Self::ReassemblyFailed { detail } => {
                format!("chunk reassembly failed: {detail}")
            }
        }
    }
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

#[derive(Debug)]
pub(crate) enum MessageWriteError {
    Unsent(String),
    Uncertain(String),
}

struct OutboundRequest {
    request: ClientRequest,
    receipt: Option<oneshot::Sender<Result<(), MessageWriteError>>>,
}

/// Gateway WebSocket client.
pub struct GatewayClient {
    /// Channel for sending requests to the WS write task.
    tx: mpsc::Sender<OutboundRequest>,
    /// Channel for receiving events from the WS read task.
    rx: mpsc::Receiver<WingEvent>,
    /// Connection info from the initial handshake.
    info: ConnectionInfo,
    /// Why the read task ended (`None` while it is still running).
    close_reason: Arc<OnceLock<CloseReason>>,
}

impl GatewayClient {
    /// Connect to a wing-gateway WebSocket endpoint.
    ///
    /// Performs the handshake: waits for the first `ConnectResponse` message.
    /// If `api_key` is provided and non-empty, the WS handshake carries
    /// an `Authorization: Bearer <key>` header.
    pub async fn connect(url: &str, api_key: Option<&str>) -> Result<Self> {
        Self::connect_with_limits(url, api_key, Limits::default()).await
    }

    /// Same as [`GatewayClient::connect`] with injectable reassembly limits.
    ///
    /// Test seam: the production limits (64 MiB window, 30 s idle timeout) are
    /// too slow to exercise in a test; the state machine itself is limit-agnostic.
    #[doc(hidden)]
    pub async fn connect_with_limits(
        url: &str,
        api_key: Option<&str>,
        limits: Limits,
    ) -> Result<Self> {
        tracing::info!("connecting to gateway: {url}");

        // Let tungstenite build the proper WS handshake request from the URL
        // (sets Host, Upgrade, Sec-WebSocket-Key, etc.), then append auth header.
        let mut request = url
            .into_client_request()
            .context("failed to build WS request")?;
        if let Some(key) = api_key
            && !key.is_empty()
        {
            let mut value: tokio_tungstenite::tungstenite::http::HeaderValue =
                format!("Bearer {key}")
                    .parse()
                    .context("API key contains invalid header characters")?;
            value.set_sensitive(true);
            request.headers_mut().insert("Authorization", value);
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

        Self::from_websocket(ws_stream, limits).await
    }

    pub(crate) async fn from_websocket<S>(
        ws_stream: WebSocketStream<S>,
        limits: Limits,
    ) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
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
        let (tx, mut rx) = mpsc::channel::<OutboundRequest>(64);
        let (event_tx, event_rx) = mpsc::channel::<WingEvent>(256);
        // Shared with the read task: why the read task ended. First write wins
        // (the read task exits exactly once), reads are lock-free.
        let close_reason: Arc<OnceLock<CloseReason>> = Arc::new(OnceLock::new());

        // The reader owns connection lifetime. Outstanding requests that the
        // writer never dequeues lose their receipt and are definitely unsent.
        let (reader_closed_tx, mut reader_closed) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                let OutboundRequest { request, receipt } = tokio::select! {
                    biased;
                    _ = &mut reader_closed => break,
                    request = rx.recv() => {
                        let Some(request) = request else { break };
                        request
                    },
                };
                let json = match serde_json::to_string(&request) {
                    Ok(json) => json,
                    Err(error) => {
                        tracing::error!(%error, "failed to serialize request");
                        if let Some(receipt) = receipt {
                            let _ = receipt.send(Err(MessageWriteError::Unsent(error.to_string())));
                        }
                        continue;
                    }
                };
                let result = tokio::select! {
                    biased;
                    result = async {
                        ws_sink.feed(Message::Text(json.into())).await.map_err(|error| {
                            match error {
                                WsError::AlreadyClosed
                                | WsError::WriteBufferFull(_)
                                | WsError::Protocol(ProtocolError::SendAfterClosing) => {
                                    MessageWriteError::Unsent(error.to_string())
                                }
                                _ => MessageWriteError::Uncertain(error.to_string()),
                            }
                        })?;
                        ws_sink.flush().await.map_err(|error| MessageWriteError::Uncertain(error.to_string()))
                    } => result,
                    _ = &mut reader_closed => Err(MessageWriteError::Uncertain("Connection closed during write".into())),
                };
                let write_failed = result.is_err();
                if let Err(error) = &result {
                    tracing::error!(?error, request_id = %request.request_id, "failed to write request");
                } else {
                    tracing::debug!(request_id = %request.request_id, "sent request");
                }
                if let Some(receipt) = receipt {
                    let _ = receipt.send(result);
                }
                if write_failed {
                    break;
                }
            }
            tracing::debug!("write task exiting");
        });

        // Spawn read task.
        let reason_slot = Arc::clone(&close_reason);
        tokio::spawn(async move {
            let mut last_pressure_warn = std::time::Instant::now();
            // Chunked-frame reassembly lives here (transport layer): the app
            // only ever sees complete events. See `gateway::chunk`.
            let mut reassembler = Reassembler::new(limits);
            // Every exit carries a reason — consumers (`wing wait`, the TUI
            // reconnect toast) must be able to say *why* the stream died.
            let reason = loop {
                // Only armed while a fragmented event is open; a window that
                // never closes must fail instead of stalling the stream.
                let reassembly_deadline = reassembler
                    .deadline()
                    .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
                tokio::select! {
                    // Zero-traffic shutdown: `event_rx` lives inside
                    // GatewayClient, so dropping the client closes the channel
                    // and wakes us here. Without this branch an idle connection
                    // (never subscribed, hence nothing but pings ever arrives)
                    // would keep this task and its socket alive forever,
                    // leaking an fd per abandoned client.
                    _ = event_tx.closed() => break CloseReason::ChannelClosed,
                    _ = tokio::time::sleep_until(reassembly_deadline),
                        if reassembler.is_assembling() =>
                    {
                        let detail = reassembler.timeout_detail();
                        tracing::error!("chunk reassembly timed out: {detail}");
                        break CloseReason::ReassemblyFailed { detail };
                    }
                    msg = ws_stream.next() => {
                        let Some(msg_result) = msg else {
                            break CloseReason::StreamEnded;
                        };
                        match msg_result {
                            Ok(Message::Text(text)) => {
                                // Complete events only: a fragmented payload is
                                // reassembled (and held back in arrival order)
                                // before anything reaches the application.
                                match reassembler.on_text(&text) {
                                    Outcome::Deliver(events) => {
                                        let mut consumer_gone = false;
                                        for event in events {
                                            tracing::debug!(
                                                event_type = %event.event_type(),
                                                "received event"
                                            );
                                            // Channel pressure: warn once per 10 s when >90% full.
                                            let capacity = event_tx.max_capacity();
                                            let used = capacity.saturating_sub(event_tx.capacity());
                                            if used >= capacity * 9 / 10
                                                && last_pressure_warn.elapsed()
                                                    > Duration::from_secs(10)
                                            {
                                                last_pressure_warn = std::time::Instant::now();
                                                tracing::warn!(
                                                    "event channel pressure: {}/{} ({}%) — \
                                                     consider a slower model or incremental rendering",
                                                    used,
                                                    capacity,
                                                    used * 100 / capacity,
                                                );
                                            }
                                            if event_tx.send(event).await.is_err() {
                                                consumer_gone = true;
                                                break;
                                            }
                                        }
                                        if consumer_gone {
                                            tracing::debug!(
                                                "event receiver dropped, stopping read task"
                                            );
                                            break CloseReason::ChannelClosed;
                                        }
                                    }
                                    Outcome::Pending => {
                                        tracing::debug!(
                                            "chunk fragment received, waiting for the last frame"
                                        );
                                    }
                                    Outcome::Fail(detail) => {
                                        tracing::error!(
                                            "chunk reassembly failed: {detail}"
                                        );
                                        break CloseReason::ReassemblyFailed { detail };
                                    }
                                }
                            }
                            Ok(Message::Close(frame)) => {
                                tracing::info!("gateway sent close frame: {frame:?}");
                                break CloseReason::CloseFrame {
                                    code: frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(0),
                                    reason: frame
                                        .as_ref()
                                        .map(|f| f.reason.to_string())
                                        .unwrap_or_default(),
                                };
                            }
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                                // Handled by tungstenite internally.
                            }
                            Ok(other) => {
                                tracing::debug!("unexpected message type: {other:?}");
                            }
                            Err(e) => {
                                let reason = CloseReason::from_read_error(&e);
                                tracing::error!("WebSocket read error ({reason}): {e}");
                                break reason;
                            }
                        }
                    }
                }
            };
            // Abnormal ends stay visible at the default `wing=warn` filter;
            // normal ones (the local consumer went away) stay at debug.
            match &reason {
                CloseReason::StreamEnded | CloseReason::ChannelClosed => {
                    tracing::debug!("read task exiting: {reason}");
                }
                _ => tracing::warn!("read task exiting: {reason}"),
            }
            let _ = reason_slot.set(reason);
            let _ = reader_closed_tx.send(());
        });

        Ok(Self {
            tx,
            rx: event_rx,
            info,
            close_reason,
        })
    }

    /// Returns the client_id from the initial handshake.
    pub fn client_id(&self) -> &str {
        &self.info.client_id
    }

    /// Why the event stream ended, or `None` while the read task is running.
    ///
    /// Stable across calls: the first reason recorded by the read task is the
    /// only one ever returned. Callers use it to fail fast with context
    /// (`wing wait`) or to explain a disconnect (TUI toast) instead of
    /// degrading silently.
    pub fn close_reason(&self) -> Option<&CloseReason> {
        self.close_reason.get()
    }

    /// Send a message to the agent.
    ///
    /// `tool_call_id` — when replying to an Ask event, pass its tool_call_id
    /// so the gateway resolves the matching feedback waiter.
    ///
    /// `request_id` — caller-generated correlation id, echoed back in the
    /// `delivered` / `user_message_accepted` events for this message.
    ///
    /// `media` — 图片引用（先经 HTTP 上传端点拿到）；`None` = 纯文本，
    /// 帧里不出现 `media` 键（旧网关逐字节兼容）。
    pub async fn send_message(
        &self,
        session_id: &str,
        content: &str,
        tool_call_id: Option<String>,
        request_id: String,
        media: Option<Vec<MediaRef>>,
    ) -> Result<()> {
        self.request_sender()
            .send_message(session_id, content, tool_call_id, request_id, media)
            .await
    }

    /// 写口句柄：把请求投进 WS 写任务的那一半（[`GatewayClient`] 本体不可克隆——
    /// 读端 `rx` 是单消费者的，归事件循环独占）。
    ///
    /// 后台任务（带图消息的逐张上传 + 发送）只需要写口，拿这份可克隆的窄句柄即可，
    /// 与 `send_message` 走同一条通道、同一套错误面。
    pub fn request_sender(&self) -> RequestSender {
        RequestSender {
            tx: self.tx.clone(),
        }
    }

    /// Receive the next event from the gateway.
    ///
    /// Returns `None` if the connection is closed.
    pub async fn recv_event(&mut self) -> Option<WingEvent> {
        self.rx.recv().await
    }
}

/// [`GatewayClient`] 的**写口**（[`GatewayClient::request_sender`] 取）：可克隆，只做投递。
///
/// 存在的理由：连接本体不可克隆（读端 `rx` 单消费者），而后台任务（带图消息的上传 +
/// 发送）只需要「把一条 [`ClientRequest`] 投进写任务」。帧构造与错误面与
/// [`GatewayClient::send_message`] 逐字一致——后者就是转发到这里。
#[derive(Clone)]
pub struct RequestSender {
    tx: mpsc::Sender<OutboundRequest>,
}

impl RequestSender {
    /// 发送一条用户消息（语义见 [`GatewayClient::send_message`]）。
    pub async fn send_message(
        &self,
        session_id: &str,
        content: &str,
        tool_call_id: Option<String>,
        request_id: String,
        media: Option<Vec<MediaRef>>,
    ) -> Result<()> {
        let request = ClientRequest {
            request_id,
            session_id: session_id.to_string(),
            content: content.to_string(),
            tool_call_id,
            media,
        };
        self.tx
            .send(OutboundRequest {
                request,
                receipt: None,
            })
            .await
            .context("gateway write channel closed")?;
        Ok(())
    }
    /// Await the TUI writer's outcome before releasing a recoverable draft.
    pub(crate) async fn send_confirmed(
        &self,
        request: ClientRequest,
    ) -> Result<(), MessageWriteError> {
        let (receipt, written) = oneshot::channel();
        self.tx
            .send(OutboundRequest {
                request,
                receipt: Some(receipt),
            })
            .await
            .map_err(|_| MessageWriteError::Unsent("Gateway write channel closed".into()))?;
        written.await.unwrap_or_else(|_| {
            Err(MessageWriteError::Unsent(
                "Gateway writer stopped before sending".into(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_too_large_is_classified_with_sizes() {
        // The exact error the 22 MB `sync_session` frame produced.
        let err = WsError::Capacity(CapacityError::MessageTooLong {
            size: 22_151_988,
            max_size: 16_777_216,
        });
        let reason = CloseReason::from_read_error(&err);
        assert_eq!(
            reason,
            CloseReason::FrameTooLarge {
                size: 22_151_988,
                max_size: 16_777_216,
            }
        );
        let text = reason.describe();
        assert!(text.contains("22151988"), "{text}");
        assert!(text.contains("16777216"), "{text}");
        assert!(
            !text.contains('\n'),
            "describe() must stay one line: {text}"
        );
    }

    #[test]
    fn closed_connection_maps_to_stream_ended() {
        assert_eq!(
            CloseReason::from_read_error(&WsError::ConnectionClosed),
            CloseReason::StreamEnded
        );
        assert_eq!(
            CloseReason::from_read_error(&WsError::AlreadyClosed),
            CloseReason::StreamEnded
        );
    }

    #[test]
    fn io_error_keeps_detail() {
        let err = WsError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        ));
        let reason = CloseReason::from_read_error(&err);
        match &reason {
            CloseReason::ReadError { detail } => {
                assert!(detail.contains("connection reset by peer"), "{detail}");
            }
            other => panic!("expected ReadError, got {other:?}"),
        }
        assert!(reason.describe().contains("connection reset by peer"));
    }

    #[test]
    fn describe_covers_every_variant() {
        let cases = [
            (
                CloseReason::FrameTooLarge {
                    size: 1,
                    max_size: 2,
                },
                "1 > 2",
            ),
            (
                CloseReason::CloseFrame {
                    code: 1000,
                    reason: "bye".into(),
                },
                "code=1000",
            ),
            (
                CloseReason::CloseFrame {
                    code: 4001,
                    reason: String::new(),
                },
                "code=4001",
            ),
            (
                CloseReason::ReadError {
                    detail: "boom".into(),
                },
                "boom",
            ),
            (CloseReason::StreamEnded, "closed the connection"),
            (CloseReason::ChannelClosed, "consumer closed"),
            (
                CloseReason::ReassemblyFailed {
                    detail: "chunk index out of sequence".into(),
                },
                "chunk reassembly failed: chunk index out of sequence",
            ),
        ];
        for (reason, needle) in cases {
            let text = reason.describe();
            assert!(text.contains(needle), "{reason:?} → {text:?}");
            assert_eq!(text, reason.to_string());
        }
    }
}
