//! Shared utilities for CLI subcommands.
//!
//! Provides gateway discovery, HTTP client construction, and output
//! formatting helpers used by `wing run`, `wing ps`, `wing tail`, etc.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use anyhow::Result;
use wing_api_client::GatewayClient as GatewayApiClient;

/// Ensure the gateway is running, returning `(host, port)`.
///
/// Checks the health endpoint first; if the gateway is not reachable,
/// starts it automatically. Delegates to `stdio::ensure_gateway_running()`
/// which already implements the full discovery + auto-start logic.
pub async fn ensure_gateway() -> Result<(String, u16)> {
    crate::stdio::ensure_gateway_running().await
}

/// Load the API key from TUI config (if any).
pub fn load_api_key() -> Option<String> {
    crate::config::AppConfig::load()
        .api_key
        .filter(|k| !k.is_empty())
}

/// Create a `GatewayApiClient` connected to the running gateway.
///
/// Loads the API key from TUI config and constructs the HTTP client.
pub fn create_api_client(host: &str, port: u16) -> Result<GatewayApiClient> {
    let api_key = load_api_key();
    let http_base = format!("http://{host}:{port}");
    GatewayApiClient::new(&http_base, api_key.as_deref())
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))
}

/// Print a value as JSON to stdout.
pub fn print_json<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize JSON: {e}"),
    }
}

/// Print a value as compact JSON to stdout (single line, for agent `jq` piping).
pub fn print_json_compact<T: serde::Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize JSON: {e}"),
    }
}

/// Truncate a string to at most `max` characters (Unicode-safe).
/// Appends "..." if truncated. Avoids splitting multi-byte characters.
pub fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    // Not enough room for ellipsis — just take what fits.
    if max <= 3 {
        return chars.into_iter().take(max).collect();
    }
    let keep = max - 3;
    let truncated: String = chars.into_iter().take(keep).collect();
    format!("{truncated}...")
}
