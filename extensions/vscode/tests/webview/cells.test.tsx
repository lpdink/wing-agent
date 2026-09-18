import { fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import type { CellModel } from '../../src/shared';
import {
  FIXTURE_EPOCH,
  makeApprovalAskCell,
  makeFailedToolCell,
  makeFixtureSession,
  makeStreamingCells,
  makeStreamingToolCell,
} from '../../src/testing/fixtures';
import { cellElement, disposeMounted, mountWebview, pushPatch } from './harness';

/**
 * Cell rendering: every kind in its main states, the collapse rules and the
 * `ask` / approval forms. Values asserted here are structural (what is on
 * screen), not pixel values — the visual constants are traced in tokens.css.
 */

afterEach(() => {
  disposeMounted();
});

/** Mount a session that contains exactly these cells. */
function mountCells(cells: readonly CellModel[], sessionId = 'session-a') {
  return mountWebview([makeFixtureSession({ sessionId, cells })]);
}

describe('user cell', () => {
  it('renders the message in a right-aligned bubble', () => {
    const { container } = mountCells([
      { kind: 'user', id: 'u1', createdAt: FIXTURE_EPOCH, text: 'hello **there**', state: 'accepted' },
    ]);

    const cell = cellElement(container, 'u1');
    expect(cell.getAttribute('data-cell-state')).toBe('accepted');
    expect(cell.textContent).toContain('hello there');
    // Markdown, not raw text.
    expect(cell.querySelector('strong')?.textContent).toBe('there');
  });

  it('marks pending and discarded messages and shows a sending marker while pending', () => {
    const { container } = mountCells([
      { kind: 'user', id: 'u-pending', createdAt: FIXTURE_EPOCH, text: 'queued', state: 'pending' },
      { kind: 'user', id: 'u-dropped', createdAt: FIXTURE_EPOCH, text: 'dropped', state: 'discarded' },
    ]);

    expect(within(cellElement(container, 'u-pending')).getByTestId('stream-caret')).toBeInTheDocument();
    expect(cellElement(container, 'u-dropped').getAttribute('data-cell-state')).toBe('discarded');
    expect(within(cellElement(container, 'u-dropped')).queryByTestId('stream-caret')).toBeNull();
  });
});

describe('assistant cell', () => {
  it('shows a caret while streaming and drops it once the answer completes', () => {
    const mounted = mountCells([
      { kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: 'partial', streaming: true },
    ]);
    const { container } = mounted;

    expect(within(cellElement(container, 'a1')).getByTestId('stream-caret')).toBeInTheDocument();

    pushPatch(mounted, 'session-a', 1, [
      {
        op: 'update',
        cell: { kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: 'done', streaming: false },
      },
    ]);

    const cell = cellElement(container, 'a1');
    expect(cell.getAttribute('data-streaming')).toBe('false');
    expect(within(cell).queryByTestId('stream-caret')).toBeNull();
  });

  it('appends streamed text into the growing cell without touching earlier cells', () => {
    const mounted = mountCells([
      { kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: 'first', streaming: false },
      { kind: 'assistant', id: 'a2', createdAt: FIXTURE_EPOCH, text: '', streaming: true },
    ]);
    const { container } = mounted;
    const firstParagraph = container.querySelector('[data-cell-id="a1"] p');

    pushPatch(mounted, 'session-a', 1, [{ op: 'append_text', cellId: 'a2', text: 'second' }]);

    expect(cellElement(container, 'a2').textContent).toContain('second');
    expect(container.querySelector('[data-cell-id="a1"] p')).toBe(firstParagraph);
  });
});

describe('thinking cell', () => {
  it('streams expanded, collapses when the answer starts, and reopens on click', () => {
    const mounted = mountCells(makeStreamingCells());
    const { container } = mounted;

    const streaming = cellElement(container, 'thinking-live');
    expect(streaming.getAttribute('data-collapsed')).toBe('false');
    expect(streaming.getAttribute('data-streaming')).toBe('true');
    expect(streaming.textContent).toContain('Thinking');

    pushPatch(mounted, 'session-a', 1, [
      {
        op: 'update',
        cell: {
          kind: 'thinking',
          id: 'thinking-live',
          createdAt: FIXTURE_EPOCH,
          streaming: false,
          durationMs: 1200,
          text: 'Cells are memoized; only the streaming tail re-parses.',
        },
      },
    ]);

    const collapsed = cellElement(container, 'thinking-live');
    expect(collapsed.getAttribute('data-collapsed')).toBe('true');
    expect(collapsed.textContent).toContain('Thought for 1.2s');
    expect(collapsed.textContent).not.toContain('only the streaming tail');

    fireEvent.click(within(collapsed).getByRole('button'));

    const expanded = cellElement(container, 'thinking-live');
    expect(expanded.getAttribute('data-collapsed')).toBe('false');
    expect(expanded.textContent).toContain('only the streaming tail');
  });

  it('keeps a user-expanded thought open across later updates', () => {
    const mounted = mountCells([
      {
        kind: 'thinking',
        id: 't1',
        createdAt: FIXTURE_EPOCH,
        text: 'reasoning',
        streaming: false,
        durationMs: 500,
      },
    ]);
    const { container } = mounted;

    const cell = cellElement(container, 't1');
    expect(cell.getAttribute('data-collapsed')).toBe('true');

    fireEvent.click(within(cell).getByRole('button'));

    expect(cellElement(container, 't1').getAttribute('data-collapsed')).toBe('false');
    // Simulate the host patching the same cell again (streaming stays false).
    pushPatch(mounted, 'session-a', 1, [
      {
        op: 'update',
        cell: {
          kind: 'thinking',
          id: 't1',
          createdAt: FIXTURE_EPOCH,
          text: 'reasoning+',
          streaming: false,
          durationMs: 600,
        },
      },
    ]);

    expect(cellElement(container, 't1').getAttribute('data-collapsed')).toBe('false');
  });
});

describe('tool call cell', () => {
  it('renders one compact row and reveals arguments + result on click', () => {
    const { container } = mountCells([makeFailedToolCell('tool-ok')]);

    const cell = cellElement(container, 'tool-ok');
    expect(cell.getAttribute('data-cell-status')).toBe('failed');
    // A failed call opens by itself so the error is visible.
    expect(cell.getAttribute('data-collapsed')).toBe('false');
    expect(cell.textContent).toContain('error TS2345');
    expect(cell.querySelectorAll('[data-card]')).toHaveLength(2);
  });

  it('collapses a successful call to a single row', () => {
    const { container } = mountCells([
      {
        kind: 'tool_call',
        id: 'tool-ok',
        createdAt: FIXTURE_EPOCH,
        toolCallId: 'call-ok',
        name: 'Bash',
        status: 'success',
        display: { title: 'Bash', subject: 'pnpm test' },
        argsText: '{"command":"pnpm test"}',
        args: { command: 'pnpm test' },
        result: { text: '12 passed', isError: false, truncated: false },
        startedAt: FIXTURE_EPOCH,
        finishedAt: FIXTURE_EPOCH + 10,
      },
    ]);

    const cell = cellElement(container, 'tool-ok');
    expect(cell.getAttribute('data-collapsed')).toBe('true');
    expect(cell.textContent).toContain('Bash');
    expect(cell.textContent).toContain('pnpm test');
    expect(cell.querySelector('[data-card]')).toBeNull();

    fireEvent.click(within(cell).getByRole('button', { name: /Bash/ }));

    const expanded = cellElement(container, 'tool-ok');
    expect(expanded.textContent).toContain('"command": "pnpm test"');
    expect(expanded.textContent).toContain('12 passed');
  });

  it('keeps streaming arguments visible even before they parse', () => {
    const { container } = mountCells([makeStreamingToolCell('tool-live')]);

    const cell = cellElement(container, 'tool-live');
    expect(cell.getAttribute('data-cell-status')).toBe('streaming');
    // Streaming calls stay collapsed (the model is still writing the call).
    expect(cell.getAttribute('data-collapsed')).toBe('true');
  });
});

describe('diff cell', () => {
  it('shows the path, the counters and the diff rows', () => {
    const { container } = mountCells([...makeFixtureSession().cells].filter((cell) => cell.kind === 'diff'));

    const cell = cellElement(container, 'diff-1');
    expect(cell.textContent).toContain('src/webview/state/store.ts');
    expect(cell.textContent).toContain('+1');
    expect(cell.textContent).toContain('−1');
    expect(cell.querySelectorAll('[data-diff-kind="add"]')).toHaveLength(1);
    expect(cell.querySelectorAll('[data-diff-kind="del"]')).toHaveLength(1);
    expect(cell.querySelectorAll('[data-diff-kind="hunk"]')).toHaveLength(1);
  });

  it('asks the host to open the native diff', () => {
    const { container, bridge } = mountCells(
      [...makeFixtureSession().cells].filter((cell) => cell.kind === 'diff'),
    );

    fireEvent.click(within(cellElement(container, 'diff-1')).getByRole('button', { name: 'Open diff' }));

    expect(bridge.sentOfType('openDiff')).toEqual([
      { type: 'openDiff', sessionId: 'session-a', cellId: 'diff-1' },
    ]);
  });
});

describe('todo cell', () => {
  it('renders every item with its status', () => {
    const { container } = mountCells([...makeFixtureSession().cells].filter((cell) => cell.kind === 'todo'));

    const cell = cellElement(container, 'todo-1');
    const items = [...cell.querySelectorAll('[data-todo-status]')];
    expect(items.map((item) => item.getAttribute('data-todo-status'))).toEqual([
      'completed',
      'in_progress',
      'pending',
    ]);
    expect(cell.textContent).toContain('Render streamed cells');
  });
});

describe('ask cell', () => {
  it('answers a question with the selected option', () => {
    const { container, bridge } = mountCells(
      [...makeFixtureSession().cells].filter((cell) => cell.kind === 'ask'),
    );

    const cell = cellElement(container, 'ask-1');
    expect(cell.getAttribute('data-ask-form')).toBe('question');

    const submit = within(cell).getByRole('button', { name: 'Submit' });
    expect(submit).toBeDisabled();

    fireEvent.click(within(cell).getAllByRole('radio')[0] as HTMLElement);
    expect(submit).toBeEnabled();

    fireEvent.click(submit);

    expect(bridge.sentOfType('answerAsk')).toEqual([
      {
        type: 'answerAsk',
        sessionId: 'session-a',
        requestId: 'ask-request-1',
        answers: [{ questionId: 'q1', selected: ['Memoized cells'], text: '' }],
      },
    ]);
  });

  it('renders the approval shape as approve/deny', () => {
    const { container, bridge } = mountCells([makeApprovalAskCell('ask-approval')]);

    const cell = cellElement(container, 'ask-approval');
    expect(cell.getAttribute('data-ask-form')).toBe('approval');
    expect(cell.textContent).toContain('Run `rm -rf build`?');

    fireEvent.click(within(cell).getByRole('button', { name: 'Deny' }));

    expect(bridge.sentOfType('approveTool')).toEqual([
      { type: 'approveTool', sessionId: 'session-a', requestId: 'approval-1', decision: 'deny' },
    ]);
  });

  it('becomes read-only once answered', () => {
    const answered = { ...makeApprovalAskCell('ask-approval'), state: 'answered' as const };
    const { container } = mountCells([answered]);

    const cell = cellElement(container, 'ask-approval');
    expect(within(cell).getByRole('button', { name: 'Approve' })).toBeDisabled();
    expect(within(cell).getByRole('button', { name: 'Deny' })).toBeDisabled();
  });
});

describe('metrics, system and separator', () => {
  it('renders one metrics line with tokens, cache and latency', () => {
    const { container } = mountCells(
      [...makeFixtureSession().cells].filter((cell) => cell.kind === 'metrics'),
    );

    const text = cellElement(container, 'metrics-1').textContent ?? '';
    expect(text).toContain('2.0k in');
    expect(text).toContain('512 out');
    expect(text).toContain('75% cache');
    expect(text).toContain('288ms TTFT');
    expect(text).toContain('5.1s');
  });

  it('marks the system level on the cell', () => {
    const { container } = mountCells(
      [...makeFixtureSession().cells].filter((cell) => cell.kind === 'system'),
    );

    const cell = cellElement(container, 'system-1');
    expect(cell.getAttribute('data-cell-level')).toBe('warning');
    expect(cell.textContent).toContain('Gateway connection dropped');
  });

  it('renders a labelled separator and a bare one', () => {
    const { container } = mountCells([
      { kind: 'separator', id: 'sep-a', createdAt: FIXTURE_EPOCH, label: 'Turn 3' },
      { kind: 'separator', id: 'sep-b', createdAt: FIXTURE_EPOCH, label: '' },
    ]);

    expect(cellElement(container, 'sep-a').textContent).toBe('Turn 3');
    expect(cellElement(container, 'sep-b').textContent).toBe('');
  });
});
