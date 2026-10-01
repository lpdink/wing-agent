/**
 * `@wing-agent/session` — the session reduction lane shared by every wing frontend.
 *
 * Public API for the shells (the VS Code extension today; web / Electron tomorrow).
 * Nothing below this barrel is a contract: import from the package root
 * (`@wing-agent/session`), never from a file inside it — `tests/layers.test.ts` keeps
 * that promise honest.
 *
 * What it is:
 * - the **view model** of one session (`CellModel` transcript cells, `SessionStateModel`
 *   state, `PanelsModel` overlays) — plain JSON, nullable rather than optional;
 * - the **reduction**: `applyLive` (gateway events) and `applySync` (the `sync_session`
 *   replay) drive the *same* handlers into one `SessionRecord`, which is what makes
 *   "replay == live" structural instead of hopeful;
 * - the **pure derivations** a renderer would otherwise duplicate: titles, tool-row
 *   subjects, Myers diffs + windowing, ask normalization/replies, todo parsing, and
 *   the tolerant partial-JSON parser for streaming tool arguments;
 * - the **command vocabulary** of the composer (`FRONTEND_COMMANDS` and its helpers).
 *
 * What it deliberately is not: it never opens a socket, never speaks HTTP and never
 * touches the editor or the DOM — the caller supplies events (decoded by
 * `@wing-agent/client`) and consumes `CellPatch`es. The host-side orchestration that
 * does the I/O (tabs, subscriptions, control plane) stays in each shell; in the VS
 * Code extension that is `src/host/session/manager.ts`.
 *
 * Environment: plain `ES2023` + `globalThis` — no `vscode`, no node builtin, no DOM
 * API, no npm dependency other than the gateway capability layer. Verified by
 * `tsconfig.json` (no DOM lib) *and* `tsconfig.dom.json` (no node types) in
 * `pnpm run typecheck`, plus the import-graph guard in `tests/layers.test.ts`.
 */

export * from './cells';
export * from './commands';
export * from './constants';
export * from './derive';
export * from './model';
export * from './partial-json';
export * from './queue';
export * from './reducer';
export * from './session';
export * from './types';
