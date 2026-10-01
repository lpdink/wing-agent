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
          include: ['tests/{protocol,state}/**/*.test.ts', 'tests/layers.test.ts'],
        },
      },
      {
        extends: true,
        plugins: [react()],
        test: {
          name: 'jsdom',
          environment: 'jsdom',
          setupFiles: ['tests/setup/webview.ts'],
          include: ['tests/webview/**/*.test.ts', 'tests/webview/**/*.test.tsx'],
          css: { modules: { classNameStrategy: 'non-scoped' } },
        },
      },
    ],
  },
});
