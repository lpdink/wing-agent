import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { FakeGateway, makeSession } from './support/fake-gateway';
import { createHarness, useFakeTimers } from './support/harness';

/**
 * The snapshot contract React renders from, the session-list poll, and the
 * notices the shell shows when something failed.
 */

const SESSION = makeSession({ id: 'session-1', name: 'Demo', lastInteraction: '2026-10-01T12:00:00Z' });

describe('GatewayRuntime snapshot', () => {
  beforeEach(() => {
    useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('keeps the snapshot identity stable until something changes', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway });
    const listener = vi.fn();
    harness.runtime.subscribe(listener);

    const before = harness.runtime.getSnapshot();
    expect(harness.runtime.getSnapshot()).toBe(before);
    expect(listener).not.toHaveBeenCalled();

    harness.runtime.start();
    await harness.settle();
    expect(listener).toHaveBeenCalled();
    const connected = harness.runtime.getSnapshot();
    expect(connected).not.toBe(before);
    expect(harness.runtime.getSnapshot()).toBe(connected);

    harness.runtime.stop();
    await harness.settle();
  });

  it('stops notifying after unsubscribe', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway });
    const listener = vi.fn();
    const unsubscribe = harness.runtime.subscribe(listener);
    harness.runtime.start();
    await harness.settle();
    unsubscribe();
    const calls = listener.mock.calls.length;

    await harness.runtime.refreshSessions();
    expect(listener.mock.calls.length).toBe(calls);

    harness.runtime.stop();
    await harness.settle();
  });

  it('polls the session list while connected and visible', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    let visible = true;
    const harness = createHarness({ gateway, listPollIntervalMs: 1_000, isPageVisible: () => visible });
    harness.runtime.start();
    await harness.settle();
    const initial = gateway.callsTo('GET', '/api/session/list').length;
    expect(initial).toBeGreaterThan(0);

    await harness.settle(2_500);
    expect(gateway.callsTo('GET', '/api/session/list').length).toBe(initial + 2);

    // A hidden page does not poll (the user is not looking; keep the phone quiet).
    visible = false;
    await harness.settle(3_000);
    expect(gateway.callsTo('GET', '/api/session/list').length).toBe(initial + 2);

    visible = true;
    await harness.settle(1_000);
    expect(gateway.callsTo('GET', '/api/session/list').length).toBe(initial + 3);

    harness.runtime.stop();
    await harness.settle(5_000);
    expect(gateway.callsTo('GET', '/api/session/list').length).toBe(initial + 3);
  });

  it('refreshes the list when a turn finishes', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway, listPollIntervalMs: 0 });
    harness.runtime.start();
    await harness.settle();
    const before = gateway.callsTo('GET', '/api/session/list').length;

    gateway.emit('session-1', {
      type: 'turn_result',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:10Z',
      request_id: 'req-turn',
      subtype: 'success',
      is_error: false,
      duration_ms: 900,
      num_turns: 2,
      total_tokens: 10,
      result: 'ok',
    });
    await harness.settle();

    expect(gateway.callsTo('GET', '/api/session/list').length).toBe(before + 1);

    harness.runtime.stop();
    await harness.settle();
  });

  it('keeps the last good rows and reports why the list failed', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway, listPollIntervalMs: 0 });
    harness.runtime.start();
    await harness.settle();

    gateway.listStatus = 500;
    await harness.runtime.refreshSessions();
    await harness.settle();

    expect(harness.snapshot().listError).not.toBeNull();
    expect(harness.snapshot().sessions).toHaveLength(1); // the last good list stays visible

    gateway.listStatus = 200;
    await harness.runtime.refreshSessions();
    expect(harness.snapshot().listError).toBeNull();

    harness.runtime.stop();
    await harness.settle();
  });

  it('expires notices after their TTL and lets the UI dismiss them early', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    gateway.missing.add('session-1');
    const harness = createHarness({ gateway, listPollIntervalMs: 0 });
    harness.runtime.start();
    await harness.settle();

    const notices = harness.snapshot().notices;
    expect(notices).toHaveLength(1);
    harness.runtime.dismissNotice(notices[0]?.id ?? -1);
    expect(harness.snapshot().notices).toHaveLength(0);

    harness.runtime.stop();
    await harness.settle();
  });

  it('auto-dismisses a notice after its TTL', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    gateway.missing.add('session-1');
    const harness = createHarness({ gateway, listPollIntervalMs: 0, noticeTtlMs: 1_000 });
    harness.runtime.start();
    await harness.settle();
    expect(harness.snapshot().notices).toHaveLength(1);

    await harness.settle(1_000);
    expect(harness.snapshot().notices).toHaveLength(0);

    harness.runtime.stop();
    await harness.settle();
  });
});
