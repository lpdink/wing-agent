import { vi } from 'vitest';

import type { HostToWebviewMessage } from '../../../src/shared';
import { silentLogger } from '../../../src/core';
import type { CoreLogger, HttpTransport } from '../../../src/core';
import { GatewayConnection, GatewayHttpClient } from '../../../src/core';
import type { WebviewIntent } from '../../../src/host/bridge';
import type { EditorActions } from '../../../src/host/editorActions';
import { GatewayLauncher } from '../../../src/host/gateway/launcher';
import type { GatewayLauncherOptions } from '../../../src/host/gateway/launcher';
import type { GatewaySettings } from '../../../src/host/settings';
import { WingHost } from '../../../src/host/wingHost';
import type { BridgeSink } from '../../../src/host/session/manager';

import { FakeGateway } from './fake-gateway';
import { WebviewMirror } from './mirror';

/**
 * Host harness: the **production** `WingHost` + `SessionManager` + reducer wired
 * to an in-process fake gateway, with the bridge's `postMessage` captured.
 *
 * Deliberately no stubs for the code under test: what the tests observe is what
 * the product does (messages on the wire to the webview, calls on the wire to
 * the gateway), which is exactly the surface a reviewer or the smoke test can
 * reason about.
 */

export interface RecordedEditor extends EditorActions {
  readonly links: string[];
  readonly files: { path: string; line: number | null }[];
  readonly diffs: { path: string; oldText: string; newText: string }[];
  readonly copies: string[];
}

function createEditor(): RecordedEditor {
  const links: string[] = [];
  const files: { path: string; line: number | null }[] = [];
  const diffs: { path: string; oldText: string; newText: string }[] = [];
  const copies: string[] = [];
  return {
    links,
    files,
    diffs,
    copies,
    openLink: (href: string) => {
      links.push(href);
      return Promise.resolve();
    },
    openFile: (path: string, line: number | null) => {
      files.push({ path, line });
      return Promise.resolve();
    },
    openDiff: (cell) => {
      const oldLines: string[] = [];
      const newLines: string[] = [];
      for (const row of cell.lines) {
        if (row.kind === 'hunk') {
          continue;
        }
        if (row.oldLine !== null) {
          oldLines.push(row.text);
        }
        if (row.newLine !== null) {
          newLines.push(row.text);
        }
      }
      diffs.push({ path: cell.path, oldText: oldLines.join('\n'), newText: newLines.join('\n') });
      return Promise.resolve();
    },
    copyText: (text: string) => {
      copies.push(text);
      return Promise.resolve();
    },
  };
}

export interface HarnessOptions {
  readonly workspace?: string | null;
  readonly now?: () => number;
  readonly launcher?: GatewayLauncherOptions;
  readonly logger?: CoreLogger;
  /** Fixed settings (defaults: 127.0.0.1:32523, no key, no explicit wing path). */
  readonly settings?: Partial<GatewaySettings>;
  /** Retry base for the host's own connect ladder (ms). */
  readonly connectRetryBaseMs?: number;
  /** HTTP transport override (tests that script transport failures). */
  readonly httpTransport?: HttpTransport;
}

export interface HostHarness {
  readonly host: WingHost;
  readonly gateway: FakeGateway;
  readonly editor: RecordedEditor;
  readonly posted: HostToWebviewMessage[];
  /**
   * The webview's mirror, fed with every message the host posts.
   *
   * It uses the shipped `applyCellPatches`, so a host bug that violates the
   * patch contract shows up as {@link WebviewMirror.errors} instead of a
   * transcript that only *looks* right.
   */
  readonly mirror: WebviewMirror;
  readonly errors: string[];
  readonly settings: GatewaySettings;
  /** `wipe` clears the capture so a test can assert "nothing was posted". */
  wipe(): void;
  /** First connect + `ready` sequence, the way activation + a mounted view do it. */
  boot(): Promise<void>;
  ready(protocolVersion?: number): Promise<void>;
  intent(intent: WebviewIntent): Promise<void>;
  /** Messages of one type, in order. */
  ofType<T extends HostToWebviewMessage['type']>(type: T): Extract<HostToWebviewMessage, { type: T }>[];
  /** The last hydrate for a session (assert the full snapshot the webview got). */
  hydrateFor(sessionId: string): Extract<HostToWebviewMessage, { type: 'hydrate' }> | undefined;
  /** `seq` values of every patch message, in order. */
  patchSeqs(): number[];
  /** Frames the client sent over the WS (the user-message path). */
  clientFrames(): Record<string, unknown>[];
  dispose(): void;
}

export function createHostHarness(options: HarnessOptions = {}): HostHarness {
  const now = options.now ?? Date.now;
  const gateway = new FakeGateway({ now });
  const posted: HostToWebviewMessage[] = [];
  const errors: string[] = [];
  const editor = createEditor();
  const settings: GatewaySettings = {
    host: '127.0.0.1',
    port: 32_523,
    apiKey: null,
    wingPath: null,
    autoStart: true,
    ...options.settings,
  };
  const launcher = new GatewayLauncher({
    // The default fake world is "a gateway is already running": the launcher's
    // probe answers without touching the HTTP transport, and `run` fails loudly
    // if a test forgot to configure the not-running scenario.
    probe: () => Promise.resolve(true),
    run: () => Promise.resolve({ code: 1, output: 'unexpected wing start' }),
    sleep: () => Promise.resolve(),
    logger: options.logger ?? silentLogger,
    ...options.launcher,
  });
  const logger = options.logger ?? silentLogger;

  const view = new WebviewMirror();
  const sink: BridgeSink = {
    post: (message) => {
      posted.push(message);
      view.apply(message);
    },
  };
  const host = new WingHost({
    sink: () => sink,
    settings: () => settings,
    gatewayFactory: {
      create: () => ({
        connection: new GatewayConnection({
          wsUrl: 'ws://fake-gateway/ws',
          socketFactory: gateway.factory,
          logger,
          now,
        }),
        http: new GatewayHttpClient({
          baseUrl: 'http://fake-gateway',
          transport: options.httpTransport ?? gateway.transport,
          logger,
        }),
      }),
    },
    launcher,
    workspaceFolder: () => (options.workspace === undefined ? '/workspace' : options.workspace),
    editor,
    reportError: (message) => {
      errors.push(message);
    },
    now,
    logger,
    ...(options.connectRetryBaseMs === undefined ? {} : { connectRetryBaseMs: options.connectRetryBaseMs }),
  });

  host.attachSink(sink);

  const harness: HostHarness = {
    host,
    gateway,
    editor,
    posted,
    mirror: view,
    errors,
    settings,
    wipe: () => {
      posted.length = 0;
    },
    boot: async () => {
      await host.start();
      await harness.ready();
    },
    ready: async (protocolVersion = 1) => {
      host.onReady(protocolVersion);
      await flushMicrotasks(20);
    },
    intent: async (intent) => {
      await host.onIntent(intent);
      await flushMicrotasks();
    },
    ofType: (type) =>
      posted.filter(
        (message): message is Extract<HostToWebviewMessage, { type: typeof type }> => message.type === type,
      ),
    hydrateFor: (sessionId) => {
      const candidates = posted.filter(
        (message): message is Extract<HostToWebviewMessage, { type: 'hydrate' }> =>
          message.type === 'hydrate' && message.session.sessionId === sessionId,
      );
      return candidates[candidates.length - 1];
    },
    patchSeqs: () =>
      posted
        .filter(
          (message): message is Extract<HostToWebviewMessage, { type: 'patch' }> => message.type === 'patch',
        )
        .map((message) => message.seq),
    clientFrames: () => gateway.frames.map((entry) => entry.frame),
    dispose: () => {
      host.dispose();
    },
  };
  return harness;
}

/** Let every queued microtask (handshake, HTTP promises) run. */
export async function flushMicrotasks(times = 8): Promise<void> {
  for (let index = 0; index < times; index += 1) {
    await Promise.resolve();
  }
  if (vi.isFakeTimers()) {
    await vi.advanceTimersByTimeAsync(0);
  }
}

/** Standard user-message wire shape assertions use this helper. */
export function textPayload(content: string, sessionId: string): Record<string, unknown> {
  return {
    type: 'text',
    content,
    session_id: sessionId,
    created_at: '2026-09-18T08:00:00.000',
    request_id: 'fake-event',
  };
}
