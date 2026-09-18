/**
 * `src/core/transport` — the two injectable I/O seams.
 *
 * - `socket.ts` — WebSocket: factory + handlers, plus the platform adapter.
 * - `http.ts`   — HTTP: request/response transport, plus the `fetch` adapter.
 *
 * Everything above this directory is pure logic over these two interfaces,
 * which is what makes the whole layer testable without a gateway.
 */

export * from './http';
export * from './socket';
