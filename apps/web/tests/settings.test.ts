import { describe, expect, it, vi } from 'vitest';

import {
  DEFAULT_GATEWAY_PORT,
  DEFAULT_SETTINGS,
  SETTINGS_STORAGE_KEY,
  SETTINGS_VERSION,
  encodeSettings,
  loadSettings,
  normalizePort,
  normalizeSettings,
  saveSettings,
} from '../src/settings/settings';
import { browserSettingsStorage, memorySettingsStorage } from '../src/settings/storage';

describe('normalizeSettings', () => {
  it('returns the defaults for anything that is not an object', () => {
    for (const value of [null, undefined, 42, 'nope', [], true]) {
      expect(normalizeSettings(value)).toEqual(DEFAULT_SETTINGS);
    }
  });

  it('keeps a stored value field by field', () => {
    expect(
      normalizeSettings({
        version: 1,
        scheme: 'https',
        host: 'gateway.lan',
        port: 8443,
        apiKey: 'secret',
        ignoreCertErrors: true,
      }),
    ).toEqual({
      scheme: 'https',
      host: 'gateway.lan',
      port: 8443,
      apiKey: 'secret',
      ignoreCertErrors: true,
    });
  });

  it('drops unknown keys and never reads the version (forward compatible)', () => {
    const settings = normalizeSettings({ version: 99, host: 'host', somethingNew: true });
    expect(settings.host).toBe('host');
    expect(Object.keys(settings).sort()).toEqual(['apiKey', 'host', 'ignoreCertErrors', 'port', 'scheme']);
  });

  it('falls back per field, keeping the rest', () => {
    const settings = normalizeSettings({
      scheme: 'ftp',
      host: 42,
      port: 'not a port',
      apiKey: 7,
      ignoreCertErrors: 'yes',
    });
    expect(settings).toEqual(DEFAULT_SETTINGS);
  });

  it('trims the host and treats a blank one as "this page"', () => {
    expect(normalizeSettings({ host: '  127.0.0.1  ' }).host).toBe('127.0.0.1');
    expect(normalizeSettings({ host: '   ' }).host).toBe('');
  });

  it('accepts a numeric string port and rejects out-of-range ones', () => {
    expect(normalizeSettings({ port: '8080' }).port).toBe(8080);
    expect(normalizeSettings({ port: 0 }).port).toBe(DEFAULT_GATEWAY_PORT);
    expect(normalizeSettings({ port: 65_536 }).port).toBe(DEFAULT_GATEWAY_PORT);
    expect(normalizeSettings({ port: 1.5 }).port).toBe(DEFAULT_GATEWAY_PORT);
  });

  it('keeps a typed host verbatim so the UI can report the typo', () => {
    // The address is validated where it is *used* (urls.ts); normalising must not
    // silently rewrite it to the default.
    expect(normalizeSettings({ host: 'localhost:8080' }).host).toBe('localhost:8080');
  });

  it('normalises the api key (blank = no auth)', () => {
    expect(normalizeSettings({ apiKey: '' }).apiKey).toBeNull();
    expect(normalizeSettings({ apiKey: '  k  ' }).apiKey).toBe('k');
    expect(normalizeSettings({ apiKey: null }).apiKey).toBeNull();
  });
});

describe('normalizePort', () => {
  it('accepts integers and numeric strings inside the range', () => {
    expect(normalizePort(1)).toBe(1);
    expect(normalizePort(65_535)).toBe(65_535);
    expect(normalizePort('32523')).toBe(32_523);
  });

  it('rejects everything else', () => {
    for (const value of [0, -1, 1.2, 65_536, '', '  ', 'abc', null, undefined, {}, []]) {
      expect(normalizePort(value)).toBeNull();
    }
  });
});

describe('persistence', () => {
  it('round-trips through the storage', () => {
    const storage = memorySettingsStorage();
    const written = saveSettings(storage, {
      scheme: 'https',
      host: 'gc.lan',
      port: 9443,
      apiKey: 'key',
      ignoreCertErrors: true,
    });
    expect(written.host).toBe('gc.lan');
    expect(loadSettings(storage)).toEqual(written);
    const raw = JSON.parse(storage.read(SETTINGS_STORAGE_KEY) ?? 'null') as Record<string, unknown>;
    expect(raw['version']).toBe(SETTINGS_VERSION);
  });

  it('falls back to the defaults when nothing was stored', () => {
    expect(loadSettings(memorySettingsStorage())).toEqual(DEFAULT_SETTINGS);
  });

  it('survives a corrupt value (truncated / hand-edited / wrong shape)', () => {
    for (const stored of ['{oops', '', 'null', '"text"', '[1,2,3]', '{"port":{"a":1}}']) {
      expect(loadSettings(memorySettingsStorage({ [SETTINGS_STORAGE_KEY]: stored }))).toEqual(
        DEFAULT_SETTINGS,
      );
    }
  });

  it('writes a normalised envelope even when handed garbage', () => {
    const storage = memorySettingsStorage();
    const written = saveSettings(storage, { port: 'nope', scheme: 'gopher', host: 5 });
    expect(written).toEqual(DEFAULT_SETTINGS);
    expect(JSON.parse(storage.read(SETTINGS_STORAGE_KEY) ?? 'null')).toEqual({
      version: SETTINGS_VERSION,
      ...DEFAULT_SETTINGS,
    });
  });

  it('encodes without leaking extra fields', () => {
    expect(JSON.parse(encodeSettings({ ...DEFAULT_SETTINGS, host: 'h' }))).toEqual({
      version: SETTINGS_VERSION,
      scheme: 'http',
      host: 'h',
      port: DEFAULT_GATEWAY_PORT,
      apiKey: null,
      ignoreCertErrors: false,
    });
  });
});

describe('settings storage', () => {
  it('falls back to memory when the browser has no usable localStorage', () => {
    const storage = browserSettingsStorage('wing.web.test-probe');
    // Depending on the node version, `localStorage` is either absent or a stub
    // that throws on access — both are exactly the fallback path.
    expect(storage.persistent).toBe(false);
    storage.write('k', 'v');
    expect(storage.read('k')).toBe('v');
    storage.remove('k');
    expect(storage.read('k')).toBeNull();
  });

  it('falls back when the probe write throws (private mode)', () => {
    const throwing = {
      getItem: () => 'stale',
      setItem: () => {
        throw new Error('QuotaExceededError');
      },
      removeItem: () => undefined,
    } as unknown as Storage;
    const original = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
    Object.defineProperty(globalThis, 'localStorage', { value: throwing, configurable: true });
    try {
      const storage = browserSettingsStorage('wing.web.test-probe');
      expect(storage.persistent).toBe(false);
      expect(storage.read('wing.web.gateway')).toBeNull();
    } finally {
      if (original === undefined) {
        delete (globalThis as { localStorage?: unknown }).localStorage;
      } else {
        Object.defineProperty(globalThis, 'localStorage', original);
      }
    }
  });

  it('uses localStorage when the probe round trip works', () => {
    const map = new Map<string, string>();
    const storageImpl = {
      getItem: (key: string) => map.get(key) ?? null,
      setItem: (key: string, value: string) => {
        map.set(key, value);
      },
      removeItem: (key: string) => {
        map.delete(key);
      },
    } as unknown as Storage;
    const original = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
    Object.defineProperty(globalThis, 'localStorage', { value: storageImpl, configurable: true });
    try {
      const storage = browserSettingsStorage('wing.web.test-probe');
      expect(storage.persistent).toBe(true);
      storage.write('wing.web.gateway', 'x');
      expect(storage.read('wing.web.gateway')).toBe('x');
      // The probe key never lingers.
      expect(map.has('wing.web.test-probe')).toBe(false);
      storage.remove('wing.web.gateway');
      expect(storage.read('wing.web.gateway')).toBeNull();
    } finally {
      if (original === undefined) {
        delete (globalThis as { localStorage?: unknown }).localStorage;
      } else {
        Object.defineProperty(globalThis, 'localStorage', original);
      }
    }
  });

  it('swallows a storage failure on read/write (never throws into the app)', () => {
    const impl = {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
      removeItem: () => {
        throw new Error('denied');
      },
    } as unknown as Storage;
    const original = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
    Object.defineProperty(globalThis, 'localStorage', { value: impl, configurable: true });
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    try {
      const storage = browserSettingsStorage('wing.web.test-probe');
      // The probe threw → memory fallback; the throwing object is never touched again.
      expect(storage.persistent).toBe(false);
      expect(() => {
        storage.write('k', 'v');
      }).not.toThrow();
      expect(storage.read('k')).toBe('v');
    } finally {
      warn.mockRestore();
      if (original === undefined) {
        delete (globalThis as { localStorage?: unknown }).localStorage;
      } else {
        Object.defineProperty(globalThis, 'localStorage', original);
      }
    }
  });
});
