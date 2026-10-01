# `@wing-agent/web`

The mobile-friendly web client: a Vite + React shell that talks to the wing gateway
directly. Steps 07–08 of `wing-app` — the shell, the connection layer, the session list,
the screenshot infrastructure and the **transcript** (every cell kind, rendered by the
shared `@wing-agent/ui` components). The control plane / composer (09) and the mobile
polish + PWA (11) build on top of it.

## Run it

```bash
# Development: the dev server proxies /api + /ws to the local gateway (127.0.0.1:32523),
# so the default settings ("this page's origin") need no configuration and no CORS.
pnpm --filter @wing-agent/web dev
# …point it at another gateway:
WING_DEV_GATEWAY=http://192.168.1.10:32523 pnpm --filter @wing-agent/web dev

# Production build (this is what step 03's `gateway.static_dir` serves):
pnpm --filter @wing-agent/web build

# Gates (also run by the root `make check-ts` / `make test-ts` and CI):
pnpm --filter @wing-agent/web typecheck | lint | test
```

## Layering

```
src/lib        ← pure helpers (no dependency besides react's absence of it)
src/settings   ← the gateway address model, its storage, and the URL construction
src/sessions   ← the session list as rows (pure derivation)
src/images     ← the workspace image adapter: source policy, gateway URL, three-state resolver
src/bridge     ← the renderer's intents (`@wing-agent/ui`) routed to this app
src/connection ← GatewayRuntime: connection supervision, subscription, event reduction, ask replies
src/app        ← React: the shell, the session list, the settings dialog, notices, the bridge hook
src/transcript ← React: the transcript container (the shared renderer's rows)
tests/         ← vitest (node project: the framework-free core; dom project: the shell)
tools/shot/    ← the screenshot fixture gateway + runner (node, bundled by esbuild)
```

## The transcript (step 08)

The transcript is the shared renderer: `Transcript` renders `@wing-agent/ui`'s
`TranscriptView`, whose `CellView` dispatches every cell kind to the same component the
VS Code webview mounts. What lives here is only what the package cannot own:

- **the projection**: `record` + `recordVersion` (step 07's snapshot) → `record.viewModel()`,
  memoized on the version;
- **the adapter** (`src/bridge/webBridge.ts`, injected by `src/app/useWebBridge.ts`):
  `resolveImages` → the gateway's image endpoint, `answerAsk` / `approveTool` → a
  `ClientRequest` on the socket (`GatewayRuntime.answerAsk`), `openLink` / `copyText` →
  the browser, `openFile` / `openDiff` → a notice (there is no editor to open — see the
  step's `design.md` D7 for the full list of differences from the webview);
- **the image adapter** (`src/images/`): the markdown source is pre-filtered exactly like
  the extension host's policy, fetched from `GET /api/workspace/image` _with the API key_
  (an `<img>` cannot send a header, and the bytes are what decides "image or link" before
  the renderer sees anything), and turned into a `blob:` object URL with an LRU lifetime;
- **the theme bridge** (`src/ui-theme.css`): the package reads `--vscode-*`, so this app
  maps them onto its own tokens — plus the renderer's own token layer, which the bundler
  would otherwise tree-shake (see the `wing-ui-tokens.css` alias in `vite.config.mts`).

Dependencies only ever point downwards. Three mechanisms keep that honest: the split
tsconfigs (`tsconfig.app.json` has no node types, `tsconfig.tools.json` has no DOM),
the ESLint zones in `eslint.config.mjs`, and the import-graph guard in
`tests/layers.test.ts`.

## Screenshots (visual acceptance)

```bash
pnpm --filter @wing-agent/web shot                            # build + bundle + render every scene
pnpm --filter @wing-agent/web shot --list                     # scene names
pnpm --filter @wing-agent/web shot --only=sessions,empty      # a subset
pnpm --filter @wing-agent/web shot --out=/tmp/shots           # another directory
```

> **No `--` separator.** This repo pins pnpm 11, which forwards `--` to the script
> verbatim (`node out/shot/shot.mjs -- --out=…`), where it used to be an unknown
> argument. `tools/shot/main.ts` now skips a bare `--`, so both spellings work — but
> the commands above are the documented ones (and what the gates run).

- Output defaults to `$WING_HOME/tasks/wing-app/08_web_transcript/shots` (task evidence,
  not committed; each step points this constant at its own directory) and each image must
  be ≤ 2 MiB — the runner fails if it is not.
- Scenes are code (`tools/shot/scenes.ts`): a fixture world + viewports + an optional
  interaction. The fixture gateway (`tools/shot/server.ts`) is a **real** HTTP + WS server
  serving `dist/` and the gateway protocol on one origin, on an OS-assigned port (the
  user's own 32523 is refused), so the page exercises its real `fetch` / `WebSocket` path.
- Browser: the local Chrome by default (`channel: 'chrome'`). For Playwright's own build:
  `pnpm --filter @wing-agent/web exec playwright install chromium` once, then
  `shot -- --browser=chromium`. `playwright`'s download script is blocked in
  `pnpm-workspace.yaml#allowBuilds` so a fresh install stays fast.
- A `manifest.json` (scene, viewport, file, bytes, what it shows) lands next to the images.
- Screenshots are taken with CSS animations disabled (the streaming caret and the thinking
  shimmer are infinite; a frame mid-animation is not reproducible), and a scene's `live`
  script (see `tools/shot/server.ts`) is what produces a frozen "mid-stream" state.
- Reproducibility: a rerun is pixel-identical **except for the fixture port digits**
  (each scene allocates a fresh OS-assigned port, and the number appears in the address
  pill / the settings preview / the connect-failure banner). Verified by diffing two
  runs pixel by pixel: every desktop shot differs only inside the address's 20×10 box,
  and the mobile shots (no address in the top bar) differ by zero pixels.

## Seams for the next steps

- **09 (control plane)**: `GatewayRuntime.sendClientRequest(frame)` is already the
  outbound primitive (ask replies use it), and `runtime.connection` is the socket; the
  wire helpers (`createClientRequest`, `encodeClientRequest`) come from
  `@wing-agent/client`. The intents the transcript cannot produce (`sendMessage`,
  `interrupt`, the panels) are already listed in `src/bridge/webBridge.ts` with the step
  that will own them. The composer goes into the `Shell`'s main column, below the pane.
- **11 (mobile)**: the drawer, the safe-area insets and the breakpoints live in
  `src/styles.css` + `src/app/Shell.tsx`; the manifest/service worker are not here yet.
  The transcript has no virtualization: the bundle is one chunk (the renderer brings
  shiki + KaTeX), and both are candidates for this step.
