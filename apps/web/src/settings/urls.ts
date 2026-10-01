/**
 * Settings → gateway URLs.
 *
 * `@wing-agent/client` derives `http://host:port` / `ws://host:port/ws` from a
 * host/port pair (`urls.ts::gatewayUrls`), which is everything the VSCode
 * extension needs. The web client needs one more dimension — `https` / `wss` —
 * and one more mode — "use the origin this page was served from". Both are
 * *settings* concerns, so they live here rather than in the shared capability
 * layer (promote it there if a third shell ever needs it; see design.md D2).
 *
 * The API key is never part of these URLs: HTTP carries it in an
 * `Authorization` header. The one exception is the browser WebSocket, whose
 * constructor cannot set headers — the caller appends the key with the client
 * package's `wsUrlWithApiKey` and redacts its logs with `redactUrl` (design.md D7).
 */

import { type GatewaySettings, type GatewayScheme, normalizePort } from './settings';

/** The page origin a same-origin setting resolves against. */
export interface PageLocation {
  /** e.g. `http://localhost:5173` (no trailing slash). */
  readonly origin: string;
}

export interface GatewayEndpoints {
  /** e.g. `http://127.0.0.1:32523` — for `GatewayHttpClient({ baseUrl })`. */
  readonly httpBaseUrl: string;
  /** e.g. `ws://127.0.0.1:32523/ws` — for `GatewayConnection({ wsUrl })`. */
  readonly wsUrl: string;
  /** `true` when the address came from the page origin, not from the settings. */
  readonly sameOrigin: boolean;
  /** Human-readable address for the UI (never carries credentials). */
  readonly label: string;
}

/** A settings value that cannot be turned into a URL — the UI shows the reason. */
export class GatewayAddressError extends Error {
  override readonly name: string = 'GatewayAddressError';
}

const MAX_HOST_LENGTH = 253;

/** Count `:` characters (an IPv6 literal has at least two). */
function colonCount(host: string): number {
  return host.split(':').length - 1;
}

/**
 * Normalise the configured host into a URL authority fragment (no port).
 *
 * - `''` → the same-origin mode (handled by the caller);
 * - a bare IPv6 literal (`::1`, `fe80::1%en0`) → bracketed (`[::1]`);
 * - `[::1]` stays as it is;
 * - anything else with a `:` is a `host:port` mistake — the port has its own
 *   field, and letting it through produces `http://localhost:8080:32523`, which
 *   fails much later with an opaque transport error.
 */
export function normalizeGatewayHost(host: string): string {
  const trimmed = host.trim();
  if (trimmed === '') {
    return '';
  }
  if (trimmed.length > MAX_HOST_LENGTH) {
    throw new GatewayAddressError('that host name is too long to be a gateway address');
  }
  if (/[\s/\\?#@]/.test(trimmed)) {
    // A full URL (`http://host`) or a path pasted into the field — the scheme has
    // its own control, and a path never belongs to a host.
    throw new GatewayAddressError(
      `"${trimmed}" is not a host name — enter the host only (the scheme has its own field)`,
    );
  }
  if (trimmed.startsWith('[')) {
    if (trimmed.endsWith(']') && trimmed.length > 2) {
      return trimmed;
    }
    throw new GatewayAddressError(`"${trimmed}" is not a valid IPv6 literal`);
  }
  if (colonCount(trimmed) >= 2) {
    return `[${trimmed}]`;
  }
  if (trimmed.includes(':')) {
    throw new GatewayAddressError(
      `"${trimmed}" contains a port — set the host and the port separately ("::1" is fine)`,
    );
  }
  return trimmed;
}

/** `http://x` → `ws://x`, `https://x` → `wss://x`; anything else → `null`. */
function toWsOrigin(origin: string): string | null {
  if (origin.startsWith('https://')) {
    return `wss://${origin.slice('https://'.length)}`;
  }
  if (origin.startsWith('http://')) {
    return `ws://${origin.slice('http://'.length)}`;
  }
  return null;
}

/** Drop a trailing slash (and any path) from a page origin. */
function cleanOrigin(origin: string): string {
  const withoutSlash = origin.replace(/\/+$/, '');
  try {
    const url = new URL(withoutSlash);
    return `${url.protocol}//${url.host}`;
  } catch {
    return withoutSlash;
  }
}

/** The scheme of a same-origin address, derived from the page itself. */
export function schemeOfOrigin(origin: string): GatewayScheme | null {
  if (origin.startsWith('https://')) {
    return 'https';
  }
  if (origin.startsWith('http://')) {
    return 'http';
  }
  return null;
}

/**
 * Resolve the settings against the page location.
 *
 * Throws {@link GatewayAddressError} when the address cannot be used (bad host,
 * unusable page origin); the runtime turns that into the first-connect guide.
 */
export function gatewayEndpoints(settings: GatewaySettings, location: PageLocation): GatewayEndpoints {
  const host = normalizeGatewayHost(settings.host);
  if (host === '') {
    const origin = cleanOrigin(location.origin);
    const wsOrigin = toWsOrigin(origin);
    const scheme = schemeOfOrigin(origin);
    if (wsOrigin === null || scheme === null) {
      throw new GatewayAddressError(
        `this page was not served over http(s) (origin: "${location.origin}") — ` +
          'set the gateway host and port explicitly',
      );
    }
    return {
      httpBaseUrl: origin,
      wsUrl: `${wsOrigin}/ws`,
      sameOrigin: true,
      label: `${origin} (this page)`,
    };
  }
  const port = normalizePort(settings.port);
  if (port === null) {
    throw new GatewayAddressError(`port ${String(settings.port)} is not a valid port (1-65535)`);
  }
  const authority = `${host}:${port}`;
  const scheme = settings.scheme;
  const httpBaseUrl = `${scheme}://${authority}`;
  const wsUrl = `${scheme === 'https' ? 'wss' : 'ws'}://${authority}/ws`;
  // The browser parses the host once more with WHATWG rules, which are stricter
  // than the structural checks above: `a|b` (and an IPv6 zone id like
  // `fe80::1%en0`) passes them but is not a URL. Validating here puts the failure
  // next to the field instead of inside the reconnect ladder, where it looks like
  // a dead gateway forever (review r1 N3).
  assertParsable(httpBaseUrl, host);
  assertParsable(wsUrl, host);
  return { httpBaseUrl, wsUrl, sameOrigin: false, label: httpBaseUrl };
}

/** `new URL` accepts the string, or the host is not usable from a browser. */
function assertParsable(url: string, host: string): void {
  try {
    void new URL(url);
  } catch {
    throw new GatewayAddressError(
      host.includes('%')
        ? `"${host}" cannot be used: IPv6 zone ids ("%en0") are not allowed in a URL — ` +
            'use the address without the zone, or a host name'
        : `"${host}" is not a host name a browser can use`,
    );
  }
}
