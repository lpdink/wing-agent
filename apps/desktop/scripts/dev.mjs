import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * `pnpm --filter @wing-agent/desktop dev`
 *
 * Bundles the main process, then starts Electron against the dev server URL
 * (`WING_APP_URL`, default `http://localhost:5173` — the `apps/web` Vite server).
 * Nothing here waits for the dev server: the window shows Chromium's error page
 * until it is up, which is honest feedback while the web shell is being built.
 */

const root = fileURLToPath(new URL('..', import.meta.url));
const appUrl = process.env['WING_APP_URL']?.trim() || 'http://localhost:5173';

/** Resolves the Electron binary path from the installed `electron` package. */
async function electronBinary() {
  const electronPath = (await import('electron')).default;
  if (typeof electronPath !== 'string') {
    throw new Error('the electron package did not resolve to a binary path (did the install script run?)');
  }
  return electronPath;
}

function runNode(args) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, args, { cwd: root, stdio: 'inherit' });
    child.on('error', reject);
    child.on('close', (code) => {
      if (code === 0) {
        resolve();
      } else {
        reject(new Error(`node ${args.join(' ')} exited with ${String(code)}`));
      }
    });
  });
}

await runNode([path.join(root, 'esbuild.mjs')]);

const electron = await electronBinary();
// Extra arguments are forwarded to Electron, so `pnpm dev -- --smoke` runs the
// same headless self-check against the dev bundle (no window).
const forwarded = process.argv.slice(2);
// Progress goes to stderr: stdout stays the child's (the `--smoke` report is JSON).
process.stderr.write(`[dev] launching Electron against ${appUrl}\n`);
const child = spawn(electron, ['.', ...forwarded], {
  cwd: root,
  stdio: 'inherit',
  env: { ...process.env, WING_APP_URL: appUrl },
});

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    child.kill(signal);
  });
}
child.on('close', (code) => {
  process.exit(code ?? 0);
});
