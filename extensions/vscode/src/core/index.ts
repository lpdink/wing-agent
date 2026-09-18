/**
 * `src/core` — the gateway capability layer (step 02).
 *
 * Public API for the extension host (03). Nothing below this barrel is a
 * contract: import from `../core` (or `../../src/core`), never from a file
 * inside it.
 *
 * What it is:
 * - the typed mirror of the gateway protocol (Python is the source of truth);
 * - one WebSocket connection per client, with handshake, `_chunk` reassembly and
 *   reconnect supervision (`GatewayConnection`);
 * - every HTTP endpoint as one method (`GatewayHttpClient`);
 * - typed errors (`GatewayHttpError` / `GatewaySocketError`).
 *
 * What it deliberately is not: it does not know that sessions exist. It never
 * subscribes, never resubscribes and never buffers business events — after a
 * reconnect it announces the new `clientId` and the host decides what to
 * re-subscribe (see `design.md` D5).
 */

export * from './backoff';
export * from './chunk';
export * from './connection';
export * from './errors';
export * from './http-client';
export * from './logging';
export * from './protocol';
export * from './transport';
export * from './urls';
