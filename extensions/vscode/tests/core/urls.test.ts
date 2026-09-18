import { describe, expect, it } from 'vitest';

import { reconnectDelayMs } from '../../src/core/backoff';
import { apiKeyHeaders, gatewayUrls, normalizeApiKey, redactUrl, wsUrlWithApiKey } from '../../src/core/urls';

/**
 * URL derivation, auth headers and the reconnect ladder — the small pure
 * policies the connection and the host both rely on.
 */

describe('gatewayUrls', () => {
  it('derives the HTTP base and the WS URL from host / port, with no credentials', () => {
    expect(gatewayUrls({ host: '127.0.0.1', port: 32523 })).toStrictEqual({
      httpBaseUrl: 'http://127.0.0.1:32523',
      wsUrl: 'ws://127.0.0.1:32523/ws',
    });
    // The URLs are not sensitive data any more: a key never reaches them (review
    // #109 [P3-6] — the gateway takes headers, and the host sends them).
    expect(gatewayUrls({ host: 'localhost', port: 8080 }).wsUrl).not.toContain('api_key');
  });

  it('keeps the API-key header helper in sync with the gateway priority', () => {
    // `gateway/auth.py::extract_key_from_headers`: Authorization: Bearer first.
    expect(apiKeyHeaders('secret')).toStrictEqual({ Authorization: 'Bearer secret' });
    expect(apiKeyHeaders('  secret  ')).toStrictEqual({ Authorization: 'Bearer secret' });
    expect(apiKeyHeaders('')).toStrictEqual({});
    expect(apiKeyHeaders('   ')).toStrictEqual({});
    expect(apiKeyHeaders(null)).toStrictEqual({});
    expect(apiKeyHeaders(undefined)).toStrictEqual({});
  });

  it('offers the query form for hosts that cannot set headers (and escapes it)', () => {
    expect(wsUrlWithApiKey('ws://localhost:8080/ws', 'a b&c')).toBe(
      'ws://localhost:8080/ws?api_key=a%20b%26c',
    );
    expect(wsUrlWithApiKey('ws://h:1/ws', ' secret ')).toBe('ws://h:1/ws?api_key=secret');
    expect(wsUrlWithApiKey('ws://h:1/ws?x=1', 'k')).toBe('ws://h:1/ws?x=1&api_key=k');
    // No key → the URL is returned untouched (never a dangling `?api_key=`).
    expect(wsUrlWithApiKey('ws://h:1/ws', '   ')).toBe('ws://h:1/ws');
    expect(wsUrlWithApiKey('ws://h:1/ws', null)).toBe('ws://h:1/ws');
  });

  it('brackets only real IPv6 literals and defaults an empty host', () => {
    expect(gatewayUrls({ host: '::1', port: 32523 }).httpBaseUrl).toBe('http://[::1]:32523');
    expect(gatewayUrls({ host: '[::1]', port: 32523 }).httpBaseUrl).toBe('http://[::1]:32523');
    expect(gatewayUrls({ host: 'fe80::1%en0', port: 32523 }).httpBaseUrl).toBe('http://[fe80::1%en0]:32523');
    expect(gatewayUrls({ host: '::ffff:127.0.0.1', port: 1 }).httpBaseUrl).toBe(
      'http://[::ffff:127.0.0.1]:1',
    );
    expect(gatewayUrls({ host: '  ', port: 9 }).httpBaseUrl).toBe('http://127.0.0.1:9');
  });

  it('reports a host that carries a port as a configuration error (review r1 N3)', () => {
    // Bracketing anything containing ":" produced `http://[localhost:8080]:32523`,
    // an invalid URL that only failed deep in the WS layer with an opaque message.
    for (const host of ['localhost:8080', '127.0.0.1:32523', 'example.com:', '[::1]:8080']) {
      expect(() => gatewayUrls({ host, port: 32523 }), host).toThrowError(
        expect.objectContaining({ name: 'GatewayHttpError', kind: 'config' }),
      );
    }
    // The message must name the offending value and the fix.
    expect(() => gatewayUrls({ host: 'localhost:8080', port: 32523 })).toThrowError(
      /host must not contain a port: "localhost:8080"/,
    );
    expect(() => gatewayUrls({ host: '[::1]:8080', port: 32523 })).toThrowError(/must not contain a port/);
    expect(() => gatewayUrls({ host: '[::1', port: 32523 })).toThrowError(/not a valid IPv6 literal/);
  });

  it('redacts the key before it can reach a log line', () => {
    expect(redactUrl('ws://h:1/ws?api_key=secret')).toBe('ws://h:1/ws?api_key=***');
    expect(redactUrl('ws://h:1/ws?api_key=secret&x=1')).toBe('ws://h:1/ws?api_key=***&x=1');
    expect(redactUrl('ws://h:1/ws')).toBe('ws://h:1/ws');
  });

  it('normalizeApiKey trims and treats blanks as absent', () => {
    expect(normalizeApiKey(undefined)).toBeNull();
    expect(normalizeApiKey(null)).toBeNull();
    expect(normalizeApiKey('  ')).toBeNull();
    expect(normalizeApiKey(' k ')).toBe('k');
  });
});

describe('reconnectDelayMs', () => {
  it('is min(base · 2^attempt, max) — the Rust ladder', () => {
    const ladder = [0, 1, 2, 3, 4, 5, 6, 9].map((attempt) => reconnectDelayMs(attempt));
    expect(ladder).toStrictEqual([1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]);
  });

  it('honours custom bounds and clamps odd attempts', () => {
    expect(reconnectDelayMs(2, { baseDelayMs: 100, maxDelayMs: 400 })).toBe(400);
    expect(reconnectDelayMs(-1, { baseDelayMs: 100, maxDelayMs: 400 })).toBe(100);
    expect(reconnectDelayMs(2.9, { baseDelayMs: 100, maxDelayMs: 10_000 })).toBe(400);
  });
});
