/**
 * The gateway connection settings — the model, its persistence and its
 * normalisation.
 *
 * Field-for-field this is the VSCode extension's `GatewaySettings` (`wing.host`,
 * `wing.port`, `wing.apiKey`) plus the two things a *browser* client needs:
 *
 * - `scheme` — the gateway can be exposed over TLS (self-signed HTTPS in front of
 *   the reverse proxy is the planned mobile deployment), so `http` is not the
 *   only answer;
 * - `host: ''` — **the page's own origin**. That is the zero-config case: `pnpm
 *   dev` proxies `/api` + `/ws` to the local gateway, and in production the
 *   gateway itself serves this bundle (step 03), so the app talks to whatever
 *   origin it was loaded from. Setting a host opts into an absolute address
 *   (LAN / Tailscale / the Electron shell) and with it into CORS.
 *
 * Storage format (v1, `wing.web.gateway`):
 *
 * ```jsonc
 * { "version": 1, "scheme": "http", "host": "", "port": 32523, "apiKey": null, "ignoreCertErrors": false }
 * ```
 *
 * `normalizeSettings` is the single entry point for *every* read and write path,
 * so a half-written or hand-edited value can never reach the client factory. It
 * never throws and never discards the whole object: unknown keys are dropped,
 * invalid fields fall back to their default, and forward compatibility is
 * structural (a newer version's extra field is simply not read).
 *
 * The host is stored as typed, not as "validated": a typo like `localhost:8080`
 * survives the round trip and is *reported* by `gatewayEndpoints` (see
 * `urls.ts`) instead of being silently rewritten to the default address. The user
 * sees the error next to the field they typed.
 */

import { normalizeApiKey } from '@wing-agent/client';

import type { SettingsStorage } from './storage';

/** Wire scheme of the *HTTP* side; the WS side is derived (`ws` / `wss`). */
export type GatewayScheme = 'http' | 'https';

export interface GatewaySettings {
  readonly scheme: GatewayScheme;
  /** `''` = use the page's own origin (dev proxy / gateway-hosted build). */
  readonly host: string;
  readonly port: number;
  /** `null` = no auth (blank / missing). */
  readonly apiKey: string | null;
  /**
   * Ask a shell that *can* (Electron) to accept a self-signed certificate.
   *
   * A browser cannot bypass its own certificate validation; see the settings
   * dialog for the guidance this flag produces on the web side.
   */
  readonly ignoreCertErrors: boolean;
}

/** Backend default (`libs/core/wing/default_config.py` → `gateway.port`). */
export const DEFAULT_GATEWAY_PORT = 32_523;

/** `localStorage` key. One key: the format carries its own version. */
export const SETTINGS_STORAGE_KEY = 'wing.web.gateway';

/** Current storage format version (see the module doc for the migration rule). */
export const SETTINGS_VERSION = 1;

export const DEFAULT_SETTINGS: GatewaySettings = Object.freeze({
  scheme: 'http',
  host: '',
  port: DEFAULT_GATEWAY_PORT,
  apiKey: null,
  ignoreCertErrors: false,
});

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** `1..65535` integer, from a number or a numeric string; `null` when unusable. */
export function normalizePort(value: unknown): number | null {
  const parsed =
    typeof value === 'number'
      ? value
      : typeof value === 'string' && value.trim() !== ''
        ? Number(value)
        : null;
  if (parsed === null || !Number.isInteger(parsed) || parsed < 1 || parsed > 65_535) {
    return null;
  }
  return parsed;
}

/** The settings a stored value actually means (never throws). */
export function normalizeSettings(value: unknown): GatewaySettings {
  if (!isRecord(value)) {
    return { ...DEFAULT_SETTINGS };
  }
  const scheme = value['scheme'];
  const host = value['host'];
  const port = normalizePort(value['port']);
  return {
    scheme: scheme === 'https' || scheme === 'http' ? scheme : DEFAULT_SETTINGS.scheme,
    host: typeof host === 'string' ? host.trim() : DEFAULT_SETTINGS.host,
    port: port ?? DEFAULT_SETTINGS.port,
    apiKey: normalizeApiKey(typeof value['apiKey'] === 'string' ? value['apiKey'] : null),
    ignoreCertErrors: value['ignoreCertErrors'] === true,
  };
}

/** The stored envelope (version + fields). Written through `saveSettings`. */
export function encodeSettings(settings: GatewaySettings): string {
  return JSON.stringify({ version: SETTINGS_VERSION, ...settings });
}

/** Parse a stored string; `null` when it is missing or not JSON. */
function decodeSettings(text: string | null): GatewaySettings | null {
  if (text === null || text.trim() === '') {
    return null;
  }
  try {
    return normalizeSettings(JSON.parse(text));
  } catch {
    return null; // hand-edited / truncated value: fall back to the defaults
  }
}

/** Read + normalise; a missing or corrupt value yields the defaults. */
export function loadSettings(storage: SettingsStorage): GatewaySettings {
  return decodeSettings(storage.read(SETTINGS_STORAGE_KEY)) ?? { ...DEFAULT_SETTINGS };
}

/** Normalise, persist, and return what was written (the caller's new truth). */
export function saveSettings(storage: SettingsStorage, settings: unknown): GatewaySettings {
  const normalized = normalizeSettings(settings);
  storage.write(SETTINGS_STORAGE_KEY, encodeSettings(normalized));
  return normalized;
}
