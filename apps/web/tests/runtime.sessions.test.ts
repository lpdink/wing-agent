import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { FakeGateway, makeSession } from './support/fake-gateway';
import { createHarness, simpleHistory, useFakeTimers } from './support/harness';

/**
 * Session operations: create / open / switch, the resume-on-404 recovery, the
 * record cache, and the serialisation that keeps a double click from subscribing
 * twice.
 */

const FIRST = makeSession({
  id: 'session-1',
  name: 'First',
  messages: simpleHistory('first'),
  lastInteraction: '2026-10-01T12:00:00Z',
});

const SECOND = makeSession({
  id: 'session-2',
  name: 'Second',
  messages: simpleHistory('second'),
  lastInteraction: '2026-10-01T11:00:00Z',
});

describe('GatewayRuntime sessions', () => {
  beforeEach(() => {
    useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('creates a session on the gateway, opens it and lists it', async () => {
    const gateway = new FakeGateway();
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    expect(harness.snapshot().activeSessionId).toBeNull();
    expect(harness.snapshot().sessions).toHaveLength(0);

    const created = await harness.runtime.newSession();
    await harness.settle();

    expect(created).toBe('created-1');
    expect(harness.snapshot().activeSessionId).toBe('created-1');
    expect(gateway.callsTo('POST', '/api/session/subscribe')[0]?.body?.['session_id']).toBe('created-1');
    expect(harness.snapshot().sessions.map((row) => row.id)).toContain('created-1');

    harness.runtime.stop();
    await harness.settle();
  });

  it('switches the subscription when another session is opened', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();
    expect(harness.snapshot().activeSessionId).toBe('session-1');

    await harness.runtime.activate('session-2');
    await harness.settle();

    expect(harness.snapshot().activeSessionId).toBe('session-2');
    const cells = harness.snapshot().record?.cells ?? [];
    expect(cells[0] !== undefined && cells[0].kind === 'user' ? cells[0].text : '').toBe('second');

    // The old route was dropped before the new one was attached.
    const unsubscribed = gateway.callsTo('POST', '/api/session/unsubscribe');
    expect(unsubscribed.map((call) => call.body?.['session_id'])).toEqual(['session-1']);
    expect(
      gateway.callsTo('POST', '/api/session/subscribe').map((call) => call.body?.['session_id']),
    ).toEqual(['session-1', 'session-2']);

    // Events for the session that is no longer open reach nobody.
    gateway.emit('session-1', {
      type: 'text',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:09Z',
      request_id: 'req-late',
      content: 'stale',
    });
    await harness.settle();
    expect(gateway.droppedDeliveries).toBe(1);
    expect(harness.snapshot().record?.cells).toHaveLength(2);

    harness.runtime.stop();
    await harness.settle();
  });

  it('is a no-op when the open session is activated again', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();
    const before = gateway.callsTo('POST', '/api/session/subscribe').length;

    await harness.runtime.activate('session-1');
    await harness.settle();

    expect(gateway.callsTo('POST', '/api/session/subscribe')).toHaveLength(before);
    expect(gateway.callsTo('POST', '/api/session/unsubscribe')).toHaveLength(0);

    harness.runtime.stop();
    await harness.settle();
  });

  it('serialises concurrent activations (one subscribe, not two)', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    const [first, second] = [harness.runtime.activate('session-2'), harness.runtime.activate('session-2')];
    await Promise.all([first, second]);
    await harness.settle();

    expect(
      gateway.callsTo('POST', '/api/session/subscribe').map((call) => call.body?.['session_id']),
    ).toEqual(['session-1', 'session-2']);

    harness.runtime.stop();
    await harness.settle();
  });

  it('resumes a session the gateway does not have in memory, then subscribes', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND] });
    gateway.missingSubscribe.add('session-2');
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    await harness.runtime.activate('session-2');
    await harness.settle();

    const order = gateway.calls
      .filter(
        (call) =>
          call.path === '/api/session/subscribe' ||
          call.path === '/api/session/resume' ||
          call.path === '/api/session/unsubscribe',
      )
      .map((call) => {
        const sessionId = call.body?.['session_id'];
        return `${call.path.split('/').at(-1)}:${typeof sessionId === 'string' ? sessionId : ''}`;
      });
    expect(order).toEqual([
      // Opening the first session is a resume too: the gateway may have lost it.
      'resume:session-1',
      'subscribe:session-1',
      // The switch re-resumes a session the client never opened, and when the
      // subscribe answers 404 it resumes *again* before attaching.
      'resume:session-2',
      'unsubscribe:session-1',
      'subscribe:session-2',
      'resume:session-2',
      'subscribe:session-2',
    ]);
    expect(harness.snapshot().activeSessionId).toBe('session-2');

    harness.runtime.stop();
    await harness.settle();
  });

  it('reports a session that vanished instead of retrying forever', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    // The gateway restarts and loses the session entirely.
    gateway.missing.add('session-1');
    gateway.lastSocket()?.drop();
    await harness.settle(2_000);

    expect(gateway.callsTo('POST', '/api/session/resume').length).toBeGreaterThan(0);
    const notices = harness.snapshot().notices.map((notice) => notice.text);
    expect(notices.some((text) => text.includes('no longer exists'))).toBe(true);
    const cells = harness.snapshot().record?.cells ?? [];
    expect(cells.at(-1)?.kind).toBe('system');

    harness.runtime.stop();
    await harness.settle();
  });

  it('surfaces a failed resume as a notice', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND] });
    gateway.missing.add('session-2');
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    await harness.runtime.activate('session-2');
    await harness.settle();

    const notices = harness.snapshot().notices;
    expect(notices).toHaveLength(1);
    expect(notices[0]?.text).toContain('Could not open that session');
    expect(harness.snapshot().activeSessionId).toBe('session-1');

    harness.runtime.dismissNotice(notices[0]?.id ?? -1);
    expect(harness.snapshot().notices).toHaveLength(0);

    harness.runtime.stop();
    await harness.settle();
  });

  it('resumes a session that was never opened, and reuses a visited record', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    await harness.runtime.activate('session-2');
    await harness.settle();
    expect(gateway.callsTo('POST', '/api/session/resume').map((call) => call.body?.['session_id'])).toEqual([
      'session-1',
      'session-2',
    ]);
    const secondRecord = harness.snapshot().record;

    await harness.runtime.activate('session-1');
    await harness.settle();
    expect(harness.snapshot().record).not.toBe(secondRecord);

    await harness.runtime.activate('session-2');
    await harness.settle();
    // Cached: switching back does not rebuild the record (no flash of empty view)
    // and never re-resumes on the gateway.
    expect(harness.snapshot().record).toBe(secondRecord);
    expect(gateway.callsTo('POST', '/api/session/resume')).toHaveLength(2);

    harness.runtime.stop();
    await harness.settle();
  });

  it('evicts records beyond the cache limit', async () => {
    const third = makeSession({ id: 'session-3', messages: simpleHistory('third') });
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND, third] });
    const harness = createHarness({ gateway, maxCachedRecords: 2 });
    harness.runtime.start();
    await harness.settle();

    const firstRecord = harness.snapshot().record;
    await harness.runtime.activate('session-2');
    await harness.settle();
    await harness.runtime.activate('session-3');
    await harness.settle();

    await harness.runtime.activate('session-1');
    await harness.settle();
    // Evicted: rebuilt from scratch (and replayed), not the same object.
    expect(harness.snapshot().record).not.toBe(firstRecord);
    expect(harness.snapshot().record?.cells).toHaveLength(2);

    harness.runtime.stop();
    await harness.settle();
  });

  it('fills the model knobs from /api/session/info (sync_session does not carry them)', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST] });
    FIRST.runtime = {
      ...FIRST.runtime,
      model: 'glm-4.6',
      thinking: true,
      reasoning_effort: 'high',
      yolo: true,
      workdir: '/tmp/other',
      context_window_tokens: 128_000,
    };
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    const meta = harness.snapshot().record?.meta;
    expect(meta?.model).toBe('glm-4.6');
    expect(meta?.thinking).toBe(true);
    expect(meta?.reasoningEffort).toBe('high');
    expect(meta?.yolo).toBe(true);
    expect(meta?.workspace).toBe('/tmp/other');

    harness.runtime.stop();
    await harness.settle();
  });

  it('opens a session while the gateway is down and subscribes once it is back', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND], failDials: 1 });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();
    expect(harness.snapshot().connection.everConnected).toBe(false);

    // The user picks a session while the socket is still down. HTTP is a separate
    // channel, so the resume works and the pane shows the session immediately —
    // only the subscription has to wait for the connection.
    await harness.runtime.activate('session-2');
    await harness.settle();
    expect(harness.snapshot().activeSessionId).toBe('session-2');
    expect(gateway.callsTo('POST', '/api/session/subscribe')).toHaveLength(0);

    // The ladder lands: the open session is subscribed and replayed.
    await harness.settle(1_000);
    expect(harness.snapshot().connection.phase).toBe('connected');
    expect(
      gateway.callsTo('POST', '/api/session/subscribe').map((call) => call.body?.['session_id']),
    ).toEqual(['session-2']);
    const cells = harness.snapshot().record?.cells ?? [];
    expect(cells[0] !== undefined && cells[0].kind === 'user' ? cells[0].text : '').toBe('second');

    harness.runtime.stop();
    await harness.settle();
  });
});
