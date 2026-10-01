import { describe, expect, it } from 'vitest';

import { DEFAULT_SETTINGS, type GatewaySettings } from '../src/settings/settings';
import {
  GatewayAddressError,
  gatewayEndpoints,
  normalizeGatewayHost,
  schemeOfOrigin,
} from '../src/settings/urls';

const LOCATION = { origin: 'http://localhost:5173' };

function settings(overrides: Partial<GatewaySettings>): GatewaySettings {
  return { ...DEFAULT_SETTINGS, ...overrides };
}

describe('normalizeGatewayHost', () => {
  it('passes through plain hosts and blanks', () => {
    expect(normalizeGatewayHost('gateway.lan')).toBe('gateway.lan');
    expect(normalizeGatewayHost('  127.0.0.1 ')).toBe('127.0.0.1');
    expect(normalizeGatewayHost('')).toBe('');
  });

  it('brackets bare IPv6 literals and leaves bracketed ones alone', () => {
    expect(normalizeGatewayHost('::1')).toBe('[::1]');
    // Structural only: a zone id is bracketed here but rejected by
    // `gatewayEndpoints`, because WHATWG URLs do not allow it (see below).
    expect(normalizeGatewayHost('fe80::1%en0')).toBe('[fe80::1%en0]');
    expect(normalizeGatewayHost('[::1]')).toBe('[::1]');
  });

  it('rejects a host that carries a port (the port has its own field)', () => {
    expect(() => normalizeGatewayHost('localhost:8080')).toThrow(GatewayAddressError);
    expect(() => normalizeGatewayHost('[::1]:8080')).toThrow(GatewayAddressError);
  });

  it('rejects a pasted URL or path', () => {
    for (const value of ['http://host', 'host/thing', 'host?x=1', 'host#frag', 'user@host']) {
      expect(() => normalizeGatewayHost(value)).toThrow(GatewayAddressError);
    }
  });

  it('rejects an over-long host name', () => {
    expect(() => normalizeGatewayHost('a'.repeat(300))).toThrow(GatewayAddressError);
  });
});

describe('gatewayEndpoints', () => {
  it('resolves the same-origin mode from the page location', () => {
    const endpoints = gatewayEndpoints(settings({ host: '' }), LOCATION);
    expect(endpoints).toEqual({
      httpBaseUrl: 'http://localhost:5173',
      wsUrl: 'ws://localhost:5173/ws',
      sameOrigin: true,
      label: 'http://localhost:5173 (this page)',
    });
  });

  it('derives wss for a https page (self-signed reverse proxy deployment)', () => {
    const endpoints = gatewayEndpoints(settings({ host: '' }), { origin: 'https://wing.example' });
    expect(endpoints.httpBaseUrl).toBe('https://wing.example');
    expect(endpoints.wsUrl).toBe('wss://wing.example/ws');
  });

  it('builds a custom http address', () => {
    const endpoints = gatewayEndpoints(settings({ host: '192.168.1.10', port: 32_523 }), LOCATION);
    expect(endpoints).toEqual({
      httpBaseUrl: 'http://192.168.1.10:32523',
      wsUrl: 'ws://192.168.1.10:32523/ws',
      sameOrigin: false,
      label: 'http://192.168.1.10:32523',
    });
  });

  it('switches both sides to TLS when the scheme is https', () => {
    const endpoints = gatewayEndpoints(settings({ scheme: 'https', host: 'gc.lan', port: 8443 }), LOCATION);
    expect(endpoints.httpBaseUrl).toBe('https://gc.lan:8443');
    expect(endpoints.wsUrl).toBe('wss://gc.lan:8443/ws');
    expect(endpoints.sameOrigin).toBe(false);
  });

  it('brackets an IPv6 literal in both URLs', () => {
    const endpoints = gatewayEndpoints(settings({ host: '::1' }), LOCATION);
    expect(endpoints.httpBaseUrl).toBe('http://[::1]:32523');
    expect(endpoints.wsUrl).toBe('ws://[::1]:32523/ws');
  });

  it('ignores the scheme in the same-origin mode (the page decides it)', () => {
    const endpoints = gatewayEndpoints(settings({ scheme: 'https', host: '' }), LOCATION);
    expect(endpoints.httpBaseUrl).toBe('http://localhost:5173');
    expect(endpoints.wsUrl).toBe('ws://localhost:5173/ws');
  });

  it('strips a trailing slash / path from the page origin', () => {
    const endpoints = gatewayEndpoints(settings({ host: '' }), { origin: 'http://host:8080/app/' });
    expect(endpoints.httpBaseUrl).toBe('http://host:8080');
  });

  it('refuses an origin it cannot derive a socket URL from', () => {
    expect(() => gatewayEndpoints(settings({ host: '' }), { origin: 'file:///app/index.html' })).toThrow(
      GatewayAddressError,
    );
  });

  it('refuses an unusable port (defence in depth: loadSettings normalises first)', () => {
    expect(() => gatewayEndpoints(settings({ host: 'host', port: 0 }), LOCATION)).toThrow(
      GatewayAddressError,
    );
  });

  it('refuses a host the browser cannot parse, instead of looping on connect', () => {
    // `normalizeGatewayHost` is a structural check; these pass it but produce an
    // unparseable URL, which used to surface as a dead gateway with a permanent
    // "connecting… (attempt N)" (review r1 N3).
    for (const host of ['a|b', 'a<b', 'a^b', 'a%b', 'a b']) {
      expect(() => gatewayEndpoints(settings({ host }), LOCATION)).toThrow(GatewayAddressError);
    }
  });

  it('refuses an IPv6 zone id and says why', () => {
    expect(() => gatewayEndpoints(settings({ host: 'fe80::1%en0' }), LOCATION)).toThrow(/zone id/);
  });

  it('still accepts the host shapes a gateway really uses', () => {
    for (const host of ['127.0.0.1', 'localhost', 'gateway.lan', 'my_host', '::1', '[fe80::1]']) {
      expect(gatewayEndpoints(settings({ host }), LOCATION).httpBaseUrl).toContain('://');
    }
  });

  it('never leaks the API key into a URL', () => {
    const endpoints = gatewayEndpoints(settings({ host: 'host', apiKey: 'super-secret' }), LOCATION);
    expect(JSON.stringify(endpoints)).not.toContain('super-secret');
  });
});

describe('schemeOfOrigin', () => {
  it('reports the scheme of an http(s) origin and null otherwise', () => {
    expect(schemeOfOrigin('http://a')).toBe('http');
    expect(schemeOfOrigin('https://a')).toBe('https');
    expect(schemeOfOrigin('file:///a')).toBeNull();
    expect(schemeOfOrigin('ws://a')).toBeNull();
  });
});
