import { mkdir, readFile, rename, unlink, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { normalizeCertificateEntry } from './certificate';

/**
 * The desktop shell's configuration file.
 *
 * It lives in `app.getPath('userData')` (see `configFileFor`) and is written
 * atomically with `0600` — it may hold an API key. The main process is the only
 * reader/writer: the renderer asks over IPC (see `src/bridge.ts`), so a page that
 * has not loaded yet can neither block nor change the certificate policy.
 *
 * Everything here is plain Node: no Electron import, so `vitest` covers the
 * normalization matrix directly.
 */

export interface DesktopConfig {
  /** Gateway origin, e.g. `http://127.0.0.1:32523` or `https://wing.example.com`. No trailing slash. */
  readonly gatewayBaseUrl: string;
  /** `null` = the gateway has no auth configured. */
  readonly apiKey: string | null;
  /** Accept self-signed / invalid certificates — only for the whitelisted targets below. */
  readonly ignoreCertErrors: boolean;
  /** `host:port` entries (see `src/certificate.ts`); required for `ignoreCertErrors` to allow anything. */
  readonly certificateWhitelist: readonly string[];
  /** Explicit `wing` executable; `null` = discover on well-known paths / `PATH`. */
  readonly wingPath: string | null;
  /** Run `wing start` once when the (local) gateway is not answering. */
  readonly autoStart: boolean;
}

/** `libs/core/wing/default_config.py` → `gateway.host` + `gateway.port` (same default as the TUI / VS Code). */
export const DEFAULT_GATEWAY_BASE_URL = 'http://127.0.0.1:32523';

export const CONFIG_FILE_NAME = 'config.json';

export const DEFAULT_CONFIG: DesktopConfig = Object.freeze({
  gatewayBaseUrl: DEFAULT_GATEWAY_BASE_URL,
  apiKey: null,
  ignoreCertErrors: false,
  certificateWhitelist: Object.freeze([]),
  wingPath: null,
  autoStart: true,
});

/** `true` when `value` is a JSON object (not null, not an array). */
function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Read errors that mean "first run", not "broken configuration". */
function isMissingFileError(error: unknown): boolean {
  return (error as NodeJS.ErrnoException | null)?.code === 'ENOENT';
}

/** Drop trailing slashes so `baseUrl + '/api/health'` never doubles one. */
function stripTrailingSlash(value: string): string {
  return value.replace(/\/+$/u, '');
}

function normalizeGatewayBaseUrl(value: unknown, issues: string[]): string {
  if (value === undefined) {
    return DEFAULT_GATEWAY_BASE_URL;
  }
  if (typeof value !== 'string' || value.trim() === '') {
    issues.push('gatewayBaseUrl must be a non-empty string');
    return DEFAULT_GATEWAY_BASE_URL;
  }
  const trimmed = value.trim();
  try {
    const url = new URL(trimmed);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') {
      throw new Error(`unsupported protocol ${url.protocol}`);
    }
    if (url.hostname === '') {
      throw new Error('missing host');
    }
    return stripTrailingSlash(url.toString());
  } catch (error) {
    issues.push(`gatewayBaseUrl "${trimmed}" is not an absolute http(s) URL (${String(error)})`);
    return DEFAULT_GATEWAY_BASE_URL;
  }
}

function normalizeApiKey(value: unknown, issues: string[]): string | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== 'string') {
    issues.push('apiKey must be a string or null');
    return null;
  }
  const trimmed = value.trim();
  return trimmed === '' ? null : trimmed;
}

function normalizeWingPath(value: unknown, issues: string[]): string | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== 'string') {
    issues.push('wingPath must be a string or null');
    return null;
  }
  const trimmed = value.trim();
  return trimmed === '' ? null : trimmed;
}

function normalizeBoolean(value: unknown, fallback: boolean, key: string, issues: string[]): boolean {
  if (value === undefined) {
    return fallback;
  }
  if (typeof value !== 'boolean') {
    issues.push(`${key} must be a boolean`);
    return fallback;
  }
  return value;
}

function normalizeWhitelist(value: unknown, issues: string[]): string[] {
  if (value === undefined) {
    return [];
  }
  if (!Array.isArray(value)) {
    issues.push('certificateWhitelist must be an array of "host:port" strings');
    return [];
  }
  const entries: string[] = [];
  for (const raw of value) {
    const normalized = typeof raw === 'string' ? normalizeCertificateEntry(raw) : null;
    if (normalized === null) {
      issues.push(`certificateWhitelist entry ${JSON.stringify(raw)} is not a "host:port" target`);
      continue;
    }
    if (!entries.includes(normalized)) {
      entries.push(normalized);
    }
  }
  return entries;
}

/**
 * Turn arbitrary JSON into a usable config: **never throws**, every bad value
 * falls back to its default and lands in `issues` (the shell logs them, the
 * `--smoke` report carries them).
 */
export function normalizeConfig(raw: unknown): { config: DesktopConfig; issues: string[] } {
  const issues: string[] = [];
  if (!isPlainObject(raw)) {
    return { config: DEFAULT_CONFIG, issues: ['configuration must be a JSON object'] };
  }
  const config: DesktopConfig = {
    gatewayBaseUrl: normalizeGatewayBaseUrl(raw['gatewayBaseUrl'], issues),
    apiKey: normalizeApiKey(raw['apiKey'], issues),
    ignoreCertErrors: normalizeBoolean(raw['ignoreCertErrors'], false, 'ignoreCertErrors', issues),
    certificateWhitelist: normalizeWhitelist(raw['certificateWhitelist'], issues),
    wingPath: normalizeWingPath(raw['wingPath'], issues),
    autoStart: normalizeBoolean(raw['autoStart'], true, 'autoStart', issues),
  };
  return { config, issues };
}

/** Apply a partial update (`settings.write` over IPC) on top of the current config. */
export function mergeConfig(
  base: DesktopConfig,
  patch: unknown,
): { config: DesktopConfig; issues: string[] } {
  if (!isPlainObject(patch)) {
    return { config: normalizeConfig(base).config, issues: ['the settings patch must be a JSON object'] };
  }
  return normalizeConfig({ ...base, ...patch });
}

/** `<userData>/config.json`. */
export function configFileFor(userDataDirectory: string): string {
  return path.join(userDataDirectory, CONFIG_FILE_NAME);
}

/** Stable key order + trailing newline: the file is meant to be human-edited and diffed. */
export function serializeConfig(config: DesktopConfig): string {
  const ordered: DesktopConfig = {
    gatewayBaseUrl: config.gatewayBaseUrl,
    apiKey: config.apiKey,
    ignoreCertErrors: config.ignoreCertErrors,
    certificateWhitelist: [...config.certificateWhitelist],
    wingPath: config.wingPath,
    autoStart: config.autoStart,
  };
  return `${JSON.stringify(ordered, null, 2)}\n`;
}

export type ConfigStatus =
  /** The file was read and parsed (fields may still have been repaired — see `issues`). */
  | 'loaded'
  /** No file yet: this is a first run, not a problem. */
  | 'missing'
  /** Unreadable, not JSON, or not a JSON object — defaults are in effect. */
  | 'invalid';

export interface ConfigLoadResult {
  readonly config: DesktopConfig;
  readonly path: string;
  readonly status: ConfigStatus;
  readonly issues: readonly string[];
}

/** Read + normalize; never throws (a broken file degrades to defaults). */
export async function loadConfig(file: string): Promise<ConfigLoadResult> {
  let text: string;
  try {
    text = await readFile(file, 'utf8');
  } catch (error) {
    if (isMissingFileError(error)) {
      return { config: DEFAULT_CONFIG, path: file, status: 'missing', issues: [] };
    }
    return {
      config: DEFAULT_CONFIG,
      path: file,
      status: 'invalid',
      issues: [`cannot read ${file}: ${String(error)}`],
    };
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (error) {
    return {
      config: DEFAULT_CONFIG,
      path: file,
      status: 'invalid',
      issues: [`${file} is not valid JSON: ${String(error)}`],
    };
  }
  if (!isPlainObject(parsed)) {
    return {
      config: DEFAULT_CONFIG,
      path: file,
      status: 'invalid',
      issues: [`${file} must contain a JSON object`],
    };
  }

  const { config, issues } = normalizeConfig(parsed);
  return { config, path: file, status: 'loaded', issues };
}

/**
 * Write `config` atomically (`tmp` + `rename`, mirroring
 * `libs/core/wing/common/fs.py`), mode `0600` because of the API key.
 */
export async function writeConfig(file: string, config: DesktopConfig): Promise<void> {
  const text = serializeConfig(normalizeConfig(config).config);
  await mkdir(path.dirname(file), { recursive: true });
  const temporary = `${file}.${process.pid}.tmp`;
  try {
    await writeFile(temporary, text, { encoding: 'utf8', mode: 0o600 });
    await rename(temporary, file);
  } catch (error) {
    await unlink(temporary).catch(() => undefined);
    throw error;
  }
}
