/**
 * The screenshot fixture server: the *built* app plus a fake gateway on one origin.
 *
 * A real HTTP + WebSocket server rather than Playwright's route interception, for
 * one reason: the page then exercises its real `fetch` / `WebSocket` path against a
 * same-origin gateway — exactly the production shape (step 03 hosts the built
 * assets on the gateway, and the default settings talk to the page's own origin).
 * Requests never leave 127.0.0.1, the port is OS-assigned, and the user's own
 * gateway port is refused outright (the same rule the VS Code smoke follows).
 *
 * Protocol fidelity that matters here:
 *
 * - `/ws` answers the handshake with `{type:"connected", client_id}`;
 * - `POST /api/session/subscribe` answers 200 and *then* pushes `sync_session` —
 *   the replay the shell renders;
 * - `/api/session/info` answers a complete `SessionInfoResponse` (the decoder is
 *   strict, and the main pane shows those fields);
 * - `dropAfterSubscribe` closes the socket right after that replay, which is how
 *   the "connection lost — reconnecting" banner is produced.
 */

import { readFile, stat } from 'node:fs/promises';
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import path from 'node:path';

import { WebSocketServer, type RawData, type WebSocket } from 'ws';

/** One session in the fixture world. */
export interface WorldSession {
  readonly id: string;
  readonly name: string | null;
  readonly workspace: string | null;
  /** Wire status: `inactive | idle | working | waiting`. */
  readonly status: string;
  readonly lastInteraction: string | null;
  readonly messages: readonly Record<string, unknown>[];
  readonly events: readonly Record<string, unknown>[];
  readonly info: {
    readonly model: string;
    readonly thinking: boolean;
    readonly reasoningEffort: string | null;
    readonly yolo: boolean;
    readonly workdir: string;
    readonly usedTokens: number;
    readonly windowTokens: number;
    readonly messageCount: number;
  };
}

export interface World {
  readonly sessions: readonly WorldSession[];
  /** Close the socket once the subscribe replay has been delivered. */
  readonly dropAfterSubscribe?: boolean;
}

export interface FixtureServer {
  readonly url: string;
  readonly port: number;
  close(): Promise<void>;
}

/** What one request handler needs: the assets, the world, and the socket set. */
interface FixtureContext {
  readonly distDir: string;
  readonly world: World;
  broadcast(payload: Record<string, unknown>): void;
  drop(): void;
}

/**
 * The user's own gateway — never touch it, not even by accident.
 *
 * `listen(0)` does not hand out 32523 on any real system (it is below the ephemeral
 * range), so this is an invariant guard rather than a live filter: it is what stops
 * a future "let me just pin the port for reproducibility" change from taking over
 * the gateway the user is running. Covered by `tests/shot-server.test.ts`.
 */
export const FORBIDDEN_PORT = 32_523;

/** Throw when `port` is the user's gateway. Exported so the guard is testable. */
export function assertPortAllowed(port: number): void {
  if (port === FORBIDDEN_PORT) {
    throw new Error(`refusing to bind the user's gateway port ${FORBIDDEN_PORT}`);
  }
}

const CONTENT_TYPES: Readonly<Record<string, string>> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
};

export async function startFixtureServer(options: {
  readonly distDir: string;
  readonly world: World;
}): Promise<FixtureServer> {
  const clients = new Set<WebSocket>();
  const context: FixtureContext = {
    distDir: options.distDir,
    world: options.world,
    broadcast: (payload) => {
      for (const ws of clients) {
        send(ws, payload);
      }
    },
    drop: () => {
      for (const ws of clients) {
        ws.close(1006, 'fixture: dropped after subscribe');
      }
    },
  };

  const server = createServer((request, response) => {
    void handleRequest(request, response, context).catch((error: unknown) => {
      if (!response.headersSent) {
        response.writeHead(500, { 'content-type': 'text/plain' });
      }
      response.end(`fixture error: ${String(error)}`);
    });
  });
  const wss = new WebSocketServer({ noServer: true });

  server.on('upgrade', (request, socket, head) => {
    if (new URL(request.url ?? '/', 'http://127.0.0.1').pathname !== '/ws') {
      socket.destroy();
      return;
    }
    wss.handleUpgrade(request, socket, head, (ws) => {
      clients.add(ws);
      ws.on('close', () => {
        clients.delete(ws);
      });
      handshake(ws, clients.size);
    });
  });

  const port = await listen(server);
  return {
    url: `http://127.0.0.1:${port}`,
    port,
    close: () =>
      new Promise<void>((resolve) => {
        for (const client of wss.clients) {
          client.terminate();
        }
        wss.close(() => {
          server.close(() => {
            resolve();
          });
        });
      }),
  };
}

/** Bind an ephemeral port on the loopback interface (never the user's gateway). */
async function listen(server: Server): Promise<number> {
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  const port = typeof address === 'object' && address !== null ? address.port : 0;
  try {
    assertPortAllowed(port);
  } catch (error) {
    server.close();
    throw error;
  }
  return port;
}

function handshake(ws: WebSocket, connectionIndex: number): void {
  ws.on('message', (data) => {
    const text = rawText(data);
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(text) as Record<string, unknown>;
    } catch {
      return;
    }
    // A `ClientRequest` (the step 09 composer path): echo `delivered`, like the
    // gateway does. The shell has no composer yet, so this only keeps the fixture
    // honest for the next step.
    if (typeof frame['request_id'] === 'string' && typeof frame['session_id'] === 'string') {
      send(ws, {
        type: 'delivered',
        session_id: frame['session_id'],
        request_id: frame['request_id'],
        created_at: '2026-10-01T12:00:00Z',
      });
    }
  });
  setImmediate(() => {
    send(ws, { type: 'connected', client_id: `shot-client-${connectionIndex}` });
  });
}

/** Text of a WebSocket frame (text frames arrive as strings, binary ones as bytes). */
function rawText(data: RawData): string {
  if (typeof data === 'string') {
    return data;
  }
  if (Array.isArray(data)) {
    return Buffer.concat(data).toString('utf8');
  }
  return Buffer.isBuffer(data) ? data.toString('utf8') : Buffer.from(data).toString('utf8');
}

/** A string field of a decoded request body (`''` when absent or not a string). */
function bodyString(body: Record<string, unknown>, key: string): string {
  const value = body[key];
  return typeof value === 'string' ? value : '';
}

function send(ws: WebSocket, payload: Record<string, unknown>): void {
  if (ws.readyState === ws.OPEN) {
    ws.send(JSON.stringify(payload));
  }
}

// ── HTTP ────────────────────────────────────────────────────────────────

async function handleRequest(
  request: IncomingMessage,
  response: ServerResponse,
  context: FixtureContext,
): Promise<void> {
  const url = new URL(request.url ?? '/', 'http://127.0.0.1');
  if (url.pathname.startsWith('/api/')) {
    await handleApi(request, response, url, context);
    return;
  }
  await serveStatic(response, context.distDir, url.pathname);
}

async function handleApi(
  request: IncomingMessage,
  response: ServerResponse,
  url: URL,
  context: FixtureContext,
): Promise<void> {
  const body = await readBody(request);
  const sessions = context.world.sessions;
  switch (`${request.method ?? 'GET'} ${url.pathname}`) {
    case 'GET /api/session/list':
      json(response, {
        sessions: sessions.map((session) => ({
          id: session.id,
          name: session.name,
          created_at: '2026-10-01T09:00:00Z',
          template_name: 'default',
          workspace: session.workspace,
          last_interaction: session.lastInteraction,
          status: session.status,
        })),
      });
      return;
    case 'POST /api/session/create': {
      const template = sessions[0];
      json(response, {
        session_id: `created-${Date.now()}`,
        template_name: 'default',
        workspace: template?.workspace ?? '/srv/app',
        backend: 'file',
      });
      return;
    }
    case 'POST /api/session/resume': {
      const id = bodyString(body, 'session_id');
      const session = sessions.find((candidate) => candidate.id === id);
      if (session === undefined) {
        json(response, { error: 'session not found' }, 404);
        return;
      }
      json(response, { session_id: id, template_name: 'default', workspace: session.workspace });
      return;
    }
    case 'POST /api/session/subscribe': {
      const id = bodyString(body, 'session_id');
      const session = sessions.find((candidate) => candidate.id === id);
      if (session === undefined) {
        json(response, { error: 'session not found' }, 404);
        return;
      }
      json(response, { ok: true });
      // The replay rides on the socket, right after the HTTP answer.
      setTimeout(() => {
        context.broadcast(syncPayload(session));
        if (context.world.dropAfterSubscribe === true) {
          setTimeout(() => {
            context.drop();
          }, 120);
        }
      }, 10);
      return;
    }
    case 'POST /api/session/unsubscribe':
      json(response, { ok: true });
      return;
    case 'GET /api/session/info': {
      const id = url.searchParams.get('session_id') ?? '';
      const session = sessions.find((candidate) => candidate.id === id);
      if (session === undefined) {
        json(response, { error: 'session not found' }, 404);
        return;
      }
      json(response, {
        model: session.info.model,
        api_url: 'http://127.0.0.1:9/v1',
        tools: ['Bash', 'Read', 'Edit', 'Write'],
        total_tokens: session.info.usedTokens,
        context_window_tokens: session.info.windowTokens,
        thinking: session.info.thinking,
        reasoning_effort: session.info.reasoningEffort,
        yolo: session.info.yolo,
        session_name: session.name,
        workdir: session.info.workdir,
        status: session.status,
        context_stats: { message_count: session.info.messageCount, total_tokens: session.info.usedTokens },
        skills_info: '',
        system_prompt: '',
      });
      return;
    }
    default:
      json(response, { error: 'not found', path: url.pathname }, 404);
  }
}

function syncPayload(session: WorldSession): Record<string, unknown> {
  return {
    type: 'sync_session',
    session_id: session.id,
    created_at: '2026-10-01T12:00:00Z',
    request_id: 'sync',
    messages: session.messages,
    uncommitted: null,
    uncommitted_tools: [],
    events: session.events,
    status: session.status,
    turn_started_at: session.status === 'working' ? '2026-10-01T12:09:00Z' : null,
    agent: null,
    name: session.name,
    draft: null,
  };
}

function json(response: ServerResponse, value: unknown, status = 200): void {
  const payload = JSON.stringify(value);
  response.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(payload),
  });
  response.end(payload);
}

async function readBody(request: IncomingMessage): Promise<Record<string, unknown>> {
  if (request.method !== 'POST') {
    return {};
  }
  const chunks: Buffer[] = [];
  for await (const chunk of request) {
    chunks.push(chunk as Buffer);
  }
  const text = Buffer.concat(chunks).toString('utf8');
  if (text === '') {
    return {};
  }
  try {
    return JSON.parse(text) as Record<string, unknown>;
  } catch {
    return {};
  }
}

/** Serve `dist/` with an SPA fallback, like step 03's gateway hosting. */
async function serveStatic(response: ServerResponse, distDir: string, pathname: string): Promise<void> {
  const relative = pathname === '/' ? '/index.html' : pathname;
  const target = path.join(distDir, relative);
  if (!target.startsWith(distDir)) {
    response.writeHead(403);
    response.end();
    return;
  }
  try {
    const stats = await stat(target);
    if (!stats.isFile()) {
      throw new Error('not a file');
    }
    const body = await readFile(target);
    response.writeHead(200, {
      'content-type': CONTENT_TYPES[path.extname(target)] ?? 'application/octet-stream',
      'content-length': body.byteLength,
    });
    response.end(body);
  } catch {
    const body = await readFile(path.join(distDir, 'index.html'));
    response.writeHead(200, {
      'content-type': 'text/html; charset=utf-8',
      'content-length': body.byteLength,
    });
    response.end(body);
  }
}
