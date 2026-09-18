# Wing for VS Code (`extensions/vscode`)

VS Code frontend for the wing agent: a sidebar view that talks to the same gateway the TUI talks to
(HTTP for lifecycle, one WebSocket for the ReAct event stream).

> **Status: scaffold (step 01).** The extension installs, activates and renders a fixture-driven
> placeholder page through the real webview pipeline. The gateway client (`src/core`), the session
> host (`src/host`) and the chat UI (`src/webview`) land in the next steps. Placeholder code is marked
> `SCAFFOLD(01)` — grep for it to see what is temporary.

## Requirements

- **To build/develop**: Node.js ≥ 22 and pnpm 11 (`packageManager` in `package.json` pins the exact version —
  `corepack enable` is enough). These are _toolchain_ requirements; they say nothing about the VS Code
  runtime this extension targets.
- **To run**: VS Code ≥ 1.100 (`engines.vscode`). The extension host bundle is built with `target: node20`
  on purpose — VS Code 1.100 ships Electron 34 / Node 20.19, so anything newer would risk using APIs the
  host does not have.

## Quick start

```bash
cd extensions/vscode
pnpm install
pnpm build
```

Then open **this folder** (`extensions/vscode`) in VS Code and press <kbd>F5</kbd>.

`F5` starts an _Extension Development Host_ (a second VS Code window) with the extension loaded from
source:

1. In the new window, click the **Wing** icon in the Activity Bar (left rail).
2. The **Chat** view opens. You should see the scaffold page: a session header, a fixture transcript
   (user / assistant / thinking / tool call / diff / todo / ask / metrics cells) and a footer showing
   `bridge: ready`, the protocol version and a **Ping host** button.
3. Pressing **Ping host** round-trips one message through the host and shows the measured latency —
   that is the whole bridge working end to end.

Notes:

- `F5` reuses whatever is in `out/` and `dist/`. **Build first**, then press F5. For iterative work run
  `pnpm run watch` (extension host) and `pnpm run dev:preview` (webview, in a browser) in a terminal.
- The launch configuration passes `--disable-extensions` so the development window is not affected by
  other installed extensions; the development extension itself still loads.
- No gateway is needed for the scaffold. Once `src/core` lands, start it with `wing start` (or let the
  extension do it) and the sessions appear in the same view.

## Commands

| Command                            | What it does                                                                                        |
| ---------------------------------- | --------------------------------------------------------------------------------------------------- |
| `pnpm run build`                   | Bundle everything: extension host → `out/extension.js`, webview → `dist/webview/{main.js,main.css}` |
| `pnpm run watch`                   | Incremental esbuild for the extension host                                                          |
| `pnpm run dev:preview`             | Vite dev server for the webview preview harness (no VS Code needed)                                 |
| `pnpm run build:preview`           | Static build of the preview harness → `dist/preview/`                                               |
| `pnpm run typecheck`               | `tsc --noEmit` over the three projects (node / webview / tools)                                     |
| `pnpm run lint`                    | ESLint (type-aware) incl. the layer zones                                                           |
| `pnpm run format` / `format:check` | Prettier                                                                                            |
| `pnpm run test`                    | vitest — `node` and `webview` projects, including the layer guard                                   |
| `pnpm run package`                 | `vsce package` → `wing-vscode.vsix`                                                                 |
| `make check-ts` / `make test-ts`   | Same gates from the repository root (what CI runs)                                                  |

## Install the packaged extension

```bash
cd extensions/vscode
pnpm run package                    # → wing-vscode.vsix
code --install-extension wing-vscode.vsix
```

`pnpm run package` runs the build through `vscode:prepublish`, so the `.vsix` always contains freshly
built bundles. Only `out/`, `dist/`, `media/` and `LICENSE.txt` are packaged (`src/`, tests, tooling
config and `node_modules/` are excluded by `.vscodeignore`). `LICENSE.txt` is a copy of the repository
license — a package must carry its own license file.

## Architecture

```
src/shared/   contract types shared by both sides — types, constants, pure helpers. No vscode, no DOM, no node.
src/core/     gateway capability layer (step 02): protocol mirror, WS/HTTP clients, chunk reassembly, reconnect.
src/host/     extension host (step 03): view + document, multi-tab orchestration, event reduction, bridge.
src/webview/  React renderer (steps 04/05): applies host-produced ops, renders the chat shell.
src/testing/  fixtures + scripted host (test and preview only).
preview/      preview harness: the webview app against the scripted host, without VS Code.
tests/        vitest suites (node + jsdom).
```

The rule that ties it together: **the host is the only authority, the webview is a pure renderer.**
The host reduces gateway events into a `SessionViewModel` and pushes it into the webview as a full
`hydrate` plus ordered `patch` batches; the webview applies what it receives and answers `resync` when
it cannot follow, never guessing. `sync_session` replay and live events travel the same reduction path
in the host, which is what keeps replayed and live transcripts identical.

### Layering is enforced by three mechanisms

| Mechanism                                   | Catches                                                                                                                  | Runs in                           |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ | --------------------------------- |
| Split tsconfigs (`lib` / `types` per layer) | DOM globals in `core`/`host`/`shared`; node globals in `webview`/`shared`                                                | `pnpm run typecheck`              |
| ESLint zones (`no-restricted-imports`)      | `vscode` outside `host`, `host → webview`, `webview → host/core`, `src/testing` in product code                          | `pnpm run lint`                   |
| `tests/layers/layers.test.ts`               | the full matrix: static/dynamic imports, re-exports, `require`, node builtins, unresolved paths, hardcoded colors in CSS | `pnpm run test` (`make test`, CI) |

A violating import therefore fails `make check` **and** `make test`; the layer guard test is the
authoritative one because it resolves the actual import graph instead of pattern matching.

### Webview pipeline (CSP, nonce, resources)

- The host generates the document (`src/host/html.ts`, a pure function) with
  `default-src 'none'` plus exactly the sources the view needs, a per-load nonce and
  `asWebviewUri` resource URLs; bootstrap values (`window.__WING_BOOTSTRAP__`) are injected inline and
  HTML-escaped.
- The webview bundle is a single IIFE (`vite.config.mts`, lib mode) and the stylesheet sits next to it:
  no dynamic chunks, no third-party origin, no runtime `fetch`. Everything the renderer needs must be
  bundled at build time.
- Styling is CSS Modules on VS Code theme variables (`--vscode-*`). Hardcoded colors are rejected by
  the layer guard — visual constants stay traceable.
- Visual constants live in `src/webview/styles/tokens.css`, each one annotated with the **file and line
  it was taken from** in the local VS Code 1.129.1 sources (`workbench/contrib/chat/browser/widget/**`).
  Do not add a value without a source.

### Chat renderer (`src/webview/chat`)

- `CellView` maps the `CellModel` union to components; it is memoized on `(cell, sessionId)`, so a
  streamed patch re-renders one cell and leaves the rest of the transcript untouched.
- Markdown is rendered block by block: `markdown/split.ts` cuts the text at blank lines that are outside
  a code fence, finished blocks are memoized `MarkdownBlock`s, and only the trailing block re-renders
  while text keeps arriving (`tests/webview/streaming.test.tsx` asserts both the render and the parse
  count stay proportional to the tail, not to the answer).
- Syntax highlighting is shiki with the **JavaScript regex engine** (the wasm engine needs
  `'wasm-unsafe-eval'` in the webview CSP, which we do not own) and the `dark-plus` / `light-plus`
  themes — i.e. exactly the Light+/Dark+ token colors that Light Modern/Dark Modern inherit. Both
  themes ride along as inline `--shiki-*` properties and the stylesheet picks one, so switching the
  editor theme needs no re-highlight.
- Interactions always go through the bridge: copying a code block sends `copyText`, clicking a file
  path sends `openFile`, a diff card sends `openDiff`, and links send `openLink`. The renderer never
  touches the editor or the clipboard itself.
- Collapsed/expanded choices are webview-local (`chat/interaction.ts`): a memory-only map keyed by cell
  id, never part of the protocol.

## Testing

```bash
pnpm run test              # all projects
pnpm run test:watch        # watch mode
```

- `tests/{shared,host,core,state,layers}` run in a **node** environment; the `vscode` module is aliased
  to `tests/mocks/vscode.ts` (a small, recording mock), which is what makes the extension host testable
  headless — no VS Code window, ever.
- `tests/webview` runs in **jsdom** with `@testing-library/react`; components are mounted through the
  real `mountApp` against the scripted host in `src/testing/mockBridge.ts`.
- `src/testing/fixtures.ts` has one fixture per cell kind plus the step 04 scenarios (streaming turn,
  failed tool call, approval) that both tests and the preview harness use.
- The preview harness (`preview/main.tsx`) exposes a toolbar to switch fixtures, stream a turn, break
  the patch stream (resync recovery) and push a UI action — the fastest way to look at renderer changes
  without VS Code (`pnpm run dev:preview`, port 5199). `preview/preview-theme.css` emulates the Dark
  Modern theme variables so the page looks like the real sidebar.

## Repository integration

- `make check` / `make test` include the TypeScript gates (`check-ts` / `test-ts`).
- CI job `typescript-check` runs install → lint → format:check → typecheck → test → build →
  build:preview → package and uploads the `.vsix`.
- `pnpm install` in CI uses `--frozen-lockfile`; keep `pnpm-lock.yaml` committed.
- Every runtime dependency is declared as a `devDependency` on purpose: everything is bundled into
  `out/`/`dist/`, so `vsce` has no production dependency tree to ship.

## Troubleshooting

| Symptom                                        | Cause / fix                                                                                                                                      |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| F5 opens a window without the Wing icon        | `out/extension.js` missing → run `pnpm build`; check the Extension Host log in the development window (Output → Extension Host).                 |
| View is blank / white                          | Open the webview dev tools (Command Palette → _Developer: Open Webview Developer Tools_) and check the console for CSP or module-loading errors. |
| `pnpm: command not found` inside VS Code tasks | VS Code was launched without your shell `PATH`; run `pnpm build` in a terminal instead of via the task.                                          |
| `vsce` complains about `@types/vscode`         | `engines.vscode` and `@types/vscode` must stay aligned (currently `^1.100.0` / `1.100.0`).                                                       |
