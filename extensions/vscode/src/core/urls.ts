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

import { GatewayHttpError } from './errors';

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
 * Derive the HTTP base URL and the WS URL from one host/port/key triple.
 *
 * Throws `GatewayHttpError{kind:'config'}` when the host is not usable (see
 * {@link normalizeHost}), so a settings mistake surfaces as a typed, actionable
 * error instead of an opaque URL failure.
 */
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
