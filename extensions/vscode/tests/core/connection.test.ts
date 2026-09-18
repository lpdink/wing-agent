import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { reconnectDelayMs } from '../../src/core/backoff';
import {
  type ConnectionState,
  DEFAULT_HANDSHAKE_TIMEOUT_MS,
  GatewayConnection,
  type GatewayConnectionOptions,
} from '../../src/core/connection';
import { GatewaySocketError } from '../../src/core/errors';
import { silentLogger } from '../../src/core/logging';
import type { WingEvent } from '../../src/core/protocol/events';
import { FakeGateway, chunkFrames, syncSessionPayload, textPayload } from './support/fake-gateway';

/**
 * `GatewayConnection` — connection lifecycle, reconnect ladder and the
 * chunking integration.
 *
 * Everything is driven by the in-process fake gateway, so the assertions are
 * about *order and timing semantics*, not about wall-clock luck: fake timers
 * make the reconcile ladder deterministic and `vi.getTimerCount()` catches a
 * leaked retry / handshake / reassembly timer.
 */

const WS_URL = 'ws://127.0.0.1:32523/ws';

function makeConnection(
  gateway: FakeGateway,
  options: Partial<GatewayConnectionOptions> = {},
): GatewayConnection {
  return new GatewayConnection({
    wsUrl: WS_URL,
    socketFactory: gateway.factory,
    logger: silentLogger,
    reconnect: false,
    ...options,
  });
}

/** Connect successfully and return the socket that carried the handshake. */
async function connectOk(
  gateway: FakeGateway,
  clientId = 'c-1',
  options: Partial<GatewayConnectionOptions> = { reconnect: { baseDelayMs: 1_000 } },
): Promise<GatewayConnection> {
  const connection = makeConnection(gateway, options);
  const pending = connection.connect();
  gateway.last.handshake(clientId);
  await pending;
  return connection;
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('connect — handshake', () => {
  it('resolves after the ConnectResponse and reports the client id', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = makeConnection(gateway);
    const states: ConnectionState[] = [];
    connection.onStateChange((state) => states.push(state));

    const pending = connection.connect();
    expect(connection.state.status).toBe('connecting');
    expect(gateway.urls).toStrictEqual([WS_URL]);

    gateway.last.handshake('client-42');
    await pending;

    expect(connection.state).toMatchObject({ status: 'connected', clientId: 'client-42', attempt: 0 });
    expect(connection.currentClientId).toBe('client-42');
    expect(states.map((state) => state.status)).toStrictEqual(['connecting', 'connected']);
  });

  it('shares one in-flight attempt between concurrent connect() calls', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = makeConnection(gateway);

    const first = connection.connect();
    const second = connection.connect();
    expect(gateway.socketCount).toBe(1);

    gateway.last.handshake();
    await Promise.all([first, second]);
    expect(connection.state.status).toBe('connected');
  });

  it('rejects when the socket cannot be opened and stays idle (no retry)', async () => {
    const gateway = new FakeGateway();
    gateway.failNextConnect = new Error('ECONNREFUSED');

    const connection = makeConnection(gateway, { reconnect: { baseDelayMs: 1_000 } });
    await expect(connection.connect()).rejects.toMatchObject({
      name: 'GatewaySocketError',
      kind: 'connect-failed',
    });

    expect(connection.state.status).toBe('idle');
    expect(connection.state.lastError?.message).toContain('ECONNREFUSED');
    expect(vi.getTimerCount()).toBe(0);
  });

  it('rejects when the first frame is not a ConnectResponse', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = makeConnection(gateway);

    const pending = connection.connect();
    gateway.last.push(textPayload('who are you'));
    await expect(pending).rejects.toMatchObject({ kind: 'handshake' });

    expect(connection.state.status).toBe('idle');
    expect(gateway.last.closedWith).not.toBeNull();
  });

  it('rejects a ConnectResponse without a client_id', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = makeConnection(gateway);

    const pending = connection.connect();
    gateway.last.emit({ type: 'connected' });
    await expect(pending).rejects.toMatchObject({ kind: 'handshake' });
  });

  it('times out when the gateway never sends the handshake', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = makeConnection(gateway);

    const pending = connection.connect();
    const rejection = expect(pending).rejects.toMatchObject({ kind: 'handshake' });

    await vi.advanceTimersByTimeAsync(DEFAULT_HANDSHAKE_TIMEOUT_MS);
    await rejection;

    expect(connection.state.status).toBe('idle');
    expect(connection.state.lastError?.message).toContain('did not send a ConnectResponse');
    expect(connection.state.lastError?.retryable).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
  });
});

describe('events', () => {
  it('delivers complete events in arrival order', () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = createConnected(gateway);
    const events: WingEvent[] = [];
    connection.onEvent((event) => events.push(event));

    gateway.last.push(textPayload('one'));
    gateway.last.push(textPayload('two'));

    expect(events.map((event) => (event as { content: string }).content)).toStrictEqual(['one', 'two']);
  });

  it('keeps delivering after a listener throws', () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = createConnected(gateway);
    const seen: string[] = [];
    connection.onEvent(() => {
      throw new Error('listener bug');
    });
    connection.onEvent((event) => seen.push(event.type));

    gateway.last.push(textPayload('still delivered'));
    expect(seen).toStrictEqual(['text']);
  });

  it('stops delivering after unsubscribe', () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = createConnected(gateway);
    const seen: string[] = [];
    const unsubscribe = connection.onEvent((event) => seen.push(event.type));

    unsubscribe();
    gateway.last.push(textPayload('nope'));
    expect(seen).toStrictEqual([]);
  });

  it('delivers unknown event types instead of dropping them', () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = createConnected(gateway);
    const seen: WingEvent[] = [];
    connection.onEvent((event) => seen.push(event));

    gateway.last.emit({ type: 'from_the_future', session_id: 's', ...META });
    expect(seen[0]).toMatchObject({ type: 'from_the_future' });
  });

  it('reassembles a chunked sync_session and keeps the frames behind it in order', () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = createConnected(gateway);
    const events: WingEvent[] = [];
    connection.onEvent((event) => events.push(event));

    const socket = gateway.last;
    const frames = chunkFrames(syncSessionPayload(2048), { id: 'big' });
    socket.push(frames[0] as string);
    socket.push(textPayload('live-1'));
    socket.push(frames[1] as string);
    socket.push(textPayload('live-2'));

    expect(events.map((event) => event.type)).toStrictEqual(['sync_session', 'text', 'text']);
    expect(events[1]).toMatchObject({ content: 'live-1' });
    expect(events[2]).toMatchObject({ content: 'live-2' });
  });
});

const META = { created_at: '2026-09-18T08:00:00', request_id: 'req-1' };

/** Connect and return a connection that is already 'connected'. */
function createConnected(
  gateway: FakeGateway,
  options: Partial<GatewayConnectionOptions> = {},
): GatewayConnection {
  const connection = makeConnection(gateway, options);
  void connection.connect().then(() => undefined);
  gateway.last.handshake('client-live');
  return connection;
}

describe('reconnect supervisor', () => {
  it('reconnects after a loss, using the 1s → 2s → 4s ladder', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway, 'first');
    const states: ConnectionState[] = [];
    connection.onStateChange((state) => states.push(state));

    gateway.last.serverClose(1006, 'abnormal');
    expect(connection.state).toMatchObject({ status: 'reconnecting', reconnectInMs: 1_000, attempt: 0 });
    expect(connection.state.lastError).toMatchObject({ kind: 'disconnected', closeCode: 1006 });
    expect(gateway.socketCount).toBe(1);

    // First retry: the dial fails, so the ladder moves to 2s.
    await vi.advanceTimersByTimeAsync(1_000);
    expect(gateway.socketCount).toBe(2);
    gateway.last.serverClose(1006);
    await flushMicrotasks();
    expect(connection.state).toMatchObject({ status: 'reconnecting', reconnectInMs: 2_000, attempt: 1 });

    // Second retry: the handshake succeeds and the ladder resets.
    await vi.advanceTimersByTimeAsync(2_000);
    expect(gateway.socketCount).toBe(3);
    gateway.last.handshake('second');
    await flushMicrotasks();

    expect(connection.state).toMatchObject({ status: 'connected', clientId: 'second', attempt: 0 });
    expect(states.map((state) => state.status)).toContain('reconnecting');
  });

  it('caps the retry delay at 30s', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    gateway.last.serverClose(1006);
    const delays: number[] = [];
    for (let attempt = 0; attempt < 7; attempt += 1) {
      delays.push(connection.state.reconnectInMs ?? -1);
      await vi.advanceTimersByTimeAsync(connection.state.reconnectInMs ?? 0);
      gateway.last.serverClose(1006);
      await flushMicrotasks();
    }
    expect(delays).toStrictEqual([1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);
  });

  it('stops for good on 4001 (Unauthorized) instead of looping', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    gateway.last.serverClose(4001, 'Unauthorized');
    expect(connection.state.status).toBe('closed');
    expect(connection.state.lastError).toMatchObject({ kind: 'unauthorized', closeCode: 4001 });
    expect(connection.state.reconnectInMs).toBeNull();

    await vi.advanceTimersByTimeAsync(60_000);
    expect(gateway.socketCount).toBe(1);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('does not supervise when reconnect is disabled', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway, 'c', { reconnect: false });

    gateway.last.serverClose(1006);
    expect(connection.state.status).toBe('closed');
    await vi.advanceTimersByTimeAsync(60_000);
    expect(gateway.socketCount).toBe(1);
  });

  it('rejects a connect() that races a retry instead of opening a second socket', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    gateway.last.serverClose(1006);
    const racing = connection.connect();
    expect(gateway.socketCount).toBe(2);

    gateway.last.handshake('nudged');
    await racing;
    expect(connection.state).toMatchObject({ status: 'connected', clientId: 'nudged' });
  });

  it('ignores events from a socket that was already torn down', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);
    const stale = gateway.last;
    const events: WingEvent[] = [];
    connection.onEvent((event) => events.push(event));

    stale.serverClose(1006);
    await vi.advanceTimersByTimeAsync(1_000);
    gateway.last.handshake('fresh');
    await flushMicrotasks();

    // A late close/message from the old socket must not disturb the new one.
    stale.push(textPayload('zombie'));
    stale.serverClose(1006);

    expect(events).toStrictEqual([]);
    expect(connection.state).toMatchObject({ status: 'connected', clientId: 'fresh' });
  });
});

describe('failure handling inside a live connection', () => {
  it('treats a reassembly violation as a connection failure and reconnects', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    gateway.last.push(
      JSON.stringify({ type: '_chunk', id: 'a', index: 1, count: 2, of_type: 'text', data: '' }),
    );

    expect(connection.state.lastError?.kind).toBe('reassembly');
    expect(connection.state.status).toBe('reconnecting');
    expect(gateway.last.closedWith).not.toBeNull();
  });

  it('reconnects when a reassembled payload is a malformed known type (review r1 N1)', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);
    const events: WingEvent[] = [];
    connection.onEvent((event) => events.push(event));

    // `sync_session` is the realistic case: a corrupt replay must be re-fetched
    // over a fresh connection rather than delivered as an unusable unknown event.
    const frames = chunkFrames('{"type":"sync_session","messages":[]}', { id: 'broken' });
    gateway.last.push(frames[0] as string);
    expect(events).toStrictEqual([]);
    gateway.last.push(frames[1] as string);

    expect(events).toStrictEqual([]);
    expect(connection.state.lastError).toMatchObject({ kind: 'reassembly' });
    expect(connection.state.status).toBe('reconnecting');
    expect(gateway.last.closedWith).not.toBeNull();
  });

  it('fails the connection when a fragmented event never completes', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway, 'c', {
      reconnect: { baseDelayMs: 1_000 },
      chunkLimits: { idleTimeoutMs: 5_000 },
    });

    const frames = chunkFrames(syncSessionPayload(128), { id: 'never-finishes' });
    gateway.last.push(frames[0] as string);
    expect(connection.state.lastError).toBeNull();

    await vi.advanceTimersByTimeAsync(5_000);

    expect(connection.state.lastError).toMatchObject({ kind: 'reassembly' });
    expect(connection.state.lastError?.message).toContain('timed out');
    expect(connection.state.status).toBe('reconnecting');
  });

  it('reports a transport error without tearing the connection down on its own', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    gateway.last.serverError('read ECONNRESET');
    expect(connection.state.status).toBe('connected');

    gateway.last.serverClose(1006);
    expect(connection.state.status).toBe('reconnecting');
  });
});

describe('send', () => {
  it('sends an encoded ClientRequest frame', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    connection.send({
      request_id: 'req-1',
      session_id: 'sess-1',
      content: 'hello',
      tool_call_id: null,
    });

    expect(gateway.last.frames).toStrictEqual([
      { request_id: 'req-1', session_id: 'sess-1', content: 'hello' },
    ]);
  });

  it('includes the tool_call_id when answering an ask', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    connection.send({
      request_id: 'req-2',
      session_id: 'sess-1',
      content: 'yes',
      tool_call_id: 'call-9',
    });

    expect(gateway.last.frames[0]).toMatchObject({ tool_call_id: 'call-9' });
  });

  it('sendMessage() returns the frame with a uuid4().hex-shaped request id', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    const frame = connection.sendMessage({ sessionId: 'sess-1', content: 'hi' });

    expect(frame.request_id).toMatch(/^[0-9a-f]{32}$/);
    expect(frame.tool_call_id).toBeNull();
    expect(gateway.last.frames[0]).toStrictEqual({
      request_id: frame.request_id,
      session_id: 'sess-1',
      content: 'hi',
    });
  });

  it('throws a typed error when there is no open connection', () => {
    const gateway = new FakeGateway();
    const connection = makeConnection(gateway);

    expect(() => connection.sendMessage({ sessionId: 's', content: 'x' })).toThrowError(
      expect.objectContaining({ name: 'GatewaySocketError', kind: 'not-connected' }),
    );
  });

  it('throws when the socket closed underneath us', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    gateway.last.readyState = 3;
    expect(() => connection.sendMessage({ sessionId: 's', content: 'x' })).toThrowError(
      expect.objectContaining({ kind: 'not-connected' }),
    );
  });
});

describe('close', () => {
  it('closes the socket, drops the timers and never reconnects', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    connection.close(1000, 'bye');

    expect(connection.state).toMatchObject({ status: 'closed', clientId: null });
    expect(gateway.last.closedWith).toStrictEqual({ code: 1000, reason: 'bye' });
    expect(vi.getTimerCount()).toBe(0);

    gateway.last.serverClose(1006);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(gateway.socketCount).toBe(1);
  });

  it('rejects a pending connect()', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = makeConnection(gateway);

    const pending = connection.connect();
    connection.close();
    await expect(pending).rejects.toBeInstanceOf(GatewaySocketError);
    expect(connection.state.status).toBe('closed');
  });

  it('is safe to call twice and leaves nothing behind', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);

    connection.close();
    connection.close();
    expect(vi.getTimerCount()).toBe(0);
    expect(connection.state.status).toBe('closed');
  });

  it('allows connecting again after close()', async () => {
    const gateway = new FakeGateway();
    gateway.autoHandshake = false;
    const connection = await connectOk(gateway);
    connection.close();

    const pending = connection.connect();
    gateway.last.handshake('again');
    await pending;
    expect(connection.state).toMatchObject({ status: 'connected', clientId: 'again' });
  });
});

describe('reconnectDelayMs', () => {
  it('matches the Rust ladder and guards odd inputs', () => {
    expect([0, 1, 2, 3, 4, 5, 6, 10, 100].map((attempt) => reconnectDelayMs(attempt))).toStrictEqual([
      1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000, 30_000,
    ]);
    expect(reconnectDelayMs(-3)).toBe(1_000);
    expect(reconnectDelayMs(1.7)).toBe(2_000);
    expect(reconnectDelayMs(0, { baseDelayMs: 250, maxDelayMs: 900 })).toBe(250);
    expect(reconnectDelayMs(3, { baseDelayMs: 250, maxDelayMs: 900 })).toBe(900);
  });
});

/**
 * Review #109 [P3-6]: the API key must not travel in the URL. The connection
 * hands it to the socket factory as a header instead — and passes *nothing* when
 * there is no key, so a DOM-shaped `WebSocket` never receives an options object.
 */
describe('handshake headers and runtime diagnostics', () => {
  it('forwards the configured headers to the socket factory', async () => {
    const gateway = new FakeGateway();
    await connectOk(gateway, 'c-1', {
      reconnect: false,
      headers: { Authorization: 'Bearer secret' },
    });

    expect(gateway.options).toStrictEqual([{ headers: { Authorization: 'Bearer secret' } }]);
    // The URL itself carries nothing sensitive.
    expect(gateway.urls).toStrictEqual([WS_URL]);
  });

  it('passes no options at all when there is no key', async () => {
    const gateway = new FakeGateway();
    await connectOk(gateway, 'c-1', { reconnect: false });

    expect(gateway.options).toStrictEqual([undefined]);
  });

  it('names the runtime in a connect failure (node version + WebSocket source)', async () => {
    const gateway = new FakeGateway();
    gateway.failNextConnect = new Error('no WebSocket implementation available');
    const connection = makeConnection(gateway, {
      headers: { Authorization: 'Bearer secret' },
    });

    const error: unknown = await connection.connect().catch((cause: unknown) => cause);
    expect(error).toBeInstanceOf(GatewaySocketError);
    const message = error instanceof Error ? error.message : '';
    expect(message).toMatch(
      /^failed to open ws:\/\/127\.0\.0\.1:32523\/ws: no WebSocket implementation available \[node \d+\.[\d.]+/,
    );
    // Neither the URL nor the runtime description may leak the key.
    expect(message).not.toContain('secret');
  });
});

async function flushMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}
