#!/usr/bin/env node

/**
 * Copy the web client build into the desktop shell's static directory.
 *
 * Build the web client first:
 *
 *   pnpm --filter @wing-agent/web build          # → apps/web/dist/
 *
 * Then run this script:
 *
 *   node apps/desktop/scripts/copy-web-build.mjs  # → apps/desktop/renderer/
 *
 * The script:
 * - removes `apps/desktop/renderer/` (the placeholder, or a previous build)
 * - copies the contents of `apps/web/dist/` into it
 * - preserves the `index.html` entry point (now the real SPA)
 *
 * The web build is a purely static SPA: Vite produces hashed assets under
 * `assets/` and a single `index.html`. The desktop shell's `wing-app://`
 * protocol services the directory as-is (see `src/web-document.ts`).
 */

import { cp, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const DESKTOP_ROOT = resolve(SCRIPT_DIR, '..');
const WEB_ROOT = resolve(DESKTOP_ROOT, '..', '..', 'apps', 'web');

const SOURCE = join(WEB_ROOT, 'dist');
const TARGET = join(DESKTOP_ROOT, 'renderer');

async function copyWebBuild() {
  // Remove the old renderer (placeholder or previous build).
  await rm(TARGET, { recursive: true, force: true });

  // Copy the Vite build output.
  await cp(SOURCE, TARGET, { recursive: true });

  console.log(`[copy-web-build] ${SOURCE} → ${TARGET}`);
  console.log('[copy-web-build] ok');
}

copyWebBuild().catch((error) => {
  console.error('[copy-web-build] failed:', error);
  process.exit(1);
});
