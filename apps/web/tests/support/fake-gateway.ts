/**
 * In-process fake gateway for the web tests.
 *
 * A *world*, not a mock of the client: the runtime under test builds a real
 * `GatewayConnection` (over an injected `SocketFactory`) and a real
 * `GatewayHttpClient` (over an injected `HttpTransport`), so every assertion is
 * about observable protocol behaviour — frames, call order, replay — never about
 * internal calls into a class. The pattern is ported from
 * `extensions/vscode/tests/host/support/fake-gateway.ts`.
 *
 * Fidelity notes that the tests rely on:
 *
 * - the handshake is sent on a microtask (like a loopback server);
 * - `POST /api/session/subscribe` answers 200 and *then* pushes `sync_session`
 *   **synchronously in the same handler** — replay before live is structural, not
 *   timing luck;
 * - an event is delivered only to the clients whose subscription covers that
 *   session, and dropped deliveries are counted so a test can prove that an
 *   unsubscribe really stopped the stream;
 * - the dial can be made to fail (`failDials`) or to drop after connect
 *   (`dropConnection()`), which is how the reconnect/replay tests drive the state
 *   machine without a real socket.
 */

import type {
  HttpTransport,
  HttpTransportRequest,
  HttpTransportResponse,
  SocketFactory,
  SocketHandlers,
  SocketLike,
} from '@wing-agent/client';

export interface HttpCall {
  readonly method: string;
  readonly path: string;
  readonly query: Readonly<Record<string, string>>;
  readonly headers: Readonly<Record<string, string>>;
  readonly body: Record<string, unknown> | null;
}

/** One session the fake gateway knows about. */
export interface FakeSession {
  readonly id: string;
  name: string | null;
  workspace: string | null;
  /** Wire status: `inactive | idle | working | waiting`. */
  status: string;
  lastInteraction: string | null;
  /** History projection (`serialize_message` shape) replayed by `sync_session`. */
  messages: Record<string, unknown>[];
  /** Fact events replayed by `sync_session` (decoded as `WingEvent`s). */
  events: Record<string, unknown>[];
  uncommitted: Record<string, unknown> | null;
  /** Unterminated tool calls of the snapshot (`SyncSessionEvent.uncommitted_tools`). */
  uncommittedTools: Record<string, unknown>[];
  draft: string | null;
  /** `GET /api/session/info`. */
  runtime: Record<string, unknown>;
}

export function makeSession(seed: Partial<FakeSession> & { id: string }): FakeSession {
  return {
    name: null,
    workspace: '/tmp/ws',
    status: 'idle',
    lastInteraction: '2026-10-01T12:00:00Z',
    messages: [],
    events: [],
    uncommitted: null,
    uncommittedTools: [],
    draft: null,
    runtime: {
      model: 'test-model',
      api_url: 'http://127.0.0.1:9/v1',
      tools: ['Bash'],
      total_tokens: 0,
      context_window_tokens: 200_000,
      thinking: false,
      reasoning_effort: null,
      yolo: false,
      session_name: null,
      workdir: '/tmp/ws',
      status: 'idle',
      context_stats: { message_count: 0, total_tokens: 0 },
      skills_info: '',
      system_prompt: '',
    },
    ...seed,
  };
}

/** One history message, in the shape `SyncSessionEvent.messages` carries. */
export function historyMessage(
  role: string,
  content: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return { role, content, uuid: `${role}-${content.slice(0, 8)}`, ...extra };
}

/** One fake socket: records what the client sent, and can emit server frames. */
export class FakeSocket implements SocketLike {
  readyState = 0;
  readonly sent: string[] = [];
  clientId: string | null = null;
  closedWith: { readonly code: number; readonly reason: string } | null = null;

  constructor(private readonly handlers: SocketHandlers) {}

  // ── client side (what GatewayConnection calls) ──────────────────────

  send(text: string): void {
    this.sent.push(text);
  }

  close(code = 1000, reason = ''): void {
    if (this.readyState === 3) {
      return;
    }
    this.readyState = 3;
    this.closedWith = { code, reason };
    // A real socket fires `close` asynchronously; the client's state machine
    // expects the callback, not a synchronous re-entry.
    queueMicrotask(() => {
      this.handlers.onClose({ code, reason, wasClean: true });
    });
  }

  // ── server side (what a test drives) ────────────────────────────────

  /** Complete the dial + handshake for `clientId`. */
  open(clientId: string): void {
    this.clientId = clientId;
    this.readyState = 1;
    this.handlers.onOpen();
    this.emit({ type: 'connected', client_id: clientId });
  }

  /** Push one server frame (raw wire payload). */
  emit(payload: Record<string, unknown>): void {
    if (this.readyState !== 1) {
      return;
    }
    this.handlers.onMessage(JSON.stringify(payload));
  }

  /** Simulate a transport-level loss (1006). */
  drop(reason = 'connection lost'): void {
    if (this.readyState === 3) {
      return;
    }
    this.readyState = 3;
    this.handlers.onClose({ code: 1006, reason, wasClean: false });
  }

  /** Simulate a rejected credential (the gateway's close code 4001). */
  rejectUnauthorized(reason = 'Unauthorized'): void {
    this.readyState = 3;
    this.handlers.onClose({ code: 4001, reason, wasClean: false });
  }

  /** Simulate a dial that never opens. */
  failDial(detail: string): void {
    this.readyState = 3;
    this.handlers.onError(detail);
    this.handlers.onClose({ code: 1006, reason: detail, wasClean: false });
  }

  /** Decoded `ClientRequest` frames the client sent (step 09's composer path). */
  clientFrames(): Record<string, unknown>[] {
    return this.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
  }
}

export interface FakeGatewayOptions {
  readonly sessions?: readonly FakeSession[];
  /** Dials that fail before opening the handshake (decremented per attempt). */
  readonly failDials?: number;
  readonly dialError?: string;
}

export class FakeGateway {
  readonly calls: HttpCall[] = [];
  readonly sockets: FakeSocket[] = [];
  readonly sessions = new Map<string, FakeSession>();
  /** Session ids that answer 404 (a gateway that restarted without them). */
  readonly missing = new Set<string>();
  /** Dials still to fail; decremented on every `socketFactory` call. */
  failDials: number;
  /** Every dial is closed with 4001 before the handshake (auth enabled). */
  unauthorized = false;
  dialError: string;
  /** Frames the fake gateway refused to deliver because nobody was subscribed. */
  droppedDeliveries = 0;

  private clientSeq = 0;
  private createdSeq = 0;
  private readonly routes = new Map<string, Set<string>>(); // client_id → session ids

  constructor(options: FakeGatewayOptions = {}) {
    for (const session of options.sessions ?? []) {
      this.sessions.set(session.id, session);
    }
    this.failDials = options.failDials ?? 0;
    this.dialError = options.dialError ?? 'connect failed: connection refused';
  }

  // ── seams injected into the runtime ─────────────────────────────────

  get socketFactory(): SocketFactory {
    return (_url, handlers) => {
      const socket = new FakeSocket(handlers);
      this.sockets.push(socket);
      queueMicrotask(() => {
        if (this.unauthorized) {
          // The gateway closes with 4001 *before* accepting: no handshake frame.
          socket.rejectUnauthorized();
          return;
        }
        if (this.failDials > 0) {
          this.failDials -= 1;
          socket.failDial(this.dialError);
          return;
        }
        this.clientSeq += 1;
        socket.open(`client-${this.clientSeq}`);
      });
      return socket;
    };
  }

  get transport(): HttpTransport {
    return {
      request: async (request: HttpTransportRequest): Promise<HttpTransportResponse> => {
        const parsed = new URL(request.url);
        const body = request.body === null ? null : (JSON.parse(request.body) as Record<string, unknown>);
        const call: HttpCall = {
          method: request.method,
          path: parsed.pathname,
          query: Object.fromEntries(parsed.searchParams.entries()),
          headers: request.headers,
          body,
        };
        this.calls.push(call);
        if (this.holdPaths.has(call.path)) {
          await this.hold();
        }
        return this.route(call);
      },
    };
  }

  /** Hold every response whose path is in {@link holdPaths} until released. */
  private async hold(): Promise<void> {
    this.heldGate ??= new Promise<void>((resolve) => {
      this.releaseHeldGate = resolve;
    });
    await this.heldGate;
  }

  /** Let every held response through (idempotent). */
  releaseHeld(): void {
    this.releaseHeldGate?.();
    this.releaseHeldGate = null;
    this.heldGate = null;
  }

  // ── drivers ─────────────────────────────────────────────────────────

  /** The socket of the most recent dial (the connected one, normally). */
  lastSocket(): FakeSocket | null {
    return this.sockets.at(-1) ?? null;
  }

  /** Push one event for a session to every subscribed client. */
  emit(sessionId: string, payload: Record<string, unknown>): void {
    const subscribers = this.subscribersOf(sessionId);
    if (subscribers.length === 0) {
      this.droppedDeliveries += 1;
      return;
    }
    for (const socket of subscribers) {
      socket.emit(payload);
    }
  }

  /** Push the session's current snapshot (what `subscribe` does by itself). */
  replay(sessionId: string): void {
    const session = this.sessions.get(sessionId);
    if (session === undefined) {
      return;
    }
    this.emit(sessionId, this.syncPayload(session));
  }

  /** Calls to one endpoint (method + path), for order/content assertions. */
  callsTo(method: string, path: string): HttpCall[] {
    return this.calls.filter((call) => call.method === method && call.path === path);
  }

  subscribersOf(sessionId: string): FakeSocket[] {
    const sockets: FakeSocket[] = [];
    for (const [clientId, sessionIds] of this.routes) {
      if (!sessionIds.has(sessionId)) {
        continue;
      }
      const socket = this.sockets.find((candidate) => candidate.clientId === clientId);
      if (socket !== undefined && socket.readyState === 1) {
        sockets.push(socket);
      }
    }
    return sockets;
  }

  // ── HTTP surface ────────────────────────────────────────────────────

  private route(call: HttpCall): HttpTransportResponse {
    switch (`${call.method} ${call.path}`) {
      case 'POST /api/session/create':
        return this.createSession();
      case 'POST /api/session/resume':
        return this.resume(call);
      case 'POST /api/session/subscribe':
        return this.subscribe(call);
      case 'POST /api/session/unsubscribe':
        return this.unsubscribe(call);
      case 'GET /api/session/list':
        if (this.listStatus !== 200) {
          return json({ error: 'list unavailable' }, this.listStatus);
        }
        return json({ sessions: [...this.sessions.values()].map((session) => this.summary(session)) });
      case 'GET /api/session/info':
        return this.info(call);
      default:
        return json({ error: 'not found', path: call.path }, 404);
    }
  }

  private createSession(): HttpTransportResponse {
    this.createdSeq += 1;
    const id = `created-${this.createdSeq}`;
    this.sessions.set(id, makeSession({ id, name: null, lastInteraction: null }));
    return json({ session_id: id, template_name: 'default', workspace: '/tmp/ws', backend: 'file' });
  }

  private resume(call: HttpCall): HttpTransportResponse {
    const id = sessionIdOf(call);
    if (this.missing.has(id) || !this.sessions.has(id)) {
      return json({ error: 'session not found' }, 404);
    }
    const session = this.sessions.get(id);
    return json({ session_id: id, template_name: 'default', workspace: session?.workspace ?? null });
  }

  /** Subscribe answers 404 for these ids (resume still works): the re-resume path. */
  readonly missingSubscribe = new Set<string>();
  /** Status code the list endpoint answers with (500 = "the gateway is sick"). */
  listStatus = 200;
  /**
   * Paths whose response waits for {@link releaseHeld} — how a test gets a request
   * *in flight* while it emits an event (the stale-response race of review r1 S2).
   */
  readonly holdPaths = new Set<string>();

  private heldGate: Promise<void> | null = null;
  private releaseHeldGate: (() => void) | null = null;

  private subscribe(call: HttpCall): HttpTransportResponse {
    const id = sessionIdOf(call);
    const clientId = call.headers['X-Client-Id'] ?? call.headers['x-client-id'] ?? '';
    if (this.missing.has(id) || this.missingSubscribe.has(id) || !this.sessions.has(id)) {
      return json({ error: 'session not found' }, 404);
    }
    const routes = this.routes.get(clientId) ?? new Set<string>();
    routes.add(id);
    this.routes.set(clientId, routes);
    // The property the whole replay design leans on: attach, then push the
    // snapshot, in one handler — replay is always before the following live event.
    this.replay(id);
    return json({ ok: true });
  }

  private unsubscribe(call: HttpCall): HttpTransportResponse {
    const id = sessionIdOf(call);
    const clientId = call.headers['X-Client-Id'] ?? call.headers['x-client-id'] ?? '';
    this.routes.get(clientId)?.delete(id);
    return json({ ok: true });
  }

  private info(call: HttpCall): HttpTransportResponse {
    const id = call.query['session_id'] ?? '';
    const session = this.sessions.get(id);
    if (session === undefined) {
      return json({ error: 'session not found' }, 404);
    }
    // A complete `SessionInfoResponse`: the decoder is strict about the scalars it
    // requires (that is what keeps a half-filled runtime state out of the UI).
    return json({ ...session.runtime, session_name: session.name, status: session.status });
  }

  private summary(session: FakeSession): Record<string, unknown> {
    return {
      id: session.id,
      name: session.name,
      created_at: '2026-09-30T08:00:00Z',
      template_name: 'default',
      workspace: session.workspace,
      last_interaction: session.lastInteraction,
      status: session.status,
    };
  }

  private syncPayload(session: FakeSession): Record<string, unknown> {
    return {
      type: 'sync_session',
      session_id: session.id,
      created_at: '2026-10-01T12:00:00Z',
      request_id: 'sync-request',
      messages: session.messages,
      uncommitted: session.uncommitted,
      uncommitted_tools: session.uncommittedTools,
      events: session.events,
      status: session.status,
      turn_started_at: null,
      agent: null,
      name: session.name,
      draft: session.draft,
    };
  }
}

function json(value: unknown, status = 200): HttpTransportResponse {
  return { status, body: JSON.stringify(value) };
}

/** The `session_id` of a request body, as a string (`''` when absent/wrong type). */
function sessionIdOf(call: HttpCall): string {
  const value = call.body?.['session_id'];
  return typeof value === 'string' ? value : '';
}
