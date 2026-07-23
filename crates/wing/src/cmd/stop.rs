//! `wing stop` — stop gateway daemon via HTTP.

use std::time::Duration;

use super::backend_config::read_backend_gateway_config;

/// Stop the gateway daemon gracefully via HTTP shutdown endpoint.
///
/// Sends POST /api/shutdown, then polls /api/health until the gateway
/// is no longer reachable (up to 5 seconds).
pub async fn stop_gateway() -> anyhow::Result<()> {
    let config = read_backend_gateway_config();
    let http_base = format!("http://{}:{}", config.host, config.port);

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
            println!("Gateway is not running");
            return Ok(());
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
            println!("Gateway stopped");
            return Ok(());
        }
    }

    println!("Gateway shutdown initiated (still reachable after 5s)");
    Ok(())
}
