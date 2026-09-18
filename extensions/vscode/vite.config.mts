import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

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
