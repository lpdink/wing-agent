/**
 * Screenshot driver for the port preview (dev-only; not part of any product build).
 *
 * Plain `chrome --headless --screenshot` captures whenever the page happens to be
 * ready, which is how the 06b delivery shipped a CodeBlock in its *plain* arm: the
 * cards activate their highlighting from an `IntersectionObserver` callback
 * (observer → shiki build → re-render), and the capture won that race. This driver
 * talks to Chrome over the DevTools protocol instead: it navigates, waits for the
 * page's own readiness flag (`document.documentElement.dataset.previewReady`, set
 * by `main.tsx` once every fence that must highlight has), optionally runs one
 * action, optionally asserts a DOM condition, grows the viewport to the page height
 * and only then captures — so a shot's evidence (the highlighted-fence count and the
 * assertion, both printed) does not depend on timing.
 *
 * Known boundary: Chrome once returned a *tiled* frame (the page repeated across the
 * image, at the dimensions that were asked for) in ~1 of 13 runs of the same
 * command. The driver cannot detect that, so the shots this repository ships were
 * hash-checked against re-captures (see the port-preview section of the step's
 * `design.md`, `~/.wing/tasks/wing-app/06b_port_i/design.md`).
 *
 * Usage (from `packages/ui`; the built page must be served — the exact commands for
 * the shipped shots are in the step's `design.md`):
 *
 *   node tools/port-preview/shoot.mts --url http://127.0.0.1:8791/ --out /tmp/a.png
 *   node tools/port-preview/shoot.mts --url …?section=code --action "<js>" --assert "<js>"
 *
 * Flags: `--width` / `--height` (CSS px, default 1100×1650 — the viewport starts
 * there and grows to the page before the capture), `--scale` (device pixel ratio,
 * default 1), `--timeout` (ms for readiness + assertion polls, default 15000),
 * `--action <js>` (evaluated once after readiness, awaited when it returns a
 * promise), `--assert <js>` (must evaluate truthy or the run fails), `--scrollbars`
 * (keep the themed scrollbars visible; the no-wrap code card's horizontal one is
 * evidence), `--keep-open` (leave the browser up for inspection). Chrome comes from
 * `$CHROME` or the macOS default path.
 */

import { spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import process from 'node:process';

interface Options {
  readonly url: string;
  readonly out: string;
  readonly width: number;
  readonly height: number;
  readonly scale: number;
  readonly timeout: number;
  readonly action: string | undefined;
  readonly assert: string | undefined;
  readonly scrollbars: boolean;
  readonly keepOpen: boolean;
}

const DEFAULT_CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

function parseArgs(argv: readonly string[]): Options {
  const values = new Map<string, string>();
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === undefined || !flag.startsWith('--')) continue;
    const name = flag.slice(2);
    if (name === 'keep-open' || name === 'scrollbars') {
      values.set(name, 'true');
      continue;
    }
    const next = argv[index + 1];
    if (next === undefined) throw new Error(`--${name} needs a value`);
    values.set(name, next);
    index += 1;
  }
  const url = values.get('url');
  const out = values.get('out');
  if (url === undefined || out === undefined) {
    throw new Error(
      'usage: shoot.mts --url <url> --out <file.png> [--width n] [--height n] [--scale n] [--action js] [--assert js] [--timeout ms] [--scrollbars] [--keep-open]',
    );
  }
  const number = (name: string, fallback: number): number => {
    const raw = values.get(name);
    return raw === undefined ? fallback : Number(raw);
  };
  return {
    url,
    out,
    width: number('width', 1100),
    height: number('height', 1650),
    scale: number('scale', 1),
    timeout: number('timeout', 15000),
    action: values.get('action'),
    assert: values.get('assert'),
    scrollbars: values.get('scrollbars') === 'true',
    keepOpen: values.get('keep-open') === 'true',
  };
}

/** A minimal DevTools-protocol client (Node's built-in WebSocket, no dependency). */
class Cdp {
  private readonly pending = new Map<
    number,
    { resolve: (value: unknown) => void; reject: (error: Error) => void }
  >();
  private readonly listeners = new Map<string, ((params: unknown) => void)[]>();
  private nextId = 1;
  // A field, not a constructor parameter property: Node runs this file with
  // type-stripping only, which refuses parameter properties.
  private readonly socket: WebSocket;

  private constructor(socket: WebSocket) {
    this.socket = socket;
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(String(event.data)) as {
        id?: number;
        result?: unknown;
        error?: { message: string };
        method?: string;
        params?: unknown;
      };
      if (message.id !== undefined) {
        const entry = this.pending.get(message.id);
        if (entry === undefined) return;
        this.pending.delete(message.id);
        if (message.error !== undefined) entry.reject(new Error(message.error.message));
        else entry.resolve(message.result);
        return;
      }
      if (message.method !== undefined) {
        for (const listener of this.listeners.get(message.method) ?? []) listener(message.params);
      }
    });
  }

  static async connect(url: string): Promise<Cdp> {
    const socket = new WebSocket(url);
    await new Promise<void>((resolve, reject) => {
      socket.addEventListener('open', () => {
        resolve();
      });
      socket.addEventListener('error', () => {
        reject(new Error(`cannot connect to ${url}`));
      });
    });
    return new Cdp(socket);
  }

  send<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    const id = this.nextId;
    this.nextId += 1;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  once(method: string, timeoutMs: number): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`timed out waiting for ${method}`));
      }, timeoutMs);
      const list = this.listeners.get(method) ?? [];
      list.push((params) => {
        clearTimeout(timer);
        resolve(params);
      });
      this.listeners.set(method, list);
    });
  }

  /** Evaluate an expression in the page; string results come back as strings. */
  async evaluate(expression: string): Promise<unknown> {
    const result = await this.send<{
      result: { value?: unknown; description?: string; type?: string };
      exceptionDetails?: { text: string; exception?: { description?: string } };
    }>('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails !== undefined) {
      throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
    }
    return result.result.value;
  }

  close(): void {
    this.socket.close();
  }
}

/** Poll an expression until it is truthy, or fail with the last result. */
async function waitFor(cdp: Cdp, expression: string, timeoutMs: number, what: string): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  for (;;) {
    last = await cdp.evaluate(expression);
    if (last === true) return;
    if (Date.now() > deadline) {
      throw new Error(
        `timed out after ${timeoutMs}ms waiting for ${what} (last value: ${JSON.stringify(last)})`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

/** Two animation frames: the browser has laid out and painted what the DOM says. */
const SETTLE = 'new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(() => done(true))))';

async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  const chrome = process.env.CHROME ?? DEFAULT_CHROME;
  const profile = await mkdtemp(join(tmpdir(), 'wing-port-preview-'));
  const browser = spawn(
    chrome,
    [
      '--headless=new',
      '--disable-gpu',
      '--no-first-run',
      '--no-default-browser-check',
      ...(options.scrollbars ? [] : ['--hide-scrollbars']),
      '--remote-debugging-port=0',
      `--user-data-dir=${profile}`,
      'about:blank',
    ],
    { stdio: ['ignore', 'ignore', 'pipe'] },
  );

  let cdp: Cdp | undefined;
  try {
    const wsUrl = await new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error('Chrome did not report a DevTools endpoint within 15s'));
      }, 15000);
      let buffer = '';
      browser.stderr.on('data', (chunk: Buffer) => {
        buffer += chunk.toString();
        const match = /DevTools listening on (ws:\/\/\S+)/.exec(buffer);
        if (match?.[1] !== undefined) {
          clearTimeout(timer);
          resolve(match[1]);
        }
      });
      browser.once('exit', (code) => {
        clearTimeout(timer);
        reject(new Error(`Chrome exited before becoming debuggable (code ${String(code)})`));
      });
    });

    const port = new URL(wsUrl).port;
    const targets = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()) as {
      type: string;
      webSocketDebuggerUrl?: string;
    }[];
    const page = targets.find((target) => target.type === 'page');
    if (page?.webSocketDebuggerUrl === undefined) {
      throw new Error('no page target to drive');
    }

    cdp = await Cdp.connect(page.webSocketDebuggerUrl);
    await cdp.send('Page.enable');
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: options.width,
      height: options.height,
      deviceScaleFactor: options.scale,
      mobile: false,
    });
    const loaded = cdp.once('Page.loadEventFired', options.timeout);
    await cdp.send('Page.navigate', { url: options.url });
    await loaded;

    await waitFor(
      cdp,
      "document.documentElement.dataset.previewReady === 'true'",
      options.timeout,
      'the preview readiness flag',
    );
    const highlighted = await cdp.evaluate("document.querySelectorAll('pre.shiki').length");

    if (options.action !== undefined) {
      await cdp.evaluate(options.action);
      await cdp.evaluate(SETTLE);
    }

    if (options.assert !== undefined) {
      const passed = await cdp.evaluate(options.assert);
      if (passed !== true) {
        throw new Error(`assertion failed: ${options.assert} (value: ${JSON.stringify(passed)})`);
      }
    }

    // Grow the viewport to the page before capturing: `captureBeyondViewport`
    // tiled the frame in one run (the viewport was shorter than the document), and
    // a viewport that covers the page has no such edge. The actions above already
    // ran, and the cards' activation survives a resize.
    const pageHeight = await cdp.evaluate(
      'Math.max(document.documentElement.scrollHeight, document.body.scrollHeight)',
    );
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: options.width,
      height: Math.ceil(Number(pageHeight)),
      deviceScaleFactor: options.scale,
      mobile: false,
    });
    await cdp.evaluate(SETTLE);

    const shot = await cdp.send<{ data: string }>('Page.captureScreenshot', { format: 'png' });
    const bytes = Buffer.from(shot.data, 'base64');
    await writeFile(options.out, bytes);
    process.stdout.write(
      `${options.out}: ${String(bytes.length)} bytes · ${String(options.width)}×${String(options.height)}@${String(options.scale)}x · highlighted fences: ${String(highlighted)} · ${options.assert === undefined ? 'no assertion' : 'assertion passed'}\n`,
    );
  } finally {
    cdp?.close();
    if (!options.keepOpen) {
      browser.kill('SIGTERM');
      await new Promise((resolve) => setTimeout(resolve, 200));
      if (browser.exitCode === null) browser.kill('SIGKILL');
    }
    await rm(profile, { recursive: true, force: true });
  }
}

await main();
