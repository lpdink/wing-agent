import { build } from 'esbuild';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * Bundles the two Electron entry points into `dist/`.
 *
 * - `src/main.ts` → `dist/main.js` **ESM** (`package.json` says `type: module`;
 *   Electron ≥ 28 runs an ESM main process).
 * - `src/preload.ts` → `dist/preload.cjs` **CommonJS**: a sandboxed preload
 *   cannot be ESM, and the shell keeps `sandbox: true`.
 *
 * `electron` stays external (it is injected by the runtime, never bundled);
 * nothing else is a dependency — `apps/desktop` has no `dependencies` block at
 * all, which `tests/layers.test.ts` enforces.
 */

const root = fileURLToPath(new URL('.', import.meta.url));

const shared = {
  absWorkingDir: root,
  bundle: true,
  platform: 'node',
  target: 'node22',
  external: ['electron'],
  sourcemap: true,
  logLevel: 'info',
  // Electron 44 ships Node 24; node22 is a conservative floor (esbuild only downlevels).
  tsconfig: path.join(root, 'tsconfig.node.json'),
};

await build({
  ...shared,
  entryPoints: ['src/main.ts'],
  outfile: 'dist/main.js',
  format: 'esm',
});

await build({
  ...shared,
  entryPoints: ['src/preload.ts'],
  outfile: 'dist/preload.cjs',
  format: 'cjs',
});

const renderer = path.join(root, 'renderer', 'index.html');
if (!existsSync(renderer)) {
  console.warn(
    `[esbuild] renderer/index.html is missing — packaged builds would show an empty window ` +
      `(step 10 copies the apps/web build into renderer/).`,
  );
}
