import { fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { EMPTY_PANELS } from '../../src/shared';
import { makeEmptySession, makeShellSession, makeWorkingSession } from '../../src/testing/fixtures';
import { disposeMounted, mountWebview, pushPanels, pushTabs, pushUi, twoTabs } from './harness';

/**
 * The composer (step 05).
 *
 * Every assertion is about *what the user typed* and *which intent left the
 * webview* — the composer is not allowed to do anything else (design.md D3/D6).
 */

afterEach(() => {
  disposeMounted();
});

function inputOf(container: HTMLElement): HTMLTextAreaElement {
  return within(container).getByTestId('composer-input');
}

describe('composer basics', () => {
  it('focuses the editor on mount', () => {
    const { container } = mountWebview([makeShellSession()]);
    expect(inputOf(container)).toHaveFocus();
  });

  it('sends the typed text on Enter and clears the draft', () => {
    const { container, bridge } = mountWebview([makeShellSession()]);
    const input = inputOf(container);

    fireEvent.change(input, { target: { value: 'Explain the store' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    expect(bridge.sentOfType('sendMessage')).toEqual([
      { type: 'sendMessage', sessionId: 'session-a', text: 'Explain the store' },
    ]);
    expect(input.value).toBe('');
  });

  it('sends on Ctrl+Enter as well', () => {
    const { container, bridge } = mountWebview([makeShellSession()]);
    const input = inputOf(container);

    fireEvent.change(input, { target: { value: 'hello' } });
    fireEvent.keyDown(input, { key: 'Enter', ctrlKey: true });

    expect(bridge.sentOfType('sendMessage')).toHaveLength(1);
  });

  it('does not send on Shift+Enter (newline) and keeps the draft', () => {
    const { container, bridge } = mountWebview([makeShellSession()]);
    const input = inputOf(container);

    fireEvent.change(input, { target: { value: 'line one' } });
    fireEvent.keyDown(input, { key: 'Enter', shiftKey: true });

    expect(bridge.sentOfType('sendMessage')).toHaveLength(0);
    expect(input.value).toBe('line one');
  });

  it('keeps the send button disabled until there is something to send', () => {
    const { container } = mountWebview([makeShellSession()]);
    const send = within(container).getByTestId('send-button');

    expect(send).toBeDisabled();
    fireEvent.change(inputOf(container), { target: { value: '   ' } });
    expect(send).toBeDisabled();
    fireEvent.change(inputOf(container), { target: { value: 'ok' } });
    expect(send).toBeEnabled();
  });

  it('sends the trimmed text through the button as well', () => {
    const { bridge, container } = mountWebview([makeShellSession()]);

    fireEvent.change(inputOf(container), { target: { value: '  spaced  ' } });
    fireEvent.click(within(container).getByTestId('send-button'));

    expect(bridge.sentOfType('sendMessage')[0]?.text).toBe('spaced');
  });
});

describe('composer turn state', () => {
  it('turns the action into Stop while a turn runs and interrupts on click', () => {
    const { container, bridge } = mountWebview([makeWorkingSession()]);
    const ui = within(container);

    expect(ui.queryByTestId('send-button')).toBeNull();
    fireEvent.click(ui.getByTestId('stop-button'));

    expect(bridge.sentOfType('interrupt')).toEqual([{ type: 'interrupt', sessionId: 'session-a' }]);
  });

  it('interrupts on Ctrl+Escape while working', () => {
    const { container, bridge } = mountWebview([makeWorkingSession()]);

    fireEvent.keyDown(inputOf(container), { key: 'Escape', ctrlKey: true });

    expect(bridge.sentOfType('interrupt')).toHaveLength(1);
  });

  it('does not interrupt when idle (the cancel keybinding is scoped to a turn)', () => {
    const { container, bridge } = mountWebview([makeShellSession()]);

    fireEvent.keyDown(inputOf(container), { key: 'Escape', ctrlKey: true });

    expect(bridge.sentOfType('interrupt')).toHaveLength(0);
  });

  it('still allows sending while working — the host queues it', () => {
    const { bridge, container } = mountWebview([makeWorkingSession()]);

    fireEvent.change(inputOf(container), { target: { value: 'one more thing' } });
    fireEvent.keyDown(inputOf(container), { key: 'Enter' });

    expect(bridge.sentOfType('sendMessage')[0]?.text).toBe('one more thing');
  });

  it('shows the queued (pending) user messages above the composer', () => {
    const { container } = mountWebview([makeWorkingSession()]);
    const queue = within(container).getByTestId('queue-bar');

    expect(queue).toHaveTextContent('Queued · 1');
    expect(queue).toHaveTextContent('And add a test for the queue state.');
  });

  it('says so when the session waits for an answer', () => {
    const { container } = mountWebview([makeShellSession({ status: 'waiting-for-input', cells: [] })]);
    const ui = within(container);

    expect(ui.getByTestId('composer-hint')).toHaveTextContent('Waiting for your answer');
    expect(inputOf(container).placeholder).toBe('Answer the question above to continue');
  });
});

describe('composer without a session', () => {
  it('disables the editor and explains what to do', () => {
    const { container } = mountWebview([], { autoHandshake: false });

    expect(inputOf(container)).toBeDisabled();
    expect(inputOf(container).placeholder).toBe('No session — start one with +');
    expect(within(container).getByTestId('send-button')).toBeDisabled();
  });

  it('refocuses the editor when the host asks for it', () => {
    const mounted = mountWebview([makeShellSession()]);
    const input = inputOf(mounted.container);

    input.blur();
    expect(input).not.toHaveFocus();

    pushUi(mounted, { kind: 'focusComposer' });

    expect(inputOf(mounted.container)).toHaveFocus();
  });
});

describe('composer drafts', () => {
  it('keeps a draft per session', () => {
    const other = makeEmptySession('session-b');
    const mounted = mountWebview([makeShellSession(), other]);
    const { container } = mounted;

    fireEvent.change(inputOf(container), { target: { value: 'draft for A' } });

    // Switch to session-b (host-owned tab switch) and back.
    pushTabs(mounted, twoTabs(), 'session-b');
    expect(inputOf(container).value).toBe('');

    fireEvent.change(inputOf(container), { target: { value: 'draft for B' } });

    pushTabs(mounted, twoTabs(), 'session-a');
    expect(inputOf(container).value).toBe('draft for A');

    pushTabs(mounted, twoTabs(), 'session-b');
    expect(inputOf(container).value).toBe('draft for B');
  });

  it('forgets the draft of a closed tab', () => {
    const other = makeEmptySession('session-b');
    const mounted = mountWebview([makeShellSession(), other]);
    const { container } = mounted;

    fireEvent.change(inputOf(container), { target: { value: 'draft for A' } });
    pushTabs(mounted, twoTabs(), 'session-b');

    // The host closed session-a: only tab b is left.
    pushTabs(
      mounted,
      [{ sessionId: 'session-b', title: 'New session', status: 'idle', attention: 'none' }],
      'session-b',
    );
    // Re-opening it hydrates a fresh view — and no stale draft.
    pushTabs(mounted, twoTabs(), 'session-a');

    expect(inputOf(container).value).toBe('');
  });
});

describe('command candidates', () => {
  const base = makeShellSession();

  it('opens on a bare slash and merges the host catalog with the frontend table', () => {
    const { container } = mountWebview([base]);

    fireEvent.change(inputOf(container), { target: { value: '/' } });

    const rows = within(container).getAllByTestId('command-candidate');
    const text = rows.map((row) => row.textContent ?? '').join('\n');
    expect(text).toContain('/init'); // gateway prompt command from the host catalog
    expect(text).toContain('/session (/ss)'); // frontend command, alias shown
    expect(text).toContain('/compact');
    // The host's wording wins for a command both sides know.
    expect(text).toContain('Compress the session context');
  });

  it('filters as the name is typed', () => {
    const { container } = mountWebview([base]);

    fireEvent.change(inputOf(container), { target: { value: '/rew' } });

    const rows = within(container).getAllByTestId('command-candidate');
    expect(rows).toHaveLength(1);
    expect(rows[0]?.textContent).toContain('/rewind');
  });

  it('closes once the arguments start', () => {
    const { container } = mountWebview([base]);

    fireEvent.change(inputOf(container), { target: { value: '/rewind ' } });

    expect(within(container).queryByTestId('command-candidates')).toBeNull();
  });

  it('navigates with the arrow keys and accepts with Enter (no message sent)', () => {
    const { container, bridge } = mountWebview([base]);
    const input = inputOf(container);

    fireEvent.change(input, { target: { value: '/re' } });
    const rows = within(container).getAllByTestId('command-candidate');
    expect(rows.length).toBeGreaterThan(1);
    expect(rows[0]).toHaveAttribute('aria-selected', 'true');

    fireEvent.keyDown(input, { key: 'ArrowDown' });
    const afterDown = within(container).getAllByTestId('command-candidate');
    expect(afterDown[1]).toHaveAttribute('aria-selected', 'true');

    fireEvent.keyDown(input, { key: 'Enter' });

    expect(input.value).toBe('/rewind ');
    expect(bridge.sentOfType('sendMessage')).toHaveLength(0);
    expect(within(container).queryByTestId('command-candidates')).toBeNull();
  });

  it('accepts with Tab without sending', () => {
    const { container, bridge } = mountWebview([base]);
    const input = inputOf(container);

    fireEvent.change(input, { target: { value: '/yo' } });
    fireEvent.keyDown(input, { key: 'Tab' });

    expect(input.value).toBe('/yolo ');
    expect(bridge.sentOfType('sendMessage')).toHaveLength(0);
  });

  it('dismisses on Escape and re-opens on the next keystroke', () => {
    const { container } = mountWebview([base]);
    const input = inputOf(container);

    fireEvent.change(input, { target: { value: '/re' } });
    fireEvent.keyDown(input, { key: 'Escape' });
    expect(within(container).queryByTestId('command-candidates')).toBeNull();

    fireEvent.change(input, { target: { value: '/rew' } });
    expect(within(container).queryByTestId('command-candidates')).not.toBeNull();
  });

  it('shows no popover when nothing matches', () => {
    const { container } = mountWebview([base]);

    fireEvent.change(inputOf(container), { target: { value: '/zzz' } });

    expect(within(container).queryByTestId('command-candidates')).toBeNull();
  });

  it('runs an exactly typed command instead of accepting it (TUI rule)', () => {
    const { container, bridge } = mountWebview([base]);

    fireEvent.change(inputOf(container), { target: { value: '/compact' } });
    fireEvent.keyDown(inputOf(container), { key: 'Enter' });

    expect(bridge.sentOfType('compact')).toEqual([{ type: 'compact', sessionId: 'session-a' }]);
    expect(inputOf(container).value).toBe('');
  });

  it('falls back to the frontend table while the host has no catalog', () => {
    const { container } = mountWebview([makeShellSession({ panels: EMPTY_PANELS })]);

    fireEvent.change(inputOf(container), { target: { value: '/' } });

    const text = within(container)
      .getAllByTestId('command-candidate')
      .map((row) => row.textContent ?? '')
      .join('\n');
    expect(text).toContain('/session (/ss)');
    expect(text).not.toContain('/init');
  });
});

describe('submission routing', () => {
  function submitWith(draft: string, session = makeShellSession()): ReturnType<typeof mountWebview> {
    const mounted = mountWebview([session]);
    fireEvent.change(inputOf(mounted.container), { target: { value: draft } });
    fireEvent.keyDown(inputOf(mounted.container), { key: 'Enter' });
    return mounted;
  }

  it('/model opens the host-owned picker', () => {
    const { bridge } = submitWith('/model');
    expect(bridge.sentOfType('openModelPicker')).toEqual([
      { type: 'openModelPicker', sessionId: 'session-a' },
    ]);
  });

  it('/new starts a session', () => {
    expect(submitWith('/new').bridge.sentOfType('newSession')).toEqual([{ type: 'newSession' }]);
  });

  it('/think toggles thinking when it has no argument', () => {
    expect(submitWith('/think').bridge.sentOfType('setThinking')).toEqual([
      { type: 'setThinking', sessionId: 'session-a', enabled: false },
    ]);
    // The fixture has thinking on; toggling from off turns it on.
    const off = makeShellSession({ meta: { ...makeShellSession().meta, thinking: false } });
    expect(submitWith('/think', off).bridge.sentOfType('setThinking')).toEqual([
      { type: 'setThinking', sessionId: 'session-a', enabled: true },
    ]);
  });

  it('/think off and /think high map onto the two knobs', () => {
    expect(submitWith('/think off').bridge.sentOfType('setThinking')).toEqual([
      { type: 'setThinking', sessionId: 'session-a', enabled: false },
    ]);

    const high = submitWith('/think high');
    expect(high.bridge.sentOfType('setThinking')).toEqual([
      { type: 'setThinking', sessionId: 'session-a', enabled: true },
    ]);
    expect(high.bridge.sentOfType('setEffort')).toEqual([
      { type: 'setEffort', sessionId: 'session-a', effort: 'high' },
    ]);
  });

  it('/yolo flips the switch it shows', () => {
    expect(submitWith('/yolo').bridge.sentOfType('setYolo')).toEqual([
      { type: 'setYolo', sessionId: 'session-a', enabled: true },
    ]);
    expect(submitWith('/yolo off').bridge.sentOfType('setYolo')).toEqual([
      { type: 'setYolo', sessionId: 'session-a', enabled: false },
    ]);
  });

  it('/compact forwards a focus instruction, and compacts without one', () => {
    expect(submitWith('/compact').bridge.sentOfType('compact')).toHaveLength(1);
    expect(submitWith('/compact focus on the parser').bridge.sentOfType('runPromptCommand')).toEqual([
      {
        type: 'runPromptCommand',
        sessionId: 'session-a',
        name: '/compact',
        argsText: 'focus on the parser',
      },
    ]);
  });

  it('/ss with no argument opens the sessions panel', () => {
    const { container } = submitWith('/ss');
    expect(within(container).getByTestId('session-panel')).toBeInTheDocument();
  });

  it('/ss <id> is forwarded for the host to resume', () => {
    expect(submitWith('/ss session-c').bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/session', argsText: 'session-c' },
    ]);
  });

  it('/rewind and /fork open the branch panel in their own mode', () => {
    expect(within(submitWith('/rewind').container).getByTestId('branch-panel')).toBeInTheDocument();
    expect(submitWith('/rewind').container.textContent).toContain('Rewind to message');
    expect(submitWith('/fork').container.textContent).toContain('Fork from message');
  });

  it('forwards gateway prompt commands with their leading slash', () => {
    expect(submitWith('/init').bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/init', argsText: '' },
    ]);
  });

  it('forwards the frontend commands the host owns', () => {
    expect(submitWith('/copy 2').bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/copy', argsText: '2' },
    ]);
  });

  it('accepts /m as an alias of /model', () => {
    expect(submitWith('/m').bridge.sentOfType('openModelPicker')).toHaveLength(1);
  });
});

describe('composer reacts to host panels', () => {
  it('reflects a catalog that arrives after mount', () => {
    const mounted = mountWebview([makeShellSession({ panels: EMPTY_PANELS })]);

    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      commandCatalog: {
        commands: [{ name: 'init', aliases: [], description: 'Initialize', params: '' }],
      },
    });
    fireEvent.change(inputOf(mounted.container), { target: { value: '/' } });

    expect(mounted.container.textContent).toContain('/init');
  });
});
