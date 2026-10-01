/**
 * Desktop (Electron) settings storage — routes reads and writes through the
 * preload bridge (`window.wingDesktop`) instead of localStorage.
 *
 * The bridge exposes the single `DesktopConfig` object (gatewayBaseUrl, apiKey,
 * ignoreCertErrors, …) over IPC, so the `key` parameter of `SettingsStorage` is
 * ignored: there is one file, one namespace. The adapter translates between the
 * web runtime's `GatewaySettings` (scheme + host + port) and the Electron
 * shell's `DesktopConfig` (gatewayBaseUrl) on every access.
 *
 * ## Async / sync mismatch
 *
 * `SettingsStorage.read` is synchronous (`string | null`) but the IPC round-trip
 * is async. The adapter deals with this by:
 *
 * 1. Starting the async hydration eagerly at factory time (before the runtime
 *    calls `loadSettings`), so it runs in the background during app bootstrap.
 * 2. Caching the last known value in memory: `read()` returns this cache
 *    synchronously. If hydration has not completed yet, the first read returns
 *    `null` (the runtime falls back to defaults and will reconnect once hydration
 *    completes and the event fires).
 * 3. A `ready()` method that resolves after the first successful hydration — used
 *    by tests and by `main.tsx` to re-check settings after bootstrap.
 *
 * This file must not import anything from `@wing-agent/desktop` — the web build
 * must stay free of Electron dependencies. The bridge type is declared locally
 * (mirroring the subpath export `@wing-agent/desktop/bridge`), and detection uses
 * a runtime check of `window.wingDesktop`.
 */

import type { SettingsStorage } from './storage';
import { normalizeSettings, encodeSettings } from './settings';

// ── Type mirror (keeps this file free of `@wing-agent/desktop` imports) ─────────

/** The bridge object Electron's preload publishes on `window.wingDesktop`. */
interface DesktopBridge {
  readonly settings: {
    read(): Promise<DesktopConfig>;
    write(patch: Partial<DesktopConfig>): Promise<DesktopConfig>;
  };
}

interface DesktopConfig {
  readonly gatewayBaseUrl: string;
  readonly apiKey: string | null;
  readonly ignoreCertErrors: boolean;
  readonly certificateWhitelist: readonly string[];
  readonly wingPath: string | null;
  readonly autoStart: boolean;
}

// ── Key ────────────────────────────────────────────────────────────────────────

/** The key the web runtime uses for `loadSettings` / `saveSettings`. */
const SETTINGS_KEY = 'wing.web.gateway';

// ── Conversion ─────────────────────────────────────────────────────────────────

/**
 * Parse a `gatewayBaseUrl` (e.g. `https://192.168.1.100:32523`) into
 * scheme, host, port components.
 */
function parseGatewayUrl(url: string): { scheme: string; host: string; port: number } {
  const parsed = new URL(url);
  const scheme = parsed.protocol === 'https:' ? 'https' : 'http';
  const host = parsed.hostname;
  const port = parsed.port === '' ? (scheme === 'https' ? 443 : 80) : Number(parsed.port);
  return { scheme, host, port };
}

/**
 * Convert `DesktopConfig` → the web's `encodeSettings` format string.
 *
 * `certificateWhitelist` and `wingPath` are desktop-only fields and are not
 * preserved in the gateway settings round-trip — they survive on the desktop side
 * between `read` and the next `write` via the shell's config.json.
 */
function desktopToWebSettings(config: DesktopConfig): string {
  const { scheme, host, port } = parseGatewayUrl(config.gatewayBaseUrl);
  const gatewaySettings = normalizeSettings({
    scheme: scheme as 'http' | 'https',
    host: host === '127.0.0.1' ? '' : host,
    port,
    apiKey: config.apiKey,
    ignoreCertErrors: config.ignoreCertErrors,
  });
  return encodeSettings(gatewaySettings);
}

/**
 * Rebuild a partial `DesktopConfig` patch from the web serialised form.
 */
function webToDesktopPatch(serialised: string): {
  gatewayBaseUrl: string;
  apiKey: string | null;
  ignoreCertErrors: boolean;
} {
  const gatewaySettings = normalizeSettings(JSON.parse(serialised));
  const host = gatewaySettings.host === '' ? '127.0.0.1' : gatewaySettings.host;
  const gatewayBaseUrl = `${gatewaySettings.scheme}://${host}:${gatewaySettings.port}`;
  return {
    gatewayBaseUrl,
    apiKey: gatewaySettings.apiKey,
    ignoreCertErrors: gatewaySettings.ignoreCertErrors,
  };
}

// ── State ──────────────────────────────────────────────────────────────────────

export interface DesktopSettingsStorage extends SettingsStorage {
  /**
   * Resolves once the first successful hydration completes.
   *
   * The runtime calls `loadSettings(storage)` synchronously in its constructor,
   * which will get the default settings (or null) if hydration is not done yet.
   * Callers that want to be sure the settings are current can await this before
   * reading `loadSettings` again.
   */
  readonly hydration: Promise<void>;
}

// ── Factory ────────────────────────────────────────────────────────────────────

/**
 * `SettingsStorage` backed by the Electron preload bridge.
 *
 * Must only be called when `window.wingDesktop` is known to exist.
 * Hydration starts eagerly.
 */
export function desktopSettingsStorage(): DesktopSettingsStorage {
  const bridge = (globalThis as unknown as { wingDesktop: DesktopBridge }).wingDesktop;

  let cachedSerialised: string | null = null;
  let hydrationResolve: (() => void) | undefined;

  const hydration = new Promise<void>((resolve) => {
    hydrationResolve = resolve;
  });

  // Eager hydration
  void (async () => {
    try {
      const config = await bridge.settings.read();
      cachedSerialised = desktopToWebSettings(config);
    } catch {
      // IPC failed — the app shows a connection guide.
    } finally {
      hydrationResolve?.();
      if (typeof globalThis.dispatchEvent === 'function') {
        try {
          globalThis.dispatchEvent(new CustomEvent('wing:settings-hydrated'));
        } catch {
          // Not in a DOM environment (tests).
        }
      }
    }
  })();

  // ── The storage object returned to the caller ─────────────────────

  const storage: DesktopSettingsStorage = {
    hydration,

    persistent: true,

    read(key: string): string | null {
      if (key !== SETTINGS_KEY) {
        return null;
      }
      // Wait for hydration synchronously if already resolved.
      return cachedSerialised;
    },

    write(key: string, value: string): void {
      if (key !== SETTINGS_KEY) {
        return;
      }
      cachedSerialised = value;
      const patch = webToDesktopPatch(value);
      // Fire-and-forget IPC write.
      bridge.settings.write(patch).catch((error: unknown) => {
        console.warn('could not write settings to the desktop shell', error);
      });
    },

    remove(_key: string): void {
      cachedSerialised = null;
    },
  };

  return storage;
}

/**
 * `true` when the page is running inside the Electron shell (the preload bridge
 * is available).
 */
export function isDesktopShell(): boolean {
  return (typeof window !== 'undefined' && 'wingDesktop' in window) || 'wingDesktop' in globalThis;
}
