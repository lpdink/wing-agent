import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * `pnpm --filter @wing-agent/desktop smoke:app`
 *
 * Runs `--smoke` on the **packaged** app produced by `dist:dir` / `dist:mac`,
 * with a throwaway `--user-data-dir` so the check never touches real settings.
 * Exits with the app's exit code (0 = initialized, 1 = initialization failed),
 * and prints the app's JSON report.
 *
 * Packaged smoke is macOS-only in this step: the shell ships a macOS build.
 */

const root = fileURLToPath(new URL('..', import.meta.url));
const release = path.join(root, 'release');

/** Newest `<productName>.app` under the electron-builder output directory. */
function findAppBundle() {
  if (!existsSync(release)) {
    throw new Error(
      `no build output at ${release} — run \`pnpm --filter @wing-agent/desktop dist:dir\` first`,
    );
  }
  const candidates = readdirSync(release, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => path.join(release, entry.name, 'Wing.app'))
    .filter((candidate) => existsSync(candidate));
  const [first] = candidates.sort();
  if (first === undefined) {
    throw new Error(
      `no Wing.app under ${release} — run \`pnpm --filter @wing-agent/desktop dist:dir\` first`,
    );
  }
  return first;
}

function findBinary(appBundle) {
  const macosDirectory = path.join(appBundle, 'Contents', 'MacOS');
  const [binary] = readdirSync(macosDirectory);
  if (binary === undefined) {
    throw new Error(`no executable inside ${macosDirectory}`);
  }
  return path.join(macosDirectory, binary);
}

const appBundle = findAppBundle();
const binary = findBinary(appBundle);
const userData = mkdtempSync(path.join(tmpdir(), 'wing-desktop-smoke-'));
console.log(`[smoke:app] ${binary} --smoke --user-data-dir=${userData}`);

try {
  const result = spawnSync(binary, ['--smoke', `--user-data-dir=${userData}`], { stdio: 'inherit' });
  if (result.error !== undefined) {
    throw result.error;
  }
  process.exitCode = result.status ?? 1;
} finally {
  rmSync(userData, { recursive: true, force: true });
}
