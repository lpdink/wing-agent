//! Transport — encapsulates WS client + HTTP client + client_id.
//!
//! The transport layer is an atomic unit: either fully connected (`Some(Transport)`)
//! or disconnected (`None`). On reconnect, a new `Transport` replaces the old one.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::gateway::GatewayClient;
use wing_api_client::GatewayClient as GatewayApiClient;

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

/// Attempt to reconnect to the gateway.
///
/// Performs:
/// 1. WS reconnect via `GatewayClient::connect()`
/// 2. Extract `client_id` from new WS handshake
/// 3. Create new HTTP client from `http_base`
/// 4. HTTP resume_session to load session from disk into gateway memory
/// 5. HTTP subscribe to re-establish event streaming (triggers SyncSession push)
///
/// Returns a new `Transport` on success, or an error if any step fails.
pub async fn try_reconnect(endpoint: &GatewayEndpoint, session_id: &str) -> Result<Transport> {
    // 1. WS reconnect.
    let ws = GatewayClient::connect(&endpoint.ws_url, endpoint.api_key.as_deref())
        .await
        .context("WS reconnect failed")?;

    // 2. Extract client_id from handshake.
    let client_id = ws.client_id().to_string();

    // 3. Create HTTP client.
    let http = GatewayApiClient::new(&endpoint.http_base, endpoint.api_key.as_deref())
        .context("failed to create HTTP client for reconnect")?;

    // 4. Resume session (loads from disk into gateway memory after restart).
    http.resume_session(session_id)
        .await
        .context("resume_session failed during reconnect")?;

    // 5. Subscribe to re-establish event streaming (triggers SyncSession push).
    http.subscribe(session_id, &client_id)
        .await
        .context("subscribe failed during reconnect")?;

    Ok(Transport {
        ws,
        http,
        client_id,
    })
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
