//! `wing status` — show gateway daemon status via HTTP.

use super::backend_config::read_backend_gateway_config;

/// Display gateway daemon status by querying the health endpoint.
pub async fn show_status() {
    let config = read_backend_gateway_config();
    let http_base = format!("http://{}:{}", config.host, config.port);

    let client = match wing_api_client::GatewayClient::new(&http_base, None) {
        Ok(c) => c,
        Err(e) => {
            println!("Gateway is not running (failed to create HTTP client: {e})");
            return;
        }
    };

    match client.health().await {
        Ok(health) if health.service == "wing-gateway" => {
            println!("Gateway is running");
            println!("  Endpoint: ws://{}:{}/ws", config.host, config.port);
            println!("  Version:  {}", health.version);
            println!("  Uptime:   {}", format_duration(health.uptime));
        }
        _ => {
            println!("Gateway is not running");
        }
    }
}

/// Format seconds into human-readable duration.
fn format_duration(secs: i64) -> String {
    if secs < 0 {
        return "0s".to_string();
    }
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}
