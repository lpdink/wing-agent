import { act, fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { BRIDGE_PROTOCOL_VERSION, EMPTY_PANELS } from '../../src/shared';
import { MAX_TOASTS, TOAST_TIMEOUT_MS } from '../../src/webview/state/store';
import {
  makeEmptySession,
  makeFixtureSession,
  makeShellSession,
  makeWorkingSession,
} from '../../src/testing/fixtures';
import { disposeMounted, mountWebview, pushPanels, pushState, pushTabs, pushUi, twoTabs } from './harness';

/**
 * Tab bar and status area (step 05) — the two places where the shell has to answer
 * "what is going on?" without the user asking.
 */

afterEach(() => {
  disposeMounted();
});

describe('tab bar', () => {
  it('renders one tab per session and marks the active one', () => {
    const mounted = mountWebview([makeShellSession(), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');

    const tabs = within(mounted.container).getAllByTestId('tab');
    expect(tabs).toHaveLength(2);
    expect(tabs[0]).toHaveTextContent('Shell fixture');
    expect(tabs[0]).toHaveAttribute('aria-selected', 'true');
    expect(tabs[1]).toHaveAttribute('aria-selected', 'false');
    expect(within(mounted.container).getByTestId('tab-list')).toHaveAttribute('role', 'tablist');
  });

  it('activates a session when its tab is clicked', () => {
    const mounted = mountWebview([makeShellSession(), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');

    fireEvent.click(within(mounted.container).getAllByTestId('tab')[1]!);

    expect(mounted.bridge.sentOfType('activateSession')).toEqual([
      { type: 'activateSession', sessionId: 'session-b' },
    ]);
  });

  it('activates a session with the keyboard as well', () => {
    const mounted = mountWebview([makeShellSession(), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');

    fireEvent.keyDown(within(mounted.container).getAllByTestId('tab')[1]!, { key: 'Enter' });

    expect(mounted.bridge.sentOfType('activateSession')).toHaveLength(1);
  });

  it('closes a session from its close button without activating it', () => {
    const mounted = mountWebview([makeShellSession(), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');

    fireEvent.click(within(mounted.container).getAllByTestId('close-tab')[1]!);

    expect(mounted.bridge.sentOfType('closeSession')).toEqual([
      { type: 'closeSession', sessionId: 'session-b' },
    ]);
    expect(mounted.bridge.sentOfType('activateSession')).toHaveLength(0);
  });

  it('starts a session from the + button', () => {
    const mounted = mountWebview([makeShellSession()]);

    fireEvent.click(within(mounted.container).getByTestId('new-session-button'));

    expect(mounted.bridge.sentOfType('newSession')).toEqual([{ type: 'newSession' }]);
  });

  it('asks the host for the session list from the history button', () => {
    const mounted = mountWebview([makeShellSession()]);

    fireEvent.click(within(mounted.container).getByTestId('history-button'));

    // The history entry point is the `/ss` command; the host answers with
    // `sessionPicker` (interfaces.md) — the webview never opens it itself.
    expect(mounted.bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/ss', argsText: '' },
    ]);
    expect(within(mounted.container).queryByTestId('session-panel')).toBeNull();
  });

  it('leaves the history button inert while no session is active', () => {
    const mounted = mountWebview([], { autoHandshake: false });

    const button = within(mounted.container).getByTestId('history-button');
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(mounted.bridge.sentOfType('runPromptCommand')).toHaveLength(0);
  });

  it('closes a tab with Enter on its close button (the tab must not swallow it)', () => {
    const mounted = mountWebview([makeShellSession(), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');

    const close = within(mounted.container).getAllByTestId('close-tab')[1]!;
    fireEvent.keyDown(close, { key: 'Enter' });

    expect(mounted.bridge.sentOfType('closeSession')).toEqual([
      { type: 'closeSession', sessionId: 'session-b' },
    ]);
    // The regression this pins: the parent `role="tab"` handler used to turn that
    // Enter into "activate this tab" (and preventDefault the button's own activation).
    expect(mounted.bridge.sentOfType('activateSession')).toHaveLength(0);
  });

  it('closes a tab with Space on its close button too', () => {
    const mounted = mountWebview([makeShellSession(), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');

    fireEvent.keyDown(within(mounted.container).getAllByTestId('close-tab')[1]!, { key: ' ' });

    expect(mounted.bridge.sentOfType('closeSession')).toHaveLength(1);
    expect(mounted.bridge.sentOfType('activateSession')).toHaveLength(0);
  });

  it('shows the session state on the tab', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushTabs(
      mounted,
      [
        { sessionId: 'session-a', title: 'Shell fixture', status: 'working', attention: 'none' },
        { sessionId: 'session-b', title: 'Asking', status: 'waiting-for-input', attention: 'none' },
      ],
      'session-a',
    );

    const tabs = within(mounted.container).getAllByTestId('tab');
    expect(tabs[0]).toHaveAttribute('data-status', 'working');
    expect(tabs[0]?.querySelector('[data-status="working"]')).not.toBeNull();
    expect(tabs[0]?.getAttribute('aria-label')).toContain('Working');
    expect(tabs[1]).toHaveAttribute('data-status', 'waiting-for-input');
    expect(tabs[1]?.getAttribute('aria-label')).toContain('Waiting for input');
  });

  it('badges a background result on the tab, and names it for assistive tech', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushTabs(
      mounted,
      [{ sessionId: 'session-a', title: 'Shell fixture', status: 'idle', attention: 'result' }],
      'session-a',
    );

    const tab = within(mounted.container).getByTestId('tab');
    expect(tab).toHaveAttribute('data-attention', 'result');
    expect(tab.getAttribute('aria-label')).toContain('Finished in the background');
    expect(tab.querySelector('[data-attention="result"]')).not.toBeNull();

    pushTabs(
      mounted,
      [{ sessionId: 'session-a', title: 'Shell fixture', status: 'idle', attention: 'error' }],
      'session-a',
    );
    expect(within(mounted.container).getByTestId('tab').getAttribute('aria-label')).toContain(
      'Failed in the background',
    );
  });

  it('says so when there are no sessions at all', () => {
    const mounted = mountWebview([], { autoHandshake: false });
    pushTabs(mounted, [], null);

    expect(within(mounted.container).getByTestId('tab-bar')).toHaveTextContent('No sessions');
  });
});

describe('status area', () => {
  it('shows model, provider, thinking, yolo, workspace and context', () => {
    const { container } = mountWebview([makeShellSession()]);
    const ui = within(container);

    expect(ui.getByTestId('session-model')).toHaveTextContent('fixture-model · fixture-provider');
    expect(ui.getByTestId('session-thinking')).toHaveTextContent('Thinking: medium');
    expect(ui.getByTestId('yolo-chip')).toHaveAttribute('data-enabled', 'false');
    expect(ui.getByTestId('yolo-chip')).toHaveAttribute('aria-pressed', 'false');
    expect(ui.getByTestId('workspace-chip')).toHaveTextContent('workspace');
    // 2048 / 200000 → 2.0k / 200.0k
    expect(ui.getByTestId('session-context')).toHaveTextContent('2.0k / 200.0k');
    expect(ui.getByTestId('session-context')).toHaveAttribute('data-level', 'normal');
  });

  it('opens the model picker from both the model and the thinking chip', () => {
    const { container, bridge } = mountWebview([makeShellSession()]);

    fireEvent.click(within(container).getByTestId('model-chip'));
    fireEvent.click(within(container).getByTestId('thinking-chip'));

    expect(bridge.sentOfType('openModelPicker')).toEqual([
      { type: 'openModelPicker', sessionId: 'session-a' },
      { type: 'openModelPicker', sessionId: 'session-a' },
    ]);
  });

  it('toggles YOLO through its chip', () => {
    const { container, bridge } = mountWebview([makeShellSession()]);

    fireEvent.click(within(container).getByTestId('yolo-chip'));

    expect(bridge.sentOfType('setYolo')).toEqual([
      { type: 'setYolo', sessionId: 'session-a', enabled: true },
    ]);
  });

  it('keeps the yolo chip in sync with the host', () => {
    const mounted = mountWebview([makeShellSession({ meta: { ...makeShellSession().meta, yolo: true } })]);
    const ui = within(mounted.container);

    expect(ui.getByTestId('yolo-chip')).toHaveAttribute('aria-pressed', 'true');

    pushState(mounted, { ...makeShellSession(), sessionId: 'session-a' });

    expect(ui.getByTestId('yolo-chip')).toHaveAttribute('aria-pressed', 'false');
  });

  it('reports token totals and the newest TTFT', () => {
    const { container } = mountWebview([makeShellSession()]);
    const ui = within(container);

    expect(ui.getByTestId('session-tokens')).toHaveTextContent('↑2.0k ↓512 ⚡1.5k');
    expect(ui.getByTestId('session-ttft')).toHaveTextContent('TTFT 288ms');
  });

  it('has no TTFT before the first metrics cell arrives', () => {
    const { container } = mountWebview([makeEmptySession('session-a')]);

    expect(within(container).queryByTestId('session-ttft')).toBeNull();
    expect(within(container).getByTestId('session-tokens')).toHaveTextContent('↑2.0k');
  });

  it('turns the context ring warning past 75% and error past 90%', () => {
    const warn = mountWebview([
      makeShellSession({ context: { usedTokens: 160_000, windowTokens: 200_000, messageCount: 20 } }),
    ]);
    expect(within(warn.container).getByTestId('session-context')).toHaveAttribute('data-level', 'warning');

    const error = mountWebview([
      makeShellSession({ context: { usedTokens: 190_000, windowTokens: 200_000, messageCount: 30 } }),
    ]);
    expect(within(error.container).getByTestId('session-context')).toHaveAttribute('data-level', 'error');
  });

  it('degrades to "no window" when the gateway does not report one', () => {
    const { container } = mountWebview([
      makeShellSession({ context: { usedTokens: 100, windowTokens: 0, messageCount: 1 } }),
    ]);

    expect(within(container).getByTestId('session-context')).toHaveTextContent('no window');
  });

  it('round-trips a ping with the injected clock (deterministic)', () => {
    const { container } = mountWebview([makeShellSession()]);
    const ui = within(container);

    fireEvent.click(ui.getByTestId('ping-button'));

    // The controller samples the injected clock before and after the round-trip, and
    // the mock host answers with the same clock — so the measured latency is exactly 0.
    expect(ui.getByTestId('ping-rtt')).toHaveTextContent('pong 0ms');
    expect(ui.getByTestId('bridge-status')).toHaveTextContent('ready');
  });

  it('exposes the bridge protocol version for diagnostics', () => {
    const { container } = mountWebview([makeShellSession()]);

    expect(within(container).getByTestId('ping-button')).toHaveAttribute(
      'data-protocol',
      String(BRIDGE_PROTOCOL_VERSION),
    );
  });

  it('shows the connection chip even before a session exists', () => {
    const { container } = mountWebview([], { autoHandshake: false });

    expect(within(container).getByTestId('bridge-status')).toHaveTextContent('connecting');
    expect(within(container).queryByTestId('model-chip')).toBeNull();
  });

  it('announces the session status politely', () => {
    const { container } = mountWebview([makeWorkingSession()]);
    const status = within(container).getByTestId('session-status');

    expect(status).toHaveAttribute('role', 'status');
    expect(status).toHaveAttribute('aria-live', 'polite');
    expect(status).toHaveTextContent('Working');
  });
});

describe('host banners', () => {
  it('renders the global notice pushed through panels', () => {
    const mounted = mountWebview([makeShellSession()]);

    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      globalNotice: { level: 'error', text: 'gateway unreachable' },
    });

    const notice = within(mounted.container).getByTestId('global-notice');
    expect(notice).toHaveTextContent('gateway unreachable');
    expect(notice).toHaveAttribute('data-level', 'error');
    expect(notice).toHaveAttribute('role', 'alert');
  });

  it('shows the session error banner when the host reports one', () => {
    const mounted = mountWebview([makeShellSession()]);

    pushState(mounted, { ...makeShellSession(), lastError: 'turn failed: provider 500' });

    expect(within(mounted.container).getByTestId('session-error')).toHaveTextContent(
      'turn failed: provider 500',
    );
  });

  it('renders host-driven toasts', () => {
    const mounted = mountWebview([makeFixtureSession()]);

    pushUi(mounted, { kind: 'toast', level: 'warning', message: 'gateway restarting' });

    expect(within(mounted.container).getByTestId('toasts')).toHaveTextContent('gateway restarting');
  });

  /**
   * Review #109 [P2-4]: toasts used to live forever (nothing called
   * `dismissToast`), filling the `role="log"` region for the life of the window.
   */
  it('dismisses a toast on its own after the level deadline', () => {
    vi.useFakeTimers();
    try {
      const mounted = mountWebview([makeFixtureSession()]);
      pushUi(mounted, { kind: 'toast', level: 'info', message: 'copied to clipboard' });
      expect(within(mounted.container).getByTestId('toasts')).toHaveTextContent('copied to clipboard');

      act(() => {
        vi.advanceTimersByTime(TOAST_TIMEOUT_MS.info - 1);
      });
      expect(within(mounted.container).getByTestId('toasts')).toHaveTextContent('copied to clipboard');

      act(() => {
        vi.advanceTimersByTime(1);
      });
      expect(within(mounted.container).queryByText('copied to clipboard')).toBeNull();
      // An empty region is not rendered at all (the shell hides it).
      expect(within(mounted.container).queryByTestId('toasts')).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('keeps an error toast longer and lets the user close it immediately', () => {
    vi.useFakeTimers();
    try {
      const mounted = mountWebview([makeFixtureSession()]);
      pushUi(mounted, { kind: 'toast', level: 'error', message: 'boom' });

      act(() => {
        vi.advanceTimersByTime(TOAST_TIMEOUT_MS.warning);
      });
      // Still there: errors outlive the shorter levels on purpose.
      expect(within(mounted.container).getByTestId('toasts')).toHaveTextContent('boom');

      fireEvent.click(within(mounted.container).getByRole('button', { name: /Dismiss: boom/ }));
      expect(within(mounted.container).queryByTestId('toasts')).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('never renders more than MAX_TOASTS, newest last', () => {
    const mounted = mountWebview([makeFixtureSession()]);
    for (let index = 0; index < MAX_TOASTS + 3; index += 1) {
      pushUi(mounted, { kind: 'toast', level: 'info', message: `note ${index}` });
    }

    const toasts = within(mounted.container).getAllByText(/^note \d$/);
    expect(toasts).toHaveLength(MAX_TOASTS);
    expect(toasts[0]).toHaveTextContent(`note ${3}`);
    expect(toasts[MAX_TOASTS - 1]).toHaveTextContent(`note ${MAX_TOASTS + 2}`);
  });
});
