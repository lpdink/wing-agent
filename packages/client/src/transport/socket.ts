/**
 * WebSocket transport seam.
 *
 * The client package never touches `addEventListener` itself: it receives a
 * {@link SocketLike} whose callbacks are already wired to {@link SocketHandlers}
 * by a {@link SocketFactory}. That keeps listener lifecycle in exactly one place
 * (the adapter below, which removes every listener on close) and makes the
 * connection state machine fully testable with an in-process fake.
 *
 * **Which implementation is used (review #109 [P1-3]).** The default factory
 * resolves, in order: an injected implementation → `globalThis.WebSocket` →
 * the **bundled `ws` client**. The last step exists because the extension host
 * that we declare support for (`engines.vscode ^1.100.0` = Electron 34 / Node
 * 20.19, which is what `esbuild.mjs` targets) has **no** global `WebSocket`
 * (Node only exposes it from 21, stable in 22.4, and Electron does not inject
 * one) — without the fallback those users would get "not reachable (Node >= 22
 * required)" and no way to fix it.
 *
 * `ws` is a *devDependency* on purpose: it is bundled into `out/extension.js`
 * (same policy as react / markdown-it, which the webview bundle does the same
 * with — see `docs/dev/vscode-extension.md` §10), so the `.vsix` still has no
 * runtime dependency tree. The `require` is lazy and lives inside the factory
 * call: a Node ≥ 22 host never initialises the fallback.
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

/** How to open one socket. `headers` is `undefined` when auth is off. */
export interface SocketOpenOptions {
  /**
   * Request headers for the opening handshake.
   *
   * Only forwarded to implementations that accept them (undici's `WebSocket`,
   * `ws`); a DOM `WebSocket` takes protocols as its second argument, so it must
   * never receive this object — that is why it is passed only when non-empty.
   */
  readonly headers?: Readonly<Record<string, string>>;
}

/** Opens one socket and reports its events. Called once per connection attempt. */
export type SocketFactory = (
  url: string,
  handlers: SocketHandlers,
  options?: SocketOpenOptions,
) => SocketLike;

/**
 * Structural subset of the WHATWG `WebSocket` this adapter relies on.
 *
 * Declared locally so a test can inject a tiny fake and so the DOM/undici types
 * (which a DOM-only project does not include) stay out of the client layer.
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
  /** `options` is the undici / `ws` extension; a DOM implementation ignores it (we never pass it). */
  new (url: string, options?: SocketOpenOptions): WebSocketLike;
}

export interface NativeSocketFactoryOptions {
  /** Override the implementation (tests); defaults to the platform resolution below. */
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

/** Create the factory that talks to the platform `WebSocket` (or the bundled `ws`). */
export function createNativeSocketFactory(options: NativeSocketFactoryOptions = {}): SocketFactory {
  const { WebSocketImpl } = options;
  return (url, handlers, openOptions) => {
    const impl = WebSocketImpl ?? resolveWebSocket();
    if (impl === undefined) {
      throw new Error(
        `no WebSocket implementation available (neither a global WebSocket nor the bundled ws client): ${describeSocketRuntime()}`,
      );
    }
    const headers = openOptions?.headers;
    const socket =
      headers === undefined || Object.keys(headers).length === 0 ? new impl(url) : new impl(url, { headers });
    return attachNativeSocket(socket, handlers);
  };
}

/**
 * What this runtime offers, for connect-failure diagnostics.
 *
 * Deliberately does not *load* the fallback (a description must stay free), so
 * it reports the resolution order rather than a probe result.
 */
export function describeSocketRuntime(): string {
  const version = nodeVersion();
  const node = version === null ? 'unknown runtime' : `node ${version}`;
  if (readGlobalWebSocket() !== undefined) {
    return `${node}, global WebSocket available`;
  }
  return `${node}, no global WebSocket (using the bundled ws client)`;
}

/**
 * `process.versions.node`, read off `globalThis`.
 *
 * Structural access on purpose: this package is type-checked with node types
 * (`tsconfig.json` / `tsconfig.node-probe.json`) *and* against a DOM-only program
 * (`tsconfig.dom.json`, which is what a browser host compiles it as), and a bare
 * `process` identifier does not exist in the latter.
 *
 * Behaviour: identical in every *reachable* runtime state (`globalThis.process` is
 * the same object as `process` in Node, `undefined` anywhere else). One pathological
 * state differs, in this direction only: when a bundler polyfills a `process` object
 * **without** `versions.node`, the old `process.versions.node` read threw a
 * `TypeError` out of `describeSocketRuntime()`; this version reports
 * `unknown runtime, …` instead — a degradation, not a regression.
 */
function nodeVersion(): string | null {
  const runtime = (globalThis as { readonly process?: { readonly versions?: { readonly node?: unknown } } })
    .process;
  const version = runtime?.versions?.node;
  return typeof version === 'string' ? version : null;
}

/** Resolution order: the platform global, then the bundled `ws` client. */
function resolveWebSocket(): WebSocketConstructor | undefined {
  return readGlobalWebSocket() ?? loadFallbackWebSocket();
}

function readGlobalWebSocket(): WebSocketConstructor | undefined {
  const candidate: unknown = globalThis.WebSocket;
  return isWebSocketConstructor(candidate) ? candidate : undefined;
}

function isWebSocketConstructor(value: unknown): value is WebSocketConstructor {
  return typeof value === 'function';
}

/**
 * Load the bundled `ws` client.
 *
 * `require` (not a static import) is deliberate: the module is only initialised
 * on hosts that actually need it, and esbuild keeps the call lazy in both the
 * CJS extension bundle and the ESM smoke bundle (verified by building; a static
 * import would also break the ESM smoke bundle, which cannot emit `require`).
 * `undefined` when the module is missing (a broken bundle) — the caller then
 * reports the runtime description above.
 *
 * The `declare` keeps the *call* unchanged (esbuild has to see the literal
 * `require('ws')` to bundle it) while this module still compiles in a DOM-only
 * program, where neither `require` nor node types exist. A browser host never
 * reaches the line: `resolveWebSocket` finds the global `WebSocket` first, and a
 * host without one injects its own {@link SocketFactory}.
 *
 * Lint note: the local declaration also *silences* `@typescript-eslint/no-require-imports`
 * in this file — the rule resolves `require` to a binding, and a module-scoped one is not
 * the global it is looking for (measured: a stray `require('node:path')` here is not
 * reported). That is acceptable because the specifier is guarded at the package level:
 * `tests/layers.test.ts` parses every `require()` literal in `src/`, allows exactly one
 * non-relative specifier — `'ws'`, from this file — and rejects node builtins, `vscode`
 * and any other package.
 */
declare const require: (id: string) => unknown;

function loadFallbackWebSocket(): WebSocketConstructor | undefined {
  try {
    // Lazy, bundled fallback — the declaration above is what lets this line
    // compile in a DOM-only program (and what silences no-require-imports here:
    // the specifier itself is guarded by tests/layers.test.ts, see above).
    const module = require('ws') as { WebSocket?: unknown };
    return isWebSocketConstructor(module.WebSocket) ? module.WebSocket : undefined;
  } catch {
    return undefined;
  }
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
