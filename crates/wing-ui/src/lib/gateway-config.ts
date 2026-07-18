// src/lib/gateway-config.ts — Gateway URL resolution.
//
// Mirrors: crates/wing/src/cmd/backend_config.rs
//
// Default: http://127.0.0.1:32523 (same as Rust TUI default).
// Override via VITE_GATEWAY_URL environment variable.

const DEFAULT_HOST = '127.0.0.1'
const DEFAULT_PORT = 32523

/** HTTP base URL for Gateway API. */
export function getGatewayHttpBase(): string {
  return import.meta.env.VITE_GATEWAY_URL ?? `http://${DEFAULT_HOST}:${DEFAULT_PORT}`
}

/** WebSocket URL for Gateway event streaming. */
export function getGatewayWsUrl(): string {
  const httpBase = getGatewayHttpBase()
  // Convert http(s):// to ws(s)://
  return httpBase.replace(/^http/, 'ws') + '/ws'
}
