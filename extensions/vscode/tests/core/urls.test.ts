import { describe, expect, it } from 'vitest';

import { reconnectDelayMs } from '../../src/core/backoff';
import { gatewayUrls, normalizeApiKey, redactUrl } from '../../src/core/urls';

/**
 * URL derivation and the reconnect ladder — the two small pure policies the
 * connection and the host both rely on.
 */

describe('gatewayUrls', () => {
  it('derives the HTTP base and the WS URL from host / port', () => {
    expect(gatewayUrls({ host: '127.0.0.1', port: 32523 })).toStrictEqual({
      httpBaseUrl: 'http://127.0.0.1:32523',
      wsUrl: 'ws://127.0.0.1:32523/ws',
    });
  });

  it('adds the API key as a query parameter (the only portable way for WS)', () => {
    const { wsUrl, httpBaseUrl } = gatewayUrls({ host: 'localhost', port: 8080, apiKey: 'a b&c' });
    expect(wsUrl).toBe('ws://localhost:8080/ws?api_key=a%20b%26c');
    expect(httpBaseUrl).toBe('http://localhost:8080');
  });

  it('treats a blank key as no auth and trims real keys', () => {
    expect(gatewayUrls({ host: 'h', port: 1, apiKey: '   ' }).wsUrl).toBe('ws://h:1/ws');
    expect(gatewayUrls({ host: 'h', port: 1, apiKey: ' secret ' }).wsUrl).toBe('ws://h:1/ws?api_key=secret');
    expect(gatewayUrls({ host: 'h', port: 1, apiKey: null }).wsUrl).toBe('ws://h:1/ws');
  });

  it('brackets bare IPv6 literals and defaults an empty host', () => {
    expect(gatewayUrls({ host: '::1', port: 32523 }).httpBaseUrl).toBe('http://[::1]:32523');
    expect(gatewayUrls({ host: '[::1]', port: 32523 }).httpBaseUrl).toBe('http://[::1]:32523');
    expect(gatewayUrls({ host: '  ', port: 9 }).httpBaseUrl).toBe('http://127.0.0.1:9');
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
