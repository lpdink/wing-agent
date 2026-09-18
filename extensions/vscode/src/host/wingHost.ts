import type { ConnectionState, CoreLogger, HttpTransport, SocketFactory, WingEvent } from '../core';
import {
  DEFAULT_RECONNECT_OPTIONS,
  GatewayConnection,
  GatewayHttpClient,
  gatewayUrls,
  reconnectDelayMs,
  silentLogger,
} from '../core';

import type { GatewaySettings } from './settings';
import type { GatewayLauncher } from './gateway/launcher';
import type { BridgeSink } from './session/manager';
import { SessionManager } from './session/manager';
import type { EditorActions } from './editorActions';
import type { WebviewIntent } from './bridge';

/**
 * `WingHost` — the gateway's lifecycle and the one place a socket exists.
 *
 * It owns: settings → clients (WS + HTTP) → connect → reconnect supervision
 * callbacks → translating connection state into user-visible notices → feeding
 * every gateway event into {@link SessionManager}. Everything session-shaped
 * lives there; everything connection-shaped lives here.
 *
 * Reconnect policy (design.md D8):
 *
 * - a **first** connect failure is the host's problem: it retries on the same
 *   `min(1s·2ⁿ, 30s)` ladder core uses, because core only supervises *after* a
 *   connection has existed;
 * - once connected, core supervises; on every fresh `connected` the host hands
 *   the new `client_id` to the sessions, which resubscribe;
 * - `unauthorized` stops everything with an actionable message (credentials do
 *   not heal themselves) — no endless retry, no auto-start.
 */

export interface GatewayClients {
  readonly connection: GatewayConnection;
  readonly http: GatewayHttpClient;
}

/** Seam for tests: build the two clients for one settings snapshot. */
export interface GatewayFactory {
  create(settings: GatewaySettings): GatewayClients;
}

export interface GatewayFactoryOptions {
  readonly socketFactory?: SocketFactory;
  readonly httpTransport?: HttpTransport;
  readonly logger?: CoreLogger;
  readonly now?: () => number;
}

/** Production client construction (native WebSocket + fetch transports). */
export function createGatewayClients(
  settings: GatewaySettings,
  options: GatewayFactoryOptions = {},
): GatewayClients {
  const urls = gatewayUrls({ host: settings.host, port: settings.port, apiKey: settings.apiKey });
  const connection = new GatewayConnection({
    wsUrl: urls.wsUrl,
    ...(options.socketFactory === undefined ? {} : { socketFactory: options.socketFactory }),
    ...(options.logger === undefined ? {} : { logger: options.logger }),
    ...(options.now === undefined ? {} : { now: options.now }),
  });
  const http = new GatewayHttpClient({
    baseUrl: urls.httpBaseUrl,
    apiKey: settings.apiKey,
    ...(options.httpTransport === undefined ? {} : { transport: options.httpTransport }),
    ...(options.logger === undefined ? {} : { logger: options.logger }),
  });
  return { connection, http };
}

export interface WingHostOptions {
  /** The webview sink (attached while a view is resolved). */
  readonly sink: () => BridgeSink | null;
  readonly settings: () => GatewaySettings;
  readonly gatewayFactory: GatewayFactory;
  readonly launcher: GatewayLauncher;
  readonly workspaceFolder: () => string | null;
  readonly editor: EditorActions;
  readonly reportError: (message: string) => void;
  readonly now?: () => number;
  readonly logger?: CoreLogger;
  /** Base delay of the host-owned connect ladder (tests use tiny values). */
  readonly connectRetryBaseMs?: number;
}

export class WingHost {
  private readonly sessions: SessionManager;
  private readonly logger: CoreLogger;
  private readonly now: () => number;

  private clients: GatewayClients | null = null;
  private unsubscribeState: (() => void) | null = null;
  private unsubscribeEvents: (() => void) | null = null;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private attempts = 0;
  private autoStartAttempted = false;
  private disposed = false;
  private sinkRef: BridgeSink | null = null;

  constructor(private readonly options: WingHostOptions) {
    this.logger = options.logger ?? silentLogger;
    this.now = options.now ?? Date.now;
    this.sessions = new SessionManager({
      sink: () => this.sinkRef,
      connection: () => this.clients?.connection ?? null,
      http: () => this.clients?.http ?? null,
      workspaceFolder: options.workspaceFolder,
      editor: options.editor,
      reportError: options.reportError,
      now: this.now,
      logger: this.logger,
    });
  }

  // ── webview attachment ──────────────────────────────────────────────

  /** The chat view resolved: route session messages here. */
  attachSink(sink: BridgeSink): void {
    this.sinkRef = sink;
  }

  /** The chat view went away (re-resolved elsewhere or disposed). */
  detachSink(): void {
    this.sinkRef = null;
  }

  get hasSink(): boolean {
    return this.sinkRef !== null;
  }

  // ── bridge delegation (the provider talks to this) ──────────────────

  onReady(protocolVersion: number): void {
    this.sessions.onReady(protocolVersion);
  }

  onResync(sessionId: string): void {
    this.sessions.onResync(sessionId);
  }

  async onIntent(intent: WebviewIntent): Promise<void> {
    await this.sessions.onIntent(intent);
  }

  // ── lifecycle ───────────────────────────────────────────────────────

  /** Activation entry point: probe → (maybe) start → connect. */
  async start(): Promise<void> {
    if (this.disposed) {
      return;
    }
    await this.ensureRunningOnce();
    await this.connectOnce();
  }

  /**
   * Manual recovery (`wing.reconnectGateway`, or after fixing the settings).
   *
   * Resets the auto-start budget: the user explicitly asked, so probing and —
   * once — starting the gateway is exactly what they want.
   */
  async reconnect(): Promise<void> {
    if (this.disposed) {
      return;
    }
    this.closeClients();
    this.cancelRetry();
    this.attempts = 0;
    this.autoStartAttempted = false;
    await this.ensureRunningOnce();
    await this.connectOnce();
  }

  /** Create a session now (the `+` path when automation did not run yet). */
  async newSession(): Promise<string | null> {
    return this.sessions.newSession();
  }

  get connectionState(): ConnectionState | null {
    return this.clients?.connection.state ?? null;
  }

  /** The session layer (commands and tests reach sessions through the host). */
  get sessionManager(): SessionManager {
    return this.sessions;
  }

  dispose(): void {
    this.disposed = true;
    this.cancelRetry();
    this.closeClients();
    this.sessions.dispose();
    this.sinkRef = null;
  }

  // ── internals ───────────────────────────────────────────────────────

  private async ensureRunningOnce(): Promise<void> {
    if (this.autoStartAttempted) {
      return;
    }
    this.autoStartAttempted = true;
    const settings = this.options.settings();
    const outcome = await this.options.launcher.ensureRunning(settings);
    switch (outcome.status) {
      case 'already-running':
        this.logger.debug('gateway is already running');
        return;
      case 'started':
        this.logger.debug('started the gateway');
        return;
      case 'disabled':
        this.sessions.setGlobalNotice({
          level: 'info',
          text: 'Wing gateway is not running. Start it with `wing start`, or enable `wing.autoStart`.',
        });
        return;
      case 'not-found':
        this.sessions.setGlobalNotice({
          level: 'error',
          text: 'Could not find the `wing` executable — set `wing.wingPath` in Settings.',
        });
        this.options.reportError(
          'Wing: could not find the `wing` executable. Set `wing.wingPath` in Settings to its full path.',
        );
        return;
      case 'failed':
        this.sessions.setGlobalNotice({
          level: 'warning',
          text: `Could not start the gateway: ${outcome.detail}`,
        });
        this.options.reportError(`Wing: could not start the gateway — ${outcome.detail}`);
        return;
    }
  }

  private async connectOnce(): Promise<void> {
    if (this.disposed) {
      return;
    }
    const settings = this.options.settings();
    let clients: GatewayClients;
    try {
      clients = this.options.gatewayFactory.create(settings);
    } catch (error) {
      this.reportConnectFailure(error);
      return;
    }
    this.closeClients();
    this.clients = clients;
    this.unsubscribeState = clients.connection.onStateChange((state) => {
      this.handleStateChange(state);
    });
    this.unsubscribeEvents = clients.connection.onEvent((event) => {
      this.handleEvent(event);
    });
    try {
      await clients.connection.connect();
      this.attempts = 0;
    } catch (error) {
      // First-connect failures are ours to retry (core supervises only after a
      // successful connect). `reconnect`/`start` never throw either.
      this.reportConnectFailure(error);
      this.scheduleRetry();
    }
  }

  private reportConnectFailure(error: unknown): void {
    const detail = error instanceof Error ? error.message : String(error);
    this.logger.warn(`gateway connect failed: ${detail}`);
    this.sessions.setGlobalNotice({
      level: 'warning',
      text: `Wing gateway is not reachable (${detail}). Is it running? Check \`wing start\`.`,
    });
  }

  private scheduleRetry(): void {
    if (this.disposed) {
      return;
    }
    const delay = reconnectDelayMs(this.attempts, {
      ...DEFAULT_RECONNECT_OPTIONS,
      ...(this.options.connectRetryBaseMs === undefined
        ? {}
        : { baseDelayMs: this.options.connectRetryBaseMs }),
    });
    this.attempts += 1;
    this.cancelRetry();
    this.logger.debug(`retrying the gateway connection in ${delay} ms (attempt ${this.attempts})`);
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      void this.connectOnce();
    }, delay);
  }

  private cancelRetry(): void {
    if (this.retryTimer !== null) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
  }

  private closeClients(): void {
    this.unsubscribeState?.();
    this.unsubscribeEvents?.();
    this.unsubscribeState = null;
    this.unsubscribeEvents = null;
    const clients = this.clients;
    this.clients = null;
    clients?.connection.close();
  }

  private handleStateChange(state: ConnectionState): void {
    switch (state.status) {
      case 'connected':
        this.attempts = 0;
        this.sessions.setGlobalNotice(null);
        void this.sessions.onConnected();
        return;
      case 'reconnecting':
        // Re-subscribing happens on `connected` only: during the retry window
        // `clientId` is already null, so an attempt here could only no-op.
        this.sessions.onDisconnected();
        this.sessions.setGlobalNotice({
          level: 'warning',
          text: 'Gateway connection lost — reconnecting…',
        });
        return;
      case 'closed': {
        this.sessions.onDisconnected();
        const error = state.lastError;
        if (error?.kind === 'unauthorized') {
          this.sessions.setGlobalNotice({
            level: 'error',
            text: 'The gateway rejected our API key — check the `wing.apiKey` setting.',
          });
          this.options.reportError(
            'Wing: the gateway rejected the API key. Check the `wing.apiKey` setting.',
          );
          return;
        }
        if (error !== null) {
          this.sessions.setGlobalNotice({
            level: 'warning',
            text: `Gateway connection closed (${error.message}).`,
          });
        }
        return;
      }
      case 'connecting':
      case 'idle':
        return;
    }
  }

  private handleEvent(event: WingEvent): void {
    this.sessions.handleEvent(event);
  }
}
