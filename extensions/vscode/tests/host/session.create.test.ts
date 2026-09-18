import { afterEach, describe, expect, it } from 'vitest';

import type { WebviewIntent } from '../../src/host/bridge';

import { createHostHarness, flushMicrotasks } from './support/harness';
import type { HostHarness } from './support/harness';

/**
 * Control-plane timing invariants: `create → subscribe → (send allowed)`.
 *
 * These are the tests the product's correctness hinges on — a message sent
 * before the subscription is attached is delivered to a session whose events
 * this client never sees (the user would watch a pending bubble that never
 * moves). They are written as incident reconstructions, not as unit tests of a
 * function.
 */

function bootedHarness(): HostHarness {
  return createHostHarness();
}

const teardown: HostHarness[] = [];
afterEach(() => {
  while (teardown.length > 0) {
    teardown.pop()?.dispose();
  }
});

function make(): HostHarness {
  const harness = bootedHarness();
  teardown.push(harness);
  return harness;
}

describe('new session timing', () => {
  it('boots: probe, connect, create → subscribe → hydrate (in this order)', async () => {
    const harness = make();
    await harness.boot();

    // The first session is created automatically once the view is ready and the
    // connection is up.
    expect(harness.gateway.calls('/api/session/create')).toHaveLength(1);
    expect(harness.gateway.calls('/api/session/subscribe')).toHaveLength(1);

    const created = harness.gateway.createdOrder[0];
    expect(created).toBeDefined();
    const sessionId = created ?? '';

    // create strictly before subscribe.
    const paths = harness.gateway.httpCalls.map((call) => call.path);
    expect(paths.indexOf('/api/session/create')).toBeLessThan(paths.indexOf('/api/session/subscribe'));

    // The subscribe call carried the WS handshake's client id.
    const subscribe = harness.gateway.calls('/api/session/subscribe')[0];
    expect(subscribe?.headers['X-Client-Id']).toBe(harness.gateway.lastSocket.clientId);

    // The tab bar shows the session; the webview got a full hydrate.
    const tabs = harness.ofType('tabs');
    expect(tabs.length).toBeGreaterThan(0);
    expect(tabs[tabs.length - 1]?.activeSessionId).toBe(sessionId);
    expect(harness.hydrateFor(sessionId)?.session.sessionId).toBe(sessionId);
  });

  it('refuses to send before the subscription is attached', async () => {
    const harness = make();
    const gate = harness.gateway.holdNext('/api/session/subscribe');
    harness.gateway.seedOnCreate = {
      messages: [{ role: 'user', content: 'replayed hello', uuid: 'm1' }],
    };

    await harness.host.start();
    await harness.ready();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    await flushMicrotasks();

    // The subscribe call is still in flight: a send must not leave the client.
    harness.wipe();
    await harness.intent({ type: 'sendMessage', sessionId, text: 'too early' });

    expect(harness.clientFrames().filter((frame) => frame['session_id'] === sessionId)).toHaveLength(0);
    const toasts = harness.ofType('ui').filter((message) => message.action.kind === 'toast');
    expect(
      toasts.map((message) => (message.action.kind === 'toast' ? message.action.message : '')),
    ).toContain('Not sent — the gateway is not connected.');

    gate.release();
    await flushMicrotasks(20);

    // Now the message goes out, with a request id that matches the pending cell.
    harness.wipe();
    await harness.intent({ type: 'sendMessage', sessionId, text: 'now it works' });
    const frames = harness.clientFrames().filter((frame) => frame['session_id'] === sessionId);
    expect(frames).toHaveLength(1);
    expect(frames[0]?.['content']).toBe('now it works');

    const patches = harness.ofType('patch').flatMap((message) => message.patches);
    const pending = patches.find((patch) => patch.op === 'append' && patch.cell.kind === 'user');
    expect(pending?.op).toBe('append');
    if (pending?.op === 'append' && pending.cell.kind === 'user') {
      expect(pending.cell.state).toBe('pending');
      // The WS frame and the pending cell share one request id.
      const requestId = frames[0]?.['request_id'];
      const record = harness.host.sessionManager.record(sessionId);
      expect(record?.pendingRequests.get(String(requestId))).toBe(pending.cell.id);
    }
  });

  it('does not create a session while the webview is not mounted', async () => {
    const harness = make();
    await harness.host.start();
    await flushMicrotasks(20);

    expect(harness.gateway.calls('/api/session/create')).toHaveLength(0);

    // Mounting the view triggers the initial session exactly once.
    await harness.ready();
    await harness.ready();
    expect(harness.gateway.calls('/api/session/create')).toHaveLength(1);
  });

  it('reports a missing workspace instead of calling the gateway', async () => {
    const harness = createHostHarness({ workspace: null });
    teardown.push(harness);
    await harness.boot();

    // Booting does not spam an error: there is nothing to work in yet.
    expect(harness.gateway.calls('/api/session/create')).toHaveLength(0);
    expect(harness.errors).toHaveLength(0);

    await harness.intent({ type: 'newSession' });
    expect(harness.gateway.calls('/api/session/create')).toHaveLength(0);
    expect(harness.errors).toContain('Wing: open a folder before starting a session.');
  });

  it('serializes rapid new-session intents into two distinct tabs', async () => {
    const harness = make();
    await harness.boot();

    const first = harness.host.sessionManager.openSessionIds[0] ?? '';
    const gate = harness.gateway.holdNext('/api/session/create');
    const creating = harness.intent({ type: 'newSession' } satisfies WebviewIntent);
    const alsoCreating = harness.intent({ type: 'newSession' } satisfies WebviewIntent);
    gate.release();
    await creating;
    await alsoCreating;
    await flushMicrotasks(20);

    const ids = harness.host.sessionManager.openSessionIds;
    expect(ids).toHaveLength(3);
    expect(new Set(ids).size).toBe(3);
    expect(ids[0]).toBe(first);
    expect(harness.gateway.calls('/api/session/create')).toHaveLength(3);
    // Every session got its own hydrate.
    for (const id of ids) {
      expect(harness.hydrateFor(id)?.session.sessionId).toBe(id);
    }
  });

  it('serializes close behind an in-flight create and never leaves a dangling route', async () => {
    const harness = make();
    await harness.boot();
    const created = harness.gateway.createdOrder.length;
    const callsBefore = harness.gateway.httpCalls.length;

    const gate = harness.gateway.holdNext('/api/session/create');
    const creating = harness.intent({ type: 'newSession' });
    await flushMicrotasks();
    // The session does not exist yet (the fake assigns ids after the gate), so
    // the only meaningful "race" is a close queued behind the create.
    expect(harness.gateway.createdOrder.length).toBe(created);

    const closing = harness.intent({ type: 'closeSession', sessionId: 'sess-predicted' });
    gate.release();
    await creating;
    await flushMicrotasks(20);
    const inFlight = harness.gateway.createdOrder[created] ?? '';
    expect(inFlight).not.toBe('');

    // Now close the just-created session for real and check the bookkeeping.
    const reallyClosing = harness.intent({ type: 'closeSession', sessionId: inFlight });
    await closing;
    await reallyClosing;
    await flushMicrotasks(20);

    expect(harness.host.sessionManager.openSessionIds).not.toContain(inFlight);
    // Structure order per session: create → subscribe → runtime probe →
    // unsubscribe (no dangling route, no resurrect). The runtime probe
    // (`/api/session/info`) is the yolo/thinking/effort refresh that rides on a
    // fresh subscription.
    const paths = harness.gateway.httpCalls.slice(callsBefore).map((call) => call.path);
    expect(paths).toEqual([
      '/api/session/create',
      '/api/session/subscribe',
      '/api/session/info',
      '/api/session/unsubscribe',
    ]);
    // And a closed session accepts no further events.
    harness.wipe();
    harness.gateway.emit({ type: 'done', session_id: inFlight });
    await flushMicrotasks();
    expect(harness.ofType('patch')).toHaveLength(0);
  });
});
