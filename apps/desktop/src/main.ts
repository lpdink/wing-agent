import { app, BrowserWindow, ipcMain, Menu, net, protocol, session, shell } from 'electron';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { IPC_CHANNELS, parseDesktopArgs, type DesktopAppInfo } from './bridge';
import {
  allowsIgnoringCertificate,
  allowsIgnoringCertificateHost,
  createCertificatePolicy,
  type CertificatePolicy,
} from './certificate';
import {
  configFileFor,
  loadConfig,
  mergeConfig,
  writeConfig,
  type ConfigLoadResult,
  type DesktopConfig,
} from './config';
import { GatewayLauncher, findWingExecutable, healthUrl } from './gateway/launcher';
import { buildMenuTemplate } from './menu';
import {
  WING_APP_INDEX_FILE,
  WING_APP_INDEX_URL,
  WING_APP_ORIGIN,
  WING_APP_SCHEME,
  WING_APP_SCHEME_PRIVILEGES,
  serveWebDocument,
} from './web-document';

/**
 * Electron main process (step 05 scaffold).
 *
 * Responsibilities, in the order they happen:
 *
 * 1. register the `wing-app://` scheme (before `app.ready`), take the single
 *    instance lock;
 * 2. on ready — read `<userData>/config.json`, install the certificate policy on
 *    both Electron hooks, register the protocol handler, install the menu + IPC
 *    handlers;
 * 3. either start the window (normal) or print the smoke report and exit
 *    (`--smoke`).
 *
 * This file (plus `src/preload.ts`) is the only place allowed to import Electron
 * for value — everything else is plain Node covered by unit tests.
 */

const moduleDirectory = path.dirname(fileURLToPath(import.meta.url));

const ARGS = parseDesktopArgs(process.argv.slice(1));

/** Where the shell loads the app in development: the `apps/web` Vite dev server. */
const DEV_SERVER_URL = 'http://localhost:5173';

/** Kept identical to the July branch: changing it would move the userData directory. */
const APP_ID = 'com.wing-agent.app';

/** Placeholder window chrome for the shell; the web build owns the real look. */
const WINDOW_BACKGROUND = '#0b0b0f';

let mainWindow: BrowserWindow | null = null;

/**
 * The policy the certificate hooks consult. Replaced whenever settings are written
 * (the hooks are installed once and must see the current values).
 */
let certificatePolicy: CertificatePolicy = createCertificatePolicy({});

/** Health probes go through Chromium so the session's certificate policy applies. */
const launcher = new GatewayLauncher({
  probe: async (baseUrl, timeoutMs) => {
    const controller = new AbortController();
    const timer = setTimeout(() => {
      controller.abort();
    }, timeoutMs);
    try {
      const response = await net.fetch(healthUrl(baseUrl), {
        method: 'GET',
        signal: controller.signal,
      });
      return response.ok;
    } catch {
      return false;
    } finally {
      clearTimeout(timer);
    }
  },
});

/** `<userData>/config.json` — the main process owns this file (see `src/config.ts`). */
function configFilePath(): string {
  return configFileFor(app.getPath('userData'));
}

/** The packaged static root; step 10 drops the apps/web build in here. */
function rendererDirectory(): string {
  return path.join(app.getAppPath(), 'renderer');
}

/**
 * `WING_APP_URL` wins when set (dev server, or a packaged shell pointed at one).
 * Otherwise: the Vite dev server for unpackaged runs, `wing-app://app/` once packaged.
 */
function applicationUrl(): string {
  const explicit = process.env['WING_APP_URL'];
  if (explicit !== undefined && explicit.trim() !== '') {
    return explicit.trim();
  }
  return app.isPackaged ? WING_APP_INDEX_URL : DEV_SERVER_URL;
}

/** `true` for URLs the window itself may navigate to (everything else is external). */
function isApplicationUrl(candidate: string): boolean {
  if (candidate === WING_APP_ORIGIN || candidate.startsWith(`${WING_APP_ORIGIN}/`)) {
    return true;
  }
  try {
    return new URL(candidate).origin === new URL(applicationUrl()).origin;
  } catch {
    return false;
  }
}

function openInBrowser(url: string): void {
  if (!url.startsWith('https://') && !url.startsWith('http://')) {
    console.warn(`[shell] refusing to open non-http(s) url: ${url}`);
    return;
  }
  void shell
    .openExternal(url)
    .catch((error: unknown) => console.warn(`[shell] cannot open ${url}: ${String(error)}`));
}

/** macOS `certificate-error`: allow only what the policy approves. */
function onCertificateError(
  event: Electron.Event,
  _webContents: Electron.WebContents,
  url: string,
  _error: string,
  _certificate: Electron.Certificate,
  callback: (isTrusted: boolean) => void,
): void {
  if (allowsIgnoringCertificate(url, certificatePolicy)) {
    console.warn(`[certificate] trusting ${url} (whitelisted, ignoreCertErrors on)`);
    event.preventDefault();
    callback(true);
    return;
  }
  callback(false);
}

/** Session-level hook: 0 = trust, -3 = fall back to Chromium's own verification. */
function installCertificateVerifyProc(): void {
  session.defaultSession.setCertificateVerifyProc((request, callback) => {
    // Electron hands this hook a hostname only (no port, no URL; IPv6 arrives
    // unbracketed, `::1`) — see `CertificatePolicy.hosts`.
    callback(allowsIgnoringCertificateHost(request.hostname, certificatePolicy) ? 0 : -3);
  });
}

function installApplicationMenu(): number {
  const template = buildMenuTemplate(process.platform === 'darwin');
  Menu.setApplicationMenu(template.length > 0 ? Menu.buildFromTemplate(template) : null);
  return template.length;
}

/** `wing-app://` → the packaged `renderer/` directory (never `file://`, see web-document.ts). */
function registerAppProtocol(): void {
  const root = rendererDirectory();
  protocol.handle(WING_APP_SCHEME, (request) => serveWebDocument(request, root));
}

function createMainWindow(): BrowserWindow {
  const window = new BrowserWindow({
    width: 1440,
    height: 900,
    minWidth: 520,
    show: false,
    backgroundColor: WINDOW_BACKGROUND,
    webPreferences: {
      preload: path.join(moduleDirectory, 'preload.cjs'),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      webSecurity: true,
    },
  });

  window.on('ready-to-show', () => {
    window.show();
  });
  window.on('closed', () => {
    mainWindow = null;
  });
  window.webContents.setWindowOpenHandler(({ url }) => {
    openInBrowser(url);
    return { action: 'deny' };
  });
  window.webContents.on('will-navigate', (event, url) => {
    if (isApplicationUrl(url)) {
      return;
    }
    event.preventDefault();
    openInBrowser(url);
  });

  void window.loadURL(applicationUrl()).catch((error: unknown) => {
    console.error(`[window] cannot load ${applicationUrl()}: ${String(error)}`);
  });
  mainWindow = window;
  return window;
}

function focusMainWindow(): void {
  if (mainWindow === null) {
    createMainWindow();
    return;
  }
  if (mainWindow.isMinimized()) {
    mainWindow.restore();
  }
  mainWindow.show();
  mainWindow.focus();
}

function appInfo(configPath: string): DesktopAppInfo {
  return {
    name: app.getName(),
    version: app.getVersion(),
    isPackaged: app.isPackaged,
    configPath,
  };
}

/**
 * The preload contract's IPC surface (see `src/bridge.ts`).
 *
 * Settings are re-read from disk on every call: the file is the single source of
 * truth, and a second window (step 10+) would see the same values.
 */
function installIpcHandlers(configPath: string): void {
  ipcMain.handle(IPC_CHANNELS.appInfo, () => appInfo(configPath));

  ipcMain.handle(
    IPC_CHANNELS.settingsRead,
    async (): Promise<DesktopConfig> => (await loadConfig(configPath)).config,
  );

  ipcMain.handle(IPC_CHANNELS.settingsWrite, async (_event, patch: unknown): Promise<DesktopConfig> => {
    const loaded = await loadConfig(configPath);
    const merged = mergeConfig(loaded.config, patch);
    if (merged.issues.length > 0) {
      console.warn(`[settings] ${merged.issues.join('; ')}`);
    }
    await writeConfig(configPath, merged.config);
    certificatePolicy = createCertificatePolicy(merged.config);
    return merged.config;
  });

  ipcMain.handle(IPC_CHANNELS.gatewayProbe, async (): Promise<boolean> => {
    const { gatewayBaseUrl } = (await loadConfig(configPath)).config;
    return launcher.isRunning(gatewayBaseUrl);
  });

  ipcMain.handle(IPC_CHANNELS.gatewayEnsure, async () => {
    const config = (await loadConfig(configPath)).config;
    return launcher.ensureRunning({
      baseUrl: config.gatewayBaseUrl,
      wingPath: config.wingPath,
      autoStart: config.autoStart,
    });
  });
}

function warnAboutCertificatePolicy(policy: CertificatePolicy): void {
  if (policy.invalidEntries.length > 0) {
    console.warn(`[certificate] dropped whitelist entries: ${policy.invalidEntries.join(', ')}`);
  }
  if (policy.ignoreCertErrors && policy.targets.length === 0) {
    console.warn(
      '[certificate] ignoreCertErrors is on but certificateWhitelist is empty — nothing will be allowed',
    );
  }
}

/**
 * Bring the gateway up once, in the background: the window must not wait for it
 * (a stopped gateway has to render the connection guide, not a blank screen).
 */
async function ensureGatewayAtStartup(config: DesktopConfig): Promise<void> {
  try {
    const outcome = await launcher.ensureRunning({
      baseUrl: config.gatewayBaseUrl,
      wingPath: config.wingPath,
      autoStart: config.autoStart,
    });
    if (outcome.status === 'failed') {
      console.warn(`[gateway] ${outcome.detail}`);
    } else {
      console.debug(`[gateway] ${outcome.status}`);
    }
  } catch (error) {
    console.error(`[gateway] start-up check failed: ${String(error)}`);
  }
}

/** One line of stdout, flushed before the process exits. */
async function writeLine(text: string): Promise<void> {
  await new Promise<void>((resolve) => {
    process.stdout.write(`${text}\n`, () => {
      resolve();
    });
  });
}

interface RendererProbe {
  /** `null` when the request itself failed (handler missing, scheme unregistered). */
  readonly status: number | null;
  readonly contentType: string | null;
  readonly bytes: number;
  readonly error: string | null;
}

/**
 * Fetch the app document through the real protocol handler.
 *
 * `net.fetch` goes through `protocol.handle` (custom handlers are bypassed only when
 * asked), so this is the headless proof that `wing-app://` is registered, that the
 * document server answers, and that the packaged `renderer/` (inside the asar) is
 * reachable — no window required.
 */
async function probeRendererDocument(): Promise<RendererProbe> {
  try {
    const response = await net.fetch(WING_APP_INDEX_URL);
    const body = await response.arrayBuffer();
    return {
      status: response.status,
      contentType: response.headers.get('content-type'),
      bytes: body.byteLength,
      error: null,
    };
  } catch (error) {
    return { status: null, contentType: null, bytes: 0, error: String(error) };
  }
}

/**
 * `--smoke`: report what initialization produced and exit.
 *
 * No window, no gateway start (discovery only), no single-instance lock — a
 * headless check that the shell's wiring works, used by whoever reviews this step.
 * Exit code 1 when the renderer document cannot be served: smoke is a real check.
 */
async function runSmoke(loaded: ConfigLoadResult, menuItemCount: number): Promise<void> {
  const rendererRoot = rendererDirectory();
  const rendererFile = existsSync(path.join(rendererRoot, WING_APP_INDEX_FILE));
  const rendererDocument = await probeRendererDocument();
  const report = {
    smoke: true,
    appName: app.getName(),
    appVersion: app.getVersion(),
    electron: process.versions.electron ?? null,
    node: process.versions.node,
    chrome: process.versions.chrome ?? null,
    platform: process.platform,
    arch: process.arch,
    isPackaged: app.isPackaged,
    appPath: app.getAppPath(),
    applicationUrl: applicationUrl(),
    rendererRoot,
    rendererFile,
    rendererDocument,
    configPath: loaded.path,
    configStatus: loaded.status,
    configIssues: loaded.issues,
    config: loaded.config,
    certificatePolicy,
    wingExecutable: findWingExecutable(loaded.config.wingPath),
    gatewayRunning: await launcher.isRunning(loaded.config.gatewayBaseUrl),
    menuItemCount,
  };
  await writeLine(JSON.stringify(report));
  app.exit(rendererFile && rendererDocument.status === 200 ? 0 : 1);
}

async function onReady(): Promise<void> {
  app.setAppUserModelId(APP_ID);

  const configPath = configFilePath();
  const loaded = await loadConfig(configPath);
  if (loaded.issues.length > 0) {
    console.warn(`[config] ${loaded.path}: ${loaded.issues.join('; ')}`);
  }
  certificatePolicy = createCertificatePolicy(loaded.config);
  warnAboutCertificatePolicy(certificatePolicy);

  installCertificateVerifyProc();
  registerAppProtocol();
  installIpcHandlers(configPath);
  const menuItemCount = installApplicationMenu();

  if (ARGS.smoke) {
    await runSmoke(loaded, menuItemCount);
    return;
  }

  createMainWindow();
  void ensureGatewayAtStartup(loaded.config);
}

function onFatal(error: unknown): void {
  const detail = error instanceof Error ? (error.stack ?? error.message) : String(error);
  console.error(`[main] initialization failed: ${detail}`);
  app.exit(1);
}

function bootstrap(): void {
  // Must run before `app.ready`; the scheme is what gives the window its stable origin.
  protocol.registerSchemesAsPrivileged([{ scheme: WING_APP_SCHEME, privileges: WING_APP_SCHEME_PRIVILEGES }]);

  // `--smoke` is a headless self-check: it must not fight a running instance for the lock.
  if (!ARGS.smoke && !app.requestSingleInstanceLock()) {
    app.quit();
    return;
  }

  app.on('certificate-error', onCertificateError);
  app.on('second-instance', () => {
    focusMainWindow();
  });
  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) {
      createMainWindow();
    }
  });
  app.on('window-all-closed', () => {
    if (process.platform !== 'darwin') {
      app.quit();
    }
  });

  if (ARGS.smoke && process.platform === 'darwin') {
    app.dock?.hide();
  }

  app.whenReady().then(onReady).catch(onFatal);
}

bootstrap();
