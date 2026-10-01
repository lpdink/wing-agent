import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

/**
 * Two projects, matching the two runtimes this app has:
 *
 * - `node` — the framework-free core: settings (model / storage / URL construction),
 *   the connection runtime and the session-list derivations. These tests inject a
 *   fake socket factory + fake HTTP transport (the pattern from
 *   `extensions/vscode/tests/host/support/fake-gateway.ts`), so no DOM is involved.
 * - `dom` — the React shell under jsdom.
 *
 * `pnpm test` runs both, so the layering guard in `tests/layers.test.ts` is part of
 * every gate (`make test-ts`, CI).
 */
export default defineConfig({
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: 'node',
          environment: 'node',
          include: ['tests/**/*.test.ts'],
        },
      },
      {
        extends: true,
        plugins: [react()],
        test: {
          name: 'dom',
          environment: 'jsdom',
          setupFiles: ['tests/setup/dom.ts'],
          include: ['tests/**/*.test.tsx'],
        },
      },
    ],
  },
});
