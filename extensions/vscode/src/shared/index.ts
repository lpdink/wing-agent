/**
 * `src/shared` — the host ⇄ webview **bridge protocol** (the channel).
 *
 * What is here: the message unions, their runtime guards, the transport interface and
 * the constants that describe *this* channel (protocol version, patch chunk cap, the
 * webview DOM ids).
 *
 * What is deliberately **not** here: the session view model (`CellModel`,
 * `SessionStateModel`, …) and the command contract (`FRONTEND_COMMANDS`, …). Those are
 * `@wing-agent/session` (`packages/session`) — environment-agnostic vocabulary that
 * every frontend needs, so the protocol imports them instead of a second copy living
 * here. `tests/layers.test.ts` allows the package from every layer, barrel imports
 * only.
 *
 * Rules of engagement (enforced by `tests/layers.test.ts`):
 * - no `vscode`, no DOM, no node imports — plain types, constants and pure helpers;
 * - additive changes only: append new nullable fields or new union variants, never
 *   repurpose an existing one (03/04/05 develop against this in parallel);
 * - everything must survive a JSON round-trip.
 */

export * from './bridge';
export * from './constants';
export * from './exhaustive';
export * from './validate';
