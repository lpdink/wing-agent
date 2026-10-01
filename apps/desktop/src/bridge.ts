import type { DesktopConfig } from './config';
import type { LaunchOutcome } from './gateway/launcher';

/**
 * The frozen preload contract (step 05 handoff, consumed by step 10).
 *
 * `src/preload.ts` publishes exactly this object on `window[BRIDGE_KEY]` via
 * `contextBridge`, `src/main.ts` implements the handlers, and the web build
 * imports these types through the package subpath `@wing-agent/desktop/bridge`
 * instead of re-declaring them.
 *
 * Nothing here imports Electron, so the contract is testable and usable from a
 * DOM-only program.
 */

/** `window` key the bridge is published under. */
export const BRIDGE_KEY = 'wingDesktop';

export const IPC_CHANNELS = {
  /** `DesktopAppInfo`. */
  appInfo: 'wing:app-info',
  /** `DesktopConfig`. */
  settingsRead: 'wing:settings-read',
  /** `Partial<DesktopConfig>` → normalized `DesktopConfig`. */
  settingsWrite: 'wing:settings-write',
  /** `boolean` — `/api/health` reachable right now. */
  gatewayProbe: 'wing:gateway-probe',
  /** `LaunchOutcome` — probe, and start the gateway once when it is local and dead. */
  gatewayEnsure: 'wing:gateway-ensure',
} as const;

export type IpcChannel = (typeof IPC_CHANNELS)[keyof typeof IPC_CHANNELS];

/** Runtime facts the preload can answer synchronously (no IPC round-trip). */
export interface DesktopRuntimeInfo {
  readonly platform: NodeJS.Platform;
  readonly arch: string;
  readonly electron: string;
  readonly node: string;
  readonly chrome: string;
}

/** Process-level facts only the main process knows. */
export interface DesktopAppInfo {
  readonly name: string;
  readonly version: string;
  readonly isPackaged: boolean;
  readonly configPath: string;
}

export interface DesktopSettingsApi {
  /** Read the persisted config (defaults when the file is missing). */
  read(): Promise<DesktopConfig>;
  /** Merge a patch into the persisted config and return what was written. */
  write(patch: Partial<DesktopConfig>): Promise<DesktopConfig>;
}

export interface DesktopGatewayApi {
  /** `true` when the configured gateway answers `/api/health`. */
  probe(): Promise<boolean>;
  /** Probe first, then `wing start` once when the gateway is local and dead. */
  ensure(): Promise<LaunchOutcome>;
}

export interface DesktopBridge {
  readonly runtime: DesktopRuntimeInfo;
  readonly app: { info(): Promise<DesktopAppInfo> };
  readonly settings: DesktopSettingsApi;
  readonly gateway: DesktopGatewayApi;
}

export interface DesktopArgs {
  /** `--smoke`: initialize headlessly, print the JSON report, exit 0/1 without UI. */
  readonly smoke: boolean;
}

/**
 * Command-line switches the shell understands. Electron/Chromium switches
 * (`--user-data-dir`, `--remote-debugging-port`, …) are left untouched.
 */
export function parseDesktopArgs(argv: readonly string[]): DesktopArgs {
  return { smoke: argv.includes('--smoke') };
}
