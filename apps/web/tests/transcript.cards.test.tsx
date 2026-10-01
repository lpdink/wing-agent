import { fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type {
  AskCellModel,
  AssistantCellModel,
  CellModel,
  DiffCellModel,
  ThinkingCellModel,
  ToolCallCellModel,
} from '@wing-agent/session';
import {
  resetCollapseOverrides,
  setBridgeController,
  type BridgeController,
  type WebviewToHostMessage,
} from '@wing-agent/ui';

import { WebCellView } from '../src/transcript/cells/CellView';

/**
 * The swapped-in cards, asserted where they are wired (step 08b).
 *
 * These tests mount one cell through the app's own row renderer and read the DOM
 * the shared components produce. They are the *wiring* half of the package's own
 * component tests: that a markdown fence reaches the ported `CodeBlock`, that a Bash
 * call becomes a `TerminalBlock` with the session workspace as its prompt, that the
 * diff card folds, and that the ask trio posts exactly the intents the bridge
 * understands. Behaviour the swap must not change (collapse defaults, the answer
 * path, the streaming caret) is asserted end-to-end in `transcript.test.tsx`.
 */

let posted: WebviewToHostMessage[];

beforeEach(() => {
  posted = [];
  resetCollapseOverrides();
  const controller: BridgeController = {
    start: () => undefined,
    ping: () => undefined,
    dispose: () => undefined,
    post: (message) => {
      posted.push(message);
    },
  };
  setBridgeController(controller);
});

afterEach(() => {
  setBridgeController(null);
});

/** Mount one cell the way the transcript does (the app's dispatch, a fake session). */
function mountCell(cell: CellModel, workspace = ''): HTMLElement {
  const { container } = render(<WebCellView cell={cell} sessionId="s-1" workspace={workspace} />);
  const row = container.querySelector<HTMLElement>('[data-cell-kind]');
  if (row === null) {
    throw new Error('the cell did not render a row');
  }
  return row;
}

const EPOCH = 1_700_000_000_000;

function assistant(text: string, streaming = false): AssistantCellModel {
  return { kind: 'assistant', id: 'a-1', createdAt: EPOCH, text, streaming };
}

function toolCall(overrides: Partial<ToolCallCellModel> = {}): ToolCallCellModel {
  return {
    kind: 'tool_call',
    id: 't-1',
    createdAt: EPOCH,
    toolCallId: 'toolu-1',
    name: 'Read',
    status: 'success',
    display: { title: 'Read', subject: 'src/parser.ts' },
    argsText: '{"path": "src/parser.ts"}',
    args: { path: 'src/parser.ts' },
    result: { text: 'export function parse() {}', isError: false, truncated: false },
    startedAt: EPOCH,
    finishedAt: EPOCH + 1_000,
    ...overrides,
  };
}

function diffCell(lines: number): DiffCellModel {
  return {
    kind: 'diff',
    id: 'd-1',
    createdAt: EPOCH,
    path: 'src/parser.ts',
    oldStartLine: 40,
    newStartLine: 40,
    lines: Array.from({ length: lines }, (_value, index) => ({
      kind: index === 0 ? ('hunk' as const) : ('add' as const),
      text: `line ${index}`,
      oldLine: null,
      newLine: 40 + index,
    })),
    added: lines - 1,
    removed: 0,
    truncated: false,
    toolCallId: 'toolu-1',
  };
}

describe('markdown fences', () => {
  it('draws a settled fence with the shared code card', () => {
    const row = mountCell(assistant('Before.\n\n```ts\nconst answer = 42;\n```\n'));

    const card = row.querySelector('[data-code-block-content]');
    expect(card?.textContent).toContain('const answer = 42;');
    // The card's toolbar: language label, the wrap toggle and the copy control.
    expect(screen.getByText('ts')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Wrap lines' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Copy' })).toBeTruthy();
    // Highlighted (jsdom has no IntersectionObserver, so the card activates at once).
    expect(row.querySelector('pre.shiki')).not.toBeNull();
    // The prose around the fence is still the shared node renderer.
    expect(screen.getByText('Before.')).toBeTruthy();
  });

  it('renders a growing fence as plain text, highlighted only once it closes', () => {
    const open = mountCell(assistant('```ts\nconst answer = ', true));
    expect(open.querySelector('pre.shiki')).toBeNull();
    expect(open.querySelector('[data-code-block-content]')?.textContent).toContain('const answer =');
    expect(open.querySelector('[data-testid="stream-caret"]')).not.toBeNull();

    const settled = mountCell(assistant('```ts\nconst answer = 42;\n```\n'));
    expect(settled.querySelector('pre.shiki')).not.toBeNull();
  });

  it('toggles wrapping from the card toolbar', () => {
    const row = mountCell(assistant('```ts\nconst answer = 42;\n```\n'));
    const block = row.querySelector('[data-code-block-content]')?.parentElement;
    expect(block?.getAttribute('data-code-wrap')).toBe('true');

    fireEvent.click(screen.getByRole('button', { name: 'Wrap lines' }));

    expect(block?.getAttribute('data-code-wrap')).toBe('false');
  });
});

describe('tool rows', () => {
  it('renders a Bash call as a terminal card with the workspace as the prompt', () => {
    const row = mountCell(
      toolCall({
        name: 'Bash',
        display: { title: 'Bash', subject: 'pnpm test parser' },
        args: { command: 'pnpm test parser' },
        result: { text: 'FAIL src/parser.test.ts', isError: true, truncated: false },
        status: 'failed',
      }),
      '/Users/dev/projects/wing',
    );

    expect(row.querySelector('[data-terminal]')).not.toBeNull();
    expect(row.textContent).toContain('pnpm test parser');
    // The prompt label is the cwd's last segment (the session workspace).
    expect(row.querySelector('[class*="cwd"]')?.textContent).toBe('wing');
    expect(row.textContent).toContain('FAIL src/parser.test.ts');
    // A failed call opens itself, so the output is on screen without a click.
    expect(row.getAttribute('data-collapsed')).toBe('false');
  });

  it('renders a streaming Bash call as running, with the command so far', () => {
    const row = mountCell(
      toolCall({
        name: 'Bash',
        status: 'streaming',
        args: null,
        argsText: '{"command": "pnpm test',
        display: { title: 'Bash', subject: 'pnpm test' },
        result: null,
      }),
    );

    // Still collapsed (only a *failed* call opens itself) — the row shows the
    // command it can see; expanding reveals the live terminal.
    expect(row.getAttribute('data-collapsed')).toBe('true');
    fireEvent.click(row.querySelector('[data-disclosure-row]') as HTMLElement);

    expect(row.querySelector('[data-terminal]')?.getAttribute('data-running')).toBe('');
    expect(row.textContent).toContain('Running');
    expect(row.textContent).toContain('pnpm test');
  });

  it('keeps the Arguments / Result cards for every other tool', () => {
    const row = mountCell(toolCall());
    fireEvent.click(row.querySelector('[data-disclosure-row]') as HTMLElement);

    expect(row.textContent).toContain('Arguments');
    expect(row.textContent).toContain('"path": "src/parser.ts"');
    expect(row.textContent).toContain('Result');
    expect(row.textContent).toContain('export function parse() {}');
  });

  it('marks a failed result as the error card, with its truncation note', () => {
    const row = mountCell(
      toolCall({
        status: 'failed',
        result: { text: 'boom', isError: true, truncated: true },
      }),
    );

    expect(row.textContent).toContain('Error');
    expect(row.querySelector('[data-error="true"]')?.textContent).toBe('boom');
    expect(row.textContent).toContain('Output truncated');
  });
});

describe('diff card', () => {
  it('folds a long window and expands it in place', () => {
    const row = mountCell(diffCell(40));

    const body = row.querySelector('[data-testid="diff-body"]');
    const folded = body?.querySelectorAll('[data-diff-kind]').length ?? 0;
    expect(folded).toBeLessThan(40);

    fireEvent.click(within(row).getByRole('button', { name: /Show \d+ more lines/ }));

    expect(row.querySelectorAll('[data-diff-kind]').length).toBe(40);
  });

  it('hands the editor diff request to the host', () => {
    const row = mountCell(diffCell(3));

    fireEvent.click(within(row).getByRole('button', { name: 'Open diff' }));

    expect(posted).toEqual([{ type: 'openDiff', sessionId: 's-1', cellId: 'd-1' }]);
  });
});

describe('thinking row', () => {
  const thinking: ThinkingCellModel = {
    kind: 'thinking',
    id: 'th-1',
    createdAt: EPOCH,
    text: 'Check the parser first.',
    streaming: false,
    durationMs: 1_200,
  };

  it('starts open while the model is thinking and collapses when it settles', () => {
    const streaming = mountCell({ ...thinking, streaming: true });
    expect(streaming.querySelector('[data-variant="think"]')?.hasAttribute('data-expanded')).toBe(true);
    expect(streaming.textContent).toContain('Check the parser first.');

    const settled = mountCell(thinking);
    expect(settled.querySelector('[data-variant="think"]')?.hasAttribute('data-expanded')).toBe(false);
    expect(settled.textContent).toContain('Thought for 1.2s');
  });

  it('expands from the row, and the reader’s choice outranks the default', () => {
    const row = mountCell(thinking);
    fireEvent.click(row.querySelector('[data-disclosure-row]') as HTMLElement);

    expect(row.querySelector('[data-variant="think"]')?.hasAttribute('data-expanded')).toBe(true);
    expect(row.textContent).toContain('Check the parser first.');
  });
});

describe('ask rows', () => {
  function askCell(overrides: Partial<AskCellModel> = {}): AskCellModel {
    return {
      kind: 'ask',
      id: 'ask-1',
      createdAt: EPOCH,
      requestId: 'toolu-ask',
      sessionId: 's-1',
      questions: [
        {
          id: 'q1',
          header: 'Scope',
          question: 'Which files should the fix touch?',
          multiSelect: false,
          options: [
            { label: 'parser.ts', description: 'the table body parser' },
            { label: 'wrap.ts', description: 'the CJK wrapper' },
          ],
          required: false,
        },
      ],
      state: 'awaiting',
      answers: [],
      approval: false,
      ...overrides,
    };
  }

  it('answers an awaiting question through the host, batch-shaped', () => {
    const row = mountCell(askCell());
    expect(row.getAttribute('data-ask-form')).toBe('question');
    expect(row.getAttribute('data-ask-state')).toBe('awaiting');
    expect(row.textContent).toContain('Which files should the fix touch?');

    fireEvent.click(within(row).getByLabelText('parser.ts'));
    fireEvent.click(within(row).getByRole('button', { name: 'Submit' }));

    expect(posted).toEqual([
      {
        type: 'answerAsk',
        sessionId: 's-1',
        requestId: 'toolu-ask',
        answers: [{ questionId: 'q1', selected: ['parser.ts'], text: '' }],
      },
    ]);
  });

  it('walks a two-question batch with the pager and answers both', () => {
    const second = {
      id: 'q2',
      header: 'Tests',
      question: 'Add a regression test for the nested case?',
      multiSelect: false,
      options: [{ label: 'yes', description: '' }],
      required: false,
    };
    const row = mountCell(askCell({ questions: [...askCell().questions, second] }));

    expect(row.textContent).toContain('1 / 2');
    fireEvent.click(within(row).getByLabelText('parser.ts'));
    // A single choice moves on by itself.
    expect(row.textContent).toContain('2 / 2');

    fireEvent.click(within(row).getByLabelText('yes'));
    fireEvent.click(within(row).getByRole('button', { name: 'Submit' }));

    expect(posted[0]).toMatchObject({
      type: 'answerAsk',
      answers: [
        { questionId: 'q1', selected: ['parser.ts'], text: '' },
        { questionId: 'q2', selected: ['yes'], text: '' },
      ],
    });
  });

  it('decides an approval from the buttons and from the keyboard', () => {
    const approval = askCell({
      approval: true,
      questions: [
        {
          id: 'choice',
          header: '',
          question: 'Bash command needs approval: pnpm install --force',
          multiSelect: false,
          options: [
            { label: 'Approve', description: '' },
            { label: 'Deny', description: '' },
          ],
          required: true,
        },
      ],
    });

    const first = mountCell(approval);
    expect(first.getAttribute('data-ask-form')).toBe('approval');
    expect(first.textContent).toContain('Approval required');
    expect(first.textContent).toContain('pnpm install --force');
    fireEvent.click(within(first).getByRole('button', { name: 'Deny' }));
    expect(posted[0]).toMatchObject({ type: 'approveTool', decision: 'deny', requestId: 'toolu-ask' });

    posted = [];
    const second = mountCell(approval);
    // The shortcuts only fire with focus inside the card (upstream's guard), which
    // is the state the reader is in when they reach for Enter/Escape.
    const scroll = second.querySelector<HTMLElement>('[data-approval-scroll]');
    scroll?.focus();
    fireEvent.keyDown(scroll as HTMLElement, { key: 'Enter' });
    expect(posted[0]).toMatchObject({ type: 'approveTool', decision: 'approve' });

    posted = [];
    const third = mountCell(approval);
    const thirdScroll = third.querySelector<HTMLElement>('[data-approval-scroll]');
    thirdScroll?.focus();
    fireEvent.keyDown(thirdScroll as HTMLElement, { key: 'Escape' });
    expect(posted[0]).toMatchObject({ type: 'approveTool', decision: 'deny' });
  });

  it('shows a settled reply as the reply bubble, not as a dead form', () => {
    const row = mountCell(
      askCell({
        state: 'answered',
        answers: [{ questionId: 'q1', selected: ['parser.ts'], text: '' }],
      }),
    );

    expect(row.getAttribute('data-ask-state')).toBe('answered');
    expect(row.textContent).toContain('Reply to earlier pending questions');
    expect(row.textContent).toContain('parser.ts');
    // The details stay folded until asked for; there is no submit control left.
    expect(within(row).queryByRole('button', { name: 'Submit' })).toBeNull();
    fireEvent.click(within(row).getByRole('button', { name: /Open question details/ }));
    expect(row.textContent).toContain('Which files should the fix touch?');
    expect(row.textContent).toContain('Answer: parser.ts');
  });
});
