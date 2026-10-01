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
 *
 * There used to be a third: a `wing-ui-tokens` alias reaching into the package's
 * `src/styles/tokens.css`, because the package imported that sheet from `mount.tsx`
 * only and a component-consuming build tree-shook it away (rows lost their padding,
 * the ask card its border — in production builds only). Step 06c fixed the package
 * (the sheet is imported by the barrel, with `sideEffects` keeping it); step 08b
 * removed the alias, and `tests/transcript.theme.test.ts` pins the fix instead.
 */
const gateway = process.env['WING_DEV_GATEWAY'] ?? 'http://127.0.0.1:32523';

export default defineConfig({
  plugins: [react()],
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
