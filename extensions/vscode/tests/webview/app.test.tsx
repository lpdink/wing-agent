import { act, fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { BRIDGE_PROTOCOL_VERSION } from '../../src/shared';
import { makeEmptySession, makeFixtureSession, makeStreamingCells } from '../../src/testing/fixtures';
import { cellElement, disposeMounted, mountWebview, pushPatch, pushState, pushTabs } from './harness';

/**
 * End-to-end webview test: mounts the *real* app against a scripted host, so the
 * assertions cover mount → ready → hydrate → render → intent → patch through the
 * whole shell (tab bar, status area, transcript, composer).
 *
 * This is the closest thing to the VS Code view that runs headless. The shell
 * assertions (formerly against step 01's header/footer placeholders) live here too:
 * everything a user can see about the *session* has a stable anchor.
 */

afterEach(() => {
  disposeMounted();
});

describe('webview app', () => {
  it('asks the host for state as soon as it is mounted', () => {
    const { bridge } = mountWebview();

    expect(bridge.sentOfType('ready')).toEqual([{ type: 'ready', protocolVersion: BRIDGE_PROTOCOL_VERSION }]);
  });

  it('renders the hydrated session (tab title, model, context, transcript)', () => {
    const { container } = mountWebview([makeFixtureSession()]);
    const ui = within(container);

    expect(ui.getByTestId('tab')).toHaveTextContent('Fixture session');
    expect(ui.getByTestId('session-model')).toHaveTextContent('fixture-model · fixture-provider');
    expect(ui.getByTestId('session-context')).toHaveTextContent('2.0k / 200.0k');
    expect(ui.getByTestId('bridge-status')).toHaveTextContent('ready');
    expect(ui.getByTestId('ping-button')).toHaveAttribute('data-protocol', String(BRIDGE_PROTOCOL_VERSION));
    expect(ui.getByTestId('transcript')).toBeInTheDocument();
  });

  it('renders one element per cell and covers every cell kind', () => {
    const { container } = mountWebview([makeFixtureSession()]);

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

  it('announces the turn state to assistive tech', () => {
    const { container } = mountWebview([makeFixtureSession()]);

    const status = within(container).getByTestId('session-status');
    expect(status).toHaveAttribute('role', 'status');
    expect(status).toHaveAttribute('aria-live', 'polite');
  });

  it('shows the empty state when no session is hydrated', () => {
    const { container, bridge } = mountWebview([], { autoHandshake: false });
    const ui = within(container);

    expect(ui.getByTestId('empty-state')).toBeInTheDocument();
    // The host answered only the transport, not the session.
    expect(ui.getByTestId('bridge-status')).toHaveTextContent('connecting');

    act(() => {
      bridge.push({ type: 'tabs', tabs: [], activeSessionId: null });
    });

    expect(ui.getByTestId('bridge-status')).toHaveTextContent('ready');
    expect(bridge.sentOfType('ready')).toHaveLength(1);
  });

  it('round-trips a ping and shows the measured latency', () => {
    const { container } = mountWebview([makeFixtureSession()]);
    const ui = within(container);

    fireEvent.click(ui.getByTestId('ping-button'));

    // UI behaviour: the pong is rendered with the latency the *injected* clock
    // measures (0ms here) — deterministic, no wall-clock sampling.
    expect(ui.getByTestId('ping-rtt')).toHaveTextContent('pong 0ms');
  });

  it('applies streamed patches in order', () => {
    const mounted = mountWebview([makeFixtureSession()]);
    const { container, bridge } = mounted;

    pushPatch(mounted, 'session-a', 1, [
      { op: 'append', cell: { kind: 'assistant', id: 'stream-1', createdAt: 0, text: '', streaming: true } },
    ]);
    pushPatch(mounted, 'session-a', 2, [{ op: 'append_text', cellId: 'stream-1', text: 'Hello ' }]);
    pushPatch(mounted, 'session-a', 3, [{ op: 'append_text', cellId: 'stream-1', text: 'world' }]);

    expect(cellElement(container, 'stream-1').textContent).toContain('Hello world');
    expect(bridge.sentOfType('resync')).toHaveLength(0);
  });

  it('requests a resync when the patch stream breaks and recovers on hydrate', () => {
    const { bridge, container } = mountWebview([makeFixtureSession()]);

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
    expect(within(container).getByTestId('tab')).toHaveTextContent('Fixture session');
    expect(container.textContent).not.toContain('out of order');
  });

  it('follows tabs and state updates pushed by the host', () => {
    const mounted = mountWebview([makeFixtureSession()]);

    pushState(mounted, { ...makeFixtureSession(), status: 'working', title: 'Renamed by host' });
    pushTabs(
      mounted,
      [{ sessionId: 'session-a', title: 'Renamed by host', status: 'working', attention: 'none' }],
      'session-a',
    );

    const ui = within(mounted.container);
    expect(ui.getByTestId('tab')).toHaveTextContent('Renamed by host');
    expect(ui.getByTestId('session-status')).toHaveTextContent('Working');
    // The tab's own status attribute is what the dot is drawn from.
    expect(ui.getByTestId('tab')).toHaveAttribute('data-status', 'working');
  });

  it('switches the rendered session when the host activates another tab', () => {
    const other = makeEmptySession('session-b');
    const { container, bridge } = mountWebview([makeFixtureSession(), other]);

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

    expect(within(container).getByTestId('welcome')).toBeInTheDocument();
    expect(container.querySelectorAll('[data-cell-kind]')).toHaveLength(0);
  });

  it('is inert (no console error) when the host never answers', () => {
    const errorSpy = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const { container } = mountWebview([], { autoHandshake: false });

    expect(within(container).getByTestId('bridge-status')).toHaveTextContent('connecting');
    expect(errorSpy).not.toHaveBeenCalled();
    errorSpy.mockRestore();
  });

  it('unmounts cleanly', () => {
    const item = mountWebview([makeFixtureSession()]);

    expect(() => item.dispose()).not.toThrow();
    expect(item.container.querySelector('[data-cell-kind]')).toBeNull();
  });
});

describe('cell rendering details', () => {
  it('marks pending user messages', () => {
    const { container, bridge } = mountWebview([makeFixtureSession()]);

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
    const { container } = mountWebview([makeFixtureSession()]);

    expect(container.textContent).toContain('Bash');
    expect(container.textContent).toContain('src/webview/state/store.ts');
    expect(container.textContent).toContain('Mirror the host model');
  });

  it('shows the streaming caret for a live turn and hides it afterwards', () => {
    const { container } = mountWebview([
      makeFixtureSession({
        title: 'Streaming turn',
        cells: makeStreamingCells(),
        sessionId: 'session-a',
      }),
    ]);

    expect(within(cellElement(container, 'assistant-live')).getByTestId('stream-caret')).toBeInTheDocument();
    expect(cellElement(container, 'thinking-live')).toHaveAttribute('data-collapsed', 'false');
  });
});
