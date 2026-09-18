import { fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { EMPTY_PANELS } from '../../src/shared';
import {
  makeBranchPicker,
  makeCommandCatalog,
  makeModelPicker,
  makeModelPickerSession,
  makeSessionPicker,
  makeShellSession,
} from '../../src/testing/fixtures';
import { disposeMounted, mountWebview, pushPanels, pushUi } from './harness';

/**
 * The four panels (step 05), as frozen by `interfaces.md`.
 *
 * The rule under test everywhere here: **the host owns the overlays**. A panel is on
 * screen exactly while its `panels.*` member is non-null, and closing one is a request
 * (`closeOverlays`) — never a local decision. The command candidates (the fourth
 * panel) live in `composer.test.tsx`.
 */

afterEach(() => {
  disposeMounted();
});

function listbox(container: HTMLElement): HTMLElement {
  return within(container).getByRole('listbox');
}

function rowByText(container: HTMLElement, text: string): HTMLElement {
  const row = within(container)
    .getAllByRole('option')
    .find((option) => option.textContent?.includes(text));
  if (row === undefined) {
    throw new Error(`no option containing ${text}`);
  }
  return row;
}

/** A session whose `panels` carry the catalog plus the given picker (host-opened). */
function withPicker(
  picker: Partial<
    Pick<ReturnType<typeof makeShellSession>['panels'], 'modelPicker' | 'sessionPicker' | 'branchPicker'>
  >,
): ReturnType<typeof makeShellSession> {
  return makeShellSession({ panels: { ...EMPTY_PANELS, commandCatalog: makeCommandCatalog(), ...picker } });
}

describe('model panel (host-owned)', () => {
  it('is not rendered until the host opens it', () => {
    const { container } = mountWebview([makeShellSession()]);

    expect(within(container).queryByTestId('model-panel')).toBeNull();
  });

  it('groups rows by provider and marks the current model', () => {
    const { container } = mountWebview([makeModelPickerSession()]);
    const panel = within(container).getByTestId('model-panel');

    expect(panel).toHaveAttribute('role', 'dialog');
    expect(panel).toHaveAttribute('aria-label', 'Model and reasoning');
    expect(panel).toHaveAttribute('aria-modal', 'true');
    const groups = within(panel)
      .getAllByTestId('model-panel-group')
      .map((node) => node.textContent);
    expect(groups).toEqual(['anthropic', 'openai']);
    expect(within(panel).getByText('claude-sonnet-4').closest('[data-testid="model-row"]')).toHaveAttribute(
      'data-current',
      'true',
    );
  });

  it('applies a model on click', () => {
    const mounted = mountWebview([makeModelPickerSession()]);

    fireEvent.click(rowByText(mounted.container, 'gpt-5'));

    expect(mounted.bridge.sentOfType('setModel')).toEqual([
      { type: 'setModel', sessionId: 'session-a', provider: 'openai', model: 'gpt-5' },
    ]);
  });

  it('toggles thinking and sets effort from the reasoning section', () => {
    const mounted = mountWebview([makeModelPickerSession()]);

    fireEvent.click(within(mounted.container).getByTestId('thinking-row'));
    expect(mounted.bridge.sentOfType('setThinking')).toEqual([
      { type: 'setThinking', sessionId: 'session-a', enabled: false },
    ]);

    fireEvent.click(rowByText(mounted.container, 'xhigh'));
    expect(mounted.bridge.sentOfType('setEffort')).toEqual([
      { type: 'setEffort', sessionId: 'session-a', effort: 'xhigh' },
    ]);
  });

  it('hides the effort rows while thinking is off', () => {
    const session = makeModelPickerSession();
    const mounted = mountWebview([{ ...session, meta: { ...session.meta, thinking: false } }]);

    expect(within(mounted.container).queryAllByTestId('effort-row')).toHaveLength(0);
  });

  it('says so when the host has no models, and keeps the reasoning controls', () => {
    const session = makeShellSession();
    const mounted = mountWebview([withPicker({ modelPicker: { ...makeModelPicker(), rows: [] } })]);
    expect(session.panels.modelPicker).toBeNull(); // the base fixture has none open

    expect(within(mounted.container).getByTestId('model-panel-empty')).toHaveTextContent(
      'No models available.',
    );
    expect(within(mounted.container).getByTestId('thinking-row')).toBeInTheDocument();
  });

  it('starts on the row the host asked for', () => {
    const mounted = mountWebview([withPicker({ modelPicker: { ...makeModelPicker(), activeIndex: 2 } })]);

    const selected = within(mounted.container)
      .getAllByRole('option')
      .filter((option) => option.getAttribute('aria-selected') === 'true');
    expect(selected).toHaveLength(1);
    expect(selected[0]).toHaveTextContent('gpt-5');
  });

  it('moves the highlight over group headers without landing on them', () => {
    const mounted = mountWebview([makeModelPickerSession()]);
    const input = within(mounted.container).getByTestId('model-panel-list');

    // Row order: group(anthropic) · sonnet(current, highlighted) · opus · group(openai) · gpt-5 · …
    fireEvent.keyDown(input, { key: 'ArrowDown' });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(mounted.bridge.sentOfType('setModel')[0]).toMatchObject({ model: 'claude-opus-4' });

    // From opus, one more step must jump over the `openai` group header onto gpt-5.
    fireEvent.keyDown(input, { key: 'ArrowDown' });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(mounted.bridge.sentOfType('setModel')[1]).toMatchObject({ model: 'gpt-5' });
  });

  it('asks the host to close on Escape (the host owns the overlay)', () => {
    const mounted = mountWebview([makeModelPickerSession()]);

    fireEvent.keyDown(within(mounted.container).getByTestId('model-panel-list'), { key: 'Escape' });

    expect(mounted.bridge.sentOfType('closeOverlays')).toEqual([{ type: 'closeOverlays' }]);
  });

  it('keeps Escape working when the focus is on the panel chrome (not the list)', () => {
    const mounted = mountWebview([makeModelPickerSession()]);

    within(mounted.container).getByTestId('model-panel-close').focus();
    fireEvent.keyDown(within(mounted.container).getByTestId('model-panel-close'), { key: 'Escape' });

    expect(mounted.bridge.sentOfType('closeOverlays')).toEqual([{ type: 'closeOverlays' }]);
  });

  it('closes (locally) when the host pushes an empty picker', () => {
    const session = makeModelPickerSession();
    const mounted = mountWebview([session]);
    expect(within(mounted.container).getByTestId('model-panel')).toBeInTheDocument();

    pushPanels(mounted, 'session-a', EMPTY_PANELS);

    expect(within(mounted.container).queryByTestId('model-panel')).toBeNull();
  });

  it('returns the focus to the composer when the host closes it', () => {
    const mounted = mountWebview([makeModelPickerSession()]);
    expect(within(mounted.container).getByTestId('model-panel-list')).toHaveFocus();

    // The host applies `setModel` and closes the picker itself (03 does the same).
    pushPanels(mounted, 'session-a', EMPTY_PANELS);

    expect(within(mounted.container).getByTestId('composer-input')).toHaveFocus();
  });

  it('focuses its list so arrows work without a click', () => {
    const mounted = mountWebview([makeModelPickerSession()]);

    expect(within(mounted.container).getByTestId('model-panel-list')).toHaveFocus();
  });
});

describe('session picker (host-owned)', () => {
  it('is not rendered until the host opens it', () => {
    const { container } = mountWebview([makeShellSession()]);

    expect(within(container).queryByTestId('session-panel')).toBeNull();
  });

  it('renders the rows the host sent, with status, workspace and the current marker', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);

    const panel = within(mounted.container).getByTestId('session-panel');
    const rows = within(panel).getAllByTestId('session-row');
    expect(rows).toHaveLength(3);
    expect(rows[0]).toHaveAttribute('data-current', 'true');
    expect(rows[0]).toHaveTextContent('Fixture session');
    expect(rows[0]).toHaveTextContent('/workspace');
    expect(rows[2]).toHaveTextContent('Yesterday’s session');
    expect(rows[2]).toHaveTextContent('No workspace'); // workspace: null
    expect(panel.querySelector('[data-status="waiting-for-input"]')).not.toBeNull();
    expect(panel.querySelector('[data-status="inactive"]')).not.toBeNull();
    expect(panel.textContent).toContain('Not loaded');
  });

  it('starts on the current session', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);
    const selected = within(mounted.container)
      .getAllByRole('option')
      .filter((option) => option.getAttribute('aria-selected') === 'true');

    expect(selected).toHaveLength(1);
    expect(selected[0]).toHaveTextContent('Fixture session');
  });

  it('starts at the top when the host marks no current row', () => {
    const picker = makeSessionPicker();
    const rows = picker.rows.map((row) => ({ ...row, current: false }));
    const mounted = mountWebview([withPicker({ sessionPicker: { rows } })]);
    const selected = within(mounted.container)
      .getAllByRole('option')
      .filter((option) => option.getAttribute('aria-selected') === 'true');

    expect(selected[0]).toHaveTextContent('Fixture session');
  });

  it('resumes the picked session through the host (/ss <id>) and asks to close', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);

    fireEvent.click(rowByText(mounted.container, 'Yesterday’s session'));

    expect(mounted.bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/ss', argsText: 'session-c' },
    ]);
    expect(mounted.bridge.sentOfType('closeOverlays')).toEqual([{ type: 'closeOverlays' }]);
    expect(mounted.bridge.sentOfType('activateSession')).toHaveLength(0);
  });

  it('opens the highlighted row with the keyboard', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);
    const list = within(mounted.container).getByTestId('session-panel-list');

    expect(list).toHaveFocus();
    fireEvent.keyDown(list, { key: 'ArrowDown' });
    fireEvent.keyDown(list, { key: 'Enter' });

    expect(mounted.bridge.sentOfType('runPromptCommand')[0]).toMatchObject({
      name: '/ss',
      argsText: 'session-b',
    });
  });

  it('falls back to the id when the host sends an empty title', () => {
    const picker = makeSessionPicker();
    const mounted = mountWebview([
      withPicker({ sessionPicker: { rows: [{ ...picker.rows[1]!, title: '' }] } }),
    ]);

    expect(within(mounted.container).getByTestId('session-row')).toHaveTextContent('session-b');
  });

  it('has an empty state when the host sends no rows', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: { rows: [] } })]);

    expect(within(mounted.container).getByTestId('panel-empty')).toHaveTextContent('No sessions yet.');
  });

  it('asks the host to close on Escape, the close button and the backdrop', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);

    fireEvent.keyDown(within(mounted.container).getByTestId('session-panel-list'), { key: 'Escape' });
    fireEvent.click(within(mounted.container).getByTestId('session-panel-close'));
    fireEvent.mouseDown(within(mounted.container).getByTestId('session-panel-backdrop'));

    expect(mounted.bridge.sentOfType('closeOverlays')).toHaveLength(3);
  });

  it('stays up until the host clears the picker', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);

    fireEvent.keyDown(within(mounted.container).getByTestId('session-panel-list'), { key: 'Escape' });

    // The webview asked; only the host decides. (The mock host does not answer here.)
    expect(within(mounted.container).getByTestId('session-panel')).toBeInTheDocument();
  });

  it('closes when the host clears the picker, and hands back the focus', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);

    pushPanels(mounted, 'session-a', { ...EMPTY_PANELS, commandCatalog: makeCommandCatalog() });

    expect(within(mounted.container).queryByTestId('session-panel')).toBeNull();
    expect(within(mounted.container).getByTestId('composer-input')).toHaveFocus();
  });

  it('closes when the host asks through the ui channel', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);

    pushUi(mounted, { kind: 'closeOverlays' });
    pushPanels(mounted, 'session-a', { ...EMPTY_PANELS, commandCatalog: makeCommandCatalog() });

    expect(within(mounted.container).queryByTestId('session-panel')).toBeNull();
  });
});

describe('branch picker (host-owned)', () => {
  it('renders the mode the host opened it for, with a current-point row', () => {
    const mounted = mountWebview([withPicker({ branchPicker: makeBranchPicker('fork') })]);
    const panel = within(mounted.container).getByTestId('branch-panel');

    expect(panel).toHaveAttribute('aria-label', 'Fork from message');
    expect(within(panel).getByTestId('branch-panel-list')).toHaveAttribute('data-mode', 'fork');
    const current = within(panel).getByTestId('branch-current-row');
    expect(current).toHaveAttribute('data-current', 'true');
    expect(current).toHaveAttribute('aria-disabled', 'true');
    expect(current).toHaveTextContent('current');
    expect(within(panel).getAllByTestId('branch-row')).toHaveLength(3);
  });

  it('highlights the first target (the sentinel is not a target)', () => {
    const mounted = mountWebview([withPicker({ branchPicker: makeBranchPicker('rewind') })]);
    const selected = within(mounted.container)
      .getAllByRole('option')
      .filter((option) => option.getAttribute('aria-selected') === 'true');

    expect(selected).toHaveLength(1);
    expect(selected[0]).toHaveTextContent('Refactor the session store.');
  });

  it('applies the highlighted target with Enter', () => {
    const mounted = mountWebview([withPicker({ branchPicker: makeBranchPicker('rewind') })]);
    const list = within(mounted.container).getByTestId('branch-panel-list');

    fireEvent.keyDown(list, { key: 'Enter' });

    expect(mounted.bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/rewind', argsText: 'uuid-0001-first' },
    ]);
    expect(mounted.bridge.sentOfType('closeOverlays')).toEqual([{ type: 'closeOverlays' }]);
  });

  it('forks with the mode the host chose', () => {
    const mounted = mountWebview([withPicker({ branchPicker: makeBranchPicker('fork') })]);

    fireEvent.click(rowByText(mounted.container, 'Now wire the composer.'));

    expect(mounted.bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/fork', argsText: 'uuid-0003-last' },
    ]);
  });

  it('never applies the current point', () => {
    const mounted = mountWebview([withPicker({ branchPicker: makeBranchPicker('rewind') })]);

    fireEvent.click(within(mounted.container).getByTestId('branch-current-row'));

    expect(mounted.bridge.sentOfType('runPromptCommand')).toHaveLength(0);
  });

  it('has an empty state when the host sends no rows', () => {
    const mounted = mountWebview([withPicker({ branchPicker: { mode: 'fork', rows: [] } })]);

    expect(within(mounted.container).getByTestId('panel-empty')).toHaveTextContent('No fork points yet.');
  });

  it('asks the host to close on Escape and on the backdrop', () => {
    const mounted = mountWebview([withPicker({ branchPicker: makeBranchPicker('rewind') })]);

    fireEvent.keyDown(within(mounted.container).getByTestId('branch-panel-list'), { key: 'Escape' });
    fireEvent.mouseDown(within(mounted.container).getByTestId('branch-panel-backdrop'));

    expect(mounted.bridge.sentOfType('closeOverlays')).toHaveLength(2);
  });
});

describe('panel accessibility', () => {
  it('exposes a dialog with a listbox and its highlighted option', () => {
    const mounted = mountWebview([withPicker({ sessionPicker: makeSessionPicker() })]);

    const dialog = within(mounted.container).getByTestId('session-panel');
    expect(dialog).toHaveAttribute('role', 'dialog');
    expect(dialog).toHaveAttribute('aria-modal', 'true');
    expect(listbox(mounted.container)).toHaveAttribute('role', 'listbox');
    const active = listbox(mounted.container).getAttribute('aria-activedescendant');
    expect(active).not.toBeNull();
    expect(mounted.container.querySelector(`#${active ?? ''}`)).not.toBeNull();
    // Every selectable row is an option with an id (what aria-activedescendant needs).
    for (const option of within(mounted.container).getAllByRole('option')) {
      expect(option.id).not.toBe('');
    }
  });
});
