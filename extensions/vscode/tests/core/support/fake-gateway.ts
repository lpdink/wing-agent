import type {
  SocketCloseInfo,
  SocketFactory,
  SocketHandlers,
  SocketLike,
} from '../../../src/core/transport/socket';

/**
 * In-process fake gateway for `src/core` tests.
 *
 * The production seam is a {@link SocketFactory}, so the fake is just "a factory
 * whose sockets the test drives": no network, no ports, no timing luck. Tests
 * decide exactly when the handshake, a frame or a close happens — which is what
 * makes the reconnect / chunking invariants testable at all.
 */

/** One fake socket: records what the client sent, and can emit server events. */
export class FakeSocket implements SocketLike {
  readyState = 1;
  readonly sent: string[] = [];
  closedWith: { readonly code: number; readonly reason: string } | null = null;
  /** Set by the test (auto-handshake) so it does not fire after a manual close. */
  handshakeSent = false;

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

  // ── drivers ──────────────────────────────────────────────────────────

  open(): void {
    this.handlers.onOpen();
  }

  /** Push a raw text frame. */
  push(text: string): void {
    this.handlers.onMessage(text);
  }

  /** Push a JSON payload as the gateway would (`wire_dump` output). */
  emit(payload: Record<string, unknown>): void {
    this.push(JSON.stringify(payload));
  }

  /** Send the handshake frame. */
  handshake(clientId = 'client-1'): void {
    this.handshakeSent = true;
    this.emit({ type: 'connected', client_id: clientId });
  }

  /** Emit a close frame from the gateway side. */
  serverClose(code: number, reason = '', wasClean = code === 1000): void {
    this.readyState = 3;
    const info: SocketCloseInfo = { code, reason, wasClean };
    this.handlers.onClose(info);
  }

  /** Emit a transport error. */
  serverError(detail: string): void {
    this.handlers.onError(detail);
  }

  /** Frames the client sent, parsed. */
  get frames(): Record<string, unknown>[] {
    return this.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
  }
}

/** A {@link SocketFactory} the tests fully control. */
export class FakeGateway {
  readonly sockets: FakeSocket[] = [];
  readonly urls: string[] = [];
  /** When true (default) every new socket sends its handshake on a microtask. */
  autoHandshake = true;
  /** Throw this instead of creating the next socket (connect failure). */
  failNextConnect: Error | null = null;
  handshakeFailed = false;

  private nextClientId = 1;

  readonly factory: SocketFactory = (url, handlers) => {
    this.urls.push(url);
    const failure = this.failNextConnect;
    this.failNextConnect = null;
    if (failure !== null) {
      throw failure;
    }
    const socket = new FakeSocket(handlers);
    this.sockets.push(socket);
    if (this.autoHandshake) {
      const clientId = `client-${this.nextClientId++}`;
      queueMicrotask(() => {
        if (socket.readyState === 1) {
          socket.handshake(clientId);
        }
      });
    }
    return socket;
  };

  /** Most recent socket. */
  get last(): FakeSocket {
    const socket = this.sockets[this.sockets.length - 1];
    if (socket === undefined) {
      throw new Error('no socket was created yet');
    }
    return socket;
  }

  get socketCount(): number {
    return this.sockets.length;
  }
}

/**
 * Split a payload into `count` `_chunk` envelope frames.
 *
 * Slices at code-unit boundaries on purpose: that is exactly what the wire
 * (UTF-8 byte splitting) can produce for multi-byte text, and the reassembler
 * must still restore the original string and count bytes correctly.
 */
export function chunkFrames(
  payload: string,
  options: { readonly id?: string; readonly count?: number; readonly ofType?: string } = {},
): string[] {
  const count = options.count ?? 2;
  const id = options.id ?? 'chunk-1';
  const ofType = options.ofType ?? 'sync_session';
  const size = Math.ceil(payload.length / count);
  const frames: string[] = [];
  for (let index = 0; index < count; index += 1) {
    const data = payload.slice(index * size, index === count - 1 ? payload.length : (index + 1) * size);
    frames.push(JSON.stringify({ type: '_chunk', id, index, count, of_type: ofType, data }));
  }
  return frames;
}

/** One hand-built envelope frame (used to poke at validation). */
export function chunkEnvelope(fields: Partial<Record<string, unknown>>): string {
  return JSON.stringify({
    type: '_chunk',
    id: 'chunk-1',
    index: 0,
    count: 2,
    of_type: 'sync_session',
    data: '{}',
    ...fields,
  });
}

// ============================================================
// Wire payload samples (shaped exactly like `wire_dump` output)
// ============================================================

export const META = {
  created_at: '2026-09-18T08:00:00.000000',
  request_id: 'req-1',
} as const;

/** Build a `sync_session` payload of roughly `pad` characters. */
export function syncSessionPayload(pad: number, sessionId = 'sess-1'): string {
  return JSON.stringify({
    type: 'sync_session',
    session_id: sessionId,
    messages: [{ role: 'assistant', content: 'x'.repeat(pad), uuid: 'm1' }],
    uncommitted: null,
    uncommitted_tools: [],
    events: [],
    turn_started_at: null,
    agent: null,
    name: 'Session',
    draft: null,
    ...META,
  });
}

/** Build a `text` event payload. */
export function textPayload(content: string): string {
  return JSON.stringify({ type: 'text', content, ...META });
}
