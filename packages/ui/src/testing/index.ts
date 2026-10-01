/**
 * `@wing-agent/ui/testing` — fixtures and the scripted host.
 *
 * The third public entry: test doubles that any consumer's tests and preview
 * harnesses may drive the renderer with — the per-cell-kind fixtures plus
 * `createMockBridge`, a scripted `WebviewTransport` that answers the handshake,
 * `ping`, `resync` and `resolveImages` like the real host does.
 *
 * It lives in `src/` (rather than in this repository's `tests/`) because more than one
 * package needs it: the renderer's own component tests, the extension's preview page
 * and its build-artifact gate. **Product code must not import it** — `tests/layers.ts`
 * in the extension and this package's own guard refuse that import outside tests, and
 * the fixtures are written as plain JSON on purpose (if a fixture cannot survive a
 * `postMessage`, neither can the real thing).
 *
 * Environment: DOM-free like `./protocol` (it models the wire, not the document) —
 * `tsconfig.node-probe.json` compiles it without the DOM lib.
 */

export * from './fixtures';
export * from './mockBridge';
