/**
 * `pnpm shot` — the visual acceptance runner.
 *
 * It builds nothing itself (the npm script runs `vite build` first), starts one
 * fixture gateway per scene/viewport, drives the *built* app in Chrome with a fixed
 * viewport, waits for the scene's ready condition, and writes a PNG per scene.
 *
 * Contracts, in the order they are enforced:
 *
 * - the output directory defaults to the *current step's* task directory
 *   (`$WING_HOME/tasks/wing-app/08_web_transcript/shots` — each step's evidence lives
 *   with its own design/task documents; 09/11 move this constant on), `--out` overrides it;
 * - every image must be ≤ 2 MiB — the acceptance rule is machine-checked, not
 *   eyeballed;
 * - the browser is the local Chrome (`channel: 'chrome'`) by default, so a fresh
 *   `pnpm install` needs no browser download; `--browser=chromium` uses
 *   Playwright's own build (`pnpm exec playwright install chromium` once);
 * - every screenshot is taken with CSS animations and transitions disabled: the
 *   transcript has infinite ones (the streaming caret, the thinking shimmer), and a
 *   frame in the middle of an animation is not reproducible.
 * - a manifest (scene, viewport, file, bytes, what it shows) is printed and
 *   written to `manifest.json` next to the images.
 *
 * Nothing is left running: the browser and every fixture server are closed in a
 * `finally`, and the process exits non-zero on the first failed scene.
 */

import { mkdir, stat, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

import { chromium, type Browser, type BrowserContext } from 'playwright';

import { SCENES, VIEWPORTS, type SceneContext, type ShotScene, type ViewportId } from './scenes';
import { startFixtureServer } from './server';

const MAX_IMAGE_BYTES = 2 * 1024 * 1024;
/** The app root — the bundle runs from `apps/web/out/shot/`, two levels down. */
const APP_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');

interface Cli {
  readonly distDir: string;
  readonly outDir: string;
  readonly only: readonly string[];
  readonly browserChannel: 'chrome' | 'chromium' | 'msedge' | '';
  readonly list: boolean;
}

function parseArgs(argv: readonly string[]): Cli {
  const appRoot = APP_ROOT;
  let outDir = path.join(
    process.env['WING_HOME'] ?? path.join(os.homedir(), '.wing'),
    'tasks',
    'wing-app',
    '08_web_transcript',
    'shots',
  );
  let only: string[] = [];
  let browserChannel: Cli['browserChannel'] = 'chrome';
  let list = false;
  for (const arg of argv) {
    // pnpm passes the `--` separator through verbatim (`pnpm run shot -- --out=…`
    // arrives as `["--", "--out=…"]`), so accept both spellings: with the separator
    // (npm/yarn habit) and without it (what this repo documents, since pnpm needs
    // no separator to forward arguments).
    if (arg === '--') {
      continue;
    }
    if (arg.startsWith('--out=')) {
      outDir = path.resolve(arg.slice('--out='.length));
    } else if (arg.startsWith('--only=')) {
      only = arg
        .slice('--only='.length)
        .split(',')
        .map((name) => name.trim())
        .filter((name) => name !== '');
    } else if (arg.startsWith('--browser=')) {
      const value = arg.slice('--browser='.length);
      if (value !== 'chrome' && value !== 'chromium' && value !== 'msedge' && value !== 'none') {
        throw new Error(`unknown --browser value "${value}" (chrome | chromium | msedge | none)`);
      }
      browserChannel = value === 'none' ? '' : value;
    } else if (arg === '--list') {
      list = true;
    } else if (arg === '--help' || arg === '-h') {
      printUsage();
      process.exit(0);
    } else {
      throw new Error(`unknown argument "${arg}" (try --help)`);
    }
  }
  return { distDir: path.join(appRoot, 'dist'), outDir, only, browserChannel, list };
}

function printUsage(): void {
  process.stdout.write(
    [
      'usage: node out/shot/shot.mjs [--only=name[,name]] [--out=DIR] [--browser=chrome|chromium|msedge|none] [--list]',
      '',
      '  --only      render only these scenes (names from --list)',
      '  --out       output directory (default: $WING_HOME/tasks/wing-app/08_web_transcript/shots)',
      "  --browser   browser channel; `none` uses Playwright's bundled chromium",
      '  --list      print the scenes and exit',
      '',
    ].join('\n'),
  );
}

/** A loopback port with nothing listening (closed again immediately). */
async function reserveDeadPort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  const port = typeof address === 'object' && address !== null ? address.port : 0;
  await new Promise<void>((resolve) => {
    server.close(() => {
      resolve();
    });
  });
  return port;
}

function viewportIds(scene: ShotScene): ViewportId[] {
  return [...scene.viewports];
}

function fileName(scene: ShotScene, viewport: ViewportId): string {
  // The scheme only joins the name when the scene name does not already say it
  // (`sessions-dark`), so a file never reads `…-dark-…-dark.png`.
  const suffix = scene.colorScheme === 'dark' && !scene.name.endsWith('-dark') ? '-dark' : '';
  return `${scene.name}-${viewport}${suffix}.png`;
}

interface ManifestEntry {
  readonly scene: string;
  readonly viewport: ViewportId;
  readonly file: string;
  readonly bytes: number;
  readonly title: string;
  readonly colorScheme: 'light' | 'dark';
}

async function main(): Promise<void> {
  const cli = parseArgs(process.argv.slice(2));
  if (cli.list) {
    for (const scene of SCENES) {
      process.stdout.write(`${scene.name}\t${scene.viewports.join(',')}\t${scene.title}\n`);
    }
    return;
  }

  const indexHtml = path.join(cli.distDir, 'index.html');
  try {
    await stat(indexHtml);
  } catch {
    throw new Error(
      `no build to shoot: ${indexHtml} is missing — run \`pnpm build\` first (\`pnpm shot\` does it)`,
    );
  }

  const scenes = cli.only.length === 0 ? SCENES : SCENES.filter((scene) => cli.only.includes(scene.name));
  if (scenes.length === 0) {
    throw new Error(`no scene matched --only=${cli.only.join(',')} (see --list)`);
  }

  await mkdir(cli.outDir, { recursive: true });
  const context: SceneContext = { deadPort: await reserveDeadPort() };

  let browser: Browser | null = null;
  const manifest: ManifestEntry[] = [];
  try {
    browser = await chromium.launch({
      ...(cli.browserChannel === '' ? {} : { channel: cli.browserChannel }),
      headless: true,
    });
    for (const scene of scenes) {
      for (const viewportId of viewportIds(scene)) {
        const viewport = VIEWPORTS[viewportId];
        const server = await startFixtureServer({ distDir: cli.distDir, world: scene.world });
        let browserContext: BrowserContext | null = null;
        try {
          browserContext = await browser.newContext({
            viewport: { width: viewport.width, height: viewport.height },
            colorScheme: scene.colorScheme ?? 'light',
            deviceScaleFactor: 1,
            locale: 'en-US',
            timezoneId: 'UTC',
          });
          const seed = scene.seed?.(context) ?? {};
          if (Object.keys(seed).length > 0) {
            await browserContext.addInitScript((values: Record<string, string>) => {
              for (const [key, value] of Object.entries(values)) {
                globalThis.localStorage.setItem(key, value);
              }
            }, seed);
          }
          const first = await browserContext.newPage();
          await first.goto(server.url, { waitUntil: 'load' });
          await scene.ready(first);
          if (scene.interact !== undefined) {
            await scene.interact(first);
          }
          // One frame for fonts and the drawer/dialog transition to settle.
          await first.waitForTimeout(120);

          const file = path.join(cli.outDir, fileName(scene, viewportId));
          // `animations: 'disabled'` fast-forwards CSS animations and cancels the
          // infinite ones, so the streaming caret and the thinking shimmer render
          // the same frame on every run.
          await first.screenshot({ path: file, animations: 'disabled' });
          const stats = await stat(file);
          if (stats.size > MAX_IMAGE_BYTES) {
            throw new Error(
              `${path.basename(file)} is ${(stats.size / 1024 / 1024).toFixed(1)} MiB — the limit is 2 MiB`,
            );
          }
          manifest.push({
            scene: scene.name,
            viewport: viewportId,
            file,
            bytes: stats.size,
            title: scene.title,
            colorScheme: scene.colorScheme ?? 'light',
          });
          process.stdout.write(
            `✓ ${scene.name} (${viewportId} ${viewport.width}×${viewport.height}) → ${path.basename(file)} ${(stats.size / 1024).toFixed(0)} KiB\n`,
          );
        } finally {
          await browserContext?.close();
          await server.close();
        }
      }
    }
  } finally {
    await browser?.close();
  }

  await writeFile(
    path.join(cli.outDir, 'manifest.json'),
    `${JSON.stringify(
      { generatedAt: new Date().toISOString(), viewports: VIEWPORTS, images: manifest },
      null,
      2,
    )}\n`,
    'utf8',
  );
  process.stdout.write(`\n${manifest.length} screenshots in ${cli.outDir}\n`);
}

main().catch((error: unknown) => {
  process.stderr.write(`shot failed: ${error instanceof Error ? error.message : String(error)}\n`);
  process.exit(1);
});
