/**
 * `GatewayRuntime` — the web shell's connection and session control plane.
 *
 * One object owns: the settings → clients wiring, the connect/reconnect
 * supervision, the subscription of the one open session, the reduction of gateway
 * events into its `SessionRecord`, and the polled session list. React only reads
 * it (`useSyncExternalStore(runtime.subscribe, runtime.getSnapshot)`); everything
 * framework-free lives here so it can be unit-tested in node with an injected
 * socket factory / HTTP transport.
 *
 * It is the port of `extensions/vscode/src/host/{wingHost,session/manager}.ts`
 * to a single-view, single-process shell (design.md D4/D5/D6):
 *
 * - **first connect is ours to retry** (`min(1s·2ⁿ, 30s)`), because
 *   `GatewayConnection` only supervises *after* a successful connect;
 * - **once connected, the client supervises**; every fresh `connected` carries a
 *   new `clientId` (the gateway assigns one per socket) and we re-subscribe;
 * - **resubscribing means replaying**: `POST /api/session/subscribe` answers by
 *   pushing `sync_session`, and `applySync` rebuilds the record from scratch — so
 *   "reconnect → resubscribe → replay" cannot duplicate a cell. That is why the
 *   record survives a reconnect instead of being recreated;
 * - **`unauthorized` stops everything**: a wrong API key does not heal itself;
 * - `subscribe` answered with 404 means the gateway restarted and lost its
 *   in-memory session → `resume`, then subscribe again (then give up loudly).
 *
 * Auth note (design.md D7): the *browser* WebSocket constructor cannot set
 * handshake headers, so the key rides in the query string
 * (`wsUrlWithApiKey`) — the client package passes `headers` to the socket
 * implementation when given, and a DOM `WebSocket` would receive that object as
 * its protocols argument. HTTP keeps the `Authorization` header. Every log line
 * and error goes through `redactUrl`.
 */

import {
  DEFAULT_RECONNECT_OPTIONS,
  GatewayConnection,
  GatewayHttpClient,
  GatewayHttpError,
  GatewaySocketError,
  createClientRequest,
  isKnownEvent,
  reconnectDelayMs,
  silentLogger,
  wsUrlWithApiKey,
  type ClientRequest,
  type CoreLogger,
  type ConnectionState,
  type HttpTransport,
  type SessionInfo,
  type SocketFactory,
  type WingEvent,
} from '@wing-agent/client';
import {
  SerialQueue,
  SessionRecord,
  applyLive,
  applySync,
  branchRow,
  buildAskReply,
  type AskAnswerModel,
  type AskCellModel,
  type ReductionEffect,
} from '@wing-agent/session';

import { Notifier } from '../lib/observable';
import { type GatewaySettings, normalizeSettings } from '../settings/settings';
import { GatewayAddressError, type PageLocation, gatewayEndpoints } from '../settings/urls';
import { type SessionRow, type SessionRowOverlay, buildSessionRows } from '../sessions/rows';

/** Coarse connection state the UI renders a banner from. */
export type ConnectionPhase =
  /** Dialling for the first time (or retrying a first connect that failed). */
  | 'connecting'
  | 'connected'
  /** A live connection was lost and the client is retrying (or we are). */
  | 'reconnecting'
  /** Nothing is running: stopped, or the gateway rejected our credentials. */
  | 'offline';

export interface ConnectionView {
  readonly phase: ConnectionPhase;
  /** Identity of the current socket; `null` unless connected. */
  readonly clientId: string | null;
  /** `false` until the first successful handshake — the first-connect guide keys off this. */
  readonly everConnected: boolean;
  /** Retry attempt (0 = the first retry after a failure). */
  readonly attempt: number;
  /** Remaining delay of the pending retry, live-counted down for the banner. */
  readonly reconnectInMs: number | null;
  /** Human-readable cause of the last failure (redacted), `null` when healthy. */
  readonly lastError: string | null;
  /** The address in use (`http://… (this page)` for the same-origin mode). */
  readonly address: string;
  /** The gateway rejected the API key (or the role) — retrying is pointless. */
  readonly unauthorized: boolean;
}

export interface Notice {
  readonly id: number;
  readonly level: 'info' | 'warning' | 'error';
  readonly text: string;
}

/** How an ask reply ended (step 08's Ask wiring; tests and step 09 read it). */
export type AskReplyOutcome =
  /** The frame left the socket and the cell is marked answered. */
  | 'sent'
  /** There is nothing to answer (the ask is gone / already handled). */
  | 'gone'
  /** The reply encoded to nothing — the renderer disabled the button in this case. */
  | 'empty'
  /** The socket is down; the user was told. */
  | 'not-connected';

/** The immutable snapshot React renders. Rebuilt (new identity) on every change. */
export interface RuntimeSnapshot {
  readonly connection: ConnectionView;
  readonly settings: GatewaySettings;
  readonly sessions: readonly SessionRow[];
  /** Why the last list refresh failed; the rows shown are the last good ones. */
  readonly listError: string | null;
  readonly activeSessionId: string | null;
  /**
   * The open session's model — the **seam for step 08's transcript**.
   *
   * It is a live (mutable) object: read `cells` whenever {@link recordVersion}
   * changed, never earlier and never partially.
   */
  readonly record: SessionRecord | null;
  /** Bumped once per reduced event batch; the transcript's memo key. */
  readonly recordVersion: number;
  readonly notices: readonly Notice[];
}

export interface GatewayRuntimeOptions {
  readonly initialSettings: GatewaySettings;
  /** Called after `updateSettings` normalised a new value (persist it here). */
  readonly onSettingsChange?: (settings: GatewaySettings) => void;
  /** The page origin a same-origin setting resolves against. */
  readonly location: PageLocation;
  /** Socket seam (tests); defaults to the platform `WebSocket`. */
  readonly socketFactory?: SocketFactory;
  /** HTTP seam (tests); defaults to a `fetch`-backed transport. */
  readonly httpTransport?: HttpTransport;
  readonly now?: () => number;
  readonly logger?: CoreLogger;
  /** Base delay of the host-owned first-connect ladder (tests use tiny values). */
  readonly connectRetryBaseMs?: number;
  /** How often the session list is refreshed; `0` disables polling. */
  readonly listPollIntervalMs?: number;
  /**
   * Skip a poll tick when the page is hidden, and mark a turn that finished while
   * nobody was looking. The runtime is DOM-free, so the entry point passes the
   * browser's own `document.visibilityState` (`src/main.tsx`); tests drive it
   * directly, and the default is "visible".
   */
  readonly isPageVisible?: () => boolean;
  /** Auto-dismiss notices after this long; `0` keeps them (tests). */
  readonly noticeTtlMs?: number;
  /** How many visited sessions keep their record in memory (LRU). */
  readonly maxCachedRecords?: number;
}

export const DEFAULT_LIST_POLL_MS = 5_000;
export const DEFAULT_NOTICE_TTL_MS = 8_000;
export const DEFAULT_MAX_CACHED_RECORDS = 5;

interface GatewayClients {
  readonly connection: GatewayConnection;
  readonly http: GatewayHttpClient;
}

interface ManagedSession {
  readonly record: SessionRecord;
  retryAttempt: number;
  retryTimer: ReturnType<typeof setTimeout> | null;
  gone: boolean;
}

const GONE_MESSAGE =
  'This session no longer exists on the gateway. Start a new session, or pick another one.';

export class GatewayRuntime {
  private readonly options: GatewayRuntimeOptions;
  private readonly notifier = new Notifier();
  private readonly logger: CoreLogger;
  private readonly now: () => number;
  private readonly structure = new SerialQueue();
  private readonly records = new Map<string, SessionRecord>();
  /** LRU order of `records` (oldest first); the open session is kept last. */
  private readonly recordOrder: string[] = [];

  /** The page origin same-origin settings resolve against (step 09's dialog). */
  readonly location: PageLocation;

  private settings: GatewaySettings;
  private snapshot: RuntimeSnapshot;

  private clients: GatewayClients | null = null;
  private unsubscribeState: (() => void) | null = null;
  private unsubscribeEvents: (() => void) | null = null;

  private phase: ConnectionPhase = 'offline';
  private everConnected = false;
  private lastError: string | null = null;
  private unauthorized = false;
  private addressLabel = '';
  private connectAttempts = 0;
  private connectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectDeadlineMs: number | null = null;
  private countdownTimer: ReturnType<typeof setInterval> | null = null;
  private closing = false;
  private stopped = true;

  private active: ManagedSession | null = null;
  private recordVersion = 0;
  /**
   * Bumped whenever a reduced event can change the session's *state* (meta /
   * controls). An HTTP read that started before the bump describes an older session
   * and is discarded instead of applied (see `refreshRuntimeState`).
   */
  private metaEpoch = 0;

  private sessionList: readonly SessionInfo[] = [];
  private listError: string | null = null;
  private listTimer: ReturnType<typeof setInterval> | null = null;
  private autoActivateAttempted = false;

  private notices: Notice[] = [];
  private noticeSeq = 0;
  private noticeTimers = new Set<ReturnType<typeof setTimeout>>();

  constructor(options: GatewayRuntimeOptions) {
    this.options = options;
    this.location = options.location;
    this.settings = normalizeSettings(options.initialSettings);
    this.logger = options.logger ?? silentLogger;
    this.now = options.now ?? Date.now;
    this.addressLabel = this.settings.host === '' ? this.options.location.origin : this.settingsLabel();
    this.snapshot = this.buildSnapshot();
  }

  // ── reads ───────────────────────────────────────────────────────────

  /** `useSyncExternalStore` subscribe half. */
  subscribe = (listener: () => void): (() => void) => this.notifier.subscribe(listener);

  /** `useSyncExternalStore` snapshot half — stable until something changed. */
  getSnapshot = (): RuntimeSnapshot => this.snapshot;

  get settingsSnapshot(): GatewaySettings {
    return this.settings;
  }

  /**
   * The live socket (step 09's composer sends through it).
   *
   * `null` while disconnected. The runtime owns the connection's lifecycle; the
   * control plane owns what to send (`createClientRequest` / `encodeClientRequest`
   * from `@wing-agent/client`).
   */
  get connection(): GatewayConnection | null {
    return this.clients?.connection ?? null;
  }

  // ── outbound frames (ask replies now; the composer in step 09) ──────

  /**
   * Send one `ClientRequest` frame on the live socket.
   *
   * `false` when there is nothing to send it on (or the socket refused it), which is
   * the only thing a caller can act on — the frame itself is the caller's business
   * (`createClientRequest` + `encodeClientRequest` live in `@wing-agent/client`, and
   * `replyAsk` below is the one user of this method today).
   */
  sendClientRequest(frame: ClientRequest): boolean {
    const connection = this.clients?.connection ?? null;
    if (connection === null || connection.state.status !== 'connected') {
      return false;
    }
    try {
      connection.send(frame);
      return true;
    } catch (error) {
      this.logger.warn('could not send to the gateway', error);
      return false;
    }
  }

  /**
   * Answer the awaiting ask with `requestId` (an `AskUserQuestion`, or the legacy
   * single question) — the `answerAsk` intent of the renderer, end to end.
   *
   * Mirrors `extensions/vscode/src/host/session/manager.ts::answerAsk`: the cell must
   * still be awaiting (answering something the backend already moved past would put a
   * stale reply on the wire), the content is `buildAskReply`'s encoding of the
   * choices, and the reply travels as a `ClientRequest` addressed by
   * `tool_call_id = requestId` (that is how the gateway resolves the waiter).
   *
   * The cell flips to `answered` locally right after a successful send, exactly like
   * the extension: the backend echoes the authoritative state through its own events,
   * and a lost socket is reported by the notice rather than by a stuck spinner.
   */
  answerAsk(requestId: string, answers: readonly AskAnswerModel[]): AskReplyOutcome {
    const cell = this.awaitingAsk(requestId);
    if (cell === null) {
      this.pushNotice('warning', 'That question is no longer waiting for an answer.');
      return 'gone';
    }
    const content = buildAskReply({ approval: cell.approval, questions: cell.questions }, answers);
    return this.replyAsk(cell, content, answers);
  }

  /** Approve / deny a dangerous-command confirmation (`y` / `n`, the backend's words). */
  approveTool(requestId: string, decision: 'approve' | 'deny'): AskReplyOutcome {
    const cell = this.awaitingAsk(requestId);
    if (cell === null) {
      this.pushNotice('warning', 'That approval is no longer pending.');
      return 'gone';
    }
    const label = decision === 'approve' ? 'y' : 'n';
    const answers: AskAnswerModel[] = [
      { questionId: cell.questions[0]?.id ?? 'choice', selected: [label], text: '' },
    ];
    return this.replyAsk(cell, label, answers);
  }

  /** The awaiting ask cell for `requestId`, or `null` (same guard as the extension). */
  private awaitingAsk(requestId: string): AskCellModel | null {
    const record = this.active?.record ?? null;
    if (record === null) {
      return null;
    }
    const cellId = record.awaitingAsks.get(requestId);
    const cell = cellId === undefined ? undefined : record.cellById(cellId);
    if (cell === undefined || cell.kind !== 'ask' || cell.state !== 'awaiting') {
      return null;
    }
    return cell;
  }

  private replyAsk(cell: AskCellModel, content: string, answers: readonly AskAnswerModel[]): AskReplyOutcome {
    const record = this.active?.record ?? null;
    if (record === null) {
      return 'gone';
    }
    if (content === '') {
      this.pushNotice('warning', 'Nothing to send — pick an answer first.');
      return 'empty';
    }
    const frame = createClientRequest({
      sessionId: record.sessionId,
      content,
      toolCallId: cell.requestId,
    });
    if (!this.sendClientRequest(frame)) {
      this.pushNotice('warning', 'Not sent — the gateway is not connected.');
      return 'not-connected';
    }
    record.resolveAsk(cell.requestId);
    record.update({ ...cell, state: 'answered', answers: [...answers] });
    record.refreshStatus();
    this.flush(record);
    return 'sent';
  }

  // ── lifecycle ───────────────────────────────────────────────────────

  /** Connect (idempotent; a second call while running is a no-op). */
  start(): void {
    if (!this.stopped) {
      return;
    }
    this.stopped = false;
    this.logger.debug(`connecting to the gateway at ${this.addressLabel}`);
    void this.connectOnce();
  }

  /** Stop everything; the records are kept (a later `start()` resumes them). */
  stop(): void {
    if (this.stopped) {
      return;
    }
    this.stopped = true;
    this.closing = true;
    this.cancelConnectRetry();
    this.stopListPolling();
    this.stopCountdown();
    if (this.active !== null) {
      this.cancelResubscribe(this.active);
    }
    this.closeClients();
    this.closing = false;
    this.phase = 'offline';
    this.notify();
  }

  /** Manual recovery: tear the clients down and dial again with the same settings. */
  reconnect(): void {
    this.cancelConnectRetry();
    this.closing = true;
    this.closeClients();
    this.closing = false;
    this.connectAttempts = 0;
    this.autoActivateAttempted = false;
    this.unauthorized = false;
    this.lastError = null;
    if (!this.stopped) {
      this.phase = 'connecting';
      this.notify();
      void this.connectOnce();
    }
  }

  /** Apply new settings (already normalised), persist them, and reconnect. */
  updateSettings(next: unknown): GatewaySettings {
    this.settings = normalizeSettings(next);
    this.addressLabel = this.settings.host === '' ? this.options.location.origin : this.settingsLabel();
    this.options.onSettingsChange?.(this.settings);
    this.reconnect();
    this.notify();
    return this.settings;
  }

  // ── session operations ──────────────────────────────────────────────

  /** `+` — create a session on the gateway and open it. */
  async newSession(): Promise<string | null> {
    return this.structure.run(async () => {
      const http = this.clients?.http ?? null;
      if (http === null) {
        this.pushNotice('warning', 'Not connected — the gateway is not available.');
        return null;
      }
      try {
        // `workspace: null` = the gateway's default working directory; the web
        // client has no "open folder" concept (design.md assumption 2).
        const response = await http.createSession({ workspace: null });
        const record = new SessionRecord({
          sessionId: response.session_id,
          now: this.now,
          workspace: response.workspace ?? null,
          createdAt: '',
        });
        // Drop the previous session's route first: the web client shows one session
        // at a time, and a leftover subscription keeps that session pinned in the
        // gateway's memory (its reaper treats "has subscribers" as busy) while its
        // events are parsed and thrown away (review r1 S1).
        await this.releaseActive();
        this.adopt(record);
        await this.subscribeActive();
        void this.refreshSessions();
        return record.sessionId;
      } catch (error) {
        this.reportFailure('Could not create a session', error);
        return null;
      }
    });
  }

  /**
   * Open (or re-focus) a session: unsubscribe the previous one, `resume` when the
   * gateway does not know the id, then subscribe — which pushes the replay.
   */
  async activate(sessionId: string): Promise<void> {
    return this.structure.run(async () => {
      if (this.active?.record.sessionId === sessionId) {
        this.active.record.setAttention('none');
        this.notify();
        return;
      }
      const http = this.clients?.http ?? null;
      if (http === null) {
        this.pushNotice('warning', 'Not connected — the gateway is not available.');
        return;
      }
      let record = this.records.get(sessionId);
      if (record === undefined) {
        try {
          const resumed = await http.resumeSession(sessionId);
          record = new SessionRecord({
            sessionId: resumed.session_id,
            now: this.now,
            workspace: resumed.workspace,
            createdAt: '',
          });
        } catch (error) {
          // The open session stays open: a failed switch must not blank the view.
          this.reportFailure('Could not open that session', error);
          return;
        }
      }
      const previous = this.active;
      if (previous !== null) {
        await this.releaseActive();
      }
      record.setAttention('none');
      record.clearLastError();
      this.adopt(record);
      if (!this.isConnected()) {
        // Open it now, subscribe as soon as the connection is back (the record is
        // already visible, and the replay will fill it in).
        return;
      }
      await this.subscribeActive();
    });
  }

  /**
   * Close the open session's route: clear the active pointer, then best-effort
   * `unsubscribe` on the gateway.
   *
   * The pointer is cleared *before* the round trip so a concurrent
   * `subscribeActive` (a reconnect landing mid-switch) cannot attach the session
   * we are leaving. Every "a different session becomes the open one" path goes
   * through here — `activate` and `newSession` alike (review r1 S1).
   */
  private async releaseActive(): Promise<void> {
    const previous = this.active;
    if (previous === null) {
      return;
    }
    this.active = null;
    await this.unsubscribe(previous.record.sessionId);
  }

  /** Refresh the session list now (the manual path and the poll tick share this). */
  async refreshSessions(): Promise<void> {
    const http = this.clients?.http ?? null;
    if (http === null || this.stopped) {
      return;
    }
    try {
      const response = await http.listSessions();
      this.sessionList = response.sessions;
      this.listError = null;
      this.logger.debug(`session list refreshed (${response.sessions.length} sessions)`);
    } catch (error) {
      this.listError = describeError(error);
      this.logger.warn('could not refresh the session list', error);
    }
    this.notify();
    await this.maybeAutoActivate();
  }

  // ── connection plumbing ─────────────────────────────────────────────

  private isConnected(): boolean {
    return this.clients?.connection.currentClientId != null;
  }

  private settingsLabel(): string {
    const scheme = this.settings.scheme;
    const host = this.settings.host === '' ? '127.0.0.1' : this.settings.host;
    return `${scheme}://${host}:${this.settings.port}`;
  }

  private buildClients(): GatewayClients {
    const endpoints = gatewayEndpoints(this.settings, this.options.location);
    this.addressLabel = endpoints.label;
    const connection = new GatewayConnection({
      // The key is *in the URL* here, not in headers: a DOM WebSocket cannot set
      // handshake headers (design.md D7). `redactUrl` guards every log line.
      wsUrl: wsUrlWithApiKey(endpoints.wsUrl, this.settings.apiKey),
      ...(this.options.socketFactory === undefined ? {} : { socketFactory: this.options.socketFactory }),
      ...(this.options.now === undefined ? {} : { now: this.options.now }),
      logger: this.logger,
    });
    const http = new GatewayHttpClient({
      baseUrl: endpoints.httpBaseUrl,
      apiKey: this.settings.apiKey,
      ...(this.options.httpTransport === undefined ? {} : { transport: this.options.httpTransport }),
      logger: this.logger,
    });
    return { connection, http };
  }

  private async connectOnce(): Promise<void> {
    if (this.stopped) {
      return;
    }
    let clients: GatewayClients;
    try {
      clients = this.buildClients();
    } catch (error) {
      if (this.reportConnectFailure(error)) {
        this.scheduleConnectRetry();
      }
      return;
    }
    this.closing = true;
    this.closeClients();
    this.closing = false;
    this.clients = clients;
    this.unsubscribeState = clients.connection.onStateChange((state) => {
      this.handleStateChange(state);
    });
    this.unsubscribeEvents = clients.connection.onEvent((event) => {
      this.handleEvent(event);
    });
    if (this.phase !== 'reconnecting') {
      this.phase = 'connecting';
    }
    this.notify();
    try {
      await clients.connection.connect();
      this.connectAttempts = 0;
    } catch (error) {
      if (this.reportConnectFailure(error)) {
        this.scheduleConnectRetry();
      }
    }
  }

  /**
   * Report a failed dial; returns `true` when a retry makes sense.
   *
   * Two failures are final by nature, and both end in the first-connect guide
   * instead of a retry ladder: an address the settings cannot produce (a typo the
   * user has to see), and a rejected credential (a wrong key never heals).
   */
  private reportConnectFailure(error: unknown): boolean {
    if (error instanceof GatewayAddressError) {
      this.lastError = error.message;
      this.phase = 'offline';
      this.cancelConnectRetry();
      this.notify();
      return false;
    }
    if (error instanceof GatewaySocketError && error.kind === 'unauthorized') {
      this.unauthorized = true;
      this.lastError = 'The gateway rejected the API key (or the role is not allowed).';
      this.phase = 'offline';
      this.cancelConnectRetry();
      this.pushNotice('error', `${this.lastError} Check the gateway settings.`);
      this.notify();
      return false;
    }
    this.lastError = describeError(error);
    this.logger.warn(`gateway connect failed: ${this.lastError}`);
    // The phase is decided by what happens next: `scheduleConnectRetry` keeps the
    // banner in "connecting" because a retry is pending.
    return true;
  }

  private scheduleConnectRetry(): void {
    if (this.stopped || this.unauthorized) {
      return;
    }
    const delay = reconnectDelayMs(this.connectAttempts, {
      ...DEFAULT_RECONNECT_OPTIONS,
      ...(this.options.connectRetryBaseMs === undefined
        ? {}
        : { baseDelayMs: this.options.connectRetryBaseMs }),
    });
    this.connectAttempts += 1;
    this.cancelConnectRetry();
    this.phase = 'connecting';
    this.logger.debug(`retrying the gateway connection in ${delay} ms (attempt ${this.connectAttempts})`);
    this.startCountdown(this.now() + delay);
    this.connectTimer = setTimeout(() => {
      this.connectTimer = null;
      this.stopCountdown();
      void this.connectOnce();
    }, delay);
    this.notify();
  }

  private cancelConnectRetry(): void {
    if (this.connectTimer !== null) {
      clearTimeout(this.connectTimer);
      this.connectTimer = null;
    }
    this.stopCountdown();
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
    if (this.closing || this.stopped) {
      return;
    }
    switch (state.status) {
      case 'connected':
        this.phase = 'connected';
        this.everConnected = true;
        this.unauthorized = false;
        this.lastError = null;
        this.connectAttempts = 0;
        this.cancelConnectRetry();
        this.notify();
        void this.onConnected();
        return;
      case 'reconnecting':
        this.phase = 'reconnecting';
        this.onDisconnected(state);
        return;
      case 'closed': {
        const unauthorized = state.lastError?.kind === 'unauthorized';
        this.phase = 'offline';
        this.onDisconnected(state);
        if (unauthorized) {
          this.unauthorized = true;
          this.lastError = 'The gateway rejected the API key (or the role is not allowed).';
          this.pushNotice('error', `${this.lastError} Check the gateway settings.`);
          // The client does not retry an unauthorized close; neither do we. A
          // manual reconnect (after fixing the key) is the way out.
        }
        this.notify();
        return;
      }
      case 'connecting':
        this.phase = this.everConnected ? 'reconnecting' : 'connecting';
        this.notify();
        return;
      case 'idle':
        this.phase = 'offline';
        this.notify();
        return;
    }
  }

  /** Common bookkeeping for every "the socket is gone" transition. */
  private onDisconnected(state: ConnectionState): void {
    if (this.active !== null) {
      this.cancelResubscribe(this.active);
    }
    if (state.lastError !== null && state.lastError.kind !== 'disconnected') {
      this.lastError = state.lastError.message;
    }
    if (state.status === 'reconnecting') {
      const delay = state.reconnectInMs ?? null;
      this.startCountdown(delay === null ? null : this.now() + delay);
    }
    // The gateway keeps serving the list while the socket is down; pausing the
    // poll avoids a wall of identical failures.
    this.stopListPolling();
    this.notify();
  }

  private async onConnected(): Promise<void> {
    this.startListPolling();
    // Serialised with the session operations: a connect that lands while the user
    // is switching sessions must not produce two subscriptions for one session.
    await this.structure.run(async () => {
      await this.subscribeActive();
    });
    await this.refreshSessions();
  }

  // ── subscriptions ───────────────────────────────────────────────────

  /** Attach to the open session's event route (a fresh `clientId` every connect). */
  private async subscribeActive(): Promise<void> {
    const managed = this.active;
    const http = this.clients?.http ?? null;
    const clientId = this.clients?.connection.currentClientId ?? null;
    if (managed === null || http === null || clientId === null || managed.gone || this.stopped) {
      return;
    }
    const sessionId = managed.record.sessionId;
    try {
      await http.subscribe(sessionId, clientId);
      if (this.active !== managed) {
        // The user switched while the call was in flight: undo the route.
        await this.unsubscribeQuietly(sessionId, clientId);
        return;
      }
      managed.retryAttempt = 0;
      this.logger.debug(`subscribed to ${sessionId}`);
      this.notify();
      void this.refreshRuntimeState(managed.record);
    } catch (error) {
      if (error instanceof GatewayHttpError && error.isNotFound()) {
        try {
          await http.resumeSession(sessionId);
          await http.subscribe(sessionId, clientId);
          managed.retryAttempt = 0;
          void this.refreshRuntimeState(managed.record);
          return;
        } catch (resumeError) {
          this.markGone(managed, resumeError);
          return;
        }
      }
      this.scheduleResubscribe(managed, error);
    }
  }

  private async unsubscribe(sessionId: string): Promise<void> {
    const clientId = this.clients?.connection.currentClientId ?? null;
    if (clientId === null) {
      return;
    }
    await this.unsubscribeQuietly(sessionId, clientId);
  }

  private async unsubscribeQuietly(sessionId: string, clientId: string): Promise<void> {
    const http = this.clients?.http ?? null;
    if (http === null) {
      return;
    }
    try {
      await http.unsubscribe(sessionId, clientId);
    } catch (error) {
      // The gateway drops the route when the socket closes anyway; a failed
      // unsubscribe must never block a switch.
      this.logger.debug(`unsubscribe from ${sessionId} failed`, error);
    }
  }

  private scheduleResubscribe(managed: ManagedSession, error: unknown): void {
    if (this.stopped || managed.gone || this.active !== managed) {
      return;
    }
    const delay = reconnectDelayMs(managed.retryAttempt);
    managed.retryAttempt += 1;
    this.logger.warn(`subscribe failed for ${managed.record.sessionId}; retrying in ${delay} ms`, error);
    this.cancelResubscribe(managed);
    managed.retryTimer = setTimeout(() => {
      managed.retryTimer = null;
      void this.structure.run(async () => {
        await this.subscribeActive();
      });
    }, delay);
  }

  private cancelResubscribe(managed: ManagedSession): void {
    if (managed.retryTimer !== null) {
      clearTimeout(managed.retryTimer);
      managed.retryTimer = null;
    }
  }

  /** The session vanished from the gateway — say so once; do not retry. */
  private markGone(managed: ManagedSession, error: unknown): void {
    managed.gone = true;
    this.logger.warn(`session ${managed.record.sessionId} is gone from the gateway`, error);
    managed.record.pushCell({
      kind: 'system',
      id: managed.record.newCellId(),
      createdAt: managed.record.now(),
      level: 'error',
      text: GONE_MESSAGE,
    });
    this.flush(managed.record);
    this.pushNotice('error', 'That session no longer exists on the gateway.');
  }

  /**
   * Runtime knobs `sync_session` does not carry (yolo / thinking / effort / model).
   *
   * The HTTP read races the live stream: `session_state_changed` is emitted by the
   * `update_session` RPC (the model picker / thinking toggle step 09 will drive) and
   * can land *after* the request left but *before* the response arrives, in which
   * case the response describes an older session than the record does. So the write
   * is guarded by {@link metaEpoch}: every state event reduced into the open session
   * bumps it, and a response from before the bump is dropped (review r1 S2).
   */
  private async refreshRuntimeState(record: SessionRecord): Promise<void> {
    const http = this.clients?.http ?? null;
    if (http === null || this.stopped || this.active?.record !== record) {
      return;
    }
    const epoch = this.metaEpoch;
    try {
      const info = await http.sessionInfo(record.sessionId);
      if (this.metaEpoch !== epoch) {
        this.logger.debug(`runtime state of ${record.sessionId} arrived after a newer state event; ignored`);
        return;
      }
      record.meta = {
        ...record.meta,
        thinking: info.thinking,
        reasoningEffort: info.reasoning_effort ?? '',
        yolo: info.yolo,
        model: info.model,
        workspace: info.workdir ?? record.meta.workspace,
      };
      this.flush(record);
    } catch (error) {
      this.logger.debug(`could not refresh the runtime state of ${record.sessionId}`, error);
    }
  }

  // ── event routing ───────────────────────────────────────────────────

  /**
   * Reduce one gateway event into the open session's record.
   *
   * Only the open session is subscribed, so anything else is a protocol surprise:
   * log it, never guess it into the open session.
   */
  private handleEvent(event: WingEvent): void {
    const sessionId = event.session_id;
    const managed = this.active;
    if (managed === null || managed.gone) {
      return;
    }
    if (sessionId === null || sessionId === '') {
      this.logger.debug(`dropping sessionless event ${event.type}`);
      return;
    }
    if (sessionId !== managed.record.sessionId) {
      this.logger.debug(`event ${event.type} for unopened session ${sessionId}; ignored`);
      return;
    }
    const effects: ReductionEffect[] = [];
    if (isKnownEvent(event) && event.type === 'sync_session') {
      applySync(managed.record, event);
      this.metaEpoch += 1;
    } else {
      if (isKnownEvent(event) && event.type === 'session_state_changed') {
        // Local knowledge about the controls just got newer than any in-flight
        // `/api/session/info` read (review r1 S2).
        this.metaEpoch += 1;
      }
      effects.push(...applyLive(managed.record, event));
    }
    this.flush(managed.record);
    if (isKnownEvent(event) && event.type === 'turn_result') {
      // A finished turn is exactly when the list is worth re-reading: the row's
      // status and its "last interaction" both changed (design.md D6).
      void this.refreshSessions();
    }
    for (const effect of effects) {
      switch (effect.kind) {
        case 'attention':
          // "A turn finished while you were elsewhere": on the web, elsewhere is
          // a hidden page. The badge lives on the row until it is opened again.
          if (!this.pageVisible()) {
            managed.record.setAttention(effect.level);
            this.notify();
          }
          break;
        case 'toast':
          this.pushNotice(effect.level, effect.message);
          break;
        case 'scrollToBottom':
          // Owned by the transcript container (step 08); nothing to do here.
          break;
      }
    }
  }

  /** Ship whatever the reduction produced (no bridge here: the snapshot *is* the ship). */
  private flush(record: SessionRecord): void {
    record.takeJournal(); // cell patches are consumed by the React layer directly
    record.replaced = false;
    record.dirtyState = false;
    record.dirtyTabs = false;
    record.dirtyPanels = false;
    this.recordVersion += 1;
    this.notify();
  }

  // ── records ─────────────────────────────────────────────────────────

  private adopt(record: SessionRecord): void {
    this.records.set(record.sessionId, record);
    this.touchRecord(record.sessionId);
    this.active = { record, retryAttempt: 0, retryTimer: null, gone: false };
    this.evictRecords();
    this.notify();
  }

  private touchRecord(sessionId: string): void {
    const index = this.recordOrder.indexOf(sessionId);
    if (index !== -1) {
      this.recordOrder.splice(index, 1);
    }
    this.recordOrder.push(sessionId);
  }

  /**
   * Keep the cache bounded; the open session is never evicted.
   *
   * The loop removes the oldest entry that is *not* the open session, so the
   * invariant holds after this call instead of one `adopt` later (review r1 N5 —
   * the previous version gave up for the round when the oldest happened to be the
   * active record).
   */
  private evictRecords(): void {
    const limit = Math.max(1, this.options.maxCachedRecords ?? DEFAULT_MAX_CACHED_RECORDS);
    while (this.recordOrder.length > limit) {
      const index = this.recordOrder.findIndex((id) => id !== this.active?.record.sessionId);
      if (index === -1) {
        return; // only the open session is cached
      }
      const [evicted] = this.recordOrder.splice(index, 1);
      if (evicted !== undefined) {
        this.records.delete(evicted);
      }
    }
  }

  // ── session list ────────────────────────────────────────────────────

  private startListPolling(): void {
    const interval = this.options.listPollIntervalMs ?? DEFAULT_LIST_POLL_MS;
    this.stopListPolling();
    if (interval <= 0) {
      return;
    }
    this.listTimer = setInterval(() => {
      if (!this.pageVisible()) {
        return;
      }
      void this.refreshSessions();
    }, interval);
  }

  private stopListPolling(): void {
    if (this.listTimer !== null) {
      clearInterval(this.listTimer);
      this.listTimer = null;
    }
  }

  private async maybeAutoActivate(): Promise<void> {
    if (this.autoActivateAttempted || this.active !== null || this.unauthorized) {
      return;
    }
    this.autoActivateAttempted = true;
    const rows = this.rows();
    const first = rows[0];
    if (first === undefined) {
      return;
    }
    this.logger.debug(`opening the most recent session ${first.id}`);
    await this.activate(first.id);
  }

  private pageVisible(): boolean {
    return this.options.isPageVisible?.() ?? true;
  }

  private rows(): SessionRow[] {
    const overlay: SessionRowOverlay | null =
      this.active === null
        ? null
        : {
            id: this.active.record.sessionId,
            title: this.active.record.title,
            status: this.active.record.status,
            attention: this.active.record.attention,
          };
    return buildSessionRows(this.sessionList, this.now(), overlay);
  }

  // ── notices ─────────────────────────────────────────────────────────

  private pushNotice(level: Notice['level'], text: string): void {
    this.noticeSeq += 1;
    const notice: Notice = { id: this.noticeSeq, level, text };
    this.notices = [...this.notices, notice];
    const ttl = this.options.noticeTtlMs ?? DEFAULT_NOTICE_TTL_MS;
    if (ttl > 0) {
      const timer = setTimeout(() => {
        this.noticeTimers.delete(timer);
        this.dismissNoticeById(notice.id);
      }, ttl);
      this.noticeTimers.add(timer);
    }
    this.notify();
  }

  private dismissNoticeById(id: number): void {
    const next = this.notices.filter((notice) => notice.id !== id);
    if (next.length === this.notices.length) {
      return;
    }
    this.notices = next;
    this.notify();
  }

  /** Dismiss one notice from the UI (the automatic expiry is the same path). */
  dismissNotice(id: number): void {
    this.dismissNoticeById(id);
  }

  /**
   * User-visible message from outside the runtime's own event lane.
   *
   * The transcript bridge uses it for intents a browser cannot carry out
   * (opening a file / a native diff — see `src/bridge/webBridge.ts`), and step 09's
   * composer will use it for send failures. Same stack, same TTL, same dismissal as
   * every other notice.
   */
  pushUserNotice(level: Notice['level'], text: string): void {
    this.pushNotice(level, text);
  }

  private reportFailure(prefix: string, error: unknown): void {
    const detail = describeError(error);
    this.logger.warn(`${prefix}: ${detail}`);
    this.pushNotice('error', `${prefix}: ${detail}`);
  }

  // ── countdown ───────────────────────────────────────────────────────

  /** Start (or clear) the live retry countdown the banner renders. */
  private startCountdown(deadlineMs: number | null): void {
    this.reconnectDeadlineMs = deadlineMs;
    if (deadlineMs === null) {
      this.stopCountdown();
      return;
    }
    if (this.countdownTimer !== null) {
      return;
    }
    this.countdownTimer = setInterval(() => {
      if (this.reconnectDeadlineMs !== null && this.now() >= this.reconnectDeadlineMs) {
        this.reconnectDeadlineMs = null;
        this.stopCountdown();
      }
      this.notify();
    }, 1_000);
  }

  private stopCountdown(): void {
    if (this.countdownTimer !== null) {
      clearInterval(this.countdownTimer);
      this.countdownTimer = null;
    }
    this.reconnectDeadlineMs = null;
  }

  // ── snapshot ────────────────────────────────────────────────────────

  private connectionView(): ConnectionView {
    const state = this.clients?.connection.state ?? null;
    const remaining =
      this.reconnectDeadlineMs === null ? null : Math.max(0, this.reconnectDeadlineMs - this.now());
    return {
      phase: this.phase,
      clientId: state?.clientId ?? null,
      everConnected: this.everConnected,
      attempt: state?.status === 'reconnecting' ? state.attempt : this.connectAttempts,
      reconnectInMs: remaining ?? state?.reconnectInMs ?? null,
      lastError: this.lastError,
      address: this.addressLabel,
      unauthorized: this.unauthorized,
    };
  }

  private buildSnapshot(): RuntimeSnapshot {
    return {
      connection: this.connectionView(),
      settings: this.settings,
      sessions: this.rows(),
      listError: this.listError,
      activeSessionId: this.active?.record.sessionId ?? null,
      record: this.active?.record ?? null,
      recordVersion: this.recordVersion,
      notices: this.notices,
    };
  }

  /** Rebuild the snapshot and wake the subscribers. */
  private notify(): void {
    this.snapshot = this.buildSnapshot();
    this.notifier.notify();
  }

  // ── control plane (step 09) ─────────────────────────────────────────

  /**
   * Send one user message — the composer's core path.
   *
   * Mirrors `extensions/vscode/src/host/session/manager.ts::sendText`, without the
   * `resolveLocalCommand` step (that happens in the composer's submit path).
   *
   * Returns `true` when the frame left the socket. On failure the text is returned
   * to the composer via `setDraft` (vscode parity — see `returnToComposer`).
   */
  sendText(text: string): boolean {
    const managed = this.active;
    const connection = this.clients?.connection ?? null;
    if (managed === null || connection === null || connection.state.status !== 'connected') {
      this.returnToComposer(text);
      this.pushNotice('warning', 'Not sent — the gateway is not connected.');
      return false;
    }
    const frame = createClientRequest({ sessionId: managed.record.sessionId, content: text });
    const record = managed.record;
    record.addPendingUser(frame.request_id, text);
    record.refreshTitle();
    record.dirtyTabs = true;
    this.flush(record);
    try {
      connection.send(frame);
      return true;
    } catch (error) {
      record.removePending(frame.request_id);
      record.refreshTitle();
      record.dirtyTabs = true;
      this.flush(record);
      this.reportFailure('Send failed', error);
      return false;
    }
  }

  /** Return text to the composer (vscode's `returnToComposer`). */
  private returnToComposer(text: string): void {
    const managed = this.active;
    if (managed === null) {
      return;
    }
    managed.record.setDraft(text);
    this.flush(managed.record);
  }

  /** Interrupt the running turn. */
  async interrupt(): Promise<void> {
    const http = this.clients?.http ?? null;
    const sessionId = this.active?.record.sessionId ?? null;
    if (http === null || sessionId === null) {
      this.pushNotice('warning', 'Not connected — the gateway is not available.');
      return;
    }
    try {
      await http.interruptSession(sessionId);
    } catch (error) {
      this.reportFailure('Interrupt failed', error);
    }
  }

  /** Open the model picker — fetch models and set `panels.modelPicker`. */
  async openModelPicker(): Promise<void> {
    const managed = this.active;
    const http = this.clients?.http ?? null;
    if (managed === null || http === null) {
      return;
    }
    try {
      const response = await http.listModels();
      const model = managed.record.meta.model;
      const provider = managed.record.meta.provider;
      const rows = response.providers.flatMap((group) =>
        group.models.map((name) => ({
          provider: group.provider,
          model: name,
          selected: name === model && group.provider === provider,
        })),
      );
      managed.record.panels = {
        ...managed.record.panels,
        modelPicker: { sessionId: managed.record.sessionId, rows, activeIndex: null },
      };
      managed.record.dirtyPanels = true;
      this.flush(managed.record);
    } catch (error) {
      this.reportFailure('Could not list models', error);
    }
  }

  /** Open the branch picker for fork or rewind mode. */
  async openBranchesPanel(mode: 'rewind' | 'fork'): Promise<void> {
    const managed = this.active;
    const http = this.clients?.http ?? null;
    if (managed === null || http === null) {
      return;
    }
    try {
      const response = await http.sessionBranches(managed.record.sessionId);
      const rows = response.targets.map((target) => branchRow(target));
      managed.record.panels = { ...managed.record.panels, branchPicker: { mode, rows } };
      managed.record.dirtyPanels = true;
      this.flush(managed.record);
    } catch (error) {
      this.reportFailure('Could not list branches', error);
    }
  }

  /** Compact the session context. */
  async compact(instruction: string | null = null): Promise<void> {
    const http = this.clients?.http ?? null;
    const sessionId = this.active?.record.sessionId ?? null;
    if (http === null || sessionId === null) {
      this.pushNotice('warning', 'Not connected — the gateway is not available.');
      return;
    }
    this.pushNotice('info', 'Compacting context…');
    try {
      const response = await http.compactSession(sessionId, instruction);
      this.pushNotice(
        'info',
        `Context compacted: ${response.original_tokens} → ${response.compressed_tokens} tokens`,
      );
    } catch (error) {
      this.reportFailure('Compact failed', error);
    }
  }

  /** Close all overlays (Esc / overlay click). */
  closeOverlays(): void {
    const managed = this.active;
    if (managed === null) {
      return;
    }
    managed.record.panels = {
      ...managed.record.panels,
      modelPicker: null,
      sessionPicker: null,
      branchPicker: null,
    };
    managed.record.dirtyPanels = true;
    this.flush(managed.record);
  }

  /** Fork the session at a target uuid — creates a new session, subscribes to it. */
  async fork(sourceSessionId: string, targetUuid: string): Promise<string | null> {
    return this.structure.run(async () => {
      const http = this.clients?.http ?? null;
      const source = this.active;
      if (http === null || source === null) {
        this.pushNotice('warning', 'Fork failed — the session is not available.');
        return null;
      }
      const sourceSession = source;
      try {
        const response = await http.forkSession(sourceSessionId, targetUuid);
        const record = new SessionRecord({
          sessionId: response.session_id,
          now: this.now,
          workspace: sourceSession.record.meta.workspace === '' ? null : sourceSession.record.meta.workspace,
          createdAt: '',
        });
        record.setDraft(response.draft);
        await this.releaseActive();
        this.adopt(record);
        await this.subscribeActive();
        if (record.draft !== null) {
          this.flush(record);
        }
        void this.refreshSessions();
        return record.sessionId;
      } catch (error) {
        this.reportFailure('Fork failed', error);
        return null;
      }
    });
  }

  /** Rewind to a target uuid — the gateway answers with a sync_session replacement. */
  async rewind(targetUuid: string): Promise<void> {
    const managed = this.active;
    const http = this.clients?.http ?? null;
    if (managed === null || http === null) {
      this.pushNotice('warning', 'Rewind failed — the session is not available.');
      return;
    }
    try {
      await http.rewindSession(managed.record.sessionId, targetUuid);
    } catch (error) {
      this.reportFailure('Rewind failed', error);
    }
    this.closeOverlays();
  }

  /**
   * Update session metadata on the gateway.
   *
   * Mirrors `extensions/vscode/src/host/session/manager.ts::updateSession`:
   * optimistic local update for fields the gateway echoes through
   * `session_state_changed`.
   */
  async updateMeta(fields: {
    model?: string;
    provider?: string;
    thinking?: boolean;
    reasoning_effort?: string;
    yolo?: boolean;
    title?: string;
    agent?: string;
    workspace?: string;
  }): Promise<void> {
    const managed = this.active;
    const http = this.clients?.http ?? null;
    if (managed === null || http === null) {
      this.pushNotice('warning', 'Update failed — the session is not available.');
      return;
    }
    const sessionId = managed.record.sessionId;
    try {
      await http.updateSession({ session_id: sessionId, ...fields });
    } catch (error) {
      this.reportFailure('Update failed', error);
      return;
    }
    // Optimistic local update (vscode parity).
    const record = managed.record;
    const meta = { ...record.meta };
    if (fields.model !== undefined) {
      meta.model = fields.model;
    }
    if (fields.provider !== undefined) {
      meta.provider = fields.provider;
    }
    if (fields.thinking !== undefined) {
      meta.thinking = fields.thinking;
    }
    if (fields.reasoning_effort !== undefined) {
      meta.reasoningEffort = fields.reasoning_effort;
    }
    if (fields.yolo !== undefined) {
      meta.yolo = fields.yolo;
    }
    if (fields.title !== undefined) {
      record.explicitTitle = fields.title;
    }
    if (fields.agent !== undefined) {
      meta.agent = fields.agent;
    }
    if (fields.workspace !== undefined) {
      meta.workspace = fields.workspace;
    }
    record.meta = meta;
    record.refreshTitle();
    record.dirtyState = true;
    record.dirtyTabs = true;
    this.flush(record);
    if (fields.model !== undefined || fields.provider !== undefined) {
      // Close the model picker on model selection (vscode parity).
      if (record.panels.modelPicker !== null) {
        record.panels = { ...record.panels, modelPicker: null };
        record.dirtyPanels = true;
        this.flush(record);
      }
    }
  }

  /** Release every timer and socket (tests + an unmounting root). */
  dispose(): void {
    this.stop();
    for (const timer of this.noticeTimers) {
      clearTimeout(timer);
    }
    this.noticeTimers.clear();
    this.notices = [];
  }
}

/** One-line reason for a failure, without leaking a key that rode in a URL. */
export function describeError(error: unknown): string {
  if (error instanceof GatewayAddressError) {
    return error.message;
  }
  if (error instanceof GatewayHttpError) {
    const detail = error.detail;
    return detail === null || detail === '' ? error.message : `${error.message} — ${detail}`;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}
