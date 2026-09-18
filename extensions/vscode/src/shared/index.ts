/**
 * `src/shared` — the only seam between the extension host and the webview.
 *
 * Rules of engagement (enforced by `tests/layers.test.ts`):
 * - no `vscode`, no DOM, no node imports — plain types, constants and pure helpers;
 * - additive changes only: append new nullable fields or new union variants, never
 *   repurpose an existing one (03/04/05 develop against this in parallel);
 * - everything must survive a JSON round-trip.
 */

export * from './bridge';
export * from './cells';
export * from './commands';
export * from './constants';
export * from './exhaustive';
export * from './session';
export * from './types';
export * from './validate';
