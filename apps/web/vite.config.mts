import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

/**
 * The web client's build/dev config.
 *
 * Two things matter beyond the React plugin:
 *
 * 1. **Dev proxy (zero-config development).** With the default settings the app
 *    talks to its own origin (`host: ''`, see `src/settings/settings.ts`), so in
 *    `pnpm dev` the gateway has to *be* behind that origin — hence the proxy:
 *    `/api/*` (HTTP) and `/ws` (WebSocket upgrade) forward to the local gateway.
 *    That is why `pnpm dev` needs no settings, no login and no CORS
 *    (`gateway.cors_origins` stays off). Point it somewhere else with
 *    `WING_DEV_GATEWAY=http://host:port pnpm dev`.
 * 2. **`build.outDir = dist`** — the directory step 03's `gateway.static_dir`
 *    serves, and the directory the screenshot fixture gateway hosts.
 */
const gateway = process.env['WING_DEV_GATEWAY'] ?? 'http://127.0.0.1:32523';

/**
 * `@wing-agent/ui`'s token layer, named so a stylesheet can ask for it.
 *
 * The package ships its design tokens in `src/styles/tokens.css`, and it imports
 * them from **one** module: `src/mount.tsx` (`import './styles/tokens.css'`), the
 * entry a consumer reaches through `mountApp`. The web shell renders the package's
 * components directly instead of mounting its app, and `package.json` declares only
 * its stylesheets as side-effectful (the `sideEffects` glob) — so a bundler that
 * tree-shakes the unused
 * `mountApp` export drops `mount.tsx` *and with it the tokens*, leaving every
 * `var(--wing-*)` reference in the renderer's 500 rules resolving to nothing (rows
 * lose their padding, the ask card its border, …) while every `--vscode-*` rule
 * keeps working. It is invisible in `vite dev` (no tree-shaking) and in the VS Code
 * webview (it mounts `mountApp`), which is exactly why it is worth an alias and a
 * comment instead of a silent copy.
 *
 * The alias is the whole workaround: `src/ui-theme.css` imports it next to the
 * `--vscode-*` mapping. `tests/transcript.theme.test.ts` pins both ends (the package
 * still keeps its tokens there; this app still imports them), and the fix on the
 * package side is to import the stylesheet from a module every consumer loads (or to
 * export it as `./styles`) — when that lands, this alias disappears.
 */
const UI_TOKENS = fileURLToPath(new URL('../../packages/ui/src/styles/tokens.css', import.meta.url));

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { 'wing-ui-tokens.css': UI_TOKENS },
  },
  server: {
    proxy: {
      '/api': { target: gateway, changeOrigin: true },
      // `ws: true` is what makes the upgrade work; without it the proxy would
      // answer the handshake with an HTTP response and the client would see an
      // immediate close.
      '/ws': { target: gateway, ws: true, changeOrigin: true },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
    // A gateway-hosted SPA under a reverse proxy benefits from a smaller chunk
    // set; the app is small, so keeping the default warning threshold quiet is
    // enough (no manual chunking yet — 08 layers the transcript in).
    emptyOutDir: true,
    // The transcript pulls in `@wing-agent/ui`, which bundles shiki's grammars and
    // themes plus KaTeX with its fonts — ~1.7 MB of JS (~390 kB gzipped) that the
    // VS Code webview ships the same way. One bundle is still the right shape for a
    // gateway-hosted SPA (the fonts are separate requests anyway); splitting the
    // transcript out is a candidate for step 11's mobile work, and until then the
    // default 500 kB warning would only train everyone to ignore it.
    chunkSizeWarningLimit: 2_000,
  },
});
