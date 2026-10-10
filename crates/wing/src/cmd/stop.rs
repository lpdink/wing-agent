//! `wing stop` — stop gateway daemon via HTTP.

use std::time::Duration;

use super::backend_config::read_backend_gateway_config;

/// What one stop attempt found.
///
/// [`stop_gateway`] maps each outcome to a human line; `wing restart` calls
/// [`stop_gateway_quiet`] directly — its stdout is either JSON or the
/// restart report, so it cannot inherit the stop messages — and passes it the
/// **same** endpoint it will start, so both halves act on one gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The gateway was running and is now gone.
    Stopped,
    /// Nothing was listening on the configured endpoint (already stopped).
    NotRunning,
    /// Shutdown was accepted but the process was still reachable after the
    /// poll window (a restart may therefore race the old process).
    ShutdownInitiated,
}

/// Stop the gateway daemon gracefully via HTTP shutdown endpoint.
///
/// Sends POST /api/shutdown, then polls /api/health until the gateway
/// is no longer reachable (up to 5 seconds).
pub async fn stop_gateway() -> anyhow::Result<()> {
    let config = read_backend_gateway_config();
    match stop_gateway_quiet(&config.host, config.port).await? {
        StopOutcome::Stopped => println!("Gateway stopped"),
        StopOutcome::NotRunning => println!("Gateway is not running"),
        StopOutcome::ShutdownInitiated => {
            println!("Gateway shutdown initiated (still reachable after 5s)")
        }
    }
    Ok(())
}

/// The stop itself at an **explicit** endpoint, without the human-readable
/// lines (see [`StopOutcome`]).
///
/// The endpoint is a parameter because `wing restart --host/--port` must stop
/// the gateway it is about to start, not the one the config points at.
pub async fn stop_gateway_quiet(host: &str, port: u16) -> anyhow::Result<StopOutcome> {
    let http_base = format!("http://{host}:{port}");

    // Read API key from TUI config for authenticated shutdown.
    let api_key = crate::config::AppConfig::load()
        .api_key
        .filter(|k| !k.is_empty());

    let client = wing_api_client::GatewayClient::new(&http_base, api_key.as_deref())
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

    // Send shutdown request.
    match client.shutdown().await {
        Ok(()) => {}
        Err(wing_api_client::ApiClientError::Transport(e)) if e.is_connect() => {
            // Connection refused — gateway is not running.
            return Ok(StopOutcome::NotRunning);
        }
        Err(e) => {
            anyhow::bail!("Failed to stop gateway: {e}");
        }
    }

    // Poll health endpoint until gateway is down (max 5 seconds).
    let poll_interval = Duration::from_millis(100);
    let timeout = Duration::from_secs(5);
    let deadline = std::time::Instant::now() + timeout;

    while std::time::Instant::now() < deadline {
        tokio::time::sleep(poll_interval).await;
        if client.health().await.is_err() {
            return Ok(StopOutcome::Stopped);
        }
    }

    Ok(StopOutcome::ShutdownInitiated)
}
