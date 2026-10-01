import { describe, expect, it } from 'vitest';

import {
  allowsIgnoringCertificate,
  allowsIgnoringCertificateHost,
  certificateTargetFor,
  createCertificatePolicy,
  normalizeCertificateEntry,
  normalizeCertificateWhitelist,
} from '../src/certificate';

/**
 * The certificate escape hatch is the sharpest edge in the shell: getting it
 * wrong means silently trusting every site. The matrix below is the contract —
 * switch off ⇒ never; switch on ⇒ only the exact `host:port` on the list.
 */

describe('certificateTargetFor', () => {
  it('reduces URLs to a lower-cased host:port', () => {
    expect(certificateTargetFor('https://127.0.0.1:32523/api/health')).toBe('127.0.0.1:32523');
    expect(certificateTargetFor('wss://Wing.Local:8443/ws')).toBe('wing.local:8443');
    expect(certificateTargetFor('http://localhost/api/health')).toBe('localhost:80');
    expect(certificateTargetFor('ws://gateway.test:32523/ws')).toBe('gateway.test:32523');
  });

  it('uses the scheme default port when the URL has none', () => {
    expect(certificateTargetFor('https://wing.example.com/ws')).toBe('wing.example.com:443');
    expect(certificateTargetFor('wss://wing.example.com')).toBe('wing.example.com:443');
    expect(certificateTargetFor('http://wing.example.com')).toBe('wing.example.com:80');
  });

  it('keeps IPv6 hosts bracketed', () => {
    expect(certificateTargetFor('https://[::1]:8443/ws')).toBe('[::1]:8443');
  });

  it('returns null for unparsable URLs and for schemes certificates do not apply to', () => {
    expect(certificateTargetFor('not a url')).toBeNull();
    expect(certificateTargetFor('wing-app://app/')).toBeNull();
    expect(certificateTargetFor('file:///tmp/index.html')).toBeNull();
  });
});

describe('normalizeCertificateEntry', () => {
  it('accepts host:port in its various spellings', () => {
    expect(normalizeCertificateEntry('127.0.0.1:32523')).toBe('127.0.0.1:32523');
    expect(normalizeCertificateEntry('  Wing.Local:8443  ')).toBe('wing.local:8443');
    expect(normalizeCertificateEntry('https://wing.local:8443')).toBe('wing.local:8443');
    expect(normalizeCertificateEntry('https://wing.local:8443/some/path?q=1')).toBe('wing.local:8443');
    expect(normalizeCertificateEntry('[::1]:8443')).toBe('[::1]:8443');
  });

  it('rejects entries without an explicit port instead of widening to the whole host', () => {
    expect(normalizeCertificateEntry('localhost')).toBeNull();
    expect(normalizeCertificateEntry('https://wing.local')).toBeNull();
    expect(normalizeCertificateEntry('wing.local:')).toBeNull();
  });

  it('rejects empty, malformed and credential-bearing entries', () => {
    expect(normalizeCertificateEntry('')).toBeNull();
    expect(normalizeCertificateEntry('   ')).toBeNull();
    expect(normalizeCertificateEntry('host:not-a-port')).toBeNull();
    expect(normalizeCertificateEntry('host:99999')).toBeNull();
    expect(normalizeCertificateEntry('user@host:8443')).toBeNull();
  });

  it('deduplicates the whitelist and reports what it dropped', () => {
    expect(normalizeCertificateWhitelist(['a:1', 'a:1', 'https://a:1/', 'b'])).toEqual({
      targets: ['a:1'],
      hosts: ['a'],
      invalid: ['b'],
    });
    expect(normalizeCertificateWhitelist([42, null])).toEqual({
      targets: [],
      hosts: [],
      invalid: ['42', 'null'],
    });
    expect(normalizeCertificateWhitelist(['[::1]:8443'])).toEqual({
      targets: ['[::1]:8443'],
      hosts: ['[::1]'],
      invalid: [],
    });
  });
});

describe('allowsIgnoringCertificate', () => {
  const policy = createCertificatePolicy({
    ignoreCertErrors: true,
    certificateWhitelist: ['127.0.0.1:32523', 'wing.local:8443'],
  });

  it('never allows anything while the switch is off', () => {
    const off = createCertificatePolicy({
      ignoreCertErrors: false,
      certificateWhitelist: ['127.0.0.1:32523'],
    });
    expect(off.targets).toEqual(['127.0.0.1:32523']);
    expect(allowsIgnoringCertificate('https://127.0.0.1:32523/api/health', off)).toBe(false);
    expect(allowsIgnoringCertificate('https://wing.local:8443/ws', off)).toBe(false);
  });

  it('allows a whitelisted host:port (https and wss alike)', () => {
    expect(allowsIgnoringCertificate('https://127.0.0.1:32523/api/health', policy)).toBe(true);
    expect(allowsIgnoringCertificate('wss://127.0.0.1:32523/ws', policy)).toBe(true);
    expect(allowsIgnoringCertificate('https://WING.LOCAL:8443/ws', policy)).toBe(true);
  });

  it('denies a different port on an allowed host', () => {
    expect(allowsIgnoringCertificate('https://127.0.0.1:41234/api/health', policy)).toBe(false);
    expect(allowsIgnoringCertificate('https://127.0.0.1/api/health', policy)).toBe(false); // ⇒ :443
    expect(allowsIgnoringCertificate('https://wing.local:8444/ws', policy)).toBe(false);
  });

  it('denies other hosts, including lookalikes', () => {
    for (const url of [
      'https://127.0.0.2:32523/api/health',
      'https://wing.local.evil.test:8443/ws',
      'https://evil.test:32523/api/health',
      'https://127.0.0.1.evil.test:32523/api/health',
    ]) {
      expect(allowsIgnoringCertificate(url, policy)).toBe(false);
    }
  });

  it('denies everything when the switch is on but the whitelist is empty', () => {
    const empty = createCertificatePolicy({ ignoreCertErrors: true });
    expect(empty.targets).toEqual([]);
    expect(allowsIgnoringCertificate('https://127.0.0.1:32523/api/health', empty)).toBe(false);
  });

  it('denies unparsable URLs and URLs without a network scheme', () => {
    expect(allowsIgnoringCertificate('wing-app://app/', policy)).toBe(false);
    expect(allowsIgnoringCertificate('', policy)).toBe(false);
  });

  it('surfaces malformed whitelist entries so the shell can warn', () => {
    const mixed = createCertificatePolicy({
      ignoreCertErrors: true,
      certificateWhitelist: ['127.0.0.1:32523', 'localhost'],
    });
    expect(mixed.targets).toEqual(['127.0.0.1:32523']);
    expect(mixed.invalidEntries).toEqual(['localhost']);
  });

  it('ignores a whitelist that is not an array (config JSON is untrusted)', () => {
    const policyFromJunk = createCertificatePolicy({
      ignoreCertErrors: true,
      certificateWhitelist: 'host:1234',
    });
    expect(policyFromJunk.targets).toEqual([]);
    expect(policyFromJunk.invalidEntries).toEqual([]);
  });
});

describe('allowsIgnoringCertificateHost', () => {
  // `session.setCertificateVerifyProc` reports a hostname only, so this is the
  // check that actually guards the shell's own network stack. Verified against
  // Electron 44: the request object has no port and no URL.
  const policy = createCertificatePolicy({
    ignoreCertErrors: true,
    certificateWhitelist: ['127.0.0.1:32523', 'wing.local:8443'],
  });

  it('never allows anything while the switch is off', () => {
    const off = createCertificatePolicy({
      ignoreCertErrors: false,
      certificateWhitelist: ['127.0.0.1:32523'],
    });
    expect(allowsIgnoringCertificateHost('127.0.0.1', off)).toBe(false);
  });

  it('allows the host part of a whitelisted target, case-insensitively', () => {
    expect(allowsIgnoringCertificateHost('127.0.0.1', policy)).toBe(true);
    expect(allowsIgnoringCertificateHost('WING.LOCAL', policy)).toBe(true);
  });

  it('denies hosts that only look similar', () => {
    for (const host of ['127.0.0.2', 'wing.local.evil.test', 'evil.test', '127.0.0.1.evil.test', '']) {
      expect(allowsIgnoringCertificateHost(host, policy)).toBe(false);
    }
  });

  it('derives the host view from the whitelist, so an empty list allows nothing', () => {
    expect(createCertificatePolicy({ ignoreCertErrors: true }).hosts).toEqual([]);
    expect(policy.hosts).toEqual(['127.0.0.1', 'wing.local']);
  });
});
