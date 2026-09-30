import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

import { woff2OnlyFonts } from './tools/fonts.mts';

/**
 * Preview harness → `dist/preview/`: the webview app mounted against a mock
 * bridge and fixtures, with no extension host involved.
 *
 * - `pnpm run build:preview` produces a static page (assertable in tests / CI).
 * - `pnpm run dev:preview` serves the same page with HMR for design work; it does
 *   not need VS Code, a gateway, or the extension to be built.
 */
export default defineConfig({
  root: 'preview',
  // Relative asset URLs so the built page also works when opened straight from
  // disk (file://), not only through a static server.
  base: './',
  // Same font trimming as the shipped webview build (`tools/fonts.mts`): the preview
  // imports the same renderer, and a 1.5 MB dev page helps nobody.
  plugins: [woff2OnlyFonts(), react()],
  build: {
    outDir: '../dist/preview',
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: true,
  },
  server: {
    port: 5199,
    strictPort: true,
    open: false,
  },
  preview: {
    port: 5199,
    strictPort: true,
  },
  resolve: {
    alias: {
      '@preview': fileURLToPath(new URL('preview', import.meta.url)),
    },
  },
});
