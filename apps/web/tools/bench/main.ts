/**
 * Benchmark runner — long session performance measurement.
 *
 * Starts a fixture server with a 1000+ cell world, opens the page in
 * Chromium, and measures:
 *
 * - First render time (time until the transcript is interactive)
 * - Scroll frame rate (approximate)
 * - Memory usage
 *
 * Usage: `node out/bench/bench.mjs`
 *
 * Outputs a JSON report to stdout.
 */

import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

import type { Page } from 'playwright';
import { chromium } from 'playwright';

import { longSessionWorld } from './world';
import { startFixtureServer } from '../shot/server';

const APP_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const DIST_DIR = path.join(APP_ROOT, 'dist');

interface BenchReport {
  cellCount: number;
  firstRenderMs: number;
  scrollFps: number;
  memoryMb: number;
  error: string | null;
}

function measureCellCount(page: Page): Promise<number> {
  return page.evaluate(() => {
    const cells = document.querySelectorAll('[data-cell-kind]');
    return cells.length;
  });
}

async function measureFirstRenderMs(page: Page): Promise<number> {
  const start = Date.now();
  await page.getByTestId('transcript').waitFor({ timeout: 30_000 });
  // Wait for the transcript to have visible cells
  await page.waitForFunction(() => document.querySelectorAll('[data-cell-kind]').length > 10, {
    timeout: 30_000,
  });
  return Date.now() - start;
}

async function measureScrollFps(page: Page): Promise<number> {
  // Measure total scroll time through all 1000 cells using requestAnimationFrame.
  // In headless Chrome rAF fires at ~1 fps; in headed mode it's 60 fps.
  // We report the elapsed time and a qualitative assessment.
  const result = await page.evaluate(async () => {
    const transcript = document.querySelector('[data-testid="transcript"]');
    if (!transcript) return { fps: 0, ms: 0, smooth: false };

    const scrollHeight = transcript.scrollHeight;
    const step = scrollHeight / 40;

    return new Promise<{ fps: number; ms: number; smooth: boolean }>((resolve) => {
      let scrollPos = 0;
      let frames = 0;
      let janky = false;
      const startTime = performance.now();
      let lastTime = startTime;

      const tick = () => {
        const now = performance.now();
        frames++;

        // Check for jank: if a single step takes > 50ms, mark as janky
        if (now - lastTime > 50 && frames > 2) {
          janky = true;
        }
        lastTime = now;

        if (scrollPos < scrollHeight) {
          scrollPos += step;
          transcript.scrollTop = Math.min(scrollPos, scrollHeight);
          requestAnimationFrame(tick);
        } else {
          const elapsed = performance.now() - startTime;
          resolve({
            fps: frames > 0 && elapsed > 0 ? Math.round(frames / (elapsed / 1000)) : 0,
            ms: Math.round(elapsed),
            smooth: !janky,
          });
        }
      };

      requestAnimationFrame(tick);
    });
  });

  return result.fps;
}

async function measureMemory(page: Page): Promise<number> {
  try {
    const result = await page.evaluate(() => {
      const mem = (performance as unknown as { memory?: { usedJSHeapSize: number } }).memory;
      return mem?.usedJSHeapSize ?? 0;
    });
    return Math.round((result / (1024 * 1024)) * 10) / 10;
  } catch {
    return 0;
  }
}

async function main(): Promise<void> {
  const cellPairs = 500;
  const world = longSessionWorld(cellPairs);
  const firstSession = world.sessions[0];
  const worldSize = firstSession?.messages.length ?? 0;

  process.stdout.write(`Bench: starting fixture server with ${worldSize} cells (${cellPairs} pairs)...\n`);

  const server = await startFixtureServer({ distDir: DIST_DIR, world });

  let browser;
  const report: BenchReport = {
    cellCount: worldSize,
    firstRenderMs: 0,
    scrollFps: 0,
    memoryMb: 0,
    error: null,
  };

  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    const context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      deviceScaleFactor: 2,
      reducedMotion: 'reduce',
    });

    const page = await context.newPage();
    await page.goto(server.url, { waitUntil: 'load' });

    // Measure first render
    report.firstRenderMs = await measureFirstRenderMs(page);
    process.stdout.write(`  First render: ${report.firstRenderMs} ms\n`);

    // Count rendered cells
    const renderedCells = await measureCellCount(page);
    process.stdout.write(`  Rendered cells: ${renderedCells}\n`);

    // Measure scroll FPS
    report.scrollFps = await measureScrollFps(page);
    process.stdout.write(`  Scroll: ~${report.scrollFps} fps\n`);

    // Memory
    report.memoryMb = await measureMemory(page);
    process.stdout.write(`  JS heap: ${report.memoryMb} MB\n`);

    await context.close();
  } catch (error: unknown) {
    report.error = error instanceof Error ? error.message : String(error);
    process.stderr.write(`Bench error: ${report.error}\n`);
  } finally {
    await browser?.close();
    await server.close();
  }

  process.stdout.write(`\nReport:\n${JSON.stringify(report, null, 2)}\n`);
}

main().catch((error: unknown) => {
  process.stderr.write(`Fatal: ${error instanceof Error ? error.message : String(error)}\n`);
  process.exit(1);
});
