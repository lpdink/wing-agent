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
 * - the **code and tool cards**: the shared `CodeBlock` / `CodeToolbar`, the
 *   `TerminalBlock` (ANSI output) and the `DiffBlock` (host-windowed diffs) — the
 *   wing-app surface, styled by the token sheets under `styles/` (see the
 *   `./styles/*` export; a shell imports those once, this barrel does not);
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

// The renderer's token sheet lives **here**, on the barrel every `.` consumer walks
// through, and not on `mount.tsx`: the sheet is what resolves every `--wing-*` the
// components below read, and a shell that takes components from the barrel without
// mounting the app (the web / Electron shells; `apps/web` renders `TranscriptView`
// directly) must still get it. While the only import sat on `mount.tsx`, bundlers
// tree-shook that module away for such consumers and the sheet with it — every
// `var(--wing-*)` resolved to nothing, in production builds only (`vite dev` does no
// tree-shaking, and the VS Code webview mounts `mountApp`, so neither saw it).
// `sideEffects: ["**/*.css"]` in package.json keeps this import alive; the
// `tests/artifacts/consumer-build.test.ts` gate builds both consumer shapes and
// asserts the emitted CSS carries the definitions and the rules that read them.
import './styles/tokens.css';

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
export * from './chat/DisclosureRow';
export * from './chat/FileReference';
export * from './chat/Markdown';
export * from './chat/ReasoningRow';
export * from './chat/TextShimmer';
// `interaction.ts` and the ported clipboard hook both export `useCopyFeedback`, with
// different shapes: the renderer's returns the `[copied, report]` tuple its cells
// drive after the host acknowledged a copy, the card hook owns the clipboard write.
// The renderer's keeps the bare name; the card hook is exported as
// `useClipboardFeedback`, so both stay reachable and neither is ambiguous.
export {
  COPY_FEEDBACK_MS,
  resetCollapseOverrides,
  useCollapsible,
  useCopyFeedback,
  type CollapsibleState,
} from './chat/interaction';
export * from './chat/markdown/highlight';
export * from './chat/markdown/image';
export * from './chat/markdown/math';
export * from './chat/markdown/parse';
// Both the VS Code renderer (`render.tsx`) and the ported shared card export a
// `CodeBlock`. The shared card owns the bare name (that is what a shell wires into
// the transcript); the renderer's version stays reachable as `MarkdownCodeBlock`.
export { CodeBlock as MarkdownCodeBlock, MarkdownNodes } from './chat/markdown/render';
export * from './chat/markdown/split';
export * from './ask/ApprovalPanel';
export * from './ask/QuestionComposer';
export * from './ask/QuestionReplyView';
export * from './ask/question-reply';
export * from './components/Button';
export * from './components/ConnectionIndicator';
export * from './components/Pill';
export * from './components/StateDot';
export * from './icons';
export * from './markdown/CodeBlock';
export * from './markdown/CodeToolbar';
export * from './markdown/useViewportHighlighting';
export * from './tool/DiffBlock';
export * from './tool/FoldToggle';
export * from './tool/TerminalBlock';
export * from './tool/ansi';
export * from './tool/clipboard';
export * from './tool/head-tail-cap';
export { useCopyFeedback as useClipboardFeedback } from './tool/use-copy-feedback';
