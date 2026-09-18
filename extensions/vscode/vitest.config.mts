import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

/**
 * Two test projects, matching the two runtimes we ship into:
 *
 * - `node` — host / core / shared / layer-guard logic. `vscode` is aliased to
 *   `tests/mocks/vscode.ts`, which is what makes host code headless-testable.
 * - `webview` — React components under jsdom, plus the patch reducer under node
 *   (the reducer has no DOM dependency; keep those files in `tests/webview/`
 *   only if they need the DOM).
 *
 * `pnpm test` runs both, so the layer guard is part of every gate (make test, CI).
 */
export default defineConfig({
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: 'node',
          environment: 'node',
          include: ['tests/{core,host,shared,layers,state}/**/*.test.ts'],
          alias: {
            vscode: fileURLToPath(new URL('tests/mocks/vscode.ts', import.meta.url)),
          },
        },
      },
      {
        extends: true,
        plugins: [react()],
        test: {
          name: 'webview',
          environment: 'jsdom',
          setupFiles: ['tests/setup/webview.ts'],
          include: ['tests/webview/**/*.test.ts', 'tests/webview/**/*.test.tsx'],
          css: { modules: { classNameStrategy: 'non-scoped' } },
        },
      },
    ],
  },
});
