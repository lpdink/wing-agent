import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type * as vscode from 'vscode';

import { ChatViewProvider, CHAT_VIEW_ID } from '../../src/host/chatViewProvider';
import { buildContentSecurityPolicy, buildWebviewHtml, createNonce } from '../../src/host/html';
import { disposeLog } from '../../src/host/log';
import { WEBVIEW_ROOT_ID } from '../../src/shared';
import { mockState } from '../mocks/vscode';

/**
 * Headless host tests: the provider is driven through a fake `webview` object, so
 * the whole `resolve → document → ready → hydrate → intent` path is asserted
 * without ever starting VS Code (no GUI).
 */

interface FakeWebview {
  options: unknown;
  html: string;
  readonly cspSource: string;
  asWebviewUri(uri: unknown): unknown;
  onDidReceiveMessage(handler: (raw: unknown) => void): { dispose(): void };
  postMessage(message: unknown): Promise<boolean>;
}

interface Harness {
  readonly view: vscode.WebviewView;
  readonly webview: FakeWebview;
  readonly posted: unknown[];
  emit(raw: unknown): void;
  readonly receiveDisposed: () => boolean;
}

function makeHarness(): Harness {
  const posted: unknown[] = [];
  let receiveHandler: ((raw: unknown) => void) | null = null;
  let disposed = false;

  const webview: FakeWebview = {
    options: undefined,
    html: '',
    cspSource: 'vscode-webview://harness',
    asWebviewUri: (uri) => uri,
    onDidReceiveMessage: (handler) => {
      receiveHandler = handler;
      return {
        dispose: () => {
          disposed = true;
          receiveHandler = null;
        },
      };
    },
    postMessage: (message) => {
      posted.push(message);
      return Promise.resolve(true);
    },
  };

  return {
    view: { webview } as unknown as vscode.WebviewView,
    webview,
    posted,
    emit: (raw) => {
      receiveHandler?.(raw);
    },
    receiveDisposed: () => disposed,
  };
}

/** Narrow one posted message by its discriminant. */
function postedOfType<T extends string>(posted: readonly unknown[], type: T): Record<string, unknown>[] {
  return posted.filter(
    (message): message is Record<string, unknown> =>
      typeof message === 'object' && message !== null && (message as { type?: unknown }).type === type,
  );
}

function logLines(): string {
  return mockState.outputChannels.flatMap((channel) => channel.lines).join('\n');
}

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
  beforeEach(() => {
    mockState.reset();
    disposeLog();
  });

  afterEach(() => {
    disposeLog();
  });

  function resolveProvider(): { harness: Harness; provider: ChatViewProvider } {
    const harness = makeHarness();
    const provider = new ChatViewProvider({ toString: () => 'file:///extension' } as unknown as vscode.Uri);
    provider.resolveWebviewView(harness.view);
    return { harness, provider };
  }

  it('enables scripts, scopes resources to the extension and sets the document', () => {
    const { harness } = resolveProvider();

    expect(harness.webview.options).toEqual({
      enableScripts: true,
      localResourceRoots: [{ toString: expect.any(Function) }],
    });
    expect(harness.webview.html).toContain('Content-Security-Policy');
    expect(harness.webview.html).toContain('file:///extension/dist/webview/main.js');
    expect(harness.webview.html).toContain('file:///extension/dist/webview/main.css');
  });

  it('answers ready with the tab list and a full hydrate', () => {
    const { harness } = resolveProvider();

    harness.emit({ type: 'ready', protocolVersion: 1 });

    const tabs = postedOfType(harness.posted, 'tabs');
    const hydrates = postedOfType(harness.posted, 'hydrate');
    expect(tabs).toHaveLength(1);
    expect(tabs[0]?.['activeSessionId']).toBe('scaffold-session');
    expect(hydrates).toHaveLength(1);

    const session = hydrates[0]?.['session'] as
      { sessionId: string; cells: unknown[]; seq: number } | undefined;
    expect(session?.sessionId).toBe('scaffold-session');
    expect(session?.cells.length).toBeGreaterThan(5);
    expect(session?.seq).toBeGreaterThan(0);
  });

  it('warns (but keeps working) when the webview speaks another protocol version', () => {
    const { harness } = resolveProvider();

    harness.emit({ type: 'ready', protocolVersion: 99 });

    expect(logLines()).toContain('webview speaks bridge v99');
    expect(postedOfType(harness.posted, 'hydrate')).toHaveLength(1);
  });

  it('answers ping with a pong carrying the same id', () => {
    const { harness } = resolveProvider();

    harness.emit({ type: 'ping', id: 'ping-7' });

    const pongs = postedOfType(harness.posted, 'pong');
    expect(pongs).toHaveLength(1);
    expect(pongs[0]?.['id']).toBe('ping-7');
    expect(typeof pongs[0]?.['hostTimeMs']).toBe('number');
  });

  it('re-hydrates on resync and records the reason', () => {
    const { harness } = resolveProvider();
    harness.emit({ type: 'ready', protocolVersion: 1 });

    harness.emit({ type: 'resync', sessionId: 'scaffold-session', lastSeq: 2, reason: 'seq-gap' });

    expect(postedOfType(harness.posted, 'hydrate')).toHaveLength(2);
    expect(logLines()).toContain('resync requested (seq-gap, lastSeq=2)');
  });

  it('logs intents it cannot serve yet instead of silently dropping them', () => {
    const { harness } = resolveProvider();

    harness.emit({ type: 'sendMessage', sessionId: 'scaffold-session', text: 'hello' });

    expect(logLines()).toContain('intent not implemented in scaffold: sendMessage');
    expect(harness.posted).toHaveLength(0);
  });

  it('ignores unrecognized messages with a warning', () => {
    const { harness } = resolveProvider();

    harness.emit({ type: 'something-else', payload: 1 });
    harness.emit('not-an-object');

    expect(logLines()).toContain('ignoring unrecognized webview message');
    expect(harness.posted).toHaveLength(0);
  });

  it('detaches from the previous webview when the view resolves again', () => {
    const provider = new ChatViewProvider({ toString: () => 'file:///extension' } as unknown as vscode.Uri);
    const first = makeHarness();
    const second = makeHarness();

    provider.resolveWebviewView(first.view);
    provider.resolveWebviewView(second.view);

    expect(first.receiveDisposed()).toBe(true);
    first.emit({ type: 'ping', id: 'stale' });
    expect(postedOfType(first.posted, 'pong')).toHaveLength(0);
    second.emit({ type: 'ping', id: 'live' });
    expect(postedOfType(second.posted, 'pong')).toHaveLength(1);
  });

  it('disposing the provider detaches the bridge', () => {
    const { harness, provider } = resolveProvider();

    provider.dispose();

    expect(harness.receiveDisposed()).toBe(true);
  });

  it('is registered under the id declared in package.json', () => {
    expect(CHAT_VIEW_ID).toBe('wing.chatView');
  });
});
