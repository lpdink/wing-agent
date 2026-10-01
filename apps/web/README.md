# `@wing-agent/web`

The mobile-friendly web client: a Vite + React shell that talks to the wing gateway
directly. Step 07 of `wing-app` — the shell, the connection layer, the session list and
the screenshot infrastructure. The transcript (08), the control plane / composer (09) and
the mobile polish + PWA (11) build on top of it.

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
src/connection ← GatewayRuntime: connection supervision, subscription, event reduction
src/app        ← React: the shell, the session list, the settings dialog, notices
tests/         ← vitest (node project: the framework-free core; dom project: the shell)
tools/shot/    ← the screenshot fixture gateway + runner (node, bundled by esbuild)
```

Dependencies only ever point downwards. Three mechanisms keep that honest: the split
tsconfigs (`tsconfig.app.json` has no node types, `tsconfig.tools.json` has no DOM),
the ESLint zones in `eslint.config.mjs`, and the import-graph guard in
`tests/layers.test.ts`.

## Screenshots (visual acceptance)

```bash
pnpm --filter @wing-agent/web shot          # build + bundle + render every scene
pnpm --filter @wing-agent/web shot -- --list                 # scene names
pnpm --filter @wing-agent/web shot -- --only=sessions,empty  # a subset
pnpm --filter @wing-agent/web shot -- --out=/tmp/shots       # another directory
```

- Output defaults to `$WING_HOME/tasks/wing-app/07_web_shell/shots` (task evidence, not
  committed) and each image must be ≤ 2 MiB — the runner fails if it is not.
- Scenes are code (`tools/shot/scenes.ts`): a fixture world + viewports + an optional
  interaction. The fixture gateway (`tools/shot/server.ts`) is a **real** HTTP + WS server
  serving `dist/` and the gateway protocol on one origin, on an OS-assigned port (the
  user's own 32523 is refused), so the page exercises its real `fetch` / `WebSocket` path.
- Browser: the local Chrome by default (`channel: 'chrome'`). For Playwright's own build:
  `pnpm --filter @wing-agent/web exec playwright install chromium` once, then
  `shot -- --browser=chromium`. `playwright`'s download script is blocked in
  `pnpm-workspace.yaml#allowBuilds` so a fresh install stays fast.
- A `manifest.json` (scene, viewport, file, bytes, what it shows) lands next to the images.

## Seams for the next steps

- **08 (transcript)**: `GatewayRuntime.getSnapshot()` → `record` + `recordVersion` (read
  `record.cells` when the version changed). The container is the `pane` section in
  `src/app/SessionPane.tsx`.
- **09 (control plane)**: add composer semantics to `GatewayRuntime` (send / queue /
  interrupt) — the socket is already there (`runtime.connection`), the wire helpers
  (`createClientRequest`, `encodeClientRequest`) come from `@wing-agent/client`.
- **11 (mobile)**: the drawer, the safe-area insets and the breakpoints live in
  `src/styles.css` + `src/app/Shell.tsx`; the manifest/service worker are not here yet.
