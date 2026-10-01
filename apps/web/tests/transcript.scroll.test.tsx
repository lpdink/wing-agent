import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { App } from '../src/app/App';
import { GatewayRuntime } from '../src/connection/runtime';
import { DEFAULT_SETTINGS } from '../src/settings/settings';

import { FakeGateway } from './support/fake-gateway';
import { TEST_LOCATION } from './support/harness';
import { richSession } from './support/transcript-fixture';

/**
 * The web shell's half of the scroll contract.
 *
 * The rules themselves (2px tolerance, follow-while-pinned, the affordance's
 * geometry) belong to `@wing-agent/ui` and are tested there. What is *this* app's
 * responsibility — and what these tests pin — is the wiring around them:
 * step 07's `recordVersion` has to reach the renderer as a new session object on
 * every reduced batch, and the transcript has to be the scroller of a bounded pane
 * (otherwise "stick to the bottom" has nothing to scroll). jsdom does not lay out,
 * so the geometry is installed by hand, the same way the package's own scroll tests
 * do it.
 */

function makeScrollable(element: HTMLElement, scrollHeight = 1_000, clientHeight = 300): void {
  Object.defineProperty(element, 'scrollHeight', { value: scrollHeight, configurable: true });
  Object.defineProperty(element, 'clientHeight', { value: clientHeight, configurable: true });
}

beforeEach(() => {
  vi.stubGlobal(
    'fetch',
    vi.fn(() => Promise.resolve(new Response(null, { status: 404 }))),
  );
});

async function mount(): Promise<{ gateway: FakeGateway; runtime: GatewayRuntime; scroller: HTMLElement }> {
  const gateway = new FakeGateway({ sessions: [richSession()] });
  const runtime = new GatewayRuntime({
    initialSettings: DEFAULT_SETTINGS,
    location: TEST_LOCATION,
    socketFactory: gateway.socketFactory,
    httpTransport: gateway.transport,
    listPollIntervalMs: 0,
    noticeTtlMs: 0,
    connectRetryBaseMs: 3_600_000,
  });
  render(<App runtime={runtime} />);
  const scroller = await screen.findByTestId('transcript');
  makeScrollable(scroller);
  return { gateway, runtime, scroller };
}

/** One streamed assistant delta (a new cell the first time, appended afterwards). */
function emitDelta(gateway: FakeGateway, content: string): void {
  gateway.emit('session-1', {
    type: 'text',
    session_id: 'session-1',
    created_at: '2026-10-01T12:00:30Z',
    request_id: `r-${content.length}`,
    content,
  });
}

describe('transcript scrolling', () => {
  it('follows the newest content while the user is at the bottom', async () => {
    const { gateway, runtime, scroller } = await mount();

    emitDelta(gateway, 'streaming…');
    await waitFor(() => {
      expect(scroller.scrollTop).toBe(scroller.scrollHeight);
    });

    // …and it keeps following: every reduced batch hands the renderer a new view
    // model (`recordVersion`), which is what its layout effect sticks on.
    Object.defineProperty(scroller, 'scrollHeight', { value: 1_600, configurable: true });
    emitDelta(gateway, ' and more');
    await waitFor(() => {
      expect(scroller.scrollTop).toBe(1_600);
    });

    runtime.stop();
  });

  it('stops following when the user scrolls up, and offers the way back', async () => {
    const { gateway, runtime, scroller } = await mount();

    scroller.scrollTop = 100; // reading history
    fireEvent.scroll(scroller);
    const button = await screen.findByTestId('scroll-to-bottom');

    // New content must not yank the view away from what is being read.
    emitDelta(gateway, 'more text');
    await waitFor(() => {
      expect(scroller.scrollTop).toBe(100);
    });
    expect(screen.getByTestId('scroll-to-bottom')).toBe(button);

    fireEvent.click(button);
    await waitFor(() => {
      expect(scroller.scrollTop).toBe(scroller.scrollHeight);
    });
    expect(screen.queryByTestId('scroll-to-bottom')).toBeNull();

    runtime.stop();
  });

  it('does not steal the reading position when a replay replaces the transcript', async () => {
    const { gateway, runtime, scroller } = await mount();

    scroller.scrollTop = 100;
    fireEvent.scroll(scroller);
    const button = await screen.findByTestId('scroll-to-bottom');

    // A reconnect replays the session into the same record (step 07's contract):
    // wholesale replacement, and deliberately *not* a reason to move a user who
    // was reading history (the renderer's rule, the same one VS Code's list has).
    gateway.lastSocket()?.drop();
    await waitFor(() => {
      expect(gateway.callsTo('POST', '/api/session/subscribe')).toHaveLength(2);
    });
    expect(scroller.scrollTop).toBe(100);
    expect(screen.getByTestId('scroll-to-bottom')).toBe(button);

    fireEvent.click(button);
    await waitFor(() => {
      expect(scroller.scrollTop).toBe(scroller.scrollHeight);
    });

    runtime.stop();
  });
});
