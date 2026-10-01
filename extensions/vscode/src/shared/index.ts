/**
 * `src/shared` — the vocabulary of **this extension's** webview channel.
 *
 * What is here: the three constants that exist only because of how *this* host embeds
 * the renderer — the theme class names, the mount-point id and the patch-chunk cap
 * (see `./constants.ts`).
 *
 * What deliberately is **not** here any more: the bridge protocol. The message
 * unions, their runtime guards, the transport interface (`WebviewTransport`), the
 * bootstrap model and the protocol version all moved into `@wing-agent/ui/protocol`
 * when the renderer itself moved into `packages/ui` — the package cannot import the
 * extension, so the contract both sides speak has to live with the view. Anything
 * else here would be a second copy of it.
 *
 * Rules of engagement (enforced by `tests/layers.test.ts`):
 * - no `vscode`, no DOM, no node imports — plain types, constants and pure helpers;
 * - additive changes only: append new nullable fields or new union variants, never
 *   repurpose an existing one;
 * - everything must survive a JSON round-trip.
 */

export * from './constants';
