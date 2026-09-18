import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type * as vscode from 'vscode';

import { ChatViewProvider, CHAT_VIEW_ID } from '../../src/host/chatViewProvider';
import { buildContentSecurityPolicy, buildWebviewHtml, createNonce } from '../../src/host/html';
import { disposeLog } from '../../src/host/log';
import { WEBVIEW_ROOT_ID } from '../../src/shared';
import { mockState } from '../mocks/vscode';

import { createHostHarness, flushMicrotasks } from './support/harness';
import type { HostHarness } from './support/harness';

/**
 * The chat view provider: document generation plus the bridge plumbing.
 *
 * These tests drive the *real* provider on top of the harness host, so the
 * complete chain (webview message → provider → host → fake gateway → bridge →
 * webview message) is exercised without a GUI.
 */

interface FakeWebview {
  options: unknown;
  html: string;
  readonly cspSource: string;
  asWebviewUri(uri: unknown): unknown;
  onDidReceiveMessage(handler: (raw: unknown) => void): { dispose(): void };
  postMessage(message: unknown): Promise<boolean>;
}

interface ProviderHarness extends HostHarness {
  readonly provider: ChatViewProvider;
  readonly webviewPosts: unknown[];
  readonly emit: (raw: unknown) => void;
  readonly receiveDisposed: () => boolean;
}

function makeWebview(): {
  webview: FakeWebview;
  posts: unknown[];
  readonly emit: (raw: unknown) => void;
  readonly disposed: () => boolean;
} {
  const posts: unknown[] = [];
  let handler: ((raw: unknown) => void) | null = null;
  let disposed = false;
  const webview: FakeWebview = {
    options: undefined,
    html: '',
    cspSource: 'vscode-webview://harness',
    asWebviewUri: (uri) => uri,
    onDidReceiveMessage: (next) => {
      handler = next;
      return {
        dispose: () => {
          disposed = true;
          handler = null;
        },
      };
    },
    postMessage: (message) => {
      posts.push(message);
      return Promise.resolve(true);
    },
  };
  return {
    webview,
    posts,
    emit: (raw) => handler?.(raw),
    disposed: () => disposed,
  };
}

const teardown: (() => void)[] = [];

function resolveProvider(): ProviderHarness {
  const harness = createHostHarness();
  const view = makeWebview();
  const provider = new ChatViewProvider(
    { toString: () => 'file:///extension' } as unknown as vscode.Uri,
    harness.host,
  );
  provider.resolveWebviewView({ webview: view.webview } as unknown as vscode.WebviewView);
  teardown.push(() => {
    provider.dispose();
    harness.host.dispose();
  });
  return {
    ...harness,
    provider,
    webviewPosts: view.posts,
    emit: view.emit,
    receiveDisposed: view.disposed,
  };
}

/** Register a bare host harness for teardown (manual provider tests). */
function keep(harness: HostHarness): HostHarness {
  teardown.push(() => {
    harness.host.dispose();
  });
  return harness;
}

function postsOfType(messages: readonly unknown[], type: string): Record<string, unknown>[] {
  return messages.filter(
    (message): message is Record<string, unknown> =>
      typeof message === 'object' && message !== null && (message as { type?: unknown }).type === type,
  );
}

function logLines(): string {
  return mockState.outputChannels.flatMap((channel) => channel.lines).join('\n');
}

beforeEach(() => {
  mockState.reset();
  disposeLog();
});

afterEach(() => {
  while (teardown.length > 0) {
    teardown.pop()?.();
  }
  disposeLog();
});

describe('webview document', () => {
  it('builds a CSP that allows only what the webview needs', () => {
    const csp = buildContentSecurityPolicy({ cspSource: 'vscode-webview://abc', nonce: 'N1' });
    expect(csp).toContain("default-src 'none'");
    expect(csp).toContain("script-src 'nonce-N1'");
    expect(csp).toContain('img-src vscode-webview://abc data:');
    expect(csp).toContain('style-src vscode-webview://abc');
    // No remote code, and no connect-src escape hatch.
    expect(csp).not.toContain('http://');
    expect(csp).not.toContain('https://');
    expect(csp).not.toContain('connect-src');
    expect(csp).not.toContain("script-src 'unsafe-inline'");
  });

  it('embeds the nonce, the built assets and the bootstrap payload', () => {
    const html = buildWebviewHtml({
      cspSource: 'vscode-webview://abc',
      nonce: 'N1',
      scriptUri: 'vscode-webview://abc/dist/webview/main.js',
      styleUri: 'vscode-webview://abc/dist/webview/main.css',
      bootstrap: { protocolVersion: 3, assetUris: { icon: 'vscode-webview://abc/media/wing.svg' } },
      title: 'Wing',
      rootId: WEBVIEW_ROOT_ID,
    });

    expect(html).toContain('<!DOCTYPE html>');
    expect(html).toContain(`<div id="${WEBVIEW_ROOT_ID}"></div>`);
    expect(html).toContain('<script nonce="N1" src="vscode-webview://abc/dist/webview/main.js"></script>');
    expect(html).toContain('<link rel="stylesheet" href="vscode-webview://abc/dist/webview/main.css">');
    expect(html).toContain('"protocolVersion":3');
    expect(html).toContain('window.__WING_BOOTSTRAP__ =');
    expect(html.match(/nonce="N1"/g)?.length).toBe(2);
  });

  it('omits the stylesheet tag when the build emitted no CSS', () => {
    const html = buildWebviewHtml({
      cspSource: 'vscode-webview://abc',
      nonce: 'N2',
      scriptUri: 'vscode-webview://abc/dist/webview/main.js',
      styleUri: null,
      bootstrap: { protocolVersion: 1, assetUris: {} },
      title: 'Wing',
      rootId: WEBVIEW_ROOT_ID,
    });
    expect(html).not.toContain('<link rel="stylesheet"');
  });

  it('escapes bootstrap values so they cannot break out of the inline script', () => {
    const html = buildWebviewHtml({
      cspSource: 'vscode-webview://abc',
      nonce: 'N3',
      scriptUri: 'a.js',
      styleUri: null,
      bootstrap: { protocolVersion: 1, assetUris: { evil: '</script><script>alert(1)</script>' } },
      title: 'Wing',
      rootId: WEBVIEW_ROOT_ID,
    });
    expect(html).not.toContain('</script><script>alert(1)');
    expect(html).toContain('\\u003c/script\\u003e');
  });

  it('generates a fresh, URL-safe nonce per document', () => {
    const first = createNonce();
    const second = createNonce();
    expect(first).toMatch(/^[0-9a-f]{32}$/);
    expect(first).not.toBe(second);
  });
});

describe('ChatViewProvider', () => {
  it('serves the document with the built assets and no eager traffic', () => {
    const harness = keep(createHostHarness());
    const view = makeWebview();
    const provider = new ChatViewProvider(
      { toString: () => 'file:///extension' } as unknown as vscode.Uri,
      harness.host,
    );
    provider.resolveWebviewView({ webview: view.webview } as unknown as vscode.WebviewView);

    // Nothing is posted before the webview says `ready`.
    expect(view.posts).toHaveLength(0);
    expect(view.webview.options).toEqual({
      enableScripts: true,
      localResourceRoots: [{ toString: expect.any(Function) }],
    });
    expect(view.webview.html).toContain('Content-Security-Policy');
    expect(view.webview.html).toContain('file:///extension/dist/webview/main.js');
    expect(view.webview.html).toContain('file:///extension/dist/webview/main.css');
    provider.dispose();
  });

  it('answers ready with the tab list and a hydrate after creating the first session', async () => {
    const harness = resolveProvider();
    await harness.host.start();

    harness.emit({ type: 'ready', protocolVersion: 1 });
    await flushMicrotasks(20);

    expect(postsOfType(harness.webviewPosts, 'tabs').length).toBeGreaterThan(0);
    const hydrates = postsOfType(harness.webviewPosts, 'hydrate');
    expect(hydrates.length).toBeGreaterThan(0);
    const session = hydrates[hydrates.length - 1]?.['session'] as { sessionId: string } | undefined;
    expect(session?.sessionId).toBe(harness.gateway.createdOrder[0]);
  });

  it('drives a user message from the webview to the gateway', async () => {
    const harness = resolveProvider();
    await harness.host.start();
    harness.emit({ type: 'ready', protocolVersion: 1 });
    await flushMicrotasks(20);
    const sessionId = harness.gateway.createdOrder[0] ?? '';

    harness.emit({ type: 'sendMessage', sessionId, text: 'hello from the webview' });
    await flushMicrotasks(10);

    const frames = harness.clientFrames().filter((frame) => frame['session_id'] === sessionId);
    expect(frames).toHaveLength(1);
    expect(frames[0]?.['content']).toBe('hello from the webview');
    // The optimistic pending cell went back to the webview.
    const patches = postsOfType(harness.webviewPosts, 'patch');
    const appended = patches
      .flatMap((message) => (message['patches'] as Record<string, unknown>[]) ?? [])
      .find((patch) => patch['op'] === 'append');
    expect((appended?.['cell'] as { kind?: string } | undefined)?.kind).toBe('user');
  });

  it('warns (but keeps working) when the webview speaks another protocol version', async () => {
    const harness = resolveProvider();
    await harness.host.start();

    harness.emit({ type: 'ready', protocolVersion: 99 });
    await flushMicrotasks(20);

    expect(logLines()).toContain('webview speaks bridge v99');
    expect(postsOfType(harness.webviewPosts, 'tabs').length).toBeGreaterThan(0);
  });

  it('answers ping with a pong carrying the same id', () => {
    const harness = resolveProvider();

    harness.emit({ type: 'ping', id: 'ping-7' });

    const pongs = postsOfType(harness.webviewPosts, 'pong');
    expect(pongs).toHaveLength(1);
    expect(pongs[0]?.['id']).toBe('ping-7');
    expect(typeof pongs[0]?.['hostTimeMs']).toBe('number');
  });

  it('re-hydrates on resync and records the reason', async () => {
    const harness = resolveProvider();
    await harness.host.start();
    harness.emit({ type: 'ready', protocolVersion: 1 });
    await flushMicrotasks(20);
    const before = postsOfType(harness.webviewPosts, 'hydrate').length;
    const sessionId = harness.gateway.createdOrder[0] ?? '';

    harness.emit({ type: 'resync', sessionId, lastSeq: 2, reason: 'seq-gap' });
    await flushMicrotasks(10);

    expect(postsOfType(harness.webviewPosts, 'hydrate').length).toBe(before + 1);
    expect(logLines()).toContain('resync requested (seq-gap, lastSeq=2)');
  });

  it('ignores unrecognized messages with a warning', () => {
    const harness = resolveProvider();

    harness.emit({ type: 'something-else', payload: 1 });
    harness.emit('not-an-object');

    expect(logLines()).toContain('ignoring unrecognized webview message');
    expect(harness.webviewPosts).toHaveLength(0);
  });

  it('detaches from the previous webview when the view resolves again', () => {
    const hostHarness = keep(createHostHarness());
    const provider = new ChatViewProvider(
      { toString: () => 'file:///extension' } as unknown as vscode.Uri,
      hostHarness.host,
    );
    const first = makeWebview();
    const second = makeWebview();

    provider.resolveWebviewView({ webview: first.webview } as unknown as vscode.WebviewView);
    provider.resolveWebviewView({ webview: second.webview } as unknown as vscode.WebviewView);

    expect(first.disposed()).toBe(true);
    first.emit({ type: 'ping', id: 'stale' });
    expect(postsOfType(first.posts, 'pong')).toHaveLength(0);
    second.emit({ type: 'ping', id: 'live' });
    expect(postsOfType(second.posts, 'pong')).toHaveLength(1);
    provider.dispose();
  });

  it('disposing the provider detaches the bridge', () => {
    const hostHarness = keep(createHostHarness());
    const view = makeWebview();
    const provider = new ChatViewProvider(
      { toString: () => 'file:///extension' } as unknown as vscode.Uri,
      hostHarness.host,
    );
    provider.resolveWebviewView({ webview: view.webview } as unknown as vscode.WebviewView);
    expect(hostHarness.host.hasSink).toBe(true);

    provider.dispose();

    expect(view.disposed()).toBe(true);
    expect(hostHarness.host.hasSink).toBe(false);
  });

  it('is registered under the id declared in package.json', () => {
    const manifest = JSON.parse(
      readFileSync(fileURLToPath(new URL('../../package.json', import.meta.url)), 'utf8'),
    ) as {
      activationEvents?: readonly string[];
      contributes?: {
        views?: Record<string, readonly { id: string }[]>;
        viewsContainers?: { activitybar?: readonly { id: string }[] };
      };
    };

    const declaredViews = Object.entries(manifest.contributes?.views ?? {}).flatMap(([container, views]) =>
      views.map((view) => ({ container, id: view.id })),
    );
    const ours = declaredViews.filter((view) => view.id === CHAT_VIEW_ID);

    expect(ours).toHaveLength(1);
    expect(manifest.activationEvents).toContain(`onView:${CHAT_VIEW_ID}`);
    const containers = manifest.contributes?.viewsContainers?.activitybar ?? [];
    expect(containers.map((container) => container.id)).toContain(ours[0]?.container);
  });
});
