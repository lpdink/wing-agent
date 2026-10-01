/**
 * Globals the renderer's document provides beyond the DOM.
 *
 * Declared here (not in the extension) because `bootstrap.ts` — the reader of the
 * injected value — is part of this package: `readBootstrap` defaults to `window`, and
 * without the property declared the weak-type check refuses the default argument.
 * The *host* injects it (the extension writes the inline script in
 * `src/host/html.ts`); this declaration only says the renderer may look for it, and
 * a document that never got one degrades to `FALLBACK_BOOTSTRAP`.
 *
 * **Consumers that compile these sources** (this package is source-first, `exports`
 * points at `src/`) must include this file in their program: an ambient `.d.ts` is
 * never pulled in by an import. One glob in the consumer's tsconfig — see
 * `extensions/vscode/tsconfig.webview.json`, which lists the package's declaration
 * files explicitly.
 *
 * Per-project declarations otherwise: an embedder that injects extra globals declares
 * them in its own project (the extension's `src/webview/globals.d.ts` adds
 * `acquireVsCodeApi`, which exists only inside VS Code).
 */

interface Window {
  /** Injected by the host document before the bundle runs; absent in test harnesses. */
  __WING_BOOTSTRAP__?: unknown;
}
