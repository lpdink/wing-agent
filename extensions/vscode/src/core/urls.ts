/**
 * Gateway URL construction and secret redaction.
 *
 * The gateway speaks plain HTTP/WS on localhost by default (`gateway.host` /
 * `gateway.port` in `~/.wing/config.yaml`; default 127.0.0.1:32523). TLS is the
 * reverse proxy's job, so a configurable host/port is all the extension needs.
 *
 * WS auth travels as a *query parameter* (`?api_key=`), not a header: the
 * standard `WebSocket` constructor has no way to set request headers (only
 * undici's non-standard `headers` option does), and `src/core` must stay
 * portable to a DOM/Electron-renderer runtime. The gateway supports both
 * (`gateway/auth.py::extract_key_from_ws`, query param has priority 2), so the
 * only cost is that the key appears in URLs — hence `redactUrl` is used for
 * every log line and error message that carries one.
 */

export interface GatewayUrlOptions {
  /** Host from the extension settings (`127.0.0.1`, `localhost`, `::1`, …). */
  readonly host: string;
  readonly port: number;
  /** API key; empty / `null` / `undefined` turns auth off. */
  readonly apiKey?: string | null;
}

export interface GatewayUrls {
  /** e.g. `http://127.0.0.1:32523` — for {@link GatewayHttpClient}. */
  readonly httpBaseUrl: string;
  /** e.g. `ws://127.0.0.1:32523/ws?api_key=…` — for {@link GatewayConnection}. */
  readonly wsUrl: string;
}

/** Bracket bare IPv6 literals (`::1` → `[::1]`); everything else passes through. */
function normalizeHost(host: string): string {
  const trimmed = host.trim();
  if (trimmed === '') {
    return '127.0.0.1';
  }
  return trimmed.includes(':') && !trimmed.startsWith('[') ? `[${trimmed}]` : trimmed;
}

/** Trim, and treat the empty string as "no key" (settings leave it blank by default). */
export function normalizeApiKey(apiKey: string | null | undefined): string | null {
  if (apiKey === null || apiKey === undefined) {
    return null;
  }
  const trimmed = apiKey.trim();
  return trimmed === '' ? null : trimmed;
}

/** Derive the HTTP base URL and the WS URL from one host/port/key triple. */
export function gatewayUrls(options: GatewayUrlOptions): GatewayUrls {
  const host = normalizeHost(options.host);
  const authority = `${host}:${options.port}`;
  const apiKey = normalizeApiKey(options.apiKey);
  const wsQuery = apiKey === null ? '' : `?api_key=${encodeURIComponent(apiKey)}`;
  return {
    httpBaseUrl: `http://${authority}`,
    wsUrl: `ws://${authority}/ws${wsQuery}`,
  };
}

/** Replace the `api_key` query value with `***` before it reaches a log or an error. */
export function redactUrl(url: string): string {
  return url.replace(/([?&]api_key=)[^&]*/gi, '$1***');
}
