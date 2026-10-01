/**
 * `@wing-agent/ui/protocol` — the host ⇄ view wire contract.
 *
 * The second public entry, deliberately separate from the app barrel: this subtree is
 * **DOM-free and JSX-free**, so a Node program (the VS Code extension host, the
 * preview harness' scripted host, any future orchestrator) can compile and consume it
 * — see `tsconfig.node-probe.json`, which compiles exactly these files plus
 * `src/testing` without the DOM lib.
 *
 * What it is:
 * - the **message unions** of both directions (`HostToWebviewMessage` /
 *   `WebviewToHostMessage`), the `CellPatch` transport envelope's caps, the
 *   `BootstrapModel` and the protocol version — with the application rules written
 *   out in `./bridge`'s module doc;
 * - their **runtime guards** (`isHostToWebviewMessage`, `readImageSources`, …) — the
 *   channel also carries unrelated platform traffic, so both ends filter before they
 *   trust;
 * - the **transport interface** the host implements (`WebviewTransport`: `post` +
 *   `subscribe`) — the injection seam `mountApp` takes;
 * - the **receiver side of the patch channel** (`applyCellPatches`, `isExpectedSeq`):
 *   the executable form of the rules `./bridge` documents, returning the same
 *   `ResyncReason` vocabulary. The host's own test mirror reuses it, so "the host
 *   produced a patch stream the shipped renderer can follow" is an assertion, not a
 *   hope.
 *
 * Nothing below this barrel is a contract: import `@wing-agent/ui/protocol`, never a
 * file inside it (`tests/layers.test.ts` keeps that promise honest).
 */

export * from './bridge';
export * from './exhaustive';
export * from './patches';
export * from './validate';
