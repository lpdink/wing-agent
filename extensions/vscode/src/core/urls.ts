/**
 * Gateway URL construction, auth header helpers and secret redaction.
 *
 * The gateway speaks plain HTTP/WS on localhost by default (`gateway.host` /
 * `gateway.port` in `~/.wing/config.yaml`; default 127.0.0.1:32523). TLS is the
 * reverse proxy's job, so a configurable host/port is all the extension needs.
 *
 * **Auth never travels in the URL by default** (review #109 [P3-6]). The gateway
 * prefers headers — `gateway/auth.py::extract_key_from_ws` reads
 * `Authorization: Bearer` / `X-API-Key` first and only then `?api_key=` — and the
 * Node extension host can set them (undici's `WebSocket` and the bundled `ws`
 * both take `headers`). A key in a URL, by contrast, leaks into every layer that
 * records a request line (reverse-proxy access logs, a corporate MITM proxy, a
 * future gateway access log, crash reports), and TLS does not cover a plain
 * `ws://` default. So:
 *
 * - {@link gatewayUrls} returns URLs **without** credentials → they are not
 *   sensitive data, and no log line depends on somebody remembering to redact;
 * - {@link apiKeyHeaders} is what the host passes to the HTTP and WS clients;
 * - {@link wsUrlWithApiKey} remains for hosts that *cannot* set headers (a
 *   DOM / Electron-renderer `WebSocket` constructor takes protocols only) —
 *   {@link redactUrl} is kept for those paths' logs and error messages.
 */

import { GatewayHttpError } from './errors';

export interface GatewayUrlOptions {
  /** Host from the extension settings (`127.0.0.1`, `localhost`, `::1`, …). */
  readonly host: string;
  readonly port: number;
}

export interface GatewayUrls {
  /** e.g. `http://127.0.0.1:32523` — for {@link GatewayHttpClient}. */
  readonly httpBaseUrl: string;
  /** e.g. `ws://127.0.0.1:32523/ws` — for {@link GatewayConnection} (key goes in a header). */
  readonly wsUrl: string;
}

/** Count of `:` characters (IPv6 has at least two). */
function colonCount(host: string): number {
  return host.split(':').length - 1;
}

/**
 * `true` for a bare IPv6 literal (`::1`, `fe80::1%en0`, `::ffff:127.0.0.1`).
 *
 * The discriminator is the colon count: a `host:port` pair has exactly one, an
 * IPv6 literal at least two. (A charset check would have to accept zone indexes
 * like `%en0`, i.e. nearly anything, so it buys nothing.)
 */
function isIpv6Literal(host: string): boolean {
  return colonCount(host) >= 2;
}

/**
 * Normalise the configured host: bracket a bare IPv6 literal, reject a `host:port`
 * value with a typed configuration error.
 *
 * Bracketing anything containing a `:` (the obvious implementation) turns
 * `localhost:8080` into `[localhost:8080]` and produces an invalid URL that only
 * fails much later with an opaque message — the host/port settings are separate,
 * so such a value is a mistake worth reporting where it is made.
 */
function normalizeHost(host: string): string {
  const trimmed = host.trim();
  if (trimmed === '') {
    return '127.0.0.1';
  }
  if (trimmed.startsWith('[')) {
    if (trimmed.endsWith(']') && trimmed.length > 2) {
      return trimmed; // already bracketed: `[::1]`
    }
    if (trimmed.includes(']:')) {
      throw hostPortError(trimmed);
    }
    throw new GatewayHttpError({
      kind: 'config',
      message: `gateway host is not a valid IPv6 literal: "${trimmed}"`,
    });
  }
  if (isIpv6Literal(trimmed)) {
    return `[${trimmed}]`;
  }
  if (trimmed.includes(':')) {
    throw hostPortError(trimmed);
  }
  return trimmed;
}

/**
 * A host value that carries a `:` without being an IPv6 literal.
 *
 * The port has its own setting, so this is a configuration mistake worth naming:
 * the alternative is `http://localhost:8080:32523`, which fails much later with
 * an opaque transport error.
 */
function hostPortError(host: string): GatewayHttpError {
  return new GatewayHttpError({
    kind: 'config',
    message:
      `gateway host must not contain a port: "${host}" (set host and port separately; ` +
      'IPv6 literals may be written as "::1" or "[::1]")',
  });
}

/** Trim, and treat the empty string as "no key" (settings leave it blank by default). */
export function normalizeApiKey(apiKey: string | null | undefined): string | null {
  if (apiKey === null || apiKey === undefined) {
    return null;
  }
  const trimmed = apiKey.trim();
  return trimmed === '' ? null : trimmed;
}

/**
 * Auth headers for one key: `{ Authorization: 'Bearer …' }`, or `{}` when unset.
 *
 * The gateway accepts either header (`X-API-Key` is the fallback) and gives them
 * priority over the query parameter, so this is the preferred form for every
 * host that can set request headers — the HTTP client already sends exactly this
 * header, and the socket factory forwards the object to undici / `ws`.
 */
export function apiKeyHeaders(apiKey: string | null | undefined): Record<string, string> {
  const key = normalizeApiKey(apiKey);
  return key === null ? {} : { Authorization: `Bearer ${key}` };
}

/**
 * Append the key as a query parameter — **only** for hosts that cannot set
 * headers (a DOM / Electron-renderer `WebSocket`). The result is sensitive:
 * redact it before logging ({@link redactUrl}).
 */
export function wsUrlWithApiKey(wsUrl: string, apiKey: string | null | undefined): string {
  const key = normalizeApiKey(apiKey);
  if (key === null) {
    return wsUrl;
  }
  const separator = wsUrl.includes('?') ? '&' : '?';
  return `${wsUrl}${separator}api_key=${encodeURIComponent(key)}`;
}

/**
 * Derive the HTTP base URL and the WS URL from one host/port pair.
 *
 * The result never carries credentials (see the module doc). Throws
 * `GatewayHttpError{kind:'config'}` when the host is not usable (see
 * {@link normalizeHost}), so a settings mistake surfaces as a typed, actionable
 * error instead of an opaque URL failure.
 */
export function gatewayUrls(options: GatewayUrlOptions): GatewayUrls {
  const host = normalizeHost(options.host);
  const authority = `${host}:${options.port}`;
  return {
    httpBaseUrl: `http://${authority}`,
    wsUrl: `ws://${authority}/ws`,
  };
}

/** Replace the `api_key` query value with `***` before it reaches a log or an error. */
export function redactUrl(url: string): string {
  return url.replace(/([?&]api_key=)[^&]*/gi, '$1***');
}
