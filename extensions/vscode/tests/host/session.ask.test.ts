import { afterEach, describe, expect, it } from 'vitest';

import { createHostHarness, flushMicrotasks } from './support/harness';
import type { HostHarness } from './support/harness';
import { kinds } from './support/mirror';

/**
 * Ask / approval invariants.
 *
 * Two wire shapes, one normalization: the AskUserQuestion form (1–4 questions,
 * skippable, replied as `header: answer` lines) and the retired Bash
 * confirmation (one required choice, replied as the bare label `y` / `n`, which
 * is the only thing the backend's `_parse_feedback` accepts).
 */

const teardown: HostHarness[] = [];
afterEach(() => {
  while (teardown.length > 0) {
    teardown.pop()?.dispose();
  }
});

interface AskFixture {
  readonly harness: HostHarness;
  readonly sessionId: string;
}

async function bootWithAsk(event: Record<string, unknown>): Promise<AskFixture> {
  const harness = createHostHarness();
  teardown.push(harness);
  await harness.boot();
  const sessionId = harness.gateway.createdOrder[0] ?? '';
  harness.wipe();
  harness.gateway.emit({ ...event, session_id: sessionId });
  await flushMicrotasks();
  return { harness, sessionId };
}

function askCell(fixture: AskFixture) {
  const cell = fixture.harness.host.sessionManager
    .record(fixture.sessionId)
    ?.cells.find((candidate) => candidate.kind === 'ask');
  if (cell?.kind !== 'ask') {
    throw new Error('expected an ask cell');
  }
  return cell;
}

function lastSentContent(fixture: AskFixture): string | undefined {
  const frames = fixture.harness.clientFrames().filter((frame) => frame['session_id'] === fixture.sessionId);
  const content = frames[frames.length - 1]?.['content'];
  return typeof content === 'string' ? content : undefined;
}

describe('multi-question asks', () => {
  it('normalizes questions and replies with `header: answer` lines', async () => {
    const fixture = await bootWithAsk({
      type: 'ask',
      tool_call_id: 'ask-1',
      questions: [
        {
          id: 'color',
          header: 'Color',
          question: 'Which color?',
          options: [{ label: 'Dark', description: 'dim' }, { label: 'Light' }],
        },
        {
          id: 'extras',
          header: 'Extras',
          question: 'Anything else?',
          multiSelect: true,
          options: [{ label: 'Tests' }, { label: 'Docs' }],
        },
      ],
      question: '',
      choices: [],
      required: false,
    });
    const cell = askCell(fixture);
    expect(cell.approval).toBe(false);
    expect(cell.state).toBe('awaiting');
    expect(cell.questions.map((question) => question.header)).toEqual(['Color', 'Extras']);
    expect(cell.questions[0]?.required).toBe(false);
    // Waiting for input is a tab-visible status.
    expect(fixture.harness.host.sessionManager.record(fixture.sessionId)?.status).toBe('waiting-for-input');

    await fixture.harness.intent({
      type: 'answerAsk',
      sessionId: fixture.sessionId,
      requestId: 'ask-1',
      answers: [
        { questionId: 'color', selected: ['Dark'], text: '' },
        { questionId: 'extras', selected: ['Docs', 'Tests'], text: '' },
      ],
    });
    await flushMicrotasks();

    // Option order (not click order) and the `header: answer` line shape.
    expect(lastSentContent(fixture)).toBe('Color: Dark\nExtras: Tests, Docs');
    const frame = fixture.harness
      .clientFrames()
      .find((candidate) => candidate['content'] === 'Color: Dark\nExtras: Tests, Docs');
    expect(frame?.['tool_call_id']).toBe('ask-1');

    const answered = askCell(fixture);
    expect(answered.state).toBe('answered');
    expect(answered.answers).toEqual([
      { questionId: 'color', selected: ['Dark'], text: '' },
      { questionId: 'extras', selected: ['Docs', 'Tests'], text: '' },
    ]);
    expect(fixture.harness.host.sessionManager.record(fixture.sessionId)?.status).toBe('idle');
  });

  it('sends the placeholder for a skipped question and concatenates free-form text for multi-select', async () => {
    const fixture = await bootWithAsk({
      type: 'ask',
      tool_call_id: 'ask-2',
      questions: [
        {
          id: 'q1',
          header: 'First',
          question: 'Pick one',
          options: [{ label: 'A' }],
        },
        {
          id: 'q2',
          header: '',
          question: 'Anything?',
          multiSelect: true,
          options: [{ label: 'B' }],
        },
      ],
      question: '',
      choices: [],
      required: false,
    });

    await fixture.harness.intent({
      type: 'answerAsk',
      sessionId: fixture.sessionId,
      requestId: 'ask-2',
      answers: [
        { questionId: 'q1', selected: [], text: '' },
        { questionId: 'q2', selected: ['B'], text: 'plus my notes' },
      ],
    });
    await flushMicrotasks();

    // Question 1 skipped → placeholder; question 2 falls back to its id (empty header).
    expect(lastSentContent(fixture)).toBe('First: (user did not answer)\nq2: B, plus my notes');
  });

  it('ignores answers for a request that is no longer awaiting', async () => {
    const fixture = await bootWithAsk({
      type: 'ask',
      tool_call_id: 'ask-3',
      questions: [{ id: 'q', header: 'Q', question: 'Well?', options: [] }],
      question: '',
      choices: [],
      required: false,
    });
    fixture.harness.gateway.emit({ type: 'done', session_id: fixture.sessionId });
    await flushMicrotasks();
    expect(askCell(fixture).state).toBe('cancelled');

    await fixture.harness.intent({
      type: 'answerAsk',
      sessionId: fixture.sessionId,
      requestId: 'ask-3',
      answers: [{ questionId: 'q', selected: [], text: 'late' }],
    });
    await flushMicrotasks();

    expect(fixture.harness.clientFrames().filter((frame) => frame['tool_call_id'] === 'ask-3')).toHaveLength(
      0,
    );
    const toasts = fixture.harness
      .ofType('ui')
      .filter((message) => message.action.kind === 'toast')
      .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
    expect(toasts).toContain('That question is no longer waiting for an answer.');
  });
});

describe('bash approval asks', () => {
  const legacyAsk = {
    type: 'ask',
    tool_call_id: 'ask-bash',
    questions: [],
    question: 'Dangerous command detected: rm -rf …\nProceed?',
    choices: ['y', 'n', 'yolo'],
    required: true,
  };

  it('normalizes the legacy shape into one required choice', async () => {
    const fixture = await bootWithAsk(legacyAsk);
    const cell = askCell(fixture);
    expect(cell.approval).toBe(true);
    expect(cell.questions).toHaveLength(1);
    expect(cell.questions[0]?.required).toBe(true);
    expect(cell.questions[0]?.options.map((option) => option.label)).toEqual(['y', 'n', 'yolo']);
  });

  it('answers approve with the bare `y` label', async () => {
    const fixture = await bootWithAsk(legacyAsk);
    await fixture.harness.intent({
      type: 'approveTool',
      sessionId: fixture.sessionId,
      requestId: 'ask-bash',
      decision: 'approve',
    });
    await flushMicrotasks();

    expect(lastSentContent(fixture)).toBe('y');
    expect(askCell(fixture).state).toBe('answered');
    expect(askCell(fixture).answers[0]?.selected).toEqual(['y']);
  });

  it('answers deny with the bare `n` label', async () => {
    const fixture = await bootWithAsk(legacyAsk);
    await fixture.harness.intent({
      type: 'approveTool',
      sessionId: fixture.sessionId,
      requestId: 'ask-bash',
      decision: 'deny',
    });
    await flushMicrotasks();

    expect(lastSentContent(fixture)).toBe('n');
    expect(askCell(fixture).answers[0]?.selected).toEqual(['n']);
  });

  it('keeps the ask awaiting when the answer cannot be sent', async () => {
    const fixture = await bootWithAsk(legacyAsk);
    // Kill the socket behind the client's back: `send` throws.
    fixture.harness.gateway.lastSocket.readyState = 3;
    await fixture.harness.intent({
      type: 'approveTool',
      sessionId: fixture.sessionId,
      requestId: 'ask-bash',
      decision: 'approve',
    });
    await flushMicrotasks();

    expect(askCell(fixture).state).toBe('awaiting');
    const toasts = fixture.harness
      .ofType('ui')
      .filter((message) => message.action.kind === 'toast')
      .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
    expect(toasts.some((message) => message.startsWith('Send failed'))).toBe(true);
  });

  it('marks the ask answered when another client answered it first', async () => {
    const fixture = await bootWithAsk(legacyAsk);
    expect(askCell(fixture).state).toBe('awaiting');

    // Another frontend answered: the tool finishes, and the result reaches us.
    fixture.harness.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'Bash',
      tool_args: { command: 'rm -rf /' },
      tool_call_id: 'ask-bash',
      tool_result: 'Command rejected by user.',
      tool_success: false,
      model: 'm',
      session_id: fixture.sessionId,
    });
    await flushMicrotasks();

    expect(askCell(fixture).state).toBe('answered');
    // A late local answer must not become a stray user message.
    await fixture.harness.intent({
      type: 'approveTool',
      sessionId: fixture.sessionId,
      requestId: 'ask-bash',
      decision: 'approve',
    });
    await flushMicrotasks();
    expect(
      fixture.harness.clientFrames().filter((frame) => frame['tool_call_id'] === 'ask-bash'),
    ).toHaveLength(0);
  });
});

describe('replayed asks', () => {
  it('registers a pending ask from the replay so it can be answered', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = {
      messages: [{ role: 'user', content: 'do something dangerous', uuid: 'm1' }],
      events: [
        {
          type: 'ask',
          tool_call_id: 'ask-replayed',
          question: 'Proceed?',
          choices: ['y', 'n'],
          required: true,
        },
      ],
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';

    const cell = harness.host.sessionManager
      .record(sessionId)
      ?.cells.find((candidate) => candidate.kind === 'ask');
    expect(cell?.kind).toBe('ask');
    expect(harness.host.sessionManager.record(sessionId)?.status).toBe('waiting-for-input');
    // Hydrated asks land in the webview like any other cell.
    expect(kinds(harness.hydrateFor(sessionId)?.session.cells ?? [])).toEqual(['user', 'ask']);

    harness.wipe();
    await harness.intent({
      type: 'approveTool',
      sessionId,
      requestId: 'ask-replayed',
      decision: 'approve',
    });
    await flushMicrotasks();

    const frame = harness.clientFrames().find((candidate) => candidate['tool_call_id'] === 'ask-replayed');
    expect(frame?.['content']).toBe('y');
  });

  it('treats a re-delivered pending ask as the same cell', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();
    const askEvent = {
      type: 'ask',
      tool_call_id: 'ask-x',
      questions: [{ id: 'q', header: 'Q', question: 'Well?', options: [{ label: 'A' }] }],
      question: '',
      choices: [],
      required: false,
      session_id: sessionId,
    };
    harness.gateway.emit(askEvent);
    harness.gateway.emit(askEvent);
    await flushMicrotasks();

    const asks = harness.host.sessionManager.record(sessionId)?.cells.filter((cell) => cell.kind === 'ask');
    expect(asks).toHaveLength(1);
  });

  it('renders a retired non-required ask as non-answerable', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();
    harness.gateway.emit({
      type: 'ask',
      tool_call_id: 'ask-legacy',
      question: 'heads up',
      choices: ['a', 'b'],
      required: false,
      session_id: sessionId,
    });
    await flushMicrotasks();

    const cell = harness.host.sessionManager
      .record(sessionId)
      ?.cells.find((candidate) => candidate.kind === 'ask');
    expect(cell?.kind === 'ask' && cell.state).toBe('cancelled');
    await harness.intent({
      type: 'answerAsk',
      sessionId,
      requestId: 'ask-legacy',
      answers: [{ questionId: 'choice', selected: ['a'], text: '' }],
    });
    await flushMicrotasks();
    expect(harness.clientFrames().filter((frame) => frame['tool_call_id'] === 'ask-legacy')).toHaveLength(0);
  });
});
