/**
 * `@wing-agent/ui` — the renderer shared by every wing frontend.
 *
 * Public API for the shells (the VS Code webview today; the web / Electron shells
 * next). Nothing below this barrel is a contract: import from the package root
 * (`@wing-agent/ui`), never from a file inside it — `tests/layers.test.ts` keeps that
 * promise honest.
 *
 * What it is:
 * - the **transcript renderer**: one component per cell kind, incremental markdown
 *   (markdown-it + shiki + KaTeX), the composer, the shell (tab bar / status row /
 *   welcome) and the host-owned overlays;
 * - the **mirror**: a zustand store that follows the host's model (`hydrate` /
 *   `patch` / `state` / `panels` / `tabs` / `ui`) and never invents content, plus
 *   the bridge controller that is the only writer into it;
 * - the **mount seam**: `mountApp(root, { transport })` — the host bridge is an
 *   injected `WebviewTransport`, so the same app runs in a VS Code webview, in the
 *   preview harness and inside the web/Electron shells.
 *
 * Three public entries, one per environment:
 * - `.` (this barrel) — the browser app: DOM + React + CSS modules.
 * - `./protocol` — the DOM-free wire contract (message unions, guards, the receiver
 *   rules) that the host side consumes too.
 * - `./testing` — fixtures + the scripted host, for tests and preview harnesses of
 *   any consumer (product code must not import it).
 *
 * Environment: DOM + React (CSS modules are the package's only import-time effect,
 * see `sideEffects` in package.json) — but **no `vscode` and no node builtin**:
 * `tsconfig.dom.json` compiles `src/` without node types (a bare `process` is an
 * error) and the import-graph guard in `tests/layers.test.ts` pins the dependency
 * allowlist.
 */

export * from './protocol';
export * from './bootstrap';
export * from './mount';
export * from './bridge/channel';
export * from './bridge/controller';
export * from './state/appStore';
export * from './state/store';
export * from './app/App';
export * from './app/Composer';
export * from './app/StatusArea';
export * from './app/TabBar';
export * from './app/TranscriptView';
export * from './app/Welcome';
export * from './app/selectors';
export * from './app/panels/BranchPanel';
export * from './app/panels/ModelPanel';
export * from './app/panels/PanelShell';
export * from './app/panels/SessionPanel';
export * from './app/panels/listNav';
export * from './chat/AskCell';
export * from './chat/Cells';
export * from './chat/CellView';
export * from './chat/FileReference';
export * from './chat/Markdown';
export * from './chat/interaction';
export * from './chat/markdown/highlight';
export * from './chat/markdown/image';
export * from './chat/markdown/math';
export * from './chat/markdown/parse';
export * from './chat/markdown/render';
export * from './chat/markdown/split';
