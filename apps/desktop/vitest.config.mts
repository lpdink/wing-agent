import { defineConfig } from 'vitest/config';

/**
 * One project: the shell's logic is plain Node (config I/O, certificate policy,
 * gateway discovery, `wing-app://` document serving, menu template, argv
 * parsing), so no DOM and no Electron runtime is needed.
 *
 * `pnpm test` runs this, and the root workspace gate (`make test-ts` / CI) runs
 * it for every package — the import-graph guard in `tests/layers.test.ts` is
 * part of that, on purpose.
 */
export default defineConfig({
  test: {
    name: '@wing-agent/desktop',
    environment: 'node',
    include: ['tests/**/*.test.ts'],
  },
});
