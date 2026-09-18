import { once } from 'node:events';

import { afterEach, describe, expect, it, vi } from 'vitest';
import { WebSocketServer } from 'ws';

import { GatewayHttpError } from '../../src/core/errors';
import {
  type FetchLike,
  type FetchResponseLike,
  type SocketHandlers,
  SOCKET_CLOSED,
  attachNativeSocket,
  createFetchTransport,
  createNativeSocketFactory,
  describeSocketRuntime,
} from '../../src/core/transport';
import type { WebSocketLike } from '../../src/core/transport/socket';

/**
 * Transport adapters — the only two places `src/core` touches platform I/O.
 *
 * The socket adapter is exercised against a fake `globalThis.WebSocket`, which
 * proves the *event mapping* and, just as importantly, that every listener is
 * removed on close (a leaked listener on a long-lived extension host is a slow
 * memory leak that no other test would catch). The bundled `ws` fallback gets a
 * real server (review #109 [P1-3]) — a stub would not prove the handshake works.
 */

/** Minimal deferred, so the fallback test can await the real socket's `open`. */
function deferred(): { readonly promise: Promise<void>; resolve(): void } {
  let resolve: () => void = () => undefined;
  const promise = new Promise<void>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

class FakeWebSocket implements WebSocketLike {
  static readonly instances: FakeWebSocket[] = [];

  readyState = 1;
  binaryType = 'blob';
  readonly sent: string[] = [];
  closedWith: { code: number | undefined; reason: string | undefined } | null = null;

  private readonly listeners = new Map<string, ((event: unknown) => void)[]>();

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  addEventListener(type: string, listener: (event: unknown) => void): void {
    const list = this.listeners.get(type) ?? [];
    list.push(listener);
    this.listeners.set(type, list);
  }

  removeEventListener(type: string, listener: (event: unknown) => void): void {
    const list = this.listeners.get(type) ?? [];
    this.listeners.set(
      type,
      list.filter((candidate) => candidate !== listener),
    );
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(code?: number, reason?: string): void {
    this.readyState = 3;
    this.closedWith = { code, reason };
  }

  /** Test driver: dispatch an event to the registered listeners. */
  emit(type: string, event: unknown): void {
    for (const listener of [...(this.listeners.get(type) ?? [])]) {
      listener(event);
    }
  }

  get listenerCount(): number {
    let total = 0;
    for (const list of this.listeners.values()) {
      total += list.length;
    }
    return total;
  }
}

interface RecordedHandlers extends SocketHandlers {
  readonly messages: string[];
  readonly closes: { code: number; reason: string; wasClean: boolean }[];
  readonly errors: string[];
  readonly opens: number;
}

function recordHandlers(): RecordedHandlers {
  const messages: string[] = [];
  const closes: { code: number; reason: string; wasClean: boolean }[] = [];
  const errors: string[] = [];
  const state = { opens: 0 };
  return {
    messages,
    closes,
    errors,
    get opens() {
      return state.opens;
    },
    onOpen: () => {
      state.opens += 1;
    },
    onMessage: (text) => messages.push(text),
    onClose: (info) => closes.push(info),
    onError: (detail) => errors.push(detail),
  };
}

afterEach(() => {
  FakeWebSocket.instances.length = 0;
  RecordingWebSocket.options.length = 0;
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('native WebSocket adapter', () => {
  it('opens the socket, wires every handler and requests array buffers', () => {
    const handlers = recordHandlers();
    const factory = createNativeSocketFactory({ WebSocketImpl: FakeWebSocket });
    const socket = factory('ws://127.0.0.1:32523/ws', handlers);

    const raw = FakeWebSocket.instances[0];
    expect(raw?.url).toBe('ws://127.0.0.1:32523/ws');
    expect(raw?.binaryType).toBe('arraybuffer');
    expect(raw?.listenerCount).toBe(4);

    raw?.emit('open', {});
    raw?.emit('message', { data: 'hello' });
    expect(handlers.opens).toBe(1);
    expect(handlers.messages).toStrictEqual(['hello']);

    expect(socket.readyState).toBe(1);
    socket.send('out');
    expect(raw?.sent).toStrictEqual(['out']);
  });

  it('decodes binary frames as UTF-8 text', () => {
    const handlers = recordHandlers();
    const socket = attachNativeSocket(new FakeWebSocket('ws://x'), handlers);
    const raw = FakeWebSocket.instances[0];
    const bytes = new TextEncoder().encode('多字节 🙂 payload');

    raw?.emit('message', { data: bytes.buffer });
    raw?.emit('message', { data: new DataView(bytes.buffer, 0, bytes.byteLength) });
    expect(handlers.messages).toStrictEqual(['多字节 🙂 payload', '多字节 🙂 payload']);
    expect(socket.readyState).toBe(1);
  });

  it('reports an unsupported frame instead of silently dropping it', () => {
    const handlers = recordHandlers();
    attachNativeSocket(new FakeWebSocket('ws://x'), handlers);
    FakeWebSocket.instances[0]?.emit('message', { data: { not: 'a frame' } });

    expect(handlers.messages).toStrictEqual([]);
    expect(handlers.errors[0]).toContain('unsupported WebSocket frame');
  });

  it('maps the close frame and detaches every listener', () => {
    const handlers = recordHandlers();
    attachNativeSocket(new FakeWebSocket('ws://x'), handlers);
    const raw = FakeWebSocket.instances[0];

    raw?.emit('close', { code: 4001, reason: 'Unauthorized', wasClean: false });

    expect(handlers.closes).toStrictEqual([{ code: 4001, reason: 'Unauthorized', wasClean: false }]);
    expect(raw?.listenerCount).toBe(0);
  });

  it('defaults a close frame without a code to 0 (same as the Rust mirror)', () => {
    const handlers = recordHandlers();
    attachNativeSocket(new FakeWebSocket('ws://x'), handlers);
    FakeWebSocket.instances[0]?.emit('close', {});

    expect(handlers.closes).toStrictEqual([{ code: 0, reason: '', wasClean: false }]);
  });

  it('normalises error events to a message', () => {
    const handlers = recordHandlers();
    attachNativeSocket(new FakeWebSocket('ws://x'), handlers);
    const raw = FakeWebSocket.instances[0];

    raw?.emit('error', new Error('reset by peer'));
    raw?.emit('error', { message: 'boom' });
    raw?.emit('error', {});
    expect(handlers.errors).toStrictEqual(['reset by peer', 'boom', 'WebSocket error']);
  });

  it('forwards close() to the socket', () => {
    const handlers = recordHandlers();
    const socket = attachNativeSocket(new FakeWebSocket('ws://x'), handlers);
    socket.close(1000, 'bye');
    expect(FakeWebSocket.instances[0]?.closedWith).toStrictEqual({ code: 1000, reason: 'bye' });
  });

  it('uses the global WebSocket when none is injected', () => {
    vi.stubGlobal('WebSocket', FakeWebSocket);
    const handlers = recordHandlers();
    createNativeSocketFactory()('ws://global/ws', handlers);
    expect(FakeWebSocket.instances[0]?.url).toBe('ws://global/ws');
  });
});

/** A `WebSocketLike` that records the constructor's second argument. */
class RecordingWebSocket extends FakeWebSocket {
  static readonly options: unknown[] = [];

  constructor(url: string, options?: unknown) {
    super(url);
    RecordingWebSocket.options.push(options);
  }
}

/** Resolve once `predicate` holds (real timers; the fallback test talks to a real server). */
async function waitUntil(predicate: () => boolean, timeoutMs = 5_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error('timed out waiting for the socket');
    }
    await new Promise((resolve) => {
      setTimeout(resolve, 5);
    });
  }
}

/**
 * Review #109 [P1-3]: the extension declares `engines.vscode ^1.100.0`
 * (Electron 34 / Node 20.19) while `globalThis.WebSocket` only appears in Node
 * 21 — so the default factory must fall back to a **real** WebSocket client, and
 * a host that *does* have a global must keep using it.
 */
describe('socket implementation resolution', () => {
  it('forwards handshake headers only when the caller supplied them', () => {
    createNativeSocketFactory({ WebSocketImpl: RecordingWebSocket })('ws://h:1/ws', recordHandlers(), {
      headers: { Authorization: 'Bearer secret' },
    });
    createNativeSocketFactory({ WebSocketImpl: RecordingWebSocket })('ws://h:1/ws', recordHandlers());

    // A DOM-shaped `WebSocket` takes protocols as its second argument, so an
    // options object must never be passed when there is nothing to send.
    expect(RecordingWebSocket.options).toStrictEqual([
      { headers: { Authorization: 'Bearer secret' } },
      undefined,
    ]);
  });

  it('prefers an injected implementation over the global one', () => {
    let globalConstructions = 0;
    class GlobalSocket extends FakeWebSocket {
      constructor(url: string) {
        super(url);
        globalConstructions += 1;
      }
    }
    vi.stubGlobal('WebSocket', GlobalSocket);
    createNativeSocketFactory({ WebSocketImpl: RecordingWebSocket })('ws://injected/ws', recordHandlers());

    expect(RecordingWebSocket.options).toStrictEqual([undefined]);
    expect(globalConstructions).toBe(0);
  });

  it('describes the runtime for connect-failure reports (node version included)', () => {
    vi.stubGlobal('WebSocket', FakeWebSocket);
    expect(describeSocketRuntime()).toBe(`node ${process.versions.node}, global WebSocket available`);

    vi.stubGlobal('WebSocket', undefined);
    expect(describeSocketRuntime()).toBe(
      `node ${process.versions.node}, no global WebSocket (using the bundled ws client)`,
    );
  });

  it('connects to a real server through the bundled ws fallback (Node 20 hosts)', async () => {
    // The exact situation on a VS Code 1.100 host: no global WebSocket at all.
    vi.stubGlobal('WebSocket', undefined);
    const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
    try {
      await once(server, 'listening');
      const address = server.address();
      if (address === null || typeof address === 'string') {
        throw new Error('no server address');
      }
      server.on('connection', (socket) => {
        socket.on('message', (data: unknown) => socket.send(`echo:${String(data)}`));
      });

      const opened = deferred();
      const handlers = recordHandlers();
      const socket = createNativeSocketFactory()(`ws://127.0.0.1:${address.port}/ws`, {
        ...handlers,
        onOpen: () => {
          handlers.onOpen();
          opened.resolve();
        },
      });
      await opened.promise;
      expect(handlers.opens).toBe(1);

      socket.send('ping');
      await waitUntil(() => handlers.messages.length > 0);
      expect(handlers.messages).toStrictEqual(['echo:ping']);

      socket.close(1000, 'bye');
      await waitUntil(() => handlers.closes.length > 0);
      expect(handlers.closes[0]).toStrictEqual({ code: 1000, reason: 'bye', wasClean: true });
      expect(socket.readyState).toBe(SOCKET_CLOSED);
    } finally {
      await new Promise<void>((resolve) => {
        server.close(() => resolve());
      });
    }
  });
});

function fetchResponse(status: number, body: string): FetchResponseLike {
  return { status, text: () => Promise.resolve(body) };
}

describe('fetch transport', () => {
  it('performs the round trip and forwards method / headers / body / signal', async () => {
    const calls: { url: string; init: { method: string; body: string | undefined; hasSignal: boolean } }[] =
      [];
    const fetchImpl: FetchLike = (url, init) => {
      calls.push({
        url,
        init: { method: init.method, body: init.body, hasSignal: init.signal !== undefined },
      });
      return Promise.resolve(fetchResponse(200, '{"ok":true}'));
    };
    const transport = createFetchTransport({ fetchImpl });

    const response = await transport.request({
      method: 'POST',
      url: 'http://127.0.0.1:32523/api/session/send',
      headers: { 'Content-Type': 'application/json' },
      body: '{"content":"hi"}',
      timeoutMs: 5_000,
    });

    expect(response).toStrictEqual({ status: 200, body: '{"ok":true}' });
    expect(calls).toStrictEqual([
      {
        url: 'http://127.0.0.1:32523/api/session/send',
        init: { method: 'POST', body: '{"content":"hi"}', hasSignal: true },
      },
    ]);
  });

  it('rejects with kind timeout and aborts the request', async () => {
    vi.useFakeTimers();
    let aborted = false;
    const fetchImpl: FetchLike = (_url, init) =>
      new Promise<FetchResponseLike>(() => {
        init.signal.addEventListener('abort', () => {
          aborted = true;
        });
      });
    const transport = createFetchTransport({ fetchImpl });

    const pending = transport.request({
      method: 'GET',
      url: 'http://127.0.0.1:32523/api/health',
      headers: {},
      body: null,
      timeoutMs: 250,
    });
    const rejection = expect(pending).rejects.toMatchObject({ name: 'GatewayHttpError', kind: 'timeout' });

    await vi.advanceTimersByTimeAsync(250);
    await rejection;
    expect(aborted).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('rejects a timeout even when the implementation ignores the abort signal', async () => {
    vi.useFakeTimers();
    const transport = createFetchTransport({
      // Resolves far too late and never observes the signal.
      fetchImpl: () =>
        new Promise<FetchResponseLike>((resolve) => {
          setTimeout(() => resolve(fetchResponse(200, 'late')), 10_000);
        }),
    });

    const pending = transport.request({
      method: 'GET',
      url: 'http://x/health',
      headers: {},
      body: null,
      timeoutMs: 100,
    });
    const rejection = expect(pending).rejects.toMatchObject({ kind: 'timeout' });
    await vi.advanceTimersByTimeAsync(100);
    await rejection;
  });

  it('classifies a thrown fetch error as a network failure', async () => {
    const transport = createFetchTransport({
      fetchImpl: () => Promise.reject(new Error('ECONNREFUSED 127.0.0.1:32523')),
    });

    await expect(
      transport.request({
        method: 'GET',
        url: 'http://x/api/health',
        headers: {},
        body: null,
        timeoutMs: 1_000,
      }),
    ).rejects.toMatchObject({ kind: 'network', message: expect.stringContaining('ECONNREFUSED') });
  });

  it('classifies a body read failure as a network failure', async () => {
    const transport = createFetchTransport({
      fetchImpl: () =>
        Promise.resolve({
          status: 200,
          text: () => Promise.reject(new Error('stream closed')),
        }),
    });

    await expect(
      transport.request({
        method: 'GET',
        url: 'http://x/api/health',
        headers: {},
        body: null,
        timeoutMs: 1_000,
      }),
    ).rejects.toMatchObject({ kind: 'network', message: expect.stringContaining('stream closed') });
  });

  it('classifies a synchronous fetch throw as a network failure and leaves no timer', async () => {
    vi.useFakeTimers();
    const transport = createFetchTransport({
      fetchImpl: () => {
        throw new Error('bad url');
      },
    });

    await expect(
      transport.request({
        method: 'POST',
        url: 'http://x/api/session/send',
        headers: {},
        body: '{}',
        timeoutMs: 5_000,
      }),
    ).rejects.toMatchObject({ kind: 'network' });
    expect(vi.getTimerCount()).toBe(0);
  });

  it('reports a missing global fetch as a config error', () => {
    vi.stubGlobal('fetch', undefined);
    expect(() => createFetchTransport()).toThrowError(GatewayHttpError);
    expect(() => createFetchTransport()).toThrowError(/no global fetch implementation/);
  });

  it('uses the global fetch when none is injected', async () => {
    const seen: string[] = [];
    vi.stubGlobal('fetch', (url: string) => {
      seen.push(url);
      return Promise.resolve(fetchResponse(200, '{"status":"ok"}'));
    });

    const transport = createFetchTransport();
    const response = await transport.request({
      method: 'GET',
      url: 'http://x/api/health',
      headers: {},
      body: null,
      timeoutMs: 1_000,
    });
    expect(response.status).toBe(200);
    expect(seen).toStrictEqual(['http://x/api/health']);
  });
});
