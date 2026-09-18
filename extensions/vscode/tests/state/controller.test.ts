import { beforeEach, describe, expect, it } from 'vitest';

import type { HostToWebviewMessage, WebviewToHostMessage } from '../../src/shared';
import { BRIDGE_PROTOCOL_VERSION, EMPTY_PANELS } from '../../src/shared';
import { makeFixtureSession } from '../../src/testing/fixtures';
import { createMockBridge } from '../../src/testing/mockBridge';
import { createBridgeController } from '../../src/webview/bridge/controller';
import { createAppStore } from '../../src/webview/state/store';
import type { AppStoreApi } from '../../src/webview/state/store';

/**
 * Controller behaviour — the "never guess" rules of the bridge:
 * `ready` on start, apply everything that fits, and answer `resync` (once) for
 * anything that does not. Driven with the scripted host from `src/testing`.
 */

const session = makeFixtureSession();

let store: AppStoreApi;
let clock: number;

function setup(options: { autoHandshake?: boolean } = {}): {
  controller: ReturnType<typeof createBridgeController>;
  bridge: ReturnType<typeof createMockBridge>;
} {
  store = createAppStore();
  clock = 1_000;
  const bridge = createMockBridge({ sessions: [session], autoHandshake: options.autoHandshake ?? false });
  const controller = createBridgeController({
    transport: bridge.transport,
    store,
    now: () => clock,
  });
  return { controller, bridge };
}

beforeEach(() => {
  store = createAppStore();
  clock = 1_000;
});

describe('start', () => {
  it('announces the protocol version', () => {
    const { controller, bridge } = setup();

    controller.start();

    expect(bridge.sentOfType('ready')).toEqual([{ type: 'ready', protocolVersion: BRIDGE_PROTOCOL_VERSION }]);
  });

  it('does not subscribe twice when start is called again', () => {
    const { controller, bridge } = setup();

    controller.start();
    controller.start();

    expect(bridge.sentOfType('ready')).toHaveLength(2);
    bridge.push({ type: 'hydrate', session });
    expect(store.getState().sessions[session.sessionId]).toBeDefined();
  });
});

describe('message handling', () => {
  it('hydrates and marks the channel ready', () => {
    const { controller, bridge } = setup();
    controller.start();

    bridge.push({ type: 'hydrate', session });

    expect(store.getState().bridge.connection).toBe('ready');
    expect(store.getState().sessions[session.sessionId]?.cells).toHaveLength(session.cells.length);
  });

  it('applies a patch batch and advances the cursor', () => {
    const { controller, bridge } = setup();
    controller.start();
    bridge.push({ type: 'hydrate', session });

    bridge.push({
      type: 'patch',
      sessionId: session.sessionId,
      seq: 1,
      patches: [
        { op: 'append', cell: { kind: 'system', id: 'sys-2', createdAt: 1, level: 'notice', text: 'hi' } },
      ],
    });

    expect(store.getState().sessions[session.sessionId]?.seq).toBe(1);
    expect(bridge.sentOfType('resync')).toHaveLength(0);
  });

  it('requests a resync when a patch batch cannot be applied', () => {
    const { controller, bridge } = setup();
    controller.start();
    bridge.push({ type: 'hydrate', session });

    bridge.push({
      type: 'patch',
      sessionId: session.sessionId,
      seq: 7,
      patches: [{ op: 'remove', cellId: 'ghost' }],
    });

    expect(bridge.sentOfType('resync')).toEqual([
      { type: 'resync', sessionId: session.sessionId, lastSeq: session.seq, reason: 'seq-gap' },
    ]);
  });

  it('asks for a resync only once while the host has not answered', () => {
    const { controller, bridge } = setup();
    controller.start();
    bridge.push({ type: 'hydrate', session });

    for (let i = 0; i < 3; i += 1) {
      bridge.push({ type: 'patch', sessionId: session.sessionId, seq: 9, patches: [] });
    }

    expect(bridge.sentOfType('resync')).toHaveLength(1);
  });

  it('re-arms the resync guard after the host re-hydrates', () => {
    const { controller, bridge } = setup();
    controller.start();
    bridge.push({ type: 'hydrate', session });
    bridge.push({ type: 'patch', sessionId: session.sessionId, seq: 9, patches: [] });
    expect(bridge.sentOfType('resync')).toHaveLength(1);

    bridge.push({ type: 'hydrate', session });
    bridge.push({ type: 'patch', sessionId: session.sessionId, seq: 9, patches: [] });

    expect(bridge.sentOfType('resync')).toHaveLength(2);
  });

  it('mirrors state, panels and tabs messages', () => {
    const { controller, bridge } = setup();
    controller.start();
    bridge.push({ type: 'hydrate', session });

    bridge.push({ type: 'state', state: { ...session, status: 'working', title: 'Working on it' } });
    bridge.push({
      type: 'panels',
      sessionId: session.sessionId,
      panels: { ...EMPTY_PANELS, globalNotice: { level: 'info', text: 'note' } },
    });
    bridge.push({
      type: 'tabs',
      tabs: [{ sessionId: session.sessionId, title: 'Working on it', status: 'working', attention: 'none' }],
      activeSessionId: session.sessionId,
    });

    const state = store.getState();
    expect(state.sessions[session.sessionId]?.status).toBe('working');
    expect(state.sessions[session.sessionId]?.panels.globalNotice?.text).toBe('note');
    expect(state.tabs).toHaveLength(1);
    expect(bridge.sentOfType('resync')).toHaveLength(0);
  });

  it('queues ui toasts', () => {
    const { controller, bridge } = setup();
    controller.start();

    bridge.push({ type: 'ui', action: { kind: 'toast', level: 'warning', message: 'gateway restarting' } });

    expect(store.getState().toasts.map((toast) => toast.message)).toEqual(['gateway restarting']);
  });

  it('resyncs the active session when a payload is unrecognizable', () => {
    const { controller, bridge } = setup();
    controller.start();
    bridge.push({ type: 'hydrate', session });

    bridge.push({ type: 'future-message-we-do-not-know' } as unknown as HostToWebviewMessage);

    expect(bridge.sentOfType('resync')).toEqual([
      { type: 'resync', sessionId: session.sessionId, lastSeq: session.seq, reason: 'protocol' },
    ]);
  });

  it('drops unrecognizable payloads when no session is open', () => {
    const { controller, bridge } = setup();
    controller.start();

    bridge.push({ nonsense: true } as unknown as HostToWebviewMessage);

    expect(bridge.sentOfType('resync')).toHaveLength(0);
  });

  it('ignores host messages after dispose', () => {
    const { controller, bridge } = setup();
    controller.start();
    controller.dispose();

    bridge.push({ type: 'hydrate', session });

    expect(store.getState().sessions[session.sessionId]).toBeUndefined();
  });
});

describe('ping', () => {
  it('records the round-trip measured with the injected clock', () => {
    const { controller, bridge } = setup();
    controller.start();

    controller.ping();
    clock = 1_037;
    bridge.push({ type: 'pong', id: 'ping-1', hostTimeMs: 0 });

    expect(bridge.sentOfType('ping')).toEqual([{ type: 'ping', id: 'ping-1' }]);
    expect(store.getState().bridge.lastPong).toEqual({ id: 'ping-1', rttMs: 37 });
  });

  it('numbers pings so responses stay correlated', () => {
    const { controller, bridge } = setup();
    controller.start();

    controller.ping();
    controller.ping();

    expect(bridge.sentOfType('ping').map((message) => message.id)).toEqual(['ping-1', 'ping-2']);
  });

  it('ignores a pong it never asked for', () => {
    const { controller, bridge } = setup();
    controller.start();

    bridge.push({ type: 'pong', id: 'ping-unknown', hostTimeMs: 0 });

    expect(store.getState().bridge.lastPong).toBeNull();
  });
});

describe('post', () => {
  it('forwards intents verbatim', () => {
    const { controller, bridge } = setup();
    controller.start();

    const intent: WebviewToHostMessage = { type: 'sendMessage', sessionId: session.sessionId, text: 'hello' };
    controller.post(intent);

    expect(bridge.sent).toContainEqual(intent);
  });
});
