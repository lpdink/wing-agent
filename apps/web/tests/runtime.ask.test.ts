import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { FakeGateway, makeSession, historyMessage } from './support/fake-gateway';
import { createHarness, useFakeTimers, type RuntimeHarness } from './support/harness';

/**
 * The Ask round trip (step 08): renderer intent → `ClientRequest` on the socket →
 * optimistic `answered` cell.
 *
 * The assertion that matters is on the *wire*: the frame the fake gateway received,
 * because that is what the backend's `_parse_feedback` reads. The cell's state is the
 * second half — the renderer must stop offering buttons the moment the answer left.
 */

/** A `AskUserQuestion`-shaped ask, as the replay carries it. */
const ASK_EVENT = {
  type: 'ask',
  session_id: 'session-1',
  created_at: '2026-10-01T12:00:02Z',
  request_id: 'ask-1',
  tool_call_id: 'toolu-ask',
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
    },
    {
      id: 'q2',
      header: 'Docs',
      question: 'Update the docs too?',
      multiSelect: true,
      options: [{ label: 'changelog' }, { label: 'guide' }],
    },
  ],
  question: '',
  choices: [],
  required: false,
};

/** The legacy Bash-confirmation ask: `required` + `choices` normalizes to approval. */
const APPROVAL_EVENT = {
  type: 'ask',
  session_id: 'session-1',
  created_at: '2026-10-01T12:00:03Z',
  request_id: 'ask-2',
  tool_call_id: 'toolu-bash',
  questions: [],
  question: 'Bash command needs approval: rm -rf build',
  choices: ['Approve', 'Deny'],
  required: true,
};

function sessionWith(events: readonly Record<string, unknown>[]): ReturnType<typeof makeSession> {
  return makeSession({
    id: 'session-1',
    name: 'Fix the parser',
    messages: [historyMessage('user', 'The parser chokes on nested tables.')],
    events: [...events],
  });
}

async function openHarness(
  events: readonly Record<string, unknown>[],
): Promise<RuntimeHarness & { readonly gateway: FakeGateway }> {
  const gateway = new FakeGateway({ sessions: [sessionWith(events)] });
  const harness = createHarness({ gateway });
  harness.runtime.start();
  await harness.settle();
  return harness;
}

/** The ask cell of the open session (`null` when it is not awaiting). */
function askCell(harness: RuntimeHarness, requestId: string): Record<string, unknown> | null {
  const record = harness.snapshot().record;
  const cellId = record?.awaitingAsks.get(requestId);
  const cell = cellId === undefined ? undefined : record?.cellById(cellId);
  return cell === undefined ? null : { ...cell };
}

describe('ask replies', () => {
  beforeEach(() => {
    useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('sends the chosen answers as a ClientRequest addressed to the ask', async () => {
    const harness = await openHarness([ASK_EVENT]);
    const cell = harness.snapshot().record?.cells.find((entry) => entry.kind === 'ask');
    expect(cell?.kind === 'ask' ? cell.state : null).toBe('awaiting');

    const outcome = harness.runtime.answerAsk('toolu-ask', [
      { questionId: 'q1', selected: ['parser.ts'], text: '' },
      { questionId: 'q2', selected: ['guide'], text: '' },
    ]);
    await harness.settle();

    expect(outcome).toBe('sent');
    const frames = harness.gateway.lastSocket()?.clientFrames() ?? [];
    expect(frames).toHaveLength(1);
    expect(frames[0]).toMatchObject({
      session_id: 'session-1',
      content: 'Scope: parser.ts\nDocs: guide',
      tool_call_id: 'toolu-ask',
    });
    expect(typeof frames[0]?.['request_id']).toBe('string');

    // The renderer stops offering the form: the cell is answered, with the choices.
    expect(askCell(harness, 'toolu-ask')).toBeNull();
    const answered = harness.snapshot().record?.cells.find((entry) => entry.kind === 'ask');
    expect(answered?.kind === 'ask' ? answered.state : null).toBe('answered');
    expect(answered?.kind === 'ask' ? answered.answers : null).toEqual([
      { questionId: 'q1', selected: ['parser.ts'], text: '' },
      { questionId: 'q2', selected: ['guide'], text: '' },
    ]);
    expect(harness.snapshot().record?.status).toBe('idle');
  });

  it('skips a question the user left empty with the shared marker', async () => {
    const harness = await openHarness([ASK_EVENT]);

    harness.runtime.answerAsk('toolu-ask', [{ questionId: 'q1', selected: ['wrap.ts'], text: '' }]);
    await harness.settle();

    expect(harness.gateway.lastSocket()?.clientFrames()[0]?.['content']).toBe(
      'Scope: wrap.ts\nDocs: (user did not answer)',
    );
  });

  it('answers an approval with the bare label the backend parses', async () => {
    const harness = await openHarness([APPROVAL_EVENT]);
    const cell = harness.snapshot().record?.cells.find((entry) => entry.kind === 'ask');
    expect(cell?.kind === 'ask' ? cell.approval : null).toBe(true);

    expect(harness.runtime.approveTool('toolu-bash', 'approve')).toBe('sent');
    harness.runtime.approveTool('toolu-bash', 'deny'); // already answered → notice
    await harness.settle();

    expect(harness.gateway.lastSocket()?.clientFrames()[0]).toMatchObject({
      session_id: 'session-1',
      content: 'y',
      tool_call_id: 'toolu-bash',
    });
    expect(harness.gateway.lastSocket()?.clientFrames()).toHaveLength(1);

    const notices = harness.snapshot().notices.map((notice) => notice.text);
    expect(notices).toContain('That approval is no longer pending.');
  });

  it('refuses to answer an ask the session no longer has, without touching the wire', async () => {
    const harness = await openHarness([ASK_EVENT]);

    expect(harness.runtime.answerAsk('toolu-gone', [{ questionId: 'q1', selected: ['x'], text: '' }])).toBe(
      'gone',
    );
    await harness.settle();

    expect(harness.gateway.lastSocket()?.clientFrames()).toEqual([]);
    expect(harness.snapshot().notices.map((notice) => notice.text)).toEqual([
      'That question is no longer waiting for an answer.',
    ]);
  });

  it('reports a dead socket instead of pretending the answer left', async () => {
    const harness = await openHarness([ASK_EVENT]);
    harness.gateway.lastSocket()?.drop();
    await harness.settle();

    expect(harness.runtime.answerAsk('toolu-ask', [{ questionId: 'q1', selected: ['x'], text: '' }])).toBe(
      'not-connected',
    );
    expect(harness.snapshot().notices.map((notice) => notice.text)).toContain(
      'Not sent — the gateway is not connected.',
    );
    // The form is still there: the user's answer was not swallowed.
    expect(askCell(harness, 'toolu-ask')).not.toBeNull();
  });

  it('sends nothing when the reply encodes to an empty string', async () => {
    // An approval with no choice picked: the backend's `_parse_feedback` takes the
    // bare label, so an empty selection is not a reply at all.
    const harness = await openHarness([APPROVAL_EVENT]);

    expect(harness.runtime.answerAsk('toolu-bash', [])).toBe('empty');
    expect(harness.gateway.lastSocket()?.clientFrames()).toEqual([]);
    expect(harness.snapshot().notices.map((notice) => notice.text)).toContain(
      'Nothing to send — pick an answer first.',
    );
  });

  it('keeps the answered cell when the backend later echoes the tool result', async () => {
    const harness = await openHarness([ASK_EVENT]);
    harness.runtime.answerAsk('toolu-ask', [{ questionId: 'q1', selected: ['parser.ts'], text: '' }]);
    await harness.settle();

    harness.gateway.emit('session-1', {
      type: 'tool_call_result',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:05Z',
      request_id: 'req-9',
      tool_call_id: 'toolu-ask',
      tool_name: 'AskUserQuestion',
      tool_args: {},
      tool_result: 'answered',
      tool_success: true,
      model: 'test-model',
    });
    await harness.settle();

    expect(harness.gateway.lastSocket()?.clientFrames()).toHaveLength(1);
    const ask = harness.snapshot().record?.cells.find((entry) => entry.kind === 'ask');
    expect(ask?.kind === 'ask' ? ask.state : null).toBe('answered');
  });
});
