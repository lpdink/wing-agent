import { contextBridge, ipcRenderer } from 'electron';

import { BRIDGE_KEY, IPC_CHANNELS, type DesktopAppInfo, type DesktopBridge } from './bridge';
import type { DesktopConfig } from './config';
import type { LaunchOutcome } from './gateway/launcher';

/**
 * Preload script — the renderer's only door to the main process.
 *
 * Built as CommonJS (`dist/preload.cjs`) on purpose: ESM preload scripts require
 * `sandbox: false`, and the shell keeps `sandbox: true` (see `src/main.ts`).
 *
 * The exposed surface is exactly `DesktopBridge` (`src/bridge.ts`) — no
 * `ipcRenderer`, no module access, no node globals. `window` gets capabilities,
 * not privileges.
 */

const bridge: DesktopBridge = {
  runtime: {
    platform: process.platform,
    arch: process.arch,
    electron: process.versions.electron ?? '',
    node: process.versions.node,
    chrome: process.versions.chrome ?? '',
  },
  app: {
    info: () => ipcRenderer.invoke(IPC_CHANNELS.appInfo) as Promise<DesktopAppInfo>,
  },
  settings: {
    read: () => ipcRenderer.invoke(IPC_CHANNELS.settingsRead) as Promise<DesktopConfig>,
    write: (patch) => ipcRenderer.invoke(IPC_CHANNELS.settingsWrite, patch) as Promise<DesktopConfig>,
  },
  gateway: {
    probe: () => ipcRenderer.invoke(IPC_CHANNELS.gatewayProbe) as Promise<boolean>,
    ensure: () => ipcRenderer.invoke(IPC_CHANNELS.gatewayEnsure) as Promise<LaunchOutcome>,
  },
};

contextBridge.exposeInMainWorld(BRIDGE_KEY, bridge);
