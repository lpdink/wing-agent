/* eslint-disable @typescript-eslint/unbound-method */
import { beforeEach, describe, expect, it, vi, type Mock } from 'vitest';

import { imageUri, resetImageUris } from '@wing-agent/ui';
import type { AskAnswerModel } from '@wing-agent/session';

import { createWebBridge, type WebBridgeHost } from '../src/bridge/webBridge';
import type { ImageResolver } from '../src/images/resolver';

/**
 * The web bridge controller: every intent the transcript can produce, routed.
 *
 * The renderer's channel is asserted from the outside — `createWebBridge` is driven
 * with the exact `WebviewToHostMessage`s the package posts, and the assertions are on
 * the host calls and on the package's own image cache (the observable effect of a
 * `resolveImages` answer).
 */

interface Harness {
  readonly bridge: ReturnType<typeof createWebBridge>;
  readonly host: WebBridgeHost & {
    readonly answerAsk: Mock<WebBridgeHost['answerAsk']>;
    readonly approveTool: Mock<WebBridgeHost['approveTool']>;
    readonly notify: Mock<WebBridgeHost['notify']>;
    readonly sendMessage: Mock<WebBridgeHost['sendMessage']>;
    readonly interrupt: Mock<WebBridgeHost['interrupt']>;
    readonly setModel: Mock<WebBridgeHost['setModel']>;
    readonly setThinking: Mock<WebBridgeHost['setThinking']>;
    readonly setEffort: Mock<WebBridgeHost['setEffort']>;
    readonly setYolo: Mock<WebBridgeHost['setYolo']>;
    readonly runPromptCommand: Mock<WebBridgeHost['runPromptCommand']>;
    readonly openModelPickerAction: Mock<WebBridgeHost['openModelPickerAction']>;
    readonly closeOverlays: Mock<WebBridgeHost['closeOverlays']>;
    readonly compact: Mock<WebBridgeHost['compact']>;
  };
  readonly resolve: ReturnType<typeof vi.fn>;
  readonly openLink: ReturnType<typeof vi.fn>;
  readonly copyText: ReturnType<typeof vi.fn>;
  readonly debug: ReturnType<typeof vi.fn>;
}

function harness(): Harness {
  const host = {
    answerAsk: vi.fn<WebBridgeHost['answerAsk']>(),
    approveTool: vi.fn<WebBridgeHost['approveTool']>(),
    notify: vi.fn<WebBridgeHost['notify']>(),
    sendMessage: vi.fn<WebBridgeHost['sendMessage']>(),
    interrupt: vi.fn<WebBridgeHost['interrupt']>(),
    setModel: vi.fn<WebBridgeHost['setModel']>(),
    setThinking: vi.fn<WebBridgeHost['setThinking']>(),
    setEffort: vi.fn<WebBridgeHost['setEffort']>(),
    setYolo: vi.fn<WebBridgeHost['setYolo']>(),
    runPromptCommand: vi.fn<WebBridgeHost['runPromptCommand']>(),
    openModelPickerAction: vi.fn<WebBridgeHost['openModelPickerAction']>(),
    closeOverlays: vi.fn<WebBridgeHost['closeOverlays']>(),
    compact: vi.fn<WebBridgeHost['compact']>(),
  };
  const resolve = vi.fn<ImageResolver['resolve']>(() => Promise.resolve([]));
  const openLink = vi.fn();
  const copyText = vi.fn();
  const debug = vi.fn();
  const bridge = createWebBridge({
    host,
    images: { resolve, clear: () => undefined },
    openLink,
    copyText,
    logger: { debug, warn: vi.fn(), error: vi.fn() },
  });
  return { bridge, host, resolve, openLink, copyText, debug };
}

const ANSWERS: readonly AskAnswerModel[] = [{ questionId: 'q1', selected: ['parser.ts'], text: '' }];

describe('web bridge', () => {
  beforeEach(() => {
    resetImageUris();
  });

  it('routes an ask answer to the runtime', () => {
    const h = harness();
    h.bridge.post({ type: 'answerAsk', sessionId: 's-1', requestId: 'toolu-1', answers: ANSWERS });

    expect(h.host.answerAsk).toHaveBeenCalledWith('toolu-1', ANSWERS);
    expect(h.host.notify).not.toHaveBeenCalled();
  });

  it('routes an approval decision to the runtime', () => {
    const h = harness();
    h.bridge.post({ type: 'approveTool', sessionId: 's-1', requestId: 'toolu-2', decision: 'deny' });

    expect(h.host.approveTool).toHaveBeenCalledWith('toolu-2', 'deny');
  });

  it('resolves images and feeds the answers back into the renderer cache', async () => {
    const h = harness();
    h.resolve.mockResolvedValue([{ src: 'assets/chart.png', uri: 'blob:image-1' }]);

    expect(imageUri('assets/chart.png')).toBeNull();
    h.bridge.post({ type: 'resolveImages', srcs: ['assets/chart.png'] });
    await vi.waitFor(() => {
      expect(imageUri('assets/chart.png')).toBe('blob:image-1');
    });
    expect(h.resolve).toHaveBeenCalledWith(['assets/chart.png']);
  });

  it('survives a resolver failure without taking the channel down', async () => {
    const h = harness();
    h.resolve.mockRejectedValue(new Error('boom'));

    h.bridge.post({ type: 'resolveImages', srcs: ['a.png'] });
    await vi.waitFor(() => {
      expect(h.debug).not.toHaveBeenCalled();
    });
    expect(h.resolve).toHaveBeenCalled();
  });

  it('opens http(s) links in a new tab and ignores local paths', () => {
    const h = harness();
    h.bridge.post({ type: 'openLink', href: 'https://example.com/docs' });
    h.bridge.post({ type: 'openLink', href: 'assets/chart.png' });

    expect(h.openLink).toHaveBeenCalledTimes(1);
    expect(h.openLink).toHaveBeenCalledWith('https://example.com/docs');
  });

  it('copies text through the injected clipboard', () => {
    const h = harness();
    h.bridge.post({ type: 'copyText', text: 'npm run build' });

    expect(h.copyText).toHaveBeenCalledWith('npm run build');
  });

  it('answers a file reference with a notice naming the path (no editor in a browser)', () => {
    const h = harness();
    h.bridge.post({ type: 'openFile', path: 'src/index.ts', line: 42 });

    expect(h.openLink).not.toHaveBeenCalled();
    expect(h.host.notify).toHaveBeenCalledWith('info', 'Open in your editor: src/index.ts:42');
  });

  it('explains that a diff is already inline instead of opening one', () => {
    const h = harness();
    h.bridge.post({ type: 'openDiff', sessionId: 's-1', cellId: 'c7' });

    expect(h.host.notify.mock.calls[0]?.[0]).toBe('info');
    expect(String(h.host.notify.mock.calls[0]?.[1])).toMatch(/inline/);
  });

  it('drops the intents of later steps loudly in the log, never by guessing', () => {
    const h = harness();
    // sendMessage and interrupt are wired in step 09, so only ready and ping
    // (which are not wired on web) log a debug message.
    h.bridge.post({ type: 'sendMessage', sessionId: 's-1', text: 'hello' });
    h.bridge.post({ type: 'interrupt', sessionId: 's-1' });
    h.bridge.post({ type: 'ready', protocolVersion: 1 });
    h.bridge.post({ type: 'ping', id: 'ping-1' });

    expect(h.debug).toHaveBeenCalledTimes(2);
    expect(h.host.sendMessage).toHaveBeenCalled();
    expect(h.host.interrupt).toHaveBeenCalled();
    expect(h.host.answerAsk).not.toHaveBeenCalled();
    expect(h.host.notify).not.toHaveBeenCalled();
  });

  it('is a BridgeController: start and dispose are safe no-ops', () => {
    const h = harness();
    expect(() => {
      h.bridge.start();
      h.bridge.ping();
      h.bridge.dispose();
    }).not.toThrow();
  });
});
