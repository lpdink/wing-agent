//! Transport — encapsulates WS client + HTTP client + client_id.
//!
//! The transport layer is an atomic unit: either fully connected (`Some(Transport)`)
//! or disconnected (`None`). On reconnect, a new `Transport` replaces the old one.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::gateway::GatewayClient;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::models::CreateSessionRequest;

/// Gateway connection parameters for reconnection.
#[derive(Debug, Clone)]
pub struct GatewayEndpoint {
    pub ws_url: String,
    pub http_base: String,
    pub api_key: Option<String>,
}

/// Encapsulates the full transport layer for gateway communication.
pub struct Transport {
    /// WebSocket client for real-time event streaming.
    pub ws: GatewayClient,
    /// HTTP API client for session lifecycle operations.
    pub http: GatewayApiClient,
    /// Client ID from the WS handshake (used for subscribe/unsubscribe).
    pub client_id: String,
}

impl Transport {
    /// Switch event subscription from an old session to a new one.
    ///
    /// Subscribes to `new_session_id` and unsubscribes from `old_session_id`.
    /// Both operations are fire-and-forget — failures are logged but not propagated.
    pub async fn switch_session(&self, old_session_id: &str, new_session_id: &str) {
        if let Err(e) = self.http.subscribe(new_session_id, &self.client_id).await {
            tracing::warn!("subscribe new session failed: {e}");
        }
        if let Err(e) = self.http.unsubscribe(old_session_id, &self.client_id).await {
            tracing::warn!("unsubscribe old session failed: {e}");
        }
    }
}

/// Compute exponential backoff delay for reconnect attempts.
///
/// Formula: `min(1s × 2^attempt, 30s)`
/// Produces: 1s, 2s, 4s, 8s, 16s, 30s, 30s, ...
pub fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_secs(1);
    let max = Duration::from_secs(30);
    let delay = base * 2u32.pow(attempt.min(5));
    delay.min(max)
}

/// Establish the transport layer: WebSocket handshake + HTTP client.
///
/// Transport-level only — no session operations. Keeping this separate from
/// session recovery preserves the invariant that a client holds at most one
/// WebSocket: session-level failures (resume 404, transient errors) never
/// tear the transport down, so retries reuse the existing connection instead
/// of piling up new ones.
pub async fn connect_transport(endpoint: &GatewayEndpoint) -> Result<Transport> {
    // 1. WS connect (performs the handshake; yields our client_id).
    let ws = GatewayClient::connect(&endpoint.ws_url, endpoint.api_key.as_deref())
        .await
        .context("WS connect failed")?;

    // 2. Extract client_id from handshake.
    let client_id = ws.client_id().to_string();

    // 3. Create HTTP client.
    let http = GatewayApiClient::new(&endpoint.http_base, endpoint.api_key.as_deref())
        .context("failed to create HTTP client")?;

    Ok(Transport {
        ws,
        http,
        client_id,
    })
}

impl Transport {
    /// Recover session event streaming over this transport after a (re)connect.
    ///
    /// 1. `resume` the session (loads it from disk into gateway memory after
    ///    a gateway restart)
    /// 2. `subscribe` — re-establishes event routing; the gateway then pushes
    ///    a SyncSession which updates App state (session_id, chat replay).
    ///
    /// A 404 from resume is permanent — the session was never persisted (blank
    /// TUI whose gateway restarted before the first user message) or vanished
    /// from disk. Retrying can never succeed, so instead we silently start a
    /// fresh session over this same WebSocket, exactly like first launch.
    ///
    /// Any other failure (network error, 5xx) is transient: returned as `Err`
    /// for the caller to retry with backoff. The WebSocket itself stays
    /// untouched either way — one client, one socket, many session attempts.
    pub async fn recover_session(&self, session_id: &str, workspace: Option<&str>) -> Result<()> {
        let effective_id = match self.http.resume_session(session_id).await {
            Ok(_) => session_id.to_string(),
            Err(e) if e.is_not_found() => {
                let req = CreateSessionRequest {
                    workspace: workspace.map(str::to_string),
                    ..Default::default()
                };
                let resp = self
                    .http
                    .create_session(&req)
                    .await
                    .context("create_session failed during session recovery")?;
                tracing::info!(
                    lost = session_id,
                    fresh = %resp.session_id,
                    "session not found; started a fresh one"
                );
                resp.session_id
            }
            // Transient: caller retries with backoff; transport stays up.
            Err(e) => {
                return Err(e).context("resume failed during session recovery");
            }
        };

        self.http
            .subscribe(&effective_id, &self.client_id)
            .await
            .context("subscribe failed during session recovery")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_sequence() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(4), Duration::from_secs(16));
    }

    #[test]
    fn test_backoff_cap_at_30s() {
        assert_eq!(backoff(5), Duration::from_secs(30));
        assert_eq!(backoff(6), Duration::from_secs(30));
        assert_eq!(backoff(10), Duration::from_secs(30));
        assert_eq!(backoff(100), Duration::from_secs(30));
    }
}
