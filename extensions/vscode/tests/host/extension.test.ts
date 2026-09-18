import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type * as vscode from 'vscode';

import { COMMAND_IDS } from '../../src/host/commands';
import { ChatViewProvider } from '../../src/host/chatViewProvider';
import { activate, deactivate } from '../../src/host/extension';
import { disposeLog } from '../../src/host/log';
import { mockState } from '../mocks/vscode';

import type * as wingHostModule from '../../src/host/wingHost';

/**
 * Activation wiring.
 *
 * `WingHost` is replaced by a recording fake here **on purpose**: the real host
 * would probe `127.0.0.1:32523` and — with a `wing` binary on PATH — could
 * actually start a gateway. Wiring is what this file asserts; the host's
 * behaviour is covered by the harness tests, which never touch the network
 * either (they inject the fake gateway).
 */

// `vi.mock` is hoisted, so the fake and its state must be created in `vi.hoisted`.
const mocks = vi.hoisted(() => {
  class FakeHost {
    readonly sinkCalls: unknown[] = [];
    started = false;
    disposed = false;

    constructor(readonly options: Record<string, unknown>) {
      mocks.started.host = this;
    }

    attachSink(sink: unknown): void {
      this.sinkCalls.push(sink);
    }

    detachSink(): void {
      this.sinkCalls.push(null);
    }

    start(): Promise<void> {
      this.started = true;
      return Promise.resolve();
    }

    onReady(): void {}

    onResync(): void {}

    onIntent(): Promise<void> {
      return Promise.resolve();
    }

    newSession(): Promise<string | null> {
      return Promise.resolve('sess-new');
    }

    reconnect(): Promise<void> {
      return Promise.resolve();
    }

    dispose(): void {
      this.disposed = true;
    }
  }
  return { FakeHost, started: { host: null as FakeHost | null } };
});

vi.mock('../../src/host/wingHost', async (importOriginal) => {
  const actual = await importOriginal<typeof wingHostModule>();
  return { ...actual, WingHost: mocks.FakeHost };
});

function makeContext(): vscode.ExtensionContext & { subscriptions: { dispose(): unknown }[] } {
  return {
    extensionUri: { toString: () => 'file:///extension' },
    subscriptions: [],
  } as unknown as vscode.ExtensionContext & { subscriptions: { dispose(): unknown }[] };
}

beforeEach(() => {
  mockState.reset();
  disposeLog();
  mocks.started.host = null;
});

afterEach(() => {
  disposeLog();
});

describe('activate', () => {
  it('registers the chat view provider with retained context', () => {
    const context = makeContext();

    activate(context);

    expect(mockState.viewProviders).toHaveLength(1);
    const registration = mockState.viewProviders[0];
    expect(registration?.viewId).toBe(ChatViewProvider.viewId);
    expect(registration?.provider).toBeInstanceOf(ChatViewProvider);
    expect(registration?.options).toEqual({ webviewOptions: { retainContextWhenHidden: true } });
  });

  it('starts the gateway lifecycle and exposes the palette commands', () => {
    const context = makeContext();

    activate(context);

    expect(mocks.started.host?.started).toBe(true);
    expect([...mockState.commands.keys()].sort()).toEqual(
      [COMMAND_IDS.newSession, COMMAND_IDS.reconnectGateway].sort(),
    );
    expect(mockState.textDocumentContentProviders.has('wing-diff')).toBe(true);
  });

  it('runs the commands through the host', async () => {
    const context = makeContext();
    activate(context);

    await mockState.commands.get(COMMAND_IDS.newSession)?.();
    await mockState.commands.get(COMMAND_IDS.reconnectGateway)?.();

    expect(mocks.started.host).not.toBeNull();
  });

  it('puts every disposable on the extension context and tears the host down', () => {
    const context = makeContext();

    activate(context);

    expect(context.subscriptions.length).toBeGreaterThanOrEqual(7);
    expect(context.subscriptions).toContain(mockState.viewProviders[0]);
    for (const disposable of context.subscriptions) {
      disposable.dispose();
    }
    expect(mockState.viewProviders[0]?.disposed).toBe(true);
    expect(mockState.outputChannels[0]?.disposed).toBe(true);
    expect(mocks.started.host?.disposed).toBe(true);
  });

  it('deactivate is a no-op (the subscription list owns teardown)', () => {
    expect(() => {
      deactivate();
    }).not.toThrow();
  });
});

describe('manifest', () => {
  const manifest = JSON.parse(
    readFileSync(fileURLToPath(new URL('../../package.json', import.meta.url)), 'utf8'),
  ) as {
    contributes?: {
      commands?: readonly { command: string }[];
      configuration?: { properties?: Record<string, unknown> };
    };
  };

  it('declares both palette commands', () => {
    const declared = (manifest.contributes?.commands ?? []).map((command) => command.command);
    expect(declared).toContain(COMMAND_IDS.newSession);
    expect(declared).toContain(COMMAND_IDS.reconnectGateway);
  });

  it('declares every gateway setting the host reads', () => {
    const properties = manifest.contributes?.configuration?.properties ?? {};
    expect(Object.keys(properties).sort()).toEqual([
      'wing.apiKey',
      'wing.autoStart',
      'wing.host',
      'wing.port',
      'wing.wingPath',
    ]);
  });
});
