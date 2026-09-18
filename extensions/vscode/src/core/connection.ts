/**
 * `GatewayConnection` — the WebSocket half of the capability layer.
 *
 * It owns exactly one socket at a time plus the reconnect supervisor, and it
 * knows nothing about sessions: `src/core` never subscribes, never resubscribes
 * and never buffers business events. What it does provide is the signal 03 needs
 * to do that: every successful (re)connect is announced with a fresh `clientId`
 * (the gateway assigns a new one per connection), and every loss is announced
 * with a typed reason.
 *
 * Lifecycle:
 *
 * ```ts
 * const conn = new GatewayConnection({ wsUrl });
 * conn.onEvent((event) => …);        // complete events only (chunking is done)
 * conn.onStateChange((state) => …);  // connecting / connected / reconnecting / closed
 * await conn.connect();              // dial + handshake; rejects on failure
 * conn.send(createClientRequest({ sessionId, content }));
 * conn.close();                      // intentional: never reconnects
 * ```
 *
 * Reconnect policy (mirrors `crates/wing/src/app/transport.rs`):
 * `min(1s · 2^attempt, 30s)` — 1s, 2s, 4s, 8s, 16s, 30s, 30s, … with the ladder
 * reset on every fresh loss and every success. Deliberate difference from the
 * TUI: a connection rejected with close code 4001 (`Unauthorized`) does **not**
 * retry — credentials do not heal themselves, and a 30 s loop would only spam the
 * user; the failure is reported as `GatewaySocketError{kind:'unauthorized'}`.
 */

import { DEFAULT_RECONNECT_OPTIONS, type ReconnectOptions, reconnectDelayMs } from './backoff';
import { ChunkReassembler, type ChunkLimits, type ChunkOutcome } from './chunk';
import { GatewaySocketError, WS_CLOSE_UNAUTHORIZED, describeThrown } from './errors';
import { type CoreLogger, consoleLogger } from './logging';
import type { WingEvent } from './protocol/events';
import {
  type ClientRequest,
  createClientRequest,
  decodeConnectResponse,
  encodeClientRequest,
} from './protocol/frames';
import { redactUrl } from './urls';
import {
  SOCKET_OPEN,
  type SocketCloseInfo,
  type SocketFactory,
  type SocketHandlers,
  type SocketLike,
  createNativeSocketFactory,
} from './transport/socket';

export type ConnectionStatus =
  /** Constructed but never dialled (or the last dial failed and no retry is pending). */
  | 'idle'
  /** Initial dial in flight. */
  | 'connecting'
  /** Handshake done: `send()` works and events flow. */
  | 'connected'
  /** Lost (or never yet established) — waiting for / running the next retry. */
  | 'reconnecting'
  /** `close()` was called, or the connection stopped for good (unauthorized). */
  | 'closed';

/** Immutable snapshot of the connection state (also the state-change payload). */
export interface ConnectionState {
  readonly status: ConnectionStatus;
  /** Handshake identity; `null` unless currently connected. */
  readonly clientId: string | null;
  /** Retry attempt counter (0 = first retry after a loss). */
  readonly attempt: number;
  /** Delay of the pending retry, or `null` when none is scheduled. */
  readonly reconnectInMs: number | null;
  readonly lastError: GatewaySocketError | null;
}

export type Unsubscribe = () => void;
export type EventListener = (event: WingEvent) => void;
export type StateListener = (state: ConnectionState) => void;

export interface GatewayConnectionOptions {
  /** Full WS URL, e.g. from `gatewayUrls({ host, port, apiKey }).wsUrl`. */
  readonly wsUrl: string;
  /** Socket seam (tests); defaults to the platform `WebSocket`. */
  readonly socketFactory?: SocketFactory;
  /** `false` disables the supervisor (the host drives retries itself). */
  readonly reconnect?: Partial<ReconnectOptions> | false;
  readonly chunkLimits?: Partial<ChunkLimits>;
  /** Handshake deadline; without it a silent peer would hang `connect()` forever. */
  readonly handshakeTimeoutMs?: number;
  readonly logger?: CoreLogger;
  /** Injectable clock for the reassembly deadline (tests). */
  readonly now?: () => number;
}

export const DEFAULT_HANDSHAKE_TIMEOUT_MS = 10_000;

type DialMode = 'initial' | 'retry';

interface Deferred<T> {
  readonly promise: Promise<T>;
  resolve(value: T): void;
  reject(error: unknown): void;
}

function deferred<T>(): Deferred<T> {
  let resolve: (value: T) => void = () => undefined;
  let reject: (error: unknown) => void = () => undefined;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

export class GatewayConnection {
  private readonly wsUrl: string;
  private readonly socketFactory: SocketFactory;
  private readonly logger: CoreLogger;
  private readonly reconnectOptions: ReconnectOptions | null;
  private readonly handshakeTimeoutMs: number;
  private readonly now: () => number;
  private readonly reassembler: ChunkReassembler;
  private readonly eventListeners = new Set<EventListener>();
  private readonly stateListeners = new Set<StateListener>();

  private socket: SocketLike | null = null;
  private status: ConnectionStatus = 'idle';
  private clientId: string | null = null;
  private attempt = 0;
  private lastError: GatewaySocketError | null = null;
  private reconnectInMs: number | null = null;
  private closing = false;
  private dialToken: symbol | null = null;
  private handshake: Deferred<string> | null = null;
  private connectPromise: Promise<void> | null = null;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private chunkTimer: ReturnType<typeof setTimeout> | null = null;
  private handshakeTimer: ReturnType<typeof setTimeout> | null = null;
  private lastPublished: ConnectionState | null = null;

  constructor(options: GatewayConnectionOptions) {
    this.wsUrl = options.wsUrl;
    this.socketFactory = options.socketFactory ?? createNativeSocketFactory();
    this.logger = options.logger ?? consoleLogger;
    this.handshakeTimeoutMs = options.handshakeTimeoutMs ?? DEFAULT_HANDSHAKE_TIMEOUT_MS;
    this.now = options.now ?? Date.now;
    this.reconnectOptions =
      options.reconnect === false ? null : { ...DEFAULT_RECONNECT_OPTIONS, ...(options.reconnect ?? {}) };
    this.reassembler = new ChunkReassembler({
      ...(options.chunkLimits === undefined ? {} : { limits: options.chunkLimits }),
      now: this.now,
      logger: this.logger,
    });
  }

  /** Current state snapshot (same object shape as the state-change payload). */
  get state(): ConnectionState {
    return this.snapshot();
  }

  /** `clientId` of the live connection, or `null`. */
  get currentClientId(): string | null {
    return this.clientId;
  }

  /**
   * Dial the gateway and complete the handshake.
   *
   * Resolves once `ConnectResponse` arrived. Rejects on failure **without**
   * retrying — starting the gateway (or asking the user) is the host's call;
   * automatic retries only kick in after a connection has been established once.
   * Concurrent calls share one in-flight attempt.
   */
  connect(): Promise<void> {
    if (this.status === 'connected') {
      return Promise.resolve();
    }
    if (this.connectPromise !== null) {
      return this.connectPromise;
    }
    // A second call while the supervisor is already working must not dial a
    // second socket: bring the pending retry forward instead.
    const supervised = this.status === 'reconnecting';
    this.closing = false;
    if (supervised) {
      this.clearRetryTimer();
      this.reconnectInMs = null;
    } else {
      this.attempt = 0;
    }
    const pending = this.dial(supervised ? 'retry' : 'initial')
      .catch((error: unknown) => {
        const socketError = toSocketError(error);
        this.lastError = socketError;
        if (supervised) {
          // Keep supervising; the caller still sees this attempt's failure.
          this.attempt += 1;
          this.scheduleRetry();
        } else if (!this.closing) {
          this.setStatus('idle');
        }
        throw socketError;
      })
      .finally(() => {
        if (this.connectPromise === pending) {
          this.connectPromise = null;
        }
      });
    this.connectPromise = pending;
    return pending;
  }

  /** Register an event listener; returns the unsubscribe function. */
  onEvent(listener: EventListener): Unsubscribe {
    this.eventListeners.add(listener);
    return () => {
      this.eventListeners.delete(listener);
    };
  }

  /** Register a state listener; returns the unsubscribe function. */
  onStateChange(listener: StateListener): Unsubscribe {
    this.stateListeners.add(listener);
    return () => {
      this.stateListeners.delete(listener);
    };
  }

  /** Send one `ClientRequest`. Throws when not connected (no queueing — see design D5). */
  send(frame: ClientRequest): void {
    this.sendText(encodeClientRequest(frame));
  }

  /** Build + send a user message frame; returns it (the host keeps `request_id`). */
  sendMessage(input: {
    readonly sessionId: string;
    readonly content: string;
    readonly toolCallId?: string | null;
  }): ClientRequest {
    const frame = createClientRequest(input);
    this.send(frame);
    return frame;
  }

  /**
   * Close intentionally and stop supervising. Idempotent.
   *
   * A pending `connect()` is rejected: after `close()` this object is inert until
   * `connect()` is called again.
   */
  close(code = 1000, reason = 'client closing'): void {
    this.closing = true;
    const handshake = this.takeHandshake();
    const socket = this.takeSocket();
    this.clearTimers();
    this.clientId = null;
    this.reconnectInMs = null;
    this.setStatus('closed');
    if (handshake !== null) {
      handshake.reject(
        new GatewaySocketError({ kind: 'disconnected', message: 'connection closed by the client' }),
      );
    }
    if (socket !== null) {
      try {
        socket.close(code, reason);
      } catch (cause) {
        this.logger.debug('closing the gateway socket threw', describeThrown(cause));
      }
    }
  }

  // ── dialing ──────────────────────────────────────────────────────────

  private dial(mode: DialMode): Promise<void> {
    this.reassembler.reset();
    const token = Symbol('dial');
    this.dialToken = token;
    const handshake = deferred<string>();
    this.handshake = handshake;
    this.setStatus(mode === 'initial' ? 'connecting' : 'reconnecting');

    let socket: SocketLike;
    try {
      socket = this.socketFactory(this.wsUrl, this.handlers(token));
    } catch (cause) {
      this.handshake = null;
      this.dialToken = null;
      return Promise.reject(
        new GatewaySocketError({
          kind: 'connect-failed',
          message: `failed to open ${redactUrl(this.wsUrl)}: ${describeThrown(cause)}`,
          cause,
        }),
      );
    }
    this.socket = socket;
    this.armHandshakeTimer();

    return handshake.promise.then(
      (clientId) => {
        this.clientId = clientId;
        this.attempt = 0;
        this.lastError = null;
        this.reconnectInMs = null;
        this.setStatus('connected');
      },
      (error: unknown) => {
        const socketError = toSocketError(error);
        this.lastError = socketError;
        this.teardown();
        throw socketError;
      },
    );
  }

  private handlers(token: symbol): SocketHandlers {
    const current = (): boolean => this.dialToken === token;
    return {
      onOpen: () => {
        if (!current()) {
          return;
        }
        this.logger.debug(`gateway socket open (${redactUrl(this.wsUrl)})`);
      },
      onMessage: (text) => {
        if (!current()) {
          return;
        }
        this.handleMessage(text);
      },
      onClose: (info) => {
        if (!current()) {
          return;
        }
        this.handleClose(info);
      },
      onError: (detail) => {
        if (!current()) {
          return;
        }
        // undici reports the error right before `close`; the close handler owns
        // the state transition, so this stays a log line.
        this.logger.warn(`gateway socket error: ${detail}`);
      },
    };
  }

  private handleMessage(text: string): void {
    let outcome: ChunkOutcome;
    try {
      outcome = this.reassembler.onText(text);
    } catch (cause) {
      // A decoder bug must not kill the read path — drop the frame, keep the socket.
      this.logger.error('failed to decode a gateway frame', cause);
      return;
    }
    switch (outcome.kind) {
      case 'fail':
        this.loseConnection(
          new GatewaySocketError({
            kind: 'reassembly',
            message: `chunk reassembly failed: ${outcome.detail}`,
            detail: outcome.detail,
          }),
        );
        return;
      case 'pending':
        this.armChunkTimer();
        return;
      case 'deliver':
        this.clearChunkTimer();
        for (const event of outcome.events) {
          this.deliverEvent(event);
        }
        return;
    }
  }

  private deliverEvent(event: WingEvent): void {
    if (this.handshake !== null) {
      this.settleHandshake(event);
      return;
    }
    for (const listener of Array.from(this.eventListeners)) {
      try {
        listener(event);
      } catch (cause) {
        this.logger.error('gateway event listener threw', cause);
      }
    }
  }

  private settleHandshake(event: WingEvent): void {
    this.clearHandshakeTimer();
    if (event.type !== 'connected') {
      this.loseConnection(
        new GatewaySocketError({
          kind: 'handshake',
          message: `expected a ConnectResponse, got "${event.type}"`,
        }),
      );
      return;
    }
    const response = decodeConnectResponse(event.raw);
    if (response === null) {
      this.loseConnection(
        new GatewaySocketError({ kind: 'handshake', message: 'ConnectResponse without client_id' }),
      );
      return;
    }
    const handshake = this.takeHandshake();
    handshake?.resolve(response.client_id);
  }

  private handleClose(info: SocketCloseInfo): void {
    if (this.closing) {
      return;
    }
    const error =
      info.code === WS_CLOSE_UNAUTHORIZED
        ? new GatewaySocketError({
            kind: 'unauthorized',
            message: `gateway rejected the connection: ${info.reason === '' ? 'Unauthorized' : info.reason}`,
            closeCode: info.code,
            detail: info.reason,
          })
        : new GatewaySocketError({
            kind: 'disconnected',
            message: describeClose(info),
            closeCode: info.code,
            detail: info.reason,
          });
    this.loseConnection(error);
  }

  /**
   * Single exit for "this connection is gone": rejects a pending handshake (the
   * `dial()` caller then decides) or supervises a post-connect loss.
   */
  private loseConnection(error: GatewaySocketError): void {
    const handshake = this.takeHandshake();
    this.teardown();
    this.lastError = error;
    this.clientId = null;
    if (handshake !== null) {
      handshake.reject(error);
      return;
    }
    if (this.closing) {
      return;
    }
    if (this.reconnectOptions === null || !error.retryable) {
      this.attempt = 0;
      this.reconnectInMs = null;
      this.setStatus('closed');
      return;
    }
    this.attempt = 0;
    this.scheduleRetry();
  }

  private scheduleRetry(): void {
    const options = this.reconnectOptions ?? DEFAULT_RECONNECT_OPTIONS;
    const delay = reconnectDelayMs(this.attempt, options);
    this.clearRetryTimer();
    this.reconnectInMs = delay;
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.reconnectInMs = null;
      this.runRetry();
    }, delay);
    this.setStatus('reconnecting');
  }

  /**
   * Fired by the retry timer. `connect()` owns dial + retry ladder, so the
   * supervisor path is just "run it and swallow this attempt's failure"
   * (the next attempt is already scheduled by then).
   */
  private runRetry(): void {
    if (this.closing) {
      return;
    }
    this.publish();
    void this.connect().catch(() => undefined);
  }

  // ── timers ───────────────────────────────────────────────────────────

  private armHandshakeTimer(): void {
    this.clearHandshakeTimer();
    if (this.handshake === null) {
      // The handshake already settled (a synchronous fake/loopback) — nothing to arm.
      return;
    }
    this.handshakeTimer = setTimeout(() => {
      this.handshakeTimer = null;
      if (this.handshake === null) {
        return;
      }
      this.loseConnection(
        new GatewaySocketError({
          kind: 'handshake',
          message: `gateway did not send a ConnectResponse within ${this.handshakeTimeoutMs} ms`,
        }),
      );
    }, this.handshakeTimeoutMs);
  }

  private armChunkTimer(): void {
    this.clearChunkTimer();
    const deadline = this.reassembler.deadline();
    if (deadline === null) {
      return;
    }
    const delay = Math.max(0, deadline - this.now());
    this.chunkTimer = setTimeout(() => {
      this.chunkTimer = null;
      if (!this.reassembler.isAssembling()) {
        return;
      }
      const detail = this.reassembler.timeoutDetail();
      this.loseConnection(
        new GatewaySocketError({
          kind: 'reassembly',
          message: `chunk reassembly timed out: ${detail}`,
          detail,
        }),
      );
    }, delay);
  }

  private clearHandshakeTimer(): void {
    if (this.handshakeTimer !== null) {
      clearTimeout(this.handshakeTimer);
      this.handshakeTimer = null;
    }
  }

  private clearChunkTimer(): void {
    if (this.chunkTimer !== null) {
      clearTimeout(this.chunkTimer);
      this.chunkTimer = null;
    }
  }

  private clearRetryTimer(): void {
    if (this.retryTimer !== null) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
  }

  private clearTimers(): void {
    this.clearHandshakeTimer();
    this.clearChunkTimer();
    this.clearRetryTimer();
  }

  // ── plumbing ─────────────────────────────────────────────────────────

  private sendText(text: string): void {
    const socket = this.socket;
    if (socket === null || this.status !== 'connected' || socket.readyState !== SOCKET_OPEN) {
      throw new GatewaySocketError({
        kind: 'not-connected',
        message: 'cannot send: the gateway connection is not open',
      });
    }
    try {
      socket.send(text);
    } catch (cause) {
      throw new GatewaySocketError({
        kind: 'send-failed',
        message: `failed to send to the gateway: ${describeThrown(cause)}`,
        cause,
      });
    }
  }

  private takeHandshake(): Deferred<string> | null {
    const handshake = this.handshake;
    this.handshake = null;
    return handshake;
  }

  private takeSocket(): SocketLike | null {
    const socket = this.socket;
    this.socket = null;
    this.dialToken = null;
    return socket;
  }

  private teardown(): void {
    this.clearTimers();
    const socket = this.takeSocket();
    if (socket !== null) {
      try {
        socket.close();
      } catch (cause) {
        this.logger.debug('closing a failed socket threw', describeThrown(cause));
      }
    }
  }

  private snapshot(): ConnectionState {
    return {
      status: this.status,
      clientId: this.clientId,
      attempt: this.attempt,
      reconnectInMs: this.reconnectInMs,
      lastError: this.lastError,
    };
  }

  private setStatus(status: ConnectionStatus): void {
    this.status = status;
    this.publish();
  }

  private publish(): void {
    const snapshot = this.snapshot();
    if (this.lastPublished !== null && sameState(this.lastPublished, snapshot)) {
      return;
    }
    this.lastPublished = snapshot;
    for (const listener of Array.from(this.stateListeners)) {
      try {
        listener(snapshot);
      } catch (cause) {
        this.logger.error('gateway state listener threw', cause);
      }
    }
  }
}

function sameState(a: ConnectionState, b: ConnectionState): boolean {
  return (
    a.status === b.status &&
    a.clientId === b.clientId &&
    a.attempt === b.attempt &&
    a.reconnectInMs === b.reconnectInMs &&
    a.lastError === b.lastError
  );
}

function describeClose(info: SocketCloseInfo): string {
  const reason = info.reason === '' ? '' : `, reason=${JSON.stringify(info.reason)}`;
  return `gateway connection closed (code=${info.code}${reason})`;
}

function toSocketError(error: unknown): GatewaySocketError {
  if (error instanceof GatewaySocketError) {
    return error;
  }
  return new GatewaySocketError({
    kind: 'connect-failed',
    message: describeThrown(error),
    cause: error,
  });
}
