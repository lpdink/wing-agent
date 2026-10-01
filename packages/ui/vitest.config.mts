import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

/**
 * Two projects, matching the two halves of the package — the same split the
 * extension used while this code lived there:
 *
 * - `node` — the DOM-free half: the wire contract (`tests/protocol`), the mirror's
 *   patch/state logic (`tests/state`) and the import-graph guard
 *   (`tests/layers.test.ts`). These run at full speed without a document.
 * - `jsdom` — the React components under a document, mounted through the real
 *   `mountApp` against the scripted host (`src/testing/mockBridge.ts`), plus the
 *   shared setup that tears the document-level singletons down between cases.
 *
 * `pnpm test` runs both, so the layer guard is part of every gate
 * (`make test-ts`, CI).
 */
export default defineConfig({
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: 'node',
          environment: 'node',
          include: [
            'tests/{protocol,state}/**/*.test.ts',
            'tests/layers.test.ts',
            // Build-artifact gates: they run Vite in-process and read the emitted CSS,
            // so they need a Node environment, not a document (see the file headers).
            'tests/artifacts/**/*.test.ts',
          ],
        },
      },
      {
        extends: true,
        plugins: [react()],
        test: {
          name: 'jsdom',
          environment: 'jsdom',
          setupFiles: ['tests/setup/webview.ts'],
          include: [
            'tests/webview/**/*.test.ts',
            'tests/webview/**/*.test.tsx',
            // The wing-app cards (port batch 06b) render on their own, outside the
            // mounted app, so they get their own dirs; batch 06c adds the process
            // rows, the ask components and the shared atoms.
            'tests/markdown/**/*.test.tsx',
            'tests/tool/**/*.test.ts',
            'tests/tool/**/*.test.tsx',
            'tests/chat/**/*.test.ts',
            'tests/chat/**/*.test.tsx',
            'tests/ask/**/*.test.ts',
            'tests/ask/**/*.test.tsx',
            'tests/components/**/*.test.tsx',
          ],
          css: { modules: { classNameStrategy: 'non-scoped' } },
        },
      },
    ],
  },
});
