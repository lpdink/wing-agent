import { fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { EMPTY_PANELS } from '../../src/shared';
import {
  makeBranchCatalog,
  makeEmptySession,
  makeModelPickerSession,
  makeSessionCatalog,
  makeShellSession,
} from '../../src/testing/fixtures';
import { disposeMounted, mountWebview, pushPanels, pushTabs, pushUi, twoTabs } from './harness';

/**
 * The four panels (step 05): model, sessions, branches and the command candidates
 * (the last one lives in `composer.test.tsx`).
 *
 * Two ownership rules are asserted here, because they are easy to get wrong:
 * - the model picker is **host-owned**: it appears/disappears with `panels.modelPicker`
 *   and the webview only ever asks the host to close it;
 * - the session and branch panels are opened locally (the composer drives them) but
 *   their **content** always comes from host data.
 */

afterEach(() => {
  disposeMounted();
});

function openHistory(mounted: { container: HTMLElement }): void {
  fireEvent.click(within(mounted.container).getByTestId('history-button'));
}

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

describe('model panel (host-owned)', () => {
  it('is not rendered until the host opens it', () => {
    const { container } = mountWebview([makeShellSession()]);

    expect(within(container).queryByTestId('model-panel')).toBeNull();
  });

  it('groups rows by provider and marks the current model', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });

    const panel = within(mounted.container).getByTestId('model-panel');
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
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });

    fireEvent.click(rowByText(mounted.container, 'gpt-5'));

    expect(mounted.bridge.sentOfType('setModel')).toEqual([
      { type: 'setModel', sessionId: 'session-a', provider: 'openai', model: 'gpt-5' },
    ]);
  });

  it('toggles thinking and sets effort from the reasoning section', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });

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
    const mounted = mountWebview([
      makeShellSession({ meta: { ...makeShellSession().meta, thinking: false } }),
    ]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });

    expect(within(mounted.container).queryAllByTestId('effort-row')).toHaveLength(0);
  });

  it('starts on the row the host asked for', () => {
    const picker = makeModelPickerSession().panels.modelPicker;
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: { ...picker!, activeIndex: 2 },
    });

    const selected = within(mounted.container)
      .getAllByRole('option')
      .filter((option) => option.getAttribute('aria-selected') === 'true');
    expect(selected).toHaveLength(1);
    expect(selected[0]).toHaveTextContent('gpt-5');
  });

  it('moves the highlight over group headers without landing on them', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });
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

  it('folds its keyboard into Escape → closeOverlays (the host owns the overlay)', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });

    fireEvent.keyDown(within(mounted.container).getByTestId('model-panel-list'), { key: 'Escape' });

    expect(mounted.bridge.sentOfType('closeOverlays')).toEqual([{ type: 'closeOverlays' }]);
  });

  it('closes (locally) when the host pushes an empty picker', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });
    expect(within(mounted.container).getByTestId('model-panel')).toBeInTheDocument();

    pushPanels(mounted, 'session-a', EMPTY_PANELS);

    expect(within(mounted.container).queryByTestId('model-panel')).toBeNull();
  });

  it('focuses its list so arrows work without a click', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      modelPicker: makeModelPickerSession().panels.modelPicker,
    });

    expect(within(mounted.container).getByTestId('model-panel-list')).toHaveFocus();
  });
});

describe('session panel', () => {
  it('lists the host catalog with status, workspace and the current marker', () => {
    const mounted = mountWebview([makeShellSession()]);
    openHistory(mounted);

    const panel = within(mounted.container).getByTestId('session-panel');
    const rows = within(panel).getAllByTestId('session-row');
    expect(rows).toHaveLength(3);
    expect(rows[0]).toHaveAttribute('data-current', 'true');
    expect(rows[0]).toHaveTextContent('Fixture session');
    expect(rows[0]).toHaveTextContent('/workspace');
    expect(within(panel).getByTestId('session-panel-list')).toHaveAttribute('data-source', 'catalog');
    // Status dots key off the raw gateway status.
    expect(panel.querySelector('[data-status="waiting-for-input"]')).not.toBeNull();
    expect(panel.textContent).toContain('Not loaded');
  });

  it('activates an open session and closes', () => {
    // No catalog: the panel falls back to the open tabs, where "New session" is not
    // the active one.
    const mounted = mountWebview([makeShellSession({ panels: EMPTY_PANELS }), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');
    openHistory(mounted);

    fireEvent.click(rowByText(mounted.container, 'New session'));

    expect(mounted.bridge.sentOfType('activateSession')).toEqual([
      { type: 'activateSession', sessionId: 'session-b' },
    ]);
    expect(within(mounted.container).queryByTestId('session-panel')).toBeNull();
  });

  it('asks the host to resume a session that is not open as a tab', () => {
    const mounted = mountWebview([makeShellSession()]);
    openHistory(mounted);

    fireEvent.click(rowByText(mounted.container, 'Yesterday’s session'));

    expect(mounted.bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/ss', argsText: 'session-c' },
    ]);
  });

  it('falls back to the open tabs while the catalog is missing', () => {
    const mounted = mountWebview([makeShellSession({ panels: EMPTY_PANELS }), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');
    openHistory(mounted);

    const panel = within(mounted.container).getByTestId('session-panel');
    expect(within(panel).getByTestId('session-panel-list')).toHaveAttribute('data-source', 'tabs');
    const rows = within(panel).getAllByTestId('session-row');
    expect(rows).toHaveLength(2);
    expect(rows[1]).toHaveTextContent('New session');
  });

  it('has an empty state when there is nothing to list', () => {
    const mounted = mountWebview([makeEmptySession('session-a')]);
    pushPanels(mounted, 'session-a', { ...EMPTY_PANELS, sessionCatalog: { sessions: [] } });
    pushTabs(mounted, [], 'session-a');
    openHistory(mounted);

    expect(within(mounted.container).getByTestId('panel-empty')).toHaveTextContent('No sessions yet.');
  });

  it('navigates with the keyboard and opens with Enter', () => {
    const mounted = mountWebview([makeShellSession(), makeEmptySession('session-b')]);
    pushTabs(mounted, twoTabs(), 'session-a');
    openHistory(mounted);

    const list = within(mounted.container).getByTestId('session-panel-list');
    expect(list).toHaveFocus();
    fireEvent.keyDown(list, { key: 'ArrowDown' });
    fireEvent.keyDown(list, { key: 'Enter' });

    expect(mounted.bridge.sentOfType('activateSession')).toEqual([
      { type: 'activateSession', sessionId: 'session-b' },
    ]);
  });

  it('closes on Escape', () => {
    const mounted = mountWebview([makeShellSession()]);
    openHistory(mounted);

    fireEvent.keyDown(within(mounted.container).getByTestId('session-panel-list'), { key: 'Escape' });

    expect(within(mounted.container).queryByTestId('session-panel')).toBeNull();
  });

  it('closes when the backdrop is clicked', () => {
    const mounted = mountWebview([makeShellSession()]);
    openHistory(mounted);

    fireEvent.mouseDown(within(mounted.container).getByTestId('session-panel-backdrop'));

    expect(within(mounted.container).queryByTestId('session-panel')).toBeNull();
  });

  it('gives focus back to the composer when it closes', () => {
    const mounted = mountWebview([makeShellSession()]);
    openHistory(mounted);
    fireEvent.keyDown(within(mounted.container).getByTestId('session-panel-list'), { key: 'Escape' });

    expect(within(mounted.container).getByTestId('composer-input')).toHaveFocus();
  });

  it('closes when the host asks for it through the ui channel', () => {
    const mounted = mountWebview([makeShellSession()]);
    openHistory(mounted);

    pushUi(mounted, { kind: 'closeOverlays' });

    expect(within(mounted.container).queryByTestId('session-panel')).toBeNull();
  });

  it('lists sessions pushed after the panel was opened', () => {
    const mounted = mountWebview([makeShellSession({ panels: EMPTY_PANELS })]);
    openHistory(mounted);
    expect(within(mounted.container).getAllByTestId('session-row')).toHaveLength(1);

    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      sessionCatalog: makeSessionCatalog(),
    });

    expect(within(mounted.container).getAllByTestId('session-row')).toHaveLength(3);
  });
});

describe('branch panel', () => {
  it('shows the current point as a marked, non-selectable row', () => {
    const mounted = mountWebview([makeShellSession()]);
    fireEvent.change(within(mounted.container).getByTestId('composer-input'), {
      target: { value: '/rewind' },
    });
    fireEvent.keyDown(within(mounted.container).getByTestId('composer-input'), { key: 'Enter' });

    const panel = within(mounted.container).getByTestId('branch-panel');
    const current = within(panel).getByTestId('branch-current-row');
    expect(current).toHaveAttribute('data-current', 'true');
    expect(current).toHaveAttribute('aria-disabled', 'true');
    expect(current).toHaveTextContent('current');
    expect(within(panel).getAllByTestId('branch-row')).toHaveLength(3);
    expect(panel).toHaveAttribute('aria-label', 'Rewind to message');
  });

  it('starts on the newest target and applies it with Enter', () => {
    const mounted = mountWebview([makeShellSession()]);
    fireEvent.change(within(mounted.container).getByTestId('composer-input'), {
      target: { value: '/rewind' },
    });
    fireEvent.keyDown(within(mounted.container).getByTestId('composer-input'), { key: 'Enter' });

    const list = within(mounted.container).getByTestId('branch-panel-list');
    const highlighted = within(mounted.container)
      .getAllByRole('option')
      .filter((option) => option.getAttribute('aria-selected') === 'true');
    expect(highlighted).toHaveLength(1);
    expect(highlighted[0]).toHaveTextContent('Now wire the composer.');

    fireEvent.keyDown(list, { key: 'Enter' });

    expect(mounted.bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/rewind', argsText: 'uuid-0003-last' },
    ]);
    expect(within(mounted.container).queryByTestId('branch-panel')).toBeNull();
  });

  it('forks instead of rewinding when opened through /fork', () => {
    const mounted = mountWebview([makeShellSession()]);
    const input = within(mounted.container).getByTestId('composer-input');
    fireEvent.change(input, { target: { value: '/fork' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    expect(within(mounted.container).getByTestId('branch-panel-list')).toHaveAttribute('data-mode', 'fork');
    fireEvent.click(rowByText(mounted.container, 'Refactor the session store.'));

    expect(mounted.bridge.sentOfType('runPromptCommand')).toEqual([
      { type: 'runPromptCommand', sessionId: 'session-a', name: '/fork', argsText: 'uuid-0001-first' },
    ]);
  });

  it('does not apply the current point when it is clicked', () => {
    const mounted = mountWebview([makeShellSession()]);
    const input = within(mounted.container).getByTestId('composer-input');
    fireEvent.change(input, { target: { value: '/rewind' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    fireEvent.click(within(mounted.container).getByTestId('branch-current-row'));

    expect(mounted.bridge.sentOfType('runPromptCommand')).toHaveLength(0);
  });

  it('explains itself when the host has no branch targets', () => {
    const mounted = mountWebview([makeShellSession({ panels: EMPTY_PANELS })]);
    const input = within(mounted.container).getByTestId('composer-input');
    fireEvent.change(input, { target: { value: '/rewind' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    expect(within(mounted.container).getByTestId('panel-empty')).toHaveTextContent(
      'Branch targets are not available yet.',
    );
  });

  it('ignores a catalog that belongs to another session', () => {
    const mounted = mountWebview([makeShellSession()]);
    pushPanels(mounted, 'session-a', {
      ...EMPTY_PANELS,
      branchCatalog: { ...makeBranchCatalog('session-other') },
    });
    const input = within(mounted.container).getByTestId('composer-input');
    fireEvent.change(input, { target: { value: '/rewind' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    expect(within(mounted.container).getByTestId('panel-empty')).toBeInTheDocument();
  });
});

describe('panel accessibility', () => {
  it('exposes a dialog with a listbox and its highlighted option', () => {
    const mounted = mountWebview([makeShellSession()]);
    openHistory(mounted);

    const dialog = within(mounted.container).getByRole('dialog');
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
