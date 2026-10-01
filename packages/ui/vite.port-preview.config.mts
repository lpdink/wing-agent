import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

/**
 * Port preview → `dist/port-preview/`: a static page that renders the code/tool
 * cards ported in step 06b (`CodeBlock`, `TerminalBlock`, `DiffBlock`) against fake
 * data plus the wing token sheets, with no gateway, no shell and no extension host.
 *
 * Dev-only: it exists so the port can be inspected and screenshotted (`pnpm --filter
 * @wing-agent/ui run preview:port`, then a static server + headless Chrome) and so
 * the token sheets have one place where they are actually applied. It is not part of
 * any product build.
 */
export default defineConfig({
  root: 'tools/port-preview',
  // Relative asset URLs so the built page also works when opened straight from disk.
  base: './',
  plugins: [react()],
  build: {
    outDir: '../../dist/port-preview',
    emptyOutDir: true,
    target: 'es2022',
  },
  server: {
    port: 5299,
    strictPort: true,
    open: false,
  },
});
