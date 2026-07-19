// src/lib/gateway-config.ts — Gateway URL resolution + persistence.
//
// Priority: localStorage → VITE_GATEWAY_URL env → default.
// Mirrors: crates/wing/src/cmd/backend_config.rs

const DEFAULT_HOST = '127.0.0.1'
const DEFAULT_PORT = 32523
const STORAGE_KEY = 'wing-gateway-url'

/** Read the persisted Gateway URL from localStorage (or null). */
export function getStoredGatewayUrl(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY)
  } catch {
    return null
  }
}

/** Persist a Gateway URL to localStorage. */
export function setStoredGatewayUrl(url: string): void {
  try {
    localStorage.setItem(STORAGE_KEY, url)
  } catch {
    // Ignore storage errors (private browsing, etc.)
  }
}

/** Resolve the effective Gateway HTTP base URL. */
export function getGatewayHttpBase(): string {
  return (
    getStoredGatewayUrl() ??
    import.meta.env.VITE_GATEWAY_URL ??
    `http://${DEFAULT_HOST}:${DEFAULT_PORT}`
  )
}

/** Derive the WebSocket URL from an HTTP base URL. */
export function toWsUrl(httpBase: string): string {
  return httpBase.replace(/^http/, 'ws').replace(/\/+$/, '') + '/ws'
}

/** WebSocket URL for Gateway event streaming. */
export function getGatewayWsUrl(): string {
  return toWsUrl(getGatewayHttpBase())
}
