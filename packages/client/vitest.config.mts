import { defineConfig } from 'vitest/config';

/**
 * One project: the package is pure protocol/transport logic — no DOM, no editor.
 * The wire-level fakes (`tests/support/fake-gateway.ts`) are in-process, so plain
 * node is the only environment it needs.
 *
 * `pnpm test` runs this, and the root workspace gate (`make test-ts` / CI) runs it
 * for every package — the import-graph guard in `tests/layers.test.ts` is part of
 * that, on purpose.
 */
export default defineConfig({
  test: {
    name: '@wing-agent/client',
    environment: 'node',
    include: ['tests/**/*.test.ts'],
  },
});
