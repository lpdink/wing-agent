# Wing for VS Code (`extensions/vscode`)

VS Code frontend for the wing agent: a sidebar view that talks to the same gateway the TUI talks to
(HTTP for lifecycle, one WebSocket for the ReAct event stream). It runs the real session host inside
the extension host — multi-tab sessions, streaming transcript, control plane, approvals and reconnect
— while the webview stays a pure renderer.

Maintainer deep dive (Chinese):
[docs/dev/vscode-extension.md](https://github.com/lpdink/wing-agent/blob/develop/docs/dev/vscode-extension.md).

## Requirements

- **To build/develop**: Node.js ≥ 22 and pnpm 11 (`packageManager` in `package.json` pins the exact
  version — `corepack enable` is enough). These are _toolchain_ requirements; they say nothing about
  the VS Code runtime this extension targets.
- **To run**: VS Code ≥ 1.100 (`engines.vscode`). The extension host bundle is built with
  `target: node20` on purpose — VS Code 1.100 ships Electron 34 / **Node 20.19**. That host has no
  global `WebSocket` (Node only exposes it from 21), so `src/core/transport/socket.ts` falls back to a
  bundled `ws` client; on newer hosts (Node ≥ 22) the global implementation is used and the fallback is
  never loaded. A connect failure names the runtime it ran on, so a report is actionable.
- **To talk to a gateway**: a `wing` installation (`wing start`, or let the extension start it) — the
  same gateway the TUI uses, default `127.0.0.1:32523`.

## Install the packaged extension

```bash
# 1. see what is installed (the previous implementation used the same id)
code --list-extensions --show-versions | grep wing-agent

# 2. remove the old implementation if present — same id + same version does not
#    go through VS Code's update semantics
code --uninstall-extension wing-agent.wing-vscode

# 3. install this package (path is wherever the .vsix was handed to you)
code --install-extension wing-vscode-0.1.0.vsix
```

If you skip the uninstall, a same-version reinstall needs `--force`:

```bash
code --install-extension wing-vscode-0.1.0.vsix --force
```

Reload the window (`Developer: Reload Window`) after installing. To uninstall:

```bash
code --uninstall-extension wing-agent.wing-vscode
```

## Develop

```bash
cd extensions/vscode
pnpm install --frozen-lockfile
pnpm build
```

Open **this folder** (`extensions/vscode`) in VS Code and press <kbd>F5</kbd>. F5 starts an
_Extension Development Host_ (a second window) with the extension loaded from source:

1. Click the **Wing** icon in the Activity Bar.
2. The **Chat** view opens and, if the gateway is reachable, creates a session automatically. With no
   folder open it says so instead — open a folder first.
3. Send a message; the transcript streams. The status row shows model / thinking / YOLO / workspace,
   the context ring and token totals.

Notes:

- F5 reuses whatever is in `out/` and `dist/`. **Build first**, then press F5. For iterative work run
  `pnpm run watch` (extension host) and `pnpm run dev:preview` (webview in a browser, port 5199) in a
  terminal.
- The launch configuration passes `--disable-extensions` so the development window is not affected by
  other installed extensions; the development extension itself still loads.

## Settings

| Setting          | Default     | What it does                                                                                                                                |
| ---------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `wing.host`      | `127.0.0.1` | Gateway host (mirror of `gateway.host` in `~/.wing/config.yaml` — the extension does not parse that file).                                  |
| `wing.port`      | `32523`     | Gateway port.                                                                                                                               |
| `wing.apiKey`    | `""`        | API key for gateways with auth enabled. Leave empty when auth is off. **Stored as plain text** — see the note below.                        |
| `wing.wingPath`  | `""`        | Full path of the `wing` executable, used for auto-start. Empty = discover on `PATH` / well-known install locations.                         |
| `wing.autoStart` | `true`      | Start the gateway (`wing start`) when it is not running. The extension probes first, so a gateway you are already using is never restarted. |

Commands: `Wing: New Session`, `Wing: Reconnect to Gateway`.

### About `wing.apiKey`

- It is sent as an `Authorization: Bearer …` **header**, never in a URL (so proxy logs, access logs and
  crash reports do not collect it — `src/core/urls.ts` documents the DOM-host exception).
- It is a **plain-text setting**: a workspace-scoped value lands in `.vscode/settings.json` (easy to
  commit by accident) and Settings Sync uploads it. Put it in **User** settings, and treat it like any
  other file-resident secret.
- Storing it in `context.secrets` (SecretStorage) with the setting kept as a "configured / not
  configured" switch is a follow-up, not part of this version.

## Scripts

| Command                            | What it does                                                                                                |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `pnpm run build`                   | Bundle everything: extension host → `out/extension.js`, webview → `dist/webview/{main.js,main.css}`         |
| `pnpm run watch`                   | Incremental esbuild for the extension host                                                                  |
| `pnpm run dev:preview`             | Vite dev server for the webview preview harness (http://localhost:5199/, no VS Code needed)                 |
| `pnpm run build:preview`           | Static build of the preview harness → `dist/preview/`                                                       |
| `pnpm run typecheck`               | `tsc --noEmit` over the three projects (node / webview / tools)                                             |
| `pnpm run lint`                    | ESLint (type-aware) incl. the layer zones                                                                   |
| `pnpm run format` / `format:check` | Prettier                                                                                                    |
| `pnpm run test`                    | vitest — `node` and `webview` projects, including the layer guard and the build-artifact gate               |
| `pnpm run smoke:gateway`           | End-to-end smoke: real `wing-gateway` + scripted fake provider (12 scenarios; `--only`, `--keep`, `--list`) |
| `pnpm run smoke:list`              | List the smoke scenarios without running them                                                               |
| `pnpm run package`                 | `vsce package` → `wing-vscode.vsix`                                                                         |
| `make check-ts` / `make test-ts`   | Same gates from the repository root (what CI runs)                                                          |

## Architecture

```
src/shared/   contract types shared by both sides — types, constants, pure helpers. No vscode, no DOM, no node.
src/core/     gateway capability layer: protocol mirror, WS/HTTP clients, chunk reassembly, reconnect (the Electron seam).
src/host/     extension host: WingHost (connection lifecycle) + SessionManager (tabs, control plane, reduction) + bridge.
src/webview/  React renderer: applies host-produced ops, renders the chat shell.
src/testing/  fixtures + scripted host (test and preview only).
preview/      preview harness: the webview app against the scripted host, without VS Code.
tests/        vitest suites (node + jsdom).
```

The rule that ties it together: **the host is the only authority, the webview is a pure renderer.**
The host reduces gateway events into a `SessionViewModel` and pushes it into the webview as a full
`hydrate` plus ordered `patch` batches; the webview applies what it receives and answers `resync` when
it cannot follow, never guessing. `sync_session` replay and live events travel the same reduction path
in the host, which is what keeps replayed and live transcripts identical. One WebSocket serves every
tab (one `client_id`; subscribe on open, unsubscribe on close, resubscribe all after a reconnect).

Host-produced overlays (`panels.modelPicker` / `sessionPicker` / `branchPicker` non-null = on screen;
`commandCatalog` is data only) — the webview sends intents, never opening panels on its own state.

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
- **The bundle must not reference Node globals.** `vite.config.mts` inlines `process.env.NODE_ENV`
  (Vite does not do that for _lib_ builds) and pins `NODE_ENV=production` so the JSX transform and
  React's runtime agree; without both, React's CJS entry keeps a `process.env` branch and the view is
  blank with `ReferenceError: process is not defined` in the webview console.
  `tests/artifact/webviewBundle.test.ts` builds the bundle with that config and executes it in a DOM
  without Node globals, so the failure cannot come back unnoticed.
- Styling is CSS Modules on VS Code theme variables (`--vscode-*`). Hardcoded colors are rejected by
  the layer guard — visual constants stay traceable.
- The build emits `dist/webview/main.js.map`. DevTools may report
  `Connecting to …/main.js.map violates … default-src 'none'`: source maps are fetched through
  `connect-src`, which the document deliberately does not open. It is a harmless console note (maps
  are dev-only — `**/*.map` is excluded from the `.vsix`) and the price for being able to map a
  minified stack back to `src/`.

- Visual constants live in `src/webview/styles/tokens.css`, each one annotated with the **file and line
  it was taken from** in the local VS Code 1.129.1 sources (`workbench/contrib/chat/browser/widget/**`).
  Every spacing / font-size / line-height / radius / colour decision belongs there.

  **Where a CSS module may still use a literal** (the complete exception list — anything else must become
  a token first):

  1. the declaration is _inside a rule whose header comment already quotes the source line that contains
     it_ (e.g. `.userBubble { padding: 8px 12px }` under a `CHAT:3792-3805` comment) — the value is
     sourced at the rule level, not copied by feel;
  2. pure geometry with no Copilot counterpart, because the webview cannot ship the codicon font: the
     collapse chevron triangle (`width: 0; height: 0; border-*: 3px/4px`), the tool status dot
     (`6px`), the todo glyph box (`width: 1em`), the diff marker column (`1.2em`), and the shell's
     own stand-ins — the tab/panel state dot (`6px`), the attention dot (`8px`), the history clock
     (`10px` ring), the send arrow and the stop square (`8px`);
  3. layout glue that carries no visual decision: `0`, `auto`, `fit-content`, `100%`, `100vh`, `normal`,
     `inherit`, `1em`, unitless flex factors.

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

### Shell (`src/webview/app`)

- `App.tsx` owns the only non-protocol state of the shell: the drafts (one per session, dropped when a
  tab closes), which overlay is open, and the focus token. Everything else it renders is host data.
- The tab bar follows VS Code's editor tabs (`multiEditorTabsControl.ts:186,893` for the
  `tablist`/`tab` roles, `multieditortabscontrol.css` for the geometry) at sidebar density: status dot,
  close button, and a dot that replaces the close icon while a background turn is waiting to be seen.
- The composer routes a submission to exactly one intent (design.md D6 of step 05). `/` opens the
  command candidates, which merge the host's catalog with `FRONTEND_COMMANDS` (`src/shared/commands.ts`,
  a mirror of the TUI's table); an exactly typed command runs, anything else completes first.
- The status row is Copilot's secondary toolbar: model / thinking / YOLO / workspace chips, the context
  ring (thresholds 75% / 90%, `chatContextUsageWidget.ts:468`), token totals with TTFT, and the channel
  chip (click to ping).
- Panels are host-owned (`panels.modelPicker` / `panels.sessionPicker` / `panels.branchPicker` non-null
  = open): the webview sends `openModelPicker` / `runPromptCommand` (`/ss`, `/rewind`, `/fork`) and the
  host answers with data + open state atomically. `panels.commandCatalog` is data only.

## Testing

```bash
pnpm run test              # all projects
pnpm run test:watch        # watch mode
```

- `tests/{shared,host,core,state,layers,artifact}` run in a **node** environment; the `vscode` module is
  aliased to `tests/mocks/vscode.ts` (a small, recording mock), which is what makes the extension host
  testable headless — no VS Code window, ever.
- `tests/webview` runs in **jsdom** with `@testing-library/react`; components are mounted through the
  real `mountApp` against the scripted host in `src/testing/mockBridge.ts`.
- `src/testing/fixtures.ts` has one fixture per cell kind plus the step 04 scenarios (streaming turn,
  failed tool call, approval) that both tests and the preview harness use.
- `tests/artifact/webviewBundle.test.ts` is the build-artifact gate (see above).
- The preview harness (`preview/main.tsx`) exposes a toolbar to switch fixtures, stream a turn, break
  the patch stream (resync recovery) and push a UI action — the fastest way to look at renderer changes
  without VS Code (`pnpm run dev:preview`, port 5199). `preview/preview-theme.css` emulates the Dark
  Modern theme variables so the page looks like the real sidebar.

### End-to-end smoke (`pnpm run smoke:gateway`)

Real gateway, real host, scripted model — no API key, no user state touched:

- a Node process runs the shipped `WingHost` + `SessionManager` + reducer against a **real
  `wing-gateway` child process** (temporary `WING_HOME`, OS-assigned port, never `32523`) and a
  **scripted OpenAI-compatible fake provider**; assertions are read off the host UI model through the
  same `applyCellPatches` the webview uses;
- 12 scenarios: create→subscribe→send→stream, tool call + diff, Ask round trip, Bash approval
  (approve / yolo), interrupt, resume replay + runtime state, rewind, fork, multi-tab isolation,
  reconnect resubscribe, local commands, gateway prompt command;
- exit codes: `0` pass, `1` fail (dumps the world + gateway log tail), `3` skipped (no `wing-gateway`
  binary, or `WING_SMOKE_SKIP=1`); `--only <name>` runs one scenario, `--keep` keeps the scratch dir;
- the gateway binary is resolved as `$WING_GATEWAY_BIN` → repository `.venv` → `PATH`;
- the smoke connects through a tiny local relay that strips `Sec-WebSocket-Extensions` (no
  `permessage-deflate`): Node 25's undici can silently stall compressed frames. That is an environment
  quirk of the smoke process, not of the shipped extension; `WING_SMOKE_KEEP_COMPRESSION=1` keeps
  compression to reproduce the A/B.

## Repository integration

- `make check` / `make test` include the TypeScript gates (`check-ts` / `test-ts`). Working on Python
  or Rust only? `SKIP_TS=1 make check` skips that group with an explicit note (needs Node ≥ 22.12 +
  pnpm 11 otherwise — `corepack enable`); CI never sets it, so the gate stays mandatory there.
- CI job `typescript-check` runs install → lint → format:check → typecheck → test → build →
  build:preview → package and uploads the `.vsix`.
- `pnpm install` in CI uses `--frozen-lockfile`; keep `pnpm-lock.yaml` committed.
- Every runtime dependency is declared as a `devDependency` on purpose: everything is bundled into
  `out/`/`dist/`, so `vsce` has no production dependency tree to ship.

## Troubleshooting

| Symptom                                        | Cause / fix                                                                                                                                                  |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| F5 opens a window without the Wing icon        | `out/extension.js` missing → run `pnpm build`; check the Extension Host log in the development window (Output → Extension Host).                             |
| View is blank / white                          | Open the webview dev tools (Command Palette → _Developer: Open Webview Developer Tools_) and check the console for CSP or module-loading errors.             |
| Webview console: `main.js.map violates … CSP`  | Harmless: the source map is fetched through `connect-src`, which the document does not open. Maps are dev-only (`**/*.map` is not packaged).                 |
| `pnpm: command not found` inside VS Code tasks | VS Code was launched without your shell `PATH`; run `pnpm build` in a terminal instead of via the task.                                                      |
| `vsce` complains about `@types/vscode`         | `engines.vscode` and `@types/vscode` must stay aligned (currently `^1.100.0` / `1.100.0`).                                                                   |
| Gateway unreachable / API key rejected         | Check Output → Wing, then `wing.autoStart` / `wing.wingPath` / `wing.apiKey`; start it manually with `wing start`.                                           |
| Resume shows no history (rare)                 | Node/undici compressed-frame stall (see smoke note above): switch tab once or run `Wing: Reconnect to Gateway` to re-push the replay; collect Output → Wing. |
