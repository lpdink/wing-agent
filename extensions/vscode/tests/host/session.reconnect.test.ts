import { afterEach, describe, expect, it, vi } from 'vitest';

import { createHostHarness, flushMicrotasks, textPayload } from './support/harness';
import type { HostHarness } from './support/harness';
import { kinds } from './support/mirror';

/**
 * Connection self-healing invariants.
 *
 * The scenario that matters: a gateway restart (or a network hiccup) drops the
 * socket, the client gets a **new `client_id`** on reconnect, and every open tab
 * must re-subscribe — otherwise the UI keeps rendering a session it can no
 * longer hear. The tests drive the ladder with fake timers, so "1 s → 2 s → …"
 * is asserted, not slept through.
 */

const teardown: HostHarness[] = [];
afterEach(() => {
  vi.useRealTimers();
  while (teardown.length > 0) {
    teardown.pop()?.dispose();
  }
});

function notices(harness: HostHarness): string[] {
  return harness.ofType('panels').map((message) => message.panels.globalNotice?.text ?? '');
}

describe('reconnect', () => {
  it('resubscribes every open tab with the new client id and re-hydrates each', async () => {
    vi.useFakeTimers();
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const first = harness.gateway.createdOrder[0] ?? '';
    await harness.intent({ type: 'newSession' });
    const second = harness.gateway.createdOrder[1] ?? '';
    harness.gateway.emit(textPayload('before the drop', first));
    await flushMicrotasks();

    // The gateway persisted the assistant message (that is what a replay reads).
    harness.gateway.session(first).messages = [
      { role: 'assistant', content: 'before the drop', uuid: 'm-assistant' },
    ];
    const oldClientId = harness.gateway.lastSocket.clientId;
    harness.wipe();

    harness.gateway.dropConnections(1006);
    await flushMicrotasks();
    expect(notices(harness).some((text) => text.includes('reconnecting'))).toBe(true);

    await vi.advanceTimersByTimeAsync(1_000);
    await flushMicrotasks(20);

    const newClientId = harness.gateway.lastSocket.clientId;
    expect(newClientId).not.toBe(oldClientId);
    const resubscribed = harness.gateway
      .calls('/api/session/subscribe')
      .filter((call) => call.headers['X-Client-Id'] === newClientId)
      .map((call) => call.body?.['session_id']);
    expect(resubscribed).toEqual([first, second]);

    // Each tab got its own replay → its own hydrate, with the pre-drop content.
    const hydrated = harness.ofType('hydrate').map((message) => message.session.sessionId);
    expect(hydrated).toContain(first);
    expect(hydrated).toContain(second);
    expect(kinds(harness.hydrateFor(first)?.session.cells ?? [])).toEqual(['assistant']);

    // The reconnect notice is cleared once the connection is healthy again.
    const lastNotice = harness.ofType('panels').at(-1)?.panels.globalNotice;
    expect(lastNotice ?? null).toBeNull();
    // And live events flow again without duplication.
    harness.wipe();
    harness.gateway.emit(textPayload('after the drop', first));
    await flushMicrotasks();
    expect(kinds(harness.host.sessionManager.record(first)?.cells ?? [])).toEqual(['assistant']);
    expect(harness.host.sessionManager.record(first)?.cells[0]).toMatchObject({
      text: 'before the dropafter the drop',
    });
  });

  it('resumes a session the restarted gateway no longer has loaded', async () => {
    vi.useFakeTimers();
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';

    harness.gateway.dropConnections(1006);
    // The restarted gateway answers 404 for the first subscribe…
    harness.gateway.httpFailures.set('/api/session/subscribe', {
      status: 404,
      body: JSON.stringify({ error: 'session not loaded' }),
    });
    await vi.advanceTimersByTimeAsync(1_000);
    await flushMicrotasks(30);

    // …the host resumes it, subscribes again, and gets the replay.
    const paths = harness.gateway.httpCalls.map((call) => call.path);
    const resumeIndex = paths.lastIndexOf('/api/session/resume');
    const subscribeIndex = paths.lastIndexOf('/api/session/subscribe');
    expect(resumeIndex).toBeGreaterThan(-1);
    expect(subscribeIndex).toBeGreaterThan(resumeIndex);
    expect(harness.hydrateFor(sessionId)).toBeDefined();
    expect(harness.host.sessionManager.openSessionIds).toEqual([sessionId]);
  });

  it('marks a session the gateway has forgotten instead of retrying forever', async () => {
    vi.useFakeTimers();
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';

    harness.gateway.dropConnections(1006);
    harness.gateway.httpFailures.set('/api/session/subscribe', { status: 404 });
    harness.gateway.httpFailures.set('/api/session/resume', { status: 404 });
    await vi.advanceTimersByTimeAsync(1_000);
    await flushMicrotasks(30);

    const record = harness.host.sessionManager.record(sessionId);
    const systemCells = (record?.cells ?? []).filter((cell) => cell.kind === 'system');
    expect(systemCells).toHaveLength(1);
    expect(systemCells[0]?.kind === 'system' && systemCells[0].level).toBe('error');
    const toasts = harness
      .ofType('ui')
      .filter((message) => message.action.kind === 'toast')
      .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
    expect(toasts).toContain('Session not found on the gateway.');

    // No further subscribe attempts: the session is gone, not slow.
    const attempts = harness.gateway.calls('/api/session/subscribe').length;
    await vi.advanceTimersByTimeAsync(60_000);
    await flushMicrotasks(20);
    expect(harness.gateway.calls('/api/session/subscribe').length).toBe(attempts);
  });

  it('leaves no timers behind after dispose mid-reconnect', async () => {
    vi.useFakeTimers();
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();

    harness.gateway.dropConnections(1006);
    await flushMicrotasks();
    expect(vi.getTimerCount()).toBeGreaterThan(0);

    harness.dispose();
    await flushMicrotasks();
    expect(vi.getTimerCount()).toBe(0);
  });
});

describe('initial connect failures', () => {
  it('retries on the host ladder and attaches once the gateway appears', async () => {
    vi.useFakeTimers();
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.refuseNextConnect = new Error('connect ECONNREFUSED 127.0.0.1:32523');

    await harness.host.start();
    await flushMicrotasks();
    expect(harness.host.connectionState?.status).not.toBe('connected');
    // No session exists yet, so the failure is surfaced as a toast.
    const toasts = harness
      .ofType('ui')
      .filter((message) => message.action.kind === 'toast')
      .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
    expect(toasts.some((text) => text.includes('not reachable'))).toBe(true);

    await vi.advanceTimersByTimeAsync(1_000);
    await flushMicrotasks(20);
    expect(harness.host.connectionState?.status).toBe('connected');

    // The webview was attached late: `ready` still produces the full snapshot.
    await harness.ready();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    expect(sessionId).not.toBe('');
    expect(harness.hydrateFor(sessionId)).toBeDefined();
  });

  it('stops retrying on an unauthorized close and says what to fix', async () => {
    vi.useFakeTimers();
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();

    harness.gateway.dropConnections(4001, 'Unauthorized');
    await flushMicrotasks();
    await vi.advanceTimersByTimeAsync(60_000);
    await flushMicrotasks();

    // core refuses to retry a credential failure; the host explains it.
    expect(notices(harness).some((text) => text.includes('wing.apiKey'))).toBe(true);
    expect(harness.errors.some((message) => message.includes('API key'))).toBe(true);
    expect(harness.gateway.sockets).toHaveLength(1);
  });
});
