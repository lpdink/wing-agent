import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

import { JSDOM } from 'jsdom';
import { build } from 'vite';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { BRIDGE_PROTOCOL_VERSION, WEBVIEW_ROOT_ID } from '../../src/shared';
import { makeEmptySession } from '../../src/testing/fixtures';

/**
 * Build-artifact gate: the *bundled* webview, executed in a DOM that has no Node
 * globals.
 *
 * This exists because the failure it guards against is invisible to every other gate.
 * Source-level checks (typecheck, lint, the layer guard, component tests) see
 * TypeScript; this one sees what the bundler actually emitted. Checkpoint ① shipped a
 * webview that was white in a real VS Code window — `Uncaught ReferenceError:
 * process is not defined` — while every other gate was green, because the webview
 * build is a Vite **lib** build and Vite does not inline `process.env.NODE_ENV` there
 * (React's CJS entry then keeps its `process.env.NODE_ENV === 'production'` branch,
 * which cannot run in a document).
 *
 * The test therefore:
 *
 * 1. builds with the repository's own `vite.config.mts` into a temporary directory —
 *    no manual `pnpm run build` prerequisite, and a config regression (dropping the
 *    `define` block) is caught, because the config *is* the input. The emitted bytes
 *    are identical to the CLI build's: same config, same environment
 *    (`vite.config.mts` pins `NODE_ENV=production`, so driving the build from vitest
 *    — where `NODE_ENV=test` — does not change the artifact);
 * 2. asserts the emitted text: no `process.env`, no Node/CJS leftovers, no React
 *    development build and no development JSX transform;
 * 3. executes the bundle through `node:vm` in a fresh jsdom realm — a document with
 *    `window`/`document` and *nothing else* — and checks that the app really mounts.
 *    Before the fix this threw the very ReferenceError the user saw in the webview
 *    console (`main.js:13`).
 */
const PACKAGE_ROOT = fileURLToPath(new URL('../..', import.meta.url));
/** The bundle file name is part of the host contract (`src/host/chatViewProvider.ts`). */
const BUNDLE_FILE_NAME = 'main.js';

/**
 * Strings that only exist in React's development build.
 *
 * Verified against the pre-fix artifact (the one that crashed): `Invalid hook call`
 * appears 3×, `act(...)` 5×, `use client` 2×; after the fix, 0× each. They are a
 * *secondary* signal — the primary one is that the bundle runs at all — so a future
 * React release renaming them degrades this assertion towards "no-op" rather than
 * towards "false alarm".
 */
const REACT_DEV_MARKERS = ['Invalid hook call', 'act(...)', 'use client'] as const;

/**
 * Node/CJS leftovers that are always wrong in a browser document.
 *
 * `process` is deliberately absent: React 19 legitimately ships
 * `typeof process === 'object' && typeof process.emit === 'function'` in its error
 * reporting, and telling that form apart from a leak needs polarity-aware guard
 * analysis we do not want to maintain here. `process.env` — the form that actually
 * broke the view — is asserted separately and exactly.
 */
const NODE_LEFTOVERS = [
  'require(',
  'module.exports',
  '__dirname',
  '__filename',
  'globalThis.process',
] as const;

/** What the probe reads back out of the executed document. */
interface WebviewProbe {
  readonly pingButtonText: string | null;
  readonly bridgeStatusText: string | null;
  readonly sessionTitleText: string | null;
  readonly rootChildCount: number;
}

/** The `acquireVsCodeApi()` surface the transport uses. */
interface VsCodeApiStub {
  postMessage(message: unknown): void;
  getState(): unknown;
  setState(state: unknown): void;
}

/** The built bundle, as text. */
interface BuiltBundle {
  readonly source: string;
  readonly bytes: number;
}

/** A jsdom document that is running the bundle. */
interface ExecutedWebview {
  /** Messages the bundle pushed through `acquireVsCodeApi()` so far. */
  readonly posts: unknown[];
  /** Reads the mounted document inside the realm the bundle ran in. */
  readonly probe: () => WebviewProbe;
  /** Delivers a host→webview message the way VS Code's `postMessage` does. */
  readonly deliver: (message: unknown) => void;
  /** Closes the simulated document (stops jsdom timers so vitest can exit). */
  readonly close: () => void;
}

let outDir: string;
let built: BuiltBundle;
let executed: ExecutedWebview | undefined;

beforeAll(async () => {
  outDir = mkdtempSync(path.join(tmpdir(), 'wing-webview-artifact-'));

  // Build through the repository config, not through a copy of its options: the
  // `define` block, the entry point and the lib format are all part of what this gate
  // verifies. `outDir` is redirected so a test run never touches `dist/`.
  await build({
    configFile: path.join(PACKAGE_ROOT, 'vite.config.mts'),
    root: PACKAGE_ROOT,
    logLevel: 'silent',
    build: { outDir, emptyOutDir: true },
  });

  const source = readFileSync(path.join(outDir, BUNDLE_FILE_NAME), 'utf8');
  built = { source, bytes: Buffer.byteLength(source) };
}, 120_000);

afterAll(() => {
  executed?.close();
  executed = undefined;
  rmSync(outDir, { recursive: true, force: true });
});

/**
 * Occurrences of `needle` with a little context around each.
 *
 * Assertions compare this against `[]` instead of using `toContain`/`toMatch` on the
 * artifact: a failure then prints a handful of short snippets, not 230 kB of bundle.
 */
function occurrences(source: string, needle: string): string[] {
  const found: string[] = [];
  let index = source.indexOf(needle);
  while (index !== -1 && found.length < 5) {
    found.push(source.slice(Math.max(0, index - 40), index + needle.length + 40));
    index = source.indexOf(needle, index + needle.length);
  }
  return found;
}

/**
 * The document the bundle runs in, executed on first use.
 *
 * Lazily executed (rather than in `beforeAll`) so a broken artifact still produces
 * the exact *textual* failures — "the bundle reads process.env" — next to the crash
 * that follows from them, instead of one suite-level error.
 */
function webviewDocument(): ExecutedWebview {
  executed ??= runInWebviewRealm(built.source);
  return executed;
}

/**
 * Executes `source` the way a webview does.
 *
 * `runScripts: 'outside-only'` gives jsdom a real VM realm for its window without
 * loading scripts itself; `getInternalVMContext()` hands us that realm, which has
 * `window`, `document` and friends but **no** `process`, `require` or `Buffer` —
 * jsdom installs DOM APIs only. Executing the bundle there reproduces the browser at
 * the one thing that matters for this failure mode: which globals exist.
 */
function runInWebviewRealm(source: string): ExecutedWebview {
  const dom = new JSDOM(`<!doctype html><html><body><div id="${WEBVIEW_ROOT_ID}"></div></body></html>`, {
    runScripts: 'outside-only',
    url: 'https://webview.test/',
  });
  const realm = dom.getInternalVMContext();

  const posts: unknown[] = [];
  const hostWindow = dom.window as unknown as { acquireVsCodeApi?: () => VsCodeApiStub };
  hostWindow.acquireVsCodeApi = () => ({
    postMessage: (message) => {
      posts.push(message);
    },
    getState: () => undefined,
    setState: () => undefined,
  });

  vm.runInContext(source, realm, { filename: BUNDLE_FILE_NAME });

  return {
    posts,
    probe: () =>
      JSON.parse(
        vm.runInContext(
          `JSON.stringify({
            pingButtonText: document.querySelector('[data-testid="ping-button"]')?.textContent ?? null,
            bridgeStatusText: document.querySelector('[data-testid="bridge-status"]')?.textContent ?? null,
            sessionTitleText: document.querySelector('[data-testid="session-title"]')?.textContent ?? null,
            rootChildCount: document.getElementById(${JSON.stringify(WEBVIEW_ROOT_ID)}).childElementCount,
          })`,
          realm,
        ),
      ) as WebviewProbe,
    deliver: (message) => {
      // The transport subscribes with `window.addEventListener('message', …)`, so a
      // dispatched event is exactly how a real webview receives host traffic.
      vm.runInContext(
        `window.dispatchEvent(new MessageEvent('message', { data: ${JSON.stringify(message)} }))`,
        realm,
      );
    },
    close: () => {
      dom.window.close();
    },
  };
}

/**
 * React renders through its own scheduler, so wait for a condition instead of
 * assuming it holds on the very next microtask.
 */
async function waitFor(
  probe: () => WebviewProbe,
  ready: (value: WebviewProbe) => boolean,
): Promise<WebviewProbe> {
  const deadline = Date.now() + 5_000;
  let value = probe();
  while (!ready(value) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
    value = probe();
  }
  return value;
}

const isMounted = (value: WebviewProbe): boolean => value.rootChildCount > 0;

describe('webview bundle artifact', () => {
  it('builds and produces a non-trivial bundle', () => {
    expect(built.bytes).toBeGreaterThan(100_000);
    expect(occurrences(built.source, 'acquireVsCodeApi')).not.toEqual([]);
  });

  it('never reads process.env', () => {
    // The whole point of the `define` block in vite.config.mts. Which entry React's
    // CJS shim picks, and whether the value is inlined at all, is decided here.
    expect(occurrences(built.source, 'process.env')).toEqual([]);
  });

  it('carries no Node/CJS leftovers', () => {
    const leftovers = NODE_LEFTOVERS.filter((needle) => built.source.includes(needle));
    expect(leftovers).toEqual([]);
  });

  it('uses the production JSX transform', () => {
    // `jsxDEV` comes from React's *development* JSX runtime, which is selected from
    // `NODE_ENV` when Vite resolves the config — the value `vite.config.mts` pins so
    // that the transform and the `define` block agree. A bundle mixing the two dies
    // at load (see that comment), so this guards the build environment too.
    expect(occurrences(built.source, 'jsxDEV')).toEqual([]);
  });

  it('is built from production React', () => {
    const devMarkers = REACT_DEV_MARKERS.filter((needle) => built.source.includes(needle));
    expect(devMarkers).toEqual([]);
  });
});

describe('webview bundle in a document without Node globals', () => {
  it('mounts the scaffold UI', async () => {
    const probe = await waitFor(webviewDocument().probe, isMounted);

    expect(probe.rootChildCount).toBeGreaterThan(0);
    expect(probe.pingButtonText).toBe('Ping host');
    // No host answered yet — the transport is up, the session is not.
    expect(probe.bridgeStatusText).toBe('bridge: connecting');
  });

  it('announces the bridge handshake to the host', () => {
    expect(webviewDocument().posts).toContainEqual({
      type: 'ready',
      protocolVersion: BRIDGE_PROTOCOL_VERSION,
    });
  });

  it('renders a hydrate pushed from the host', async () => {
    const webview = webviewDocument();
    await waitFor(webview.probe, isMounted);
    const session = makeEmptySession('artifact-session');

    webview.deliver({ type: 'hydrate', session });
    const probe = await waitFor(webview.probe, (value) => value.sessionTitleText === session.title);

    expect(probe.sessionTitleText).toBe(session.title);
    expect(probe.bridgeStatusText).toBe('bridge: ready');
  });
});
