import type {
  HttpTransport,
  HttpTransportRequest,
  HttpTransportResponse,
  SocketCloseInfo,
  SocketFactory,
  SocketHandlers,
  SocketLike,
} from '../../../src/core';

/**
 * In-process fake gateway for `tests/host`.
 *
 * It is a *world*, not a mock of our client: the host under test talks to a real
 * `GatewayConnection` (over an injected `SocketFactory`) and a real
 * `GatewayHttpClient` (over an injected `HttpTransport`), so every assertion here
 * is about observable protocol behaviour — call order, frames, delivery — not
 * about internal calls into a class.
 *
 * Fidelity notes (design.md D11):
 *
 * - the WebSocket handshake is sent on a microtask, like a loopback server;
 * - `subscribe` attaches the route **and then** pushes `sync_session`
 *   synchronously, in one handler — the property that makes "replay before
 *   live" structural;
 * - events are delivered only to clients whose subscription covers the session,
 *   and dropped deliveries are recorded (`dropped`) so tests can prove that a
 *   closed tab stops receiving;
 * - every HTTP call is recorded with its headers, so `create → subscribe`
 *   ordering (and the `X-Client-Id` header) is assertable.
 */

export interface HttpCall {
  readonly method: string;
  readonly path: string;
  readonly query: Readonly<Record<string, string>>;
  readonly headers: Readonly<Record<string, string>>;
  readonly body: Record<string, unknown> | null;
}

export interface FakeSessionState {
  sessionId: string;
  workspace: string | null;
  name: string | null;
  draft: string | null;
  messages: Record<string, unknown>[];
  uncommitted: Record<string, unknown> | null;
  uncommittedTools: Record<string, unknown>[];
  events: Record<string, unknown>[];
  turnStartedAt: string | null;
  agent: Record<string, unknown> | null;
  /** `GET /api/session/info` runtime status (yolo / thinking / effort / stats). */
  runtime: FakeRuntimeState;
}

/** The `/api/session/info` values the host can read but never derives. */
export interface FakeRuntimeState {
  model: string;
  thinking: boolean;
  reasoningEffort: string | null;
  yolo: boolean;
  workdir: string | null;
  messageCount: number;
  totalTokens: number;
  contextWindowTokens: number;
  skillsInfo: string;
  systemPrompt: string;
}

export interface FakeSessionSeed {
  readonly sessionId?: string;
  readonly workspace?: string | null;
  readonly name?: string | null;
  readonly draft?: string | null;
  readonly messages?: readonly Record<string, unknown>[];
  readonly uncommitted?: Record<string, unknown> | null;
  readonly uncommittedTools?: readonly Record<string, unknown>[];
  readonly events?: readonly Record<string, unknown>[];
  readonly turnStartedAt?: string | null;
  readonly agent?: Record<string, unknown> | null;
  readonly runtime?: Partial<FakeRuntimeState>;
}

/** One fake socket: records what the client sent, and can emit server frames. */
export class FakeSocket implements SocketLike {
  readyState = 1;
  readonly sent: string[] = [];
  closedWith: { readonly code: number; readonly reason: string } | null = null;
  clientId = '';

  private readonly handlers: SocketHandlers;

  constructor(handlers: SocketHandlers) {
    this.handlers = handlers;
  }

  send(text: string): void {
    this.sent.push(text);
  }

  close(code = 1000, reason = ''): void {
    this.readyState = 3;
    this.closedWith = { code, reason };
  }

  // ── drivers (server side) ────────────────────────────────────────────

  emit(payload: Record<string, unknown>): void {
    if (this.readyState !== 1) {
      return;
    }
    this.handlers.onMessage(JSON.stringify(payload));
  }

  handshake(clientId: string): void {
    this.clientId = clientId;
    this.emit({ type: 'connected', client_id: clientId });
  }

  serverClose(code: number, reason = '', wasClean = code === 1000): void {
    this.readyState = 3;
    const info: SocketCloseInfo = { code, reason, wasClean };
    this.handlers.onClose(info);
  }

  /** Frames the client sent, parsed. */
  get frames(): Record<string, unknown>[] {
    return this.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
  }
}

export interface FakeGatewayOptions {
  /** Fixed clock for the fake's own timestamps. */
  readonly now?: () => number;
}

export class FakeGateway {
  readonly sockets: FakeSocket[] = [];
  readonly httpCalls: HttpCall[] = [];
  /** Events that found no subscriber (assert "the closed tab stopped receiving"). */
  readonly dropped: Record<string, unknown>[] = [];
  readonly sessions = new Map<string, FakeSessionState>();
  readonly subscriptions = new Map<string, Set<string>>();
  /** Session ids handed out by `create` / `fork`. */
  readonly createdOrder: string[] = [];

  /** `false` makes the next socket creation throw (connection refused). */
  refuseNextConnect: Error | null = null;
  /** Drop every socket instead of completing the next handshake. */
  handshakeNeverArrives = false;
  /** HTTP failure overrides: path → `{ status, body? }`. */
  readonly httpFailures = new Map<string, { status: number; body?: string }>();
  /** Gates that hold the *next* call to a path until the test releases it. */
  private readonly httpGates = new Map<string, { promise: Promise<void>; release: () => void }>();
  /** Extra material merged into every `create` response's session. */
  seedOnCreate: Partial<FakeSessionSeed> = {};
  /** True when `wing start` should be assumed to have succeeded. */
  healthUp = true;

  private nextClientId = 1;
  private nextSessionId = 1;
  private readonly now: () => number;

  constructor(options: FakeGatewayOptions = {}) {
    this.now = options.now ?? Date.now;
  }

  // ── I/O seams ───────────────────────────────────────────────────────

  readonly factory: SocketFactory = (_url, handlers) => {
    const failure = this.refuseNextConnect;
    this.refuseNextConnect = null;
    if (failure !== null) {
      throw failure;
    }
    const socket = new FakeSocket(handlers);
    this.sockets.push(socket);
    queueMicrotask(() => {
      if (socket.readyState === 1 && !this.handshakeNeverArrives) {
        socket.handshake(`client-${this.nextClientId++}`);
      }
    });
    return socket;
  };

  readonly transport: HttpTransport = {
    request: (request: HttpTransportRequest): Promise<HttpTransportResponse> => this.handleHttp(request),
  };

  // ── world ───────────────────────────────────────────────────────────

  get lastSocket(): FakeSocket {
    const socket = this.sockets[this.sockets.length - 1];
    if (socket === undefined) {
      throw new Error('no socket was created yet');
    }
    return socket;
  }

  /** Every frame every socket sent, in order, with the socket's client id. */
  get frames(): { clientId: string; frame: Record<string, unknown> }[] {
    return this.sockets.flatMap((socket) =>
      socket.frames.map((frame) => ({ clientId: socket.clientId, frame })),
    );
  }

  /** Calls recorded for one path suffix (e.g. `/api/session/subscribe`). */
  calls(path: string): HttpCall[] {
    return this.httpCalls.filter((call) => call.path === path);
  }

  seedSession(seed: FakeSessionSeed = {}): FakeSessionState {
    const sessionId = seed.sessionId ?? `sess-${this.nextSessionId++}`;
    const state: FakeSessionState = {
      sessionId,
      workspace: seed.workspace ?? '/workspace',
      name: seed.name ?? null,
      draft: seed.draft ?? null,
      messages: [...(seed.messages ?? [])],
      uncommitted: seed.uncommitted ?? null,
      uncommittedTools: [...(seed.uncommittedTools ?? [])],
      events: [...(seed.events ?? [])],
      turnStartedAt: seed.turnStartedAt ?? null,
      agent: seed.agent ?? null,
      runtime: {
        model: 'claude-sonnet-4-6',
        thinking: false,
        reasoningEffort: null,
        yolo: false,
        workdir: seed.workspace ?? '/workspace',
        messageCount: (seed.messages ?? []).length,
        totalTokens: 0,
        contextWindowTokens: 200_000,
        skillsInfo: '',
        systemPrompt: '',
        ...seed.runtime,
      },
    };
    this.sessions.set(sessionId, state);
    return state;
  }

  session(sessionId: string): FakeSessionState {
    const state = this.sessions.get(sessionId);
    if (state === undefined) {
      throw new Error(`unknown fake session ${sessionId}`);
    }
    return state;
  }

  /** Deliver an event to every client subscribed to its session (or broadcast). */
  emit(payload: Record<string, unknown>, options: { readonly broadcast?: boolean } = {}): void {
    const sessionId = typeof payload['session_id'] === 'string' ? payload['session_id'] : null;
    let delivered = false;
    for (const socket of this.sockets) {
      if (socket.readyState !== 1) {
        continue;
      }
      const routes = this.subscriptions.get(socket.clientId);
      const subscribed = sessionId !== null && routes?.has(sessionId) === true;
      if (options.broadcast === true || subscribed) {
        socket.emit(payload);
        delivered = true;
      }
    }
    if (!delivered) {
      this.dropped.push(payload);
    }
  }

  /** Push the `sync_session` replay for one session to one client. */
  pushSync(clientId: string, sessionId: string): void {
    const state = this.sessions.get(sessionId);
    if (state === undefined) {
      this.dropped.push({ type: 'sync_session', session_id: sessionId, reason: 'unknown session' });
      return;
    }
    const socket = this.sockets.find((candidate) => candidate.clientId === clientId);
    if (socket === undefined || socket.readyState !== 1) {
      this.dropped.push({ type: 'sync_session', session_id: sessionId, reason: 'no socket' });
      return;
    }
    socket.emit({
      type: 'sync_session',
      session_id: sessionId,
      messages: state.messages,
      uncommitted: state.uncommitted,
      uncommitted_tools: state.uncommittedTools,
      events: state.events,
      turn_started_at: state.turnStartedAt,
      agent: state.agent,
      name: state.name,
      draft: state.draft,
      created_at: new Date(this.now()).toISOString(),
      request_id: 'fake-sync',
    });
  }

  attach(clientId: string, sessionId: string): void {
    const routes = this.subscriptions.get(clientId) ?? new Set<string>();
    routes.add(sessionId);
    this.subscriptions.set(clientId, routes);
  }

  detach(clientId: string, sessionId: string): void {
    this.subscriptions.get(clientId)?.delete(sessionId);
  }

  /** Is this client subscribed to the session right now? */
  isSubscribed(clientId: string, sessionId: string): boolean {
    return this.subscriptions.get(clientId)?.has(sessionId) === true;
  }

  /** Hold the next call to `path` until `release()` (timing tests). */
  holdNext(path: string): { release: () => void } {
    let release: () => void = () => undefined;
    const promise = new Promise<void>((resolve) => {
      release = resolve;
    });
    const gate = {
      promise,
      release: () => {
        release();
        this.httpGates.delete(path);
      },
    };
    this.httpGates.set(path, gate);
    return { release: gate.release };
  }

  /** Kill every live socket (a gateway crash / network loss). */
  dropConnections(code = 1006, reason = 'connection lost'): void {
    for (const socket of this.sockets) {
      if (socket.readyState === 1) {
        socket.serverClose(code, reason, false);
      }
    }
  }

  // ── HTTP ────────────────────────────────────────────────────────────

  private async handleHttp(request: HttpTransportRequest): Promise<HttpTransportResponse> {
    const url = new URL(request.url);
    const body = request.body === null ? null : (JSON.parse(request.body) as Record<string, unknown>);
    const query: Record<string, string> = {};
    for (const [key, value] of url.searchParams.entries()) {
      query[key] = value;
    }
    const call: HttpCall = {
      method: request.method,
      path: url.pathname,
      query,
      headers: request.headers,
      body,
    };
    this.httpCalls.push(call);

    const gate = this.httpGates.get(url.pathname);
    if (gate !== undefined) {
      await gate.promise;
    }

    const failure = this.httpFailures.get(url.pathname);
    if (failure !== undefined) {
      this.httpFailures.delete(url.pathname);
      return {
        status: failure.status,
        body: failure.body ?? JSON.stringify({ error: 'fake failure', detail: null }),
      };
    }

    switch (url.pathname) {
      case '/api/health':
        if (!this.healthUp) {
          return { status: 503, body: JSON.stringify({ error: 'gateway starting' }) };
        }
        return json({
          service: 'wing-gateway',
          status: 'ok',
          version: '0.0.0-test',
          commit: null,
          uptime: 1,
        });

      case '/api/session/create': {
        const state = this.seedSession({
          ...this.seedOnCreate,
          workspace: asString(body?.['workspace']) ?? this.seedOnCreate.workspace ?? '/workspace',
        });
        this.createdOrder.push(state.sessionId);
        return json({
          session_id: state.sessionId,
          template_name: 'default',
          workspace: state.workspace,
          backend: 'file',
        });
      }

      case '/api/session/resume': {
        const sessionId = asString(body?.['session_id']);
        const state = sessionId === null ? undefined : this.sessions.get(sessionId);
        if (state === undefined) {
          return {
            status: 404,
            body: JSON.stringify({ error: 'session not found', session_id: sessionId }),
          };
        }
        return json({
          session_id: state.sessionId,
          template_name: null,
          workspace: state.workspace,
        });
      }

      case '/api/session/fork': {
        const sourceId = asString(body?.['source_session_id']);
        const targetUuid = asString(body?.['target_uuid']) ?? 'current';
        const source = sourceId === null ? undefined : this.sessions.get(sourceId);
        if (source === undefined) {
          return { status: 404, body: JSON.stringify({ error: 'session not found' }) };
        }
        const forked = this.seedSession({
          workspace: source.workspace,
          messages: targetUuid === 'current' ? [...source.messages] : [],
          draft: targetUuid === 'current' ? '' : `draft of ${targetUuid}`,
        });
        this.createdOrder.push(forked.sessionId);
        return json({ session_id: forked.sessionId, draft: forked.draft });
      }

      case '/api/session/subscribe': {
        const clientId = request.headers['X-Client-Id'] ?? '';
        const sessionId = asString(body?.['session_id']) ?? '';
        if (clientId === '') {
          return { status: 400, body: JSON.stringify({ error: 'missing X-Client-Id' }) };
        }
        if (!this.sessions.has(sessionId)) {
          return {
            status: 404,
            body: JSON.stringify({ error: `session '${sessionId}' not found` }),
          };
        }
        this.attach(clientId, sessionId);
        // The real gateway pushes the replay inside the subscribe handler,
        // after `route_attach` — model exactly that.
        this.pushSync(clientId, sessionId);
        return json({ ok: true });
      }

      case '/api/session/unsubscribe': {
        const clientId = request.headers['X-Client-Id'] ?? '';
        const sessionId = asString(body?.['session_id']) ?? '';
        this.detach(clientId, sessionId);
        return json({ ok: true });
      }

      case '/api/session/send': {
        const sessionId = asString(body?.['session_id']) ?? '';
        const requestId = asString(body?.['request_id']) ?? 'fake-request';
        return json({ ok: true, request_id: requestId, session_id: sessionId });
      }

      case '/api/session/interrupt': {
        const sessionId = asString(body?.['session_id']) ?? '';
        this.emit({ type: 'interrupted', session_id: sessionId, created_at: nowIso(this.now()) });
        return json({ ok: true });
      }

      case '/api/session/rewind': {
        const sessionId = asString(body?.['session_id']) ?? '';
        const state = this.sessions.get(sessionId);
        if (state !== undefined) {
          state.messages = state.messages.slice(0, Math.max(0, state.messages.length - 1));
          state.draft = 'rewound draft';
          this.pushSyncToSubscribers(sessionId);
          this.emit({
            type: 'branch_targets',
            session_id: sessionId,
            targets: [{ uuid: 'current', content: '(current)', role: 'user' }],
            created_at: nowIso(this.now()),
          });
        }
        return json({ ok: true, draft: state?.draft ?? null });
      }

      case '/api/session/info': {
        const sessionId = url.searchParams.get('session_id') ?? '';
        const state = this.sessions.get(sessionId);
        if (state === undefined) {
          return { status: 404, body: JSON.stringify({ error: `session '${sessionId}' not found` }) };
        }
        const runtime = state.runtime;
        return json({
          model: runtime.model,
          api_url: 'http://fake-provider/v1',
          tools: ['Bash', 'Read'],
          total_tokens: runtime.totalTokens,
          context_window_tokens: runtime.contextWindowTokens,
          thinking: runtime.thinking,
          reasoning_effort: runtime.reasoningEffort,
          yolo: runtime.yolo,
          session_name: state.name,
          workdir: runtime.workdir,
          status: 'idle',
          context_stats: {
            message_count: runtime.messageCount,
            total_tokens: runtime.totalTokens,
          },
          skills_info: runtime.skillsInfo,
          system_prompt: runtime.systemPrompt,
        });
      }

      case '/api/session/update': {
        const sessionId = asString(body?.['session_id']) ?? '';
        const state = this.sessions.get(sessionId);
        if (state !== undefined) {
          if (typeof body?.['yolo'] === 'boolean') {
            state.runtime.yolo = body['yolo'];
          }
          if (typeof body?.['thinking'] === 'boolean') {
            state.runtime.thinking = body['thinking'];
          }
          if (typeof body?.['reasoning_effort'] === 'string') {
            state.runtime.reasoningEffort = body['reasoning_effort'];
          }
          if (typeof body?.['model'] === 'string') {
            state.runtime.model = body['model'];
          }
          if (typeof body?.['title'] === 'string') {
            state.name = body['title'];
          }
          if (typeof body?.['workspace'] === 'string') {
            state.runtime.workdir = body['workspace'];
            state.workspace = body['workspace'];
          }
        }
        const agent = state?.agent ?? {};
        this.emit({
          type: 'session_state_changed',
          session_id: sessionId,
          model: body?.['model'] ?? null,
          thinking: body?.['thinking'] ?? null,
          reasoning_effort: body?.['reasoning_effort'] ?? null,
          yolo: body?.['yolo'] ?? null,
          title: body?.['title'] ?? null,
          agent: body?.['agent'] ?? null,
          created_at: nowIso(this.now()),
          ...(agent === null ? {} : {}),
        });
        return json({ ok: true });
      }

      case '/api/session/compact':
        return json({ ok: true, original_tokens: 12_345, compressed_tokens: 3_210 });

      case '/api/session/list': {
        const sessions = [...this.sessions.values()].map((state) => ({
          id: state.sessionId,
          name: state.name ?? firstUserContent(state),
          created_at: nowIso(this.now()),
          template_name: null,
          workspace: state.workspace,
          last_interaction: nowIso(this.now()),
          status: 'idle',
        }));
        return json({ sessions });
      }

      case '/api/session/branches': {
        const sessionId = url.searchParams.get('session_id') ?? '';
        const state = this.sessions.get(sessionId);
        const targets = (state?.messages ?? [])
          .filter((message) => message['role'] === 'user')
          .map((message) => ({
            uuid: asString(message['uuid']) ?? 'u',
            content: asString(message['content']) ?? '',
            role: 'user',
          }));
        targets.push({ uuid: 'current', content: '(current)', role: 'user' });
        return json({ targets });
      }

      case '/api/commands':
        return json({
          commands: [
            { name: '/init', aliases: [], description: 'Initialize the project', params: '' },
            { name: '/review', aliases: ['/rv'], description: 'Review changes', params: '<path>' },
          ],
        });

      case '/api/models':
        return json({
          providers: [
            { provider: 'anthropic', models: ['claude-sonnet-4-6', 'claude-opus-4-6'] },
            { provider: 'openai', models: ['gpt-5.2'] },
          ],
        });

      case '/api/agents':
        return json({ agents: ['default', 'explorer'], default_agent: 'default' });

      case '/api/tools':
        return json({ tools: [] });

      case '/api/system/reload':
        return json({ ok: true, results: [] });

      case '/api/shutdown':
        return json({ status: 'shutting_down' });

      default:
        return { status: 404, body: JSON.stringify({ error: `no fake route for ${url.pathname}` }) };
    }
  }

  /** Broadcast a fresh `sync_session` for a session to its subscribers. */
  pushSyncToSubscribers(sessionId: string): void {
    for (const [clientId, routes] of this.subscriptions.entries()) {
      if (routes.has(sessionId)) {
        this.pushSync(clientId, sessionId);
      }
    }
  }
}

function json(value: Record<string, unknown>): HttpTransportResponse {
  return { status: 200, body: JSON.stringify(value) };
}

function asString(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function nowIso(ms: number): string {
  return new Date(ms).toISOString();
}

/** First user message content (the gateway's `first_user_message` rule). */
function firstUserContent(state: FakeSessionState): string | null {
  for (const message of state.messages) {
    if (message['role'] === 'user' && typeof message['content'] === 'string') {
      return message['content'].slice(0, 100);
    }
  }
  return null;
}
