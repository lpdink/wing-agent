import { act, fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { BRIDGE_PROTOCOL_VERSION } from '../../src/shared';
import { makeEmptySession, makeFixtureSession } from '../../src/testing/fixtures';
import { createMockBridge } from '../../src/testing/mockBridge';
import type { MockBridge } from '../../src/testing/mockBridge';
import { mountApp } from '../../src/webview/mount';

/**
 * End-to-end webview test: mounts the *real* app against a scripted host, so the
 * assertions cover mount → ready → hydrate → render → intent → patch.
 *
 * This is the closest thing to the VS Code view that runs headless, which is why
 * it exists before any of the real chat UI does.
 */

interface Mounted {
  readonly container: HTMLElement;
  readonly bridge: MockBridge;
  dispose(): void;
}

/**
 * Fixed clock for both sides of the ping round-trip.
 *
 * The bridge measures latency itself (`now()` before send, `now()` after the
 * pong), so the test must inject the *controller's* clock too — sampling the real
 * wall clock made the RTT assertion flaky (review r1 [B1]).
 */
const FIXED_CLOCK = 1_000;

function mount(sessions = [makeFixtureSession()]): Mounted {
  const container = document.createElement('div');
  document.body.append(container);
  const bridge = createMockBridge({ sessions, now: () => FIXED_CLOCK });
  let app: { dispose(): void } | undefined;
  act(() => {
    app = mountApp(container, { transport: bridge.transport, now: () => FIXED_CLOCK });
  });
  return {
    container,
    bridge,
    dispose: () => {
      act(() => {
        app?.dispose();
      });
      container.remove();
    },
  };
}

const mounted: Mounted[] = [];

afterEach(() => {
  for (const item of mounted.splice(0)) {
    item.dispose();
  }
});

function mountTracked(sessions?: Parameters<typeof mount>[0]): Mounted {
  const item = mount(sessions);
  mounted.push(item);
  return item;
}

describe('webview app', () => {
  it('asks the host for state as soon as it is mounted', () => {
    const { bridge } = mountTracked();

    expect(bridge.sentOfType('ready')).toEqual([{ type: 'ready', protocolVersion: BRIDGE_PROTOCOL_VERSION }]);
  });

  it('renders the hydrated session (title, model, transcript)', () => {
    const { container } = mountTracked();
    const ui = within(container);

    expect(ui.getByTestId('session-title')).toHaveTextContent('Fixture session');
    expect(ui.getByTestId('session-model')).toHaveTextContent('fixture-model · fixture-provider');
    expect(ui.getByTestId('session-context')).toHaveTextContent('2048 / 200000 tokens');
    expect(ui.getByTestId('bridge-status')).toHaveTextContent('bridge: ready');
    expect(ui.getByTestId('protocol-version')).toHaveTextContent(`protocol v${BRIDGE_PROTOCOL_VERSION}`);
  });

  it('renders one element per cell and covers every cell kind', () => {
    const { container } = mountTracked();

    const kinds = new Set(
      [...container.querySelectorAll('[data-cell-kind]')].map((node) => node.getAttribute('data-cell-kind')),
    );
    expect(kinds).toEqual(
      new Set([
        'separator',
        'user',
        'assistant',
        'thinking',
        'system',
        'tool_call',
        'diff',
        'todo',
        'ask',
        'metrics',
      ]),
    );
    expect(container.querySelectorAll('[data-cell-kind]')).toHaveLength(makeFixtureSession().cells.length);
  });

  it('shows the empty state when no session is hydrated', () => {
    const { container, bridge } = mountTracked([]);
    const ui = within(container);

    expect(ui.getByTestId('empty-state')).toBeInTheDocument();
    // The host still answered the handshake, so the channel is live.
    expect(ui.getByTestId('bridge-status')).toHaveTextContent('bridge: ready');
    expect(bridge.sentOfType('ready')).toHaveLength(1);
  });

  it('round-trips a ping and shows the measured latency', () => {
    const { container, bridge } = mountTracked();
    const ui = within(container);

    fireEvent.click(ui.getByTestId('ping-button'));

    // Protocol behaviour: one correlated ping on the wire.
    const pings = bridge.sentOfType('ping');
    expect(pings).toHaveLength(1);
    // UI behaviour: the pong is rendered with the latency the *injected* clock
    // measures (0ms here) — deterministic, no wall-clock sampling.
    expect(ui.getByTestId('ping-rtt')).toHaveTextContent('pong in 0ms');
  });

  it('applies streamed patches in order', () => {
    const { container, bridge } = mountTracked();
    const sessionId = 'session-a';

    act(() => {
      bridge.push({
        type: 'patch',
        sessionId,
        seq: 1,
        patches: [
          {
            op: 'append',
            cell: { kind: 'assistant', id: 'stream-1', createdAt: 0, text: '', streaming: true },
          },
        ],
      });
    });
    act(() => {
      bridge.push({
        type: 'patch',
        sessionId,
        seq: 2,
        patches: [{ op: 'append_text', cellId: 'stream-1', text: 'Hello ' }],
      });
    });
    act(() => {
      bridge.push({
        type: 'patch',
        sessionId,
        seq: 3,
        patches: [{ op: 'append_text', cellId: 'stream-1', text: 'world' }],
      });
    });

    const cell = container.querySelector('[data-cell-id="stream-1"]');
    expect(cell?.textContent).toContain('Hello world');
    expect(bridge.sentOfType('resync')).toHaveLength(0);
  });

  it('requests a resync when the patch stream breaks and recovers on hydrate', () => {
    const { bridge, container } = mountTracked();

    act(() => {
      bridge.push({
        type: 'patch',
        sessionId: 'session-a',
        seq: 42,
        patches: [{ op: 'append_text', cellId: 'assistant-1', text: 'out of order' }],
      });
    });

    expect(bridge.sentOfType('resync')).toEqual([
      { type: 'resync', sessionId: 'session-a', lastSeq: 0, reason: 'seq-gap' },
    ]);
    // The mock host answers a resync with a fresh hydrate — the mirror is back in sync.
    expect(within(container).getByTestId('session-title')).toHaveTextContent('Fixture session');
    expect(container.textContent).not.toContain('out of order');
  });

  it('renders host-driven toasts from the ui channel', () => {
    const { container, bridge } = mountTracked();

    act(() => {
      bridge.push({ type: 'ui', action: { kind: 'toast', level: 'warning', message: 'gateway restarting' } });
    });

    expect(within(container).getByTestId('toasts')).toHaveTextContent('gateway restarting');
  });

  it('follows tabs and state updates pushed by the host', () => {
    const { container, bridge } = mountTracked();

    act(() => {
      bridge.push({
        type: 'state',
        state: { ...makeFixtureSession(), status: 'working', title: 'Renamed by host' },
      });
    });

    expect(within(container).getByTestId('session-title')).toHaveTextContent('Renamed by host');
    expect(within(container).getByTestId('session-status')).toHaveTextContent('working');
  });

  it('switches the rendered session when the host activates another tab', () => {
    const other = makeEmptySession('session-b');
    const { container, bridge } = mountTracked([makeFixtureSession(), other]);

    act(() => {
      bridge.push({
        type: 'tabs',
        tabs: [
          { sessionId: 'session-a', title: 'Fixture session', status: 'idle', attention: 'none' },
          { sessionId: 'session-b', title: 'New session', status: 'idle', attention: 'none' },
        ],
        activeSessionId: 'session-b',
      });
    });

    expect(within(container).getByTestId('session-title')).toHaveTextContent('New session');
    expect(container.querySelectorAll('[data-cell-kind]')).toHaveLength(0);
  });

  it('is inert (no console error) when the host never answers', () => {
    const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const container = document.createElement('div');
    document.body.append(container);
    const bridge = createMockBridge({ sessions: [], autoHandshake: false });

    act(() => {
      mountApp(container, { transport: bridge.transport });
    });

    expect(within(container).getByTestId('bridge-status')).toHaveTextContent('bridge: connecting');
    expect(errorSpy).not.toHaveBeenCalled();
    errorSpy.mockRestore();
    container.remove();
  });

  it('unmounts cleanly', () => {
    const item = mountTracked();

    expect(() => item.dispose()).not.toThrow();
    expect(item.container.querySelector('[data-cell-kind]')).toBeNull();
  });
});

describe('cell rendering details', () => {
  it('marks pending/discarded user messages', () => {
    const { container, bridge } = mountTracked();

    act(() => {
      bridge.push({
        type: 'patch',
        sessionId: 'session-a',
        seq: 1,
        patches: [
          {
            op: 'append',
            cell: { kind: 'user', id: 'pending-1', createdAt: 0, text: 'queued', state: 'pending' },
          },
        ],
      });
    });

    const cell = container.querySelector('[data-cell-kind="user"][data-cell-state="pending"]');
    expect(cell?.textContent).toContain('queued');
  });

  it('renders every kind without crashing (smoke over the union)', () => {
    const { container } = mountTracked();

    expect(container.textContent).toContain('Bash');
    expect(container.textContent).toContain('src/webview/state/store.ts');
    expect(container.textContent).toContain('Mirror the host model');
  });
});
