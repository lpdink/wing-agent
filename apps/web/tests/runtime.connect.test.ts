import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { FakeGateway, makeSession } from './support/fake-gateway';
import { createHarness, simpleHistory, useFakeTimers } from './support/harness';

/**
 * Connection lifecycle: the host-owned first-connect ladder, the stop conditions,
 * and what the shell's banner is told at each step.
 */

const SESSION = makeSession({
  id: 'session-1',
  name: 'First session',
  lastInteraction: '2026-10-01T12:00:00Z',
  messages: simpleHistory('hello'),
});

describe('GatewayRuntime connection', () => {
  beforeEach(() => {
    useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('connects, opens the most recent session and replays it', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway });

    harness.runtime.start();
    await harness.settle();

    const snapshot = harness.snapshot();
    expect(snapshot.connection.phase).toBe('connected');
    expect(snapshot.connection.clientId).toBe('client-1');
    expect(snapshot.connection.everConnected).toBe(true);
    expect(snapshot.connection.lastError).toBeNull();
    expect(snapshot.connection.address).toBe('http://localhost:5173 (this page)');

    // Auto-opened the most recent session (design.md: resume on load).
    expect(snapshot.activeSessionId).toBe('session-1');
    expect(snapshot.record?.cells).toHaveLength(2);
    expect(gateway.callsTo('POST', '/api/session/subscribe')[0]?.headers['X-Client-Id']).toBe('client-1');

    harness.runtime.stop();
    await harness.settle();
  });

  it('retries a failed first connect on the ladder and lands when the gateway is back', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION], failDials: 1 });
    const harness = createHarness({ gateway });

    harness.runtime.start();
    await harness.settle();

    expect(harness.snapshot().connection.phase).toBe('connecting');
    expect(harness.snapshot().connection.everConnected).toBe(false);
    expect(harness.snapshot().connection.lastError).toContain('connection refused');
    // The banner counts the pending retry down.
    expect(harness.snapshot().connection.reconnectInMs).toBeGreaterThan(0);
    expect(gateway.sockets).toHaveLength(1);

    await harness.settle(1_000);
    expect(harness.snapshot().connection.phase).toBe('connected');
    // The failed dial never got an id; this is the first *successful* connection.
    expect(harness.snapshot().connection.clientId).toBe('client-1');
    expect(harness.snapshot().connection.lastError).toBeNull();
    expect(gateway.sockets).toHaveLength(2);

    harness.runtime.stop();
    await harness.settle();
  });

  it('gives up on a rejected API key instead of looping', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    gateway.unauthorized = true;
    const harness = createHarness({ gateway });

    harness.runtime.start();
    await harness.settle();

    const view = harness.snapshot().connection;
    expect(view.phase).toBe('offline');
    expect(view.unauthorized).toBe(true);
    expect(view.lastError).toMatch(/API key/);

    // 10 minutes of ladder: a wrong key never heals, so there is no second dial.
    await harness.settle(600_000);
    expect(gateway.sockets).toHaveLength(1);
    expect(harness.snapshot().connection.phase).toBe('offline');

    harness.runtime.stop();
    await harness.settle();
  });

  it('stops retrying an address that can never work and reports why', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway, settings: { host: 'localhost:8080' } });

    harness.runtime.start();
    await harness.settle(60_000);

    const view = harness.snapshot().connection;
    expect(view.phase).toBe('offline');
    expect(view.everConnected).toBe(false);
    expect(view.lastError).toMatch(/port/);
    expect(gateway.sockets).toHaveLength(0);

    harness.runtime.stop();
    await harness.settle();
  });

  it('recovers on an explicit reconnect after the gateway came back', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION], failDials: 100 });
    const harness = createHarness({ gateway });

    harness.runtime.start();
    await harness.settle(90_000); // several ladder steps
    expect(harness.snapshot().connection.everConnected).toBe(false);
    const dialsBefore = gateway.sockets.length;
    expect(dialsBefore).toBeGreaterThan(1);

    gateway.failDials = 0;
    harness.runtime.reconnect();
    await harness.settle();

    expect(harness.snapshot().connection.phase).toBe('connected');
    expect(gateway.sockets.length).toBeGreaterThan(dialsBefore);
    expect(harness.snapshot().activeSessionId).toBe('session-1');

    harness.runtime.stop();
    await harness.settle();
  });

  it('applies new settings, persists them through the callback and reconnects', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION], failDials: 1 });
    const persisted: string[] = [];
    const harness = createHarness({
      gateway,
      settings: { host: 'old-gateway.lan' },
      onSettingsChange: (settings) =>
        persisted.push(`${settings.scheme}://${settings.host}:${settings.port}`),
    });

    harness.runtime.start();
    await harness.settle();
    expect(harness.snapshot().connection.everConnected).toBe(false);
    expect(harness.snapshot().connection.address).toBe('http://old-gateway.lan:32523');

    harness.runtime.updateSettings({
      scheme: 'http',
      host: '',
      port: 32_523,
      apiKey: null,
      ignoreCertErrors: false,
    });
    await harness.settle();

    expect(persisted).toEqual(['http://:32523']);
    expect(harness.snapshot().connection.phase).toBe('connected');
    expect(harness.snapshot().settings.host).toBe('');
    expect(harness.snapshot().connection.address).toBe('http://localhost:5173 (this page)');

    harness.runtime.stop();
    await harness.settle();
  });

  it('says "offline" after stop() and leaves no socket behind', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    await harness.settle();
    expect(gateway.lastSocket()?.readyState).toBe(1);

    harness.runtime.stop();
    await harness.settle();

    expect(harness.snapshot().connection.phase).toBe('offline');
    expect(gateway.lastSocket()?.readyState).toBe(3);
  });

  it('does not dial twice when start() is called twice', async () => {
    const gateway = new FakeGateway({ sessions: [SESSION] });
    const harness = createHarness({ gateway });
    harness.runtime.start();
    harness.runtime.start();
    await harness.settle();
    expect(gateway.sockets).toHaveLength(1);
    harness.runtime.stop();
    await harness.settle();
  });
});
