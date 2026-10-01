import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { FakeGateway, makeSession } from './support/fake-gateway';
import { createHarness, simpleHistory, useFakeTimers } from './support/harness';

/**
 * Reconnect → resubscribe → replay.
 *
 * The load-bearing property of the whole shell: losing the socket must not
 * duplicate or lose the transcript. It holds because the *same* `SessionRecord`
 * is rebuilt by `applySync` from the gateway's snapshot (`record.replaced`), not
 * because two event streams are stitched together here.
 */

const SESSION = makeSession({
  id: 'session-1',
  name: 'Demo',
  messages: simpleHistory('hello'),
});

describe('GatewayRuntime reconnect and replay', () => {
  beforeEach(() => {
    useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('replays the session into the same record after a lost connection', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway });

    harness.runtime.start();
    await harness.settle();

    const record = harness.snapshot().record;
    expect(record?.cells.map((cell) => cell.kind)).toEqual(['user', 'assistant']);
    const assistant = record?.cells[1];
    expect(assistant !== undefined && assistant.kind === 'assistant' ? assistant.text : '').toBe(
      'echo: hello',
    );

    // A live delta that the gateway's snapshot knows nothing about.
    gateway.emit('session-1', {
      type: 'text',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:01Z',
      request_id: 'req-1',
      content: ' and more',
    });
    await harness.settle();
    const streamed = harness.snapshot().record?.cells[1];
    expect(streamed !== undefined && streamed.kind === 'assistant' ? streamed.text : '').toBe(
      'echo: hello and more',
    );

    // The socket dies (1006) — the client supervises, the runtime resubscribes.
    gateway.lastSocket()?.drop();
    await harness.settle();
    expect(harness.snapshot().connection.phase).toBe('reconnecting');
    expect(harness.snapshot().connection.everConnected).toBe(true);

    await harness.settle(1_000);
    expect(harness.snapshot().connection.phase).toBe('connected');
    expect(harness.snapshot().connection.clientId).toBe('client-2');

    // One subscribe per connection, each with the client id of that socket.
    expect(
      gateway.callsTo('POST', '/api/session/subscribe').map((call) => call.headers['X-Client-Id']),
    ).toEqual(['client-1', 'client-2']);

    // And the replay *replaced* the model: the speculative delta is gone, the
    // cells are exactly the gateway's, nothing is duplicated.
    const replayed = harness.snapshot().record;
    expect(replayed).toBe(record); // the same record object: no flash of a new view
    expect(replayed?.cells.map((cell) => cell.kind)).toEqual(['user', 'assistant']);
    expect(replayed?.cells.filter((cell) => cell.kind === 'user')).toHaveLength(1);
    const assistantAfterReplay = replayed?.cells[1];
    expect(
      assistantAfterReplay !== undefined && assistantAfterReplay.kind === 'assistant'
        ? assistantAfterReplay.text
        : '',
    ).toBe('echo: hello');

    harness.runtime.stop();
    await harness.settle();
  });

  it('keeps receiving live events after the replay', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();

    gateway.lastSocket()?.drop();
    await harness.settle(1_000);

    gateway.emit('session-1', {
      type: 'text',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:05Z',
      request_id: 'req-2',
      content: ' after the reconnect',
    });
    await harness.settle();

    const cell = harness.snapshot().record?.cells[1];
    expect(cell !== undefined && cell.kind === 'assistant' ? cell.text : '').toBe(
      'echo: hello after the reconnect',
    );

    harness.runtime.stop();
    await harness.settle();
  });

  it('ignores events for a session that is not open', async () => {
    const other = makeSession({ id: 'session-2' });
    const gateway = new FakeGateway({ sessions: [SESSION, other] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();
    expect(harness.snapshot().activeSessionId).toBe('session-1');

    gateway.emit('session-2', {
      type: 'text',
      session_id: 'session-2',
      created_at: '2026-10-01T12:00:05Z',
      request_id: 'req-3',
      content: 'not mine',
    });
    await harness.settle();

    // Nobody was subscribed to session-2, so nothing was delivered at all.
    expect(gateway.droppedDeliveries).toBe(1);
    expect(harness.snapshot().record?.cells).toHaveLength(2);

    harness.runtime.stop();
    await harness.settle();
  });

  it('marks the row when a turn finishes while the page is hidden', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    let visible = true;
    const harness = createHarness({ gateway, isPageVisible: () => visible });
    harness.runtime.start();
    await harness.settle();

    visible = false;
    gateway.emit('session-1', {
      type: 'turn_result',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:10Z',
      request_id: 'req-4',
      subtype: 'success',
      is_error: false,
      duration_ms: 1_200,
      num_turns: 1,
      usage: null,
      errors: [],
      result: 'done',
    });
    await harness.settle();

    const row = harness.snapshot().sessions.find((entry) => entry.id === 'session-1');
    expect(row?.attention).toBe('result');

    // Opening the session clears the badge.
    await harness.runtime.activate('session-1');
    await harness.settle();
    expect(harness.snapshot().sessions[0]?.attention).toBe('none');

    harness.runtime.stop();
    await harness.settle();
  });
});
