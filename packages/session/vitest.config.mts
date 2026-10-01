import { defineConfig } from 'vitest/config';

/**
 * One project: the package is pure reduction/view-model logic over already-decoded
 * gateway events — no DOM, no editor, no socket. `pnpm test` runs this, and the root
 * workspace gate (`make test-ts` / CI) runs it for every package; the import-graph
 * guard in `tests/layers.test.ts` is part of that on purpose.
 */
export default defineConfig({
  test: {
    name: '@wing-agent/session',
    environment: 'node',
    include: ['tests/**/*.test.ts'],
  },
});
