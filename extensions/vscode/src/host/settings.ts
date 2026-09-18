import * as vscode from 'vscode';

import { normalizeApiKey } from '../core';

import { log } from './log';

/**
 * Gateway connection settings (`wing.*` in `contributes.configuration`).
 *
 * Defaults mirror the backend (`libs/core/wing/default_config.py`: `127.0.0.1:32523`)
 * and the TUI's fallback. The extension deliberately does not parse
 * `~/.wing/config.yaml` — a user who changed the port there mirrors it here once
 * (see design.md Assumptions 6).
 */
export interface GatewaySettings {
  readonly host: string;
  readonly port: number;
  /** `null` = auth off (blank setting). */
  readonly apiKey: string | null;
  /** Explicit `wing` executable; `null` = discover on PATH / well-known paths. */
  readonly wingPath: string | null;
  /** Start the gateway when it is not running (once per activation). */
  readonly autoStart: boolean;
}

/** Backend default (`default_config.py` → `gateway.port`). */
export const DEFAULT_GATEWAY_HOST = '127.0.0.1';
export const DEFAULT_GATEWAY_PORT = 32523;

/** Settings section contributed in `package.json`. */
export const SETTINGS_SECTION = 'wing';

function readString(config: vscode.WorkspaceConfiguration, key: string): string | null {
  const value = config.get<unknown>(key);
  if (typeof value !== 'string') {
    return null;
  }
  const trimmed = value.trim();
  return trimmed === '' ? null : trimmed;
}

function readPort(config: vscode.WorkspaceConfiguration): number {
  const value = config.get<unknown>('port');
  if (typeof value === 'number' && Number.isInteger(value) && value > 0 && value <= 65535) {
    return value;
  }
  if (typeof value === 'string' && value.trim() !== '') {
    const parsed = Number.parseInt(value, 10);
    if (Number.isInteger(parsed) && parsed > 0 && parsed <= 65535) {
      return parsed;
    }
  }
  return DEFAULT_GATEWAY_PORT;
}

/** Read + normalize the gateway settings (never throws; invalid values fall back). */
export function readGatewaySettings(): GatewaySettings {
  const config = vscode.workspace.getConfiguration(SETTINGS_SECTION);
  const autoStart = config.get<unknown>('autoStart');
  const settings: GatewaySettings = {
    host: readString(config, 'host') ?? DEFAULT_GATEWAY_HOST,
    port: readPort(config),
    apiKey: normalizeApiKey(readString(config, 'apiKey')),
    wingPath: readString(config, 'wingPath'),
    autoStart: autoStart !== false,
  };
  log().debug(
    `[settings] gateway ${settings.host}:${settings.port} (apiKey=${settings.apiKey === null ? 'off' : 'set'}, ` +
      `autoStart=${settings.autoStart}, wingPath=${settings.wingPath ?? 'auto'})`,
  );
  return settings;
}
