/**
 * Tests for the desktop settings storage adapter.
 *
 * The adapter bridges the web runtime's synchronous `SettingsStorage` to the
 * Electron shell's async IPC bridge. In tests we inject a fake `window.wingDesktop`
 * and verify the conversion, caching and persistence paths.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';

import type { GatewaySettings } from '../../src/settings/settings';
import {
  DEFAULT_SETTINGS,
  SETTINGS_STORAGE_KEY,
  encodeSettings,
  loadSettings,
  normalizeSettings,
} from '../../src/settings/settings';
import { desktopSettingsStorage, isDesktopShell } from '../../src/settings/desktop';

// ── Helpers ────────────────────────────────────────────────────────────────────

interface DesktopConfig {
  gatewayBaseUrl: string;
  apiKey: string | null;
  ignoreCertErrors: boolean;
  certificateWhitelist: readonly string[];
  wingPath: string | null;
  autoStart: boolean;
}

const DEFAULT_DESKTOP_CONFIG: DesktopConfig = {
  gatewayBaseUrl: 'http://127.0.0.1:32523',
  apiKey: null,
  ignoreCertErrors: false,
  certificateWhitelist: [],
  wingPath: null,
  autoStart: true,
};

function installFakeDesktop(seed: DesktopConfig = DEFAULT_DESKTOP_CONFIG) {
  let stored: DesktopConfig = { ...seed };
  const bridge = {
    settings: {
      read: vi.fn<() => Promise<DesktopConfig>>().mockResolvedValue({ ...stored }),
      write: vi
        .fn<(patch: Partial<DesktopConfig>) => Promise<DesktopConfig>>()
        .mockImplementation((patch: Partial<DesktopConfig>) => {
          stored = { ...stored, ...patch };
          return Promise.resolve({ ...stored });
        }),
    },
  };
  (globalThis as unknown as Record<string, unknown>).wingDesktop = bridge;
  return { bridge, getStored: () => ({ ...stored }) };
}

function restoreWindow(): void {
  delete (globalThis as unknown as Record<string, unknown>).wingDesktop;
}

// ── Tests ──────────────────────────────────────────────────────────────────────

describe('isDesktopShell', () => {
  afterEach(() => {
    restoreWindow();
  });

  it('returns true when window.wingDesktop exists', () => {
    installFakeDesktop();
    expect(isDesktopShell()).toBe(true);
  });

  it('returns false when window.wingDesktop is absent', () => {
    expect(isDesktopShell()).toBe(false);
  });
});

describe('desktopSettingsStorage — read after hydration', () => {
  afterEach(() => {
    restoreWindow();
  });

  it('reads and converts DesktopConfig to the web settings format', async () => {
    installFakeDesktop();
    const storage = desktopSettingsStorage();
    // Hydration starts eagerly; await it.
    await storage.hydration;
    const raw = storage.read(SETTINGS_STORAGE_KEY);
    expect(raw).not.toBeNull();
    const parsed = normalizeSettings(JSON.parse(raw!));
    expect(parsed.scheme).toBe('http');
    expect(parsed.host).toBe(''); // 127.0.0.1 → '' (same-origin mode)
    expect(parsed.port).toBe(32523);
    expect(parsed.apiKey).toBeNull();
    expect(parsed.ignoreCertErrors).toBe(false);
  });

  it('handles https with explicit host and port', async () => {
    installFakeDesktop({
      ...DEFAULT_DESKTOP_CONFIG,
      gatewayBaseUrl: 'https://gateway.lan:8443',
      apiKey: 'sk-secret',
      ignoreCertErrors: true,
    });
    const storage = desktopSettingsStorage();
    await storage.hydration;
    const raw = storage.read(SETTINGS_STORAGE_KEY);
    const parsed = normalizeSettings(JSON.parse(raw!));
    expect(parsed.scheme).toBe('https');
    expect(parsed.host).toBe('gateway.lan');
    expect(parsed.port).toBe(8443);
    expect(parsed.apiKey).toBe('sk-secret');
    expect(parsed.ignoreCertErrors).toBe(true);
  });

  it('returns null for a key that is not the settings key', async () => {
    installFakeDesktop();
    const storage = desktopSettingsStorage();
    await storage.hydration;
    expect(storage.read('some-other-key')).toBeNull();
  });

  it('falls back to null when the IPC call fails', async () => {
    const { bridge } = installFakeDesktop();
    bridge.settings.read = vi.fn().mockRejectedValue(new Error('IPC error'));
    const storage = desktopSettingsStorage();
    await storage.hydration;
    expect(storage.read(SETTINGS_STORAGE_KEY)).toBeNull();
  });

  it('returns null before hydration completes when read is called early', () => {
    installFakeDesktop();
    const storage = desktopSettingsStorage();
    // Before awaiting hydration, the cache is still null (hydration is in-flight).
    expect(storage.read(SETTINGS_STORAGE_KEY)).toBeNull();
  });
});

describe('desktopSettingsStorage — write', () => {
  afterEach(() => {
    restoreWindow();
  });

  it('writes GatewaySettings through the bridge as a DesktopConfig patch', async () => {
    const { bridge } = installFakeDesktop();
    const storage = desktopSettingsStorage();
    await storage.hydration;
    const settings: GatewaySettings = {
      scheme: 'https',
      host: '10.0.0.5',
      port: 9443,
      apiKey: 'key123',
      ignoreCertErrors: true,
    };
    storage.write(SETTINGS_STORAGE_KEY, encodeSettings(settings));

    // The synchronous cache should update immediately.
    expect(storage.read(SETTINGS_STORAGE_KEY)).toBe(encodeSettings(settings));

    // The IPC write should have been called.
    await vi.waitFor(() => {
      expect(bridge.settings.write).toHaveBeenCalled();
    });
    const patch = bridge.settings.write.mock.calls[0]?.[0] as Partial<DesktopConfig>;
    expect(patch.gatewayBaseUrl).toBe('https://10.0.0.5:9443');
    expect(patch.apiKey).toBe('key123');
    expect(patch.ignoreCertErrors).toBe(true);
    // Desktop-only fields are not in the patch.
    expect(patch.certificateWhitelist).toBeUndefined();
    expect(patch.wingPath).toBeUndefined();
    expect(patch.autoStart).toBeUndefined();
  });

  it('writes same-origin (empty host) as 127.0.0.1', async () => {
    const { bridge } = installFakeDesktop();
    const storage = desktopSettingsStorage();
    await storage.hydration;
    const settings: GatewaySettings = {
      scheme: 'http',
      host: '',
      port: 32523,
      apiKey: null,
      ignoreCertErrors: false,
    };
    storage.write(SETTINGS_STORAGE_KEY, encodeSettings(settings));
    await vi.waitFor(() => {
      expect(bridge.settings.write).toHaveBeenCalled();
    });
    const patch = bridge.settings.write.mock.calls[0]?.[0] as Partial<DesktopConfig>;
    expect(patch.gatewayBaseUrl).toBe('http://127.0.0.1:32523');
  });

  it('ignores writes for keys other than the settings key', async () => {
    const { bridge } = installFakeDesktop();
    const storage = desktopSettingsStorage();
    await storage.hydration;
    storage.write('irrelevant-key', 'value');
    expect(bridge.settings.write).not.toHaveBeenCalled();
  });

  it('round-trips through read after write', async () => {
    installFakeDesktop();
    const storage = desktopSettingsStorage();
    await storage.hydration;

    const settings: GatewaySettings = {
      scheme: 'https',
      host: 'remote.lan',
      port: 32523,
      apiKey: 'updated-key',
      ignoreCertErrors: true,
    };
    storage.write(SETTINGS_STORAGE_KEY, encodeSettings(settings));
    expect(storage.read(SETTINGS_STORAGE_KEY)).toBe(encodeSettings(settings));
  });
});

describe('desktopSettingsStorage — remove', () => {
  afterEach(() => {
    restoreWindow();
  });

  it('clears the cache on remove', async () => {
    installFakeDesktop();
    const storage = desktopSettingsStorage();
    await storage.hydration;
    storage.remove(SETTINGS_STORAGE_KEY);
    expect(storage.read(SETTINGS_STORAGE_KEY)).toBeNull();
  });
});

describe('loadSettings with desktop storage', () => {
  afterEach(() => {
    restoreWindow();
  });

  it('returns defaults when the cache is not hydrated yet', () => {
    installFakeDesktop();
    const storage = desktopSettingsStorage();
    // Before hydration, the cache is null → defaults.
    const settings = loadSettings(storage);
    expect(settings).toEqual(DEFAULT_SETTINGS);
  });

  it('returns real values after hydration completes', async () => {
    installFakeDesktop({
      ...DEFAULT_DESKTOP_CONFIG,
      gatewayBaseUrl: 'https://10.0.0.5:32523',
      apiKey: 'sk-abc',
    });
    const storage = desktopSettingsStorage();
    await storage.hydration;
    const reloaded = loadSettings(storage);
    expect(reloaded.host).toBe('10.0.0.5');
    expect(reloaded.scheme).toBe('https');
    expect(reloaded.apiKey).toBe('sk-abc');
  });
});

describe('read/write wire format to DesktopConfig round-trip', () => {
  afterEach(() => {
    restoreWindow();
  });

  it('preserves complex settings across the conversion', async () => {
    const originalDesktop: DesktopConfig = {
      gatewayBaseUrl: 'https://gateway.internal:8443',
      apiKey: null,
      ignoreCertErrors: true,
      certificateWhitelist: ['gateway.internal:8443'],
      wingPath: '/opt/bin/wing',
      autoStart: false,
    };
    const { bridge } = installFakeDesktop(originalDesktop);
    const storage = desktopSettingsStorage();
    await storage.hydration;
    const raw = storage.read(SETTINGS_STORAGE_KEY)!;
    const parsed = normalizeSettings(JSON.parse(raw));
    // gateway.internal should be preserved (not normalised to 127.0.0.1)
    expect(parsed.host).toBe('gateway.internal');
    expect(parsed.scheme).toBe('https');
    expect(parsed.ignoreCertErrors).toBe(true);

    // Now write back with a different port
    storage.write(SETTINGS_STORAGE_KEY, encodeSettings({ ...parsed, port: 9090 }));
    await vi.waitFor(() => {
      expect(bridge.settings.write).toHaveBeenCalled();
    });
    const patch = bridge.settings.write.mock.calls[0]?.[0] as Partial<DesktopConfig>;
    expect(patch.gatewayBaseUrl).toBe('https://gateway.internal:9090');
  });
});
