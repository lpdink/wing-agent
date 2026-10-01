/**
 * `@wing-agent/client` — the gateway capability layer.
 *
 * Public API for every host (the VS Code extension today; an Electron main
 * process / a Node script tomorrow). Nothing below this barrel is a contract:
 * import from the package root (`@wing-agent/client`), never from a file inside
 * it — `packages/client/tests/layers.test.ts` keeps that promise honest.
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
 * re-subscribe.
 *
 * Environment: plain `ES2023` + `globalThis` only — no `vscode`, no node builtin
 * imports, no DOM API, no npm dependency (the single `require('ws')` fallback is
 * lazy and wrapped). `pnpm run typecheck` runs three projects over these sources:
 * `tsconfig.json` (package + tests + vitest config), `tsconfig.node-probe.json`
 * (**the no-DOM gate**: pure `src/`, no test tooling) and `tsconfig.dom.json`
 * (the no-node gate).
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
