import { fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { makeEmptySession, makeShellSession, makeWorkingSession } from '../../src/testing/fixtures';
import { disposeMounted, mountWebview, pushHydrate, pushState, pushTabs } from './harness';

/**
 * First paint of a new session (step 05).
 *
 * The welcome view is shown for an empty, idle session; the "waiting for the host"
 * line is what a user sees before the first `hydrate`.
 */

afterEach(() => {
  disposeMounted();
});

describe('welcome view', () => {
  it('is shown for a new, empty session', () => {
    const { container } = mountWebview([makeEmptySession('session-a')]);
    const ui = within(container);

    expect(ui.getByTestId('welcome')).toBeInTheDocument();
    expect(ui.getByTestId('welcome')).toHaveTextContent('Wing agent');
    expect(ui.queryByTestId('transcript')).toBeNull();
  });

  it('offers the three starting points and the keyboard hint', () => {
    const { container } = mountWebview([makeEmptySession('session-a')]);
    const ui = within(container);

    expect(ui.getAllByTestId('welcome-suggestion')).toHaveLength(3);
    expect(ui.getByTestId('welcome')).toHaveTextContent('Enter to send');
  });

  it('fills the composer instead of sending (the shell never presses send)', () => {
    const { container, bridge } = mountWebview([makeEmptySession('session-a')]);

    const suggestions = within(container).getAllByTestId('welcome-suggestion');
    const text = suggestions[0]?.textContent ?? '';
    fireEvent.click(suggestions[0]!);

    const input = within(container).getByTestId<HTMLTextAreaElement>('composer-input');
    expect(input.value).toBe(text);
    expect(bridge.sentOfType('sendMessage')).toHaveLength(0);
  });

  it('gives way to the transcript as soon as the first message is committed', () => {
    const mounted = mountWebview([makeEmptySession('session-a')]);
    expect(within(mounted.container).getByTestId('welcome')).toBeInTheDocument();

    pushState(mounted, {
      ...makeEmptySession('session-a'),
      status: 'working',
      turn: { active: true, startedAtMs: 0, lastResult: null },
    });

    expect(within(mounted.container).queryByTestId('welcome')).toBeNull();
  });

  it('stays out of the way once the transcript has cells', () => {
    const { container } = mountWebview([makeShellSession()]);

    expect(within(container).queryByTestId('welcome')).toBeNull();
    expect(within(container).getByTestId('transcript')).toBeInTheDocument();
  });

  it('yields the screen to a running turn even with no cells yet', () => {
    const { container } = mountWebview([
      makeWorkingSession({ cells: [{ kind: 'separator', id: 'sep-1', createdAt: 0, label: '' }] }),
    ]);

    expect(within(container).queryByTestId('welcome')).toBeNull();
  });
});

describe('before the host arrives', () => {
  it('waits visibly instead of rendering a blank shell', () => {
    const { container } = mountWebview([], { autoHandshake: false });

    expect(within(container).getByTestId('empty-state')).toHaveTextContent('Waiting for the extension host…');
    expect(within(container).queryByTestId('welcome')).toBeNull();
  });

  it('switches to the session as soon as tabs arrive', () => {
    const mounted = mountWebview([], { autoHandshake: false });

    pushTabs(
      mounted,
      [{ sessionId: 'session-a', title: 'Shell fixture', status: 'idle', attention: 'none' }],
      'session-a',
    );
    pushHydrate(mounted, makeEmptySession('session-a'));

    expect(within(mounted.container).getByTestId('welcome')).toBeInTheDocument();
  });
});
