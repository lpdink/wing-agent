import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

/**
 * Pin the environment this config is written for, before Vite reads it.
 *
 * Two independent decisions are derived from `NODE_ENV`, and they have to agree:
 *
 * - the **JSX transform flavor** — Vite computes `isProduction` from
 *   `process.env.NODE_ENV`, `@vitejs/plugin-react` turns that into `jsxDEV` vs
 *   `jsx`/`jsxs`, and the `define` block below pins React's *runtime* shims to
 *   production unconditionally;
 * - Vite's own build defaults (minification, env prefixing).
 *
 * If they disagree the document dies at load: `jsxDEV` (development transform) is
 * called against a production runtime. That is not hypothetical — it is what an
 * in-process build under vitest (where `NODE_ENV=test`) produced while writing
 * `tests/artifact/`. The Vite CLI happens to set `NODE_ENV=production` itself for
 * `vite build`, but only when the variable is unset, so a config that is driven
 * from another tool, or from a shell that exports a different `NODE_ENV`, silently
 * built a broken bundle. This build has exactly one flavor by design (see the
 * `define` block below), so it states the requirement here instead of inheriting it.
 */
process.env.NODE_ENV = 'production';

/**
 * Values the bundle is built with, inlined at build time.
 *
 * The webview document has **no Node globals**: `process` is `undefined` there, so a
 * surviving `process.env` reference is a guaranteed `ReferenceError` during bundle
 * evaluation — a white view with nothing in the extension host log. Vite replaces
 * `process.env.NODE_ENV` for *app* builds but deliberately **not** for *lib* builds
 * (a library's consumer picks its own environment, and this config builds a lib), so
 * both entries below are load-bearing:
 *
 * - `NODE_ENV: 'production'` — selects React's production CJS entry. Without it the
 *   bundle carries *both* React builds (661 kB vs 233 kB) and every render path
 *   takes the development branch at runtime, on top of crashing the moment React's
 *   entry runs.
 * - `process.env: '({})'` — defence in depth for third-party code that reads
 *   `process.env.X` (later steps bundle syntax highlighting and markdown libraries).
 *   `({}).X` is `undefined` — a degraded feature — instead of an exception that
 *   kills the whole document. Today it removes nothing: `NODE_ENV` is the longer,
 *   more specific key and nothing else touches `process.env`, so the emitted bundle
 *   is byte-identical with and without it.
 *
 * The value is deliberately **constant, not `mode`-derived**: the webview bundle is
 * always a shipped artifact (no dev server, no HMR — iterative work goes through
 * `pnpm run dev:preview`), so "development React" has no legitimate consumer here.
 * Keeping the replacement unconditional also means every way of invoking the build
 * (`--mode development`, a future `build:watch`, …) produces a process-free bundle.
 * Bare `process` is intentionally **not** defined either: nothing in a browser
 * document can read it, and leaving it in place keeps Node-only code visible instead
 * of silently degrading it.
 *
 * `pnpm test` re-derives all of this in `tests/artifact/webviewBundle.test.ts` (same
 * config file, fresh build, executed in a DOM realm without Node globals) — keep the
 * two in sync.
 */
const define = {
  'process.env.NODE_ENV': JSON.stringify('production'),
  'process.env': '({})',
};

/**
 * Webview bundle → `dist/webview/main.js` + `dist/webview/main.css`.
 *
 * Format is **IIFE**, not ESM, on purpose: the webview's CSP is
 * `script-src 'nonce-…'`, and a single self-contained script needs no module
 * resolution, no dynamic chunk fetch and no `connect-src` allowance for the
 * webview origin. Everything the renderer needs (syntax highlighting grammars,
 * themes, icons) must therefore be bundled at build time — runtime `fetch` from
 * the webview is forbidden by design (design.md D4).
 *
 * The file names are part of the host contract: `src/host/chatViewProvider.ts`
 * references exactly these two paths.
 */
export default defineConfig({
  define,
  plugins: [react()],
  build: {
    outDir: 'dist/webview',
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: true,
    cssCodeSplit: false,
    lib: {
      entry: fileURLToPath(new URL('src/webview/main.tsx', import.meta.url)),
      name: 'WingWebview',
      formats: ['iife'],
      fileName: () => 'main.js',
      cssFileName: 'main',
    },
    rollupOptions: {
      output: {
        // Keep every emitted asset predictable: the host resolves them by name.
        // The stylesheet lands next to the bundle (`main.css`), other assets in
        // `assets/` (referenced through the bootstrap asset map when needed).
        assetFileNames: (assetInfo) =>
          assetInfo.names.some((name) => name.endsWith('.css')) ? 'main.css' : 'assets/[name][extname]',
      },
    },
  },
});
