/**
 * WebSocket transport seam.
 *
 * `src/core` never touches `addEventListener` itself: it receives a
 * {@link SocketLike} whose callbacks are already wired to {@link SocketHandlers}
 * by a {@link SocketFactory}. That keeps listener lifecycle in exactly one place
 * (the adapter below, which removes every listener on close) and makes the
 * connection state machine fully testable with an in-process fake.
 *
 * The default factory uses `globalThis.WebSocket` (Node ≥ 22 / Electron main),
 * so the layer has no runtime dependency beyond the platform.
 */

/** `WebSocket.readyState` values (RFC 6455 / WHATWG). */
export const SOCKET_CONNECTING = 0;
export const SOCKET_OPEN = 1;
export const SOCKET_CLOSING = 2;
export const SOCKET_CLOSED = 3;

export interface SocketCloseInfo {
  readonly code: number;
  /** Human-readable close reason (may be empty). */
  readonly reason: string;
  /** `true` when the closing handshake completed. */
  readonly wasClean: boolean;
}

export interface SocketHandlers {
  readonly onOpen: () => void;
  /** Text frame; binary frames are decoded as UTF-8 text before this is called. */
  readonly onMessage: (text: string) => void;
  readonly onClose: (info: SocketCloseInfo) => void;
  /** Transport-level error report; a `close` normally follows. */
  readonly onError: (detail: string) => void;
}

/** The handle the connection keeps for one socket. */
export interface SocketLike {
  readonly readyState: number;
  send(text: string): void;
  close(code?: number, reason?: string): void;
}

/** Opens one socket and reports its events. Called once per connection attempt. */
export type SocketFactory = (url: string, handlers: SocketHandlers) => SocketLike;

/**
 * Structural subset of the WHATWG `WebSocket` this adapter relies on.
 *
 * Declared locally so a test can inject a tiny fake and so the DOM/undici types
 * (which `tsconfig.node.json` deliberately does not include) stay out of `src/core`.
 */
export interface WebSocketLike {
  readyState: number;
  binaryType: string;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  addEventListener(type: string, listener: (event: unknown) => void): void;
  removeEventListener(type: string, listener: (event: unknown) => void): void;
}

export interface WebSocketConstructor {
  new (url: string): WebSocketLike;
}

export interface NativeSocketFactoryOptions {
  /** Override the implementation (tests); defaults to `globalThis.WebSocket`. */
  readonly WebSocketImpl?: WebSocketConstructor;
}

const decoder = new TextDecoder();

/** Attach the handlers to a live socket and return the connection handle. */
export function attachNativeSocket(socket: WebSocketLike, handlers: SocketHandlers): SocketLike {
  const detach = (): void => {
    socket.removeEventListener('open', onOpen);
    socket.removeEventListener('message', onMessage);
    socket.removeEventListener('close', onClose);
    socket.removeEventListener('error', onError);
  };
  const onOpen = (): void => {
    handlers.onOpen();
  };
  const onMessage = (event: unknown): void => {
    const text = frameText(event);
    if (text === null) {
      handlers.onError('unsupported WebSocket frame (neither text nor binary)');
      return;
    }
    handlers.onMessage(text);
  };
  const onClose = (event: unknown): void => {
    const info = closeInfo(event);
    detach();
    handlers.onClose(info);
  };
  const onError = (event: unknown): void => {
    handlers.onError(errorDetail(event));
  };

  socket.addEventListener('open', onOpen);
  socket.addEventListener('message', onMessage);
  socket.addEventListener('close', onClose);
  socket.addEventListener('error', onError);
  // Deterministic binary handling: the gateway only sends text, but a future
  // payload should arrive as bytes we can decode rather than as an opaque Blob.
  socket.binaryType = 'arraybuffer';

  return {
    get readyState(): number {
      return socket.readyState;
    },
    send: (text: string): void => {
      socket.send(text);
    },
    close: (code?: number, reason?: string): void => {
      socket.close(code, reason);
    },
  };
}

/** Create the factory that talks to the platform `WebSocket`. */
export function createNativeSocketFactory(options: NativeSocketFactoryOptions = {}): SocketFactory {
  const { WebSocketImpl } = options;
  return (url, handlers) => {
    const impl = WebSocketImpl ?? readGlobalWebSocket();
    if (impl === undefined) {
      throw new Error('no global WebSocket implementation available (Node >= 22 or Electron required)');
    }
    return attachNativeSocket(new impl(url), handlers);
  };
}

function readGlobalWebSocket(): WebSocketConstructor | undefined {
  const candidate: unknown = globalThis.WebSocket;
  return isWebSocketConstructor(candidate) ? candidate : undefined;
}

function isWebSocketConstructor(value: unknown): value is WebSocketConstructor {
  return typeof value === 'function';
}

/** Text of a message event; binary frames are decoded as UTF-8. */
function frameText(event: unknown): string | null {
  const data = (event as { data?: unknown } | null)?.data;
  if (typeof data === 'string') {
    return data;
  }
  if (data instanceof ArrayBuffer) {
    return decoder.decode(data);
  }
  if (ArrayBuffer.isView(data)) {
    return decoder.decode(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
  }
  return null;
}

function closeInfo(event: unknown): SocketCloseInfo {
  const source = (event ?? {}) as { code?: unknown; reason?: unknown; wasClean?: unknown };
  return {
    code: typeof source.code === 'number' ? source.code : 0,
    reason: typeof source.reason === 'string' ? source.reason : '',
    wasClean: source.wasClean === true,
  };
}

function errorDetail(event: unknown): string {
  if (event instanceof Error) {
    return event.message;
  }
  const message = (event as { message?: unknown } | null)?.message;
  if (typeof message === 'string' && message !== '') {
    return message;
  }
  return 'WebSocket error';
}
