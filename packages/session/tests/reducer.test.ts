import { describe, expect, it } from 'vitest';

import { SessionRecord, applyLive, applySync, parseIsoMs, truncateToolResult } from '../src';
import type { CellModel, ReductionEffect } from '../src';

import {
  SESSION,
  askEvent,
  contextStats,
  diffContent,
  done,
  errorEvent,
  interrupted,
  kinds,
  llmCallMetrics,
  message,
  notice,
  reasoning,
  stableCells,
  stateChanged,
  syncSession,
  text,
  toolCall,
  toolCallResult,
  toolCallStream,
  turnResult,
  turnStarted,
  userMessageAccepted,
} from './support/events';

/**
 * The reduction lane: `WingEvent` in, model mutations out.
 *
 * The headline invariant is **replay == live**: a `sync_session` snapshot and the
 * event stream that produced it must end in the same transcript, through the same
 * handlers. That property is what the whole frontend stack rests on (a resumed or
 * reconnected view must not render a different conversation), and it is asserted
 * here directly — no host, no bridge, no gateway — against the package that owns it.
 * The VS Code extension asserts the same property end to end through its host suite
 * (`extensions/vscode/tests/host/session.replay.test.ts`).
 */

let clock = 1_000;

function makeRecord(sessionId: string = SESSION): SessionRecord {
  clock = 1_000;
  return new SessionRecord({
    sessionId,
    now: () => clock,
    workspace: '/tmp/work',
    createdAt: '2026-09-18T08:00:00.000',
  });
}

describe('replay == live', () => {
  it('rebuilds a finished turn cell-for-cell from the history projection', () => {
    const live = makeRecord();
    applyLive(live, turnStarted());
    applyLive(live, toolCall({ toolCallId: 'tc-1', toolName: 'Bash', toolArgs: { command: 'ls -la' } }));
    applyLive(
      live,
      toolCallResult({
        toolCallId: 'tc-1',
        toolName: 'Bash',
        toolArgs: { command: 'ls -la' },
        toolResult: 'file.txt',
        toolSuccess: true,
      }),
    );
    applyLive(live, text('after the tool'));
    applyLive(live, done());

    const replayed = makeRecord();
    applySync(
      replayed,
      syncSession({
        messages: [
          message({
            role: 'assistant',
            content: '',
            toolCalls: [{ id: 'tc-1', name: 'Bash', arguments: { command: 'ls -la' } }],
          }),
          message({ role: 'tool', content: 'file.txt', toolCallId: 'tc-1' }),
          message({ role: 'assistant', content: 'after the tool' }),
        ],
      }),
    );

    expect(kinds(live.cells)).toEqual(['tool_call', 'separator', 'assistant']);
    expect(stableCells(replayed.cells)).toEqual(stableCells(live.cells));
    expect(replayed.status).toBe('idle');
  });

  it('replays the assistant text above the tool calls it announces (stream order)', () => {
    // One assistant message that carries *both* text and tool calls — the shape the
    // projection produces for every "let me look at that" sentence followed by a call.
    // The live lane renders it in stream order (text, then the calls), so the replay
    // must too: pushing the calls first would put the sentence below its own tool
    // card, with a ReAct separator in between. This is the case the projection-order
    // comment in `applyMessageProjection` exists for; without it a reordering there
    // stays invisible inside this package.
    const live = makeRecord();
    applyLive(live, turnStarted());
    applyLive(live, text('Now let me run it'));
    applyLive(live, toolCall({ toolCallId: 'tc-1', toolName: 'Bash', toolArgs: { command: 'ls' } }));

    const replayed = makeRecord();
    applySync(
      replayed,
      syncSession({
        status: 'working',
        messages: [
          message({
            role: 'assistant',
            content: 'Now let me run it',
            toolCalls: [{ id: 'tc-1', name: 'Bash', arguments: { command: 'ls' } }],
          }),
        ],
      }),
    );

    // No separator: the text and its calls are one streamed message, not two rounds.
    expect(kinds(live.cells)).toEqual(['assistant', 'tool_call']);
    expect(kinds(replayed.cells)).toEqual(kinds(live.cells));
    expect(stableCells(replayed.cells)).toEqual(stableCells(live.cells));
  });

  it('restores a turn that is still running, including its streaming text', () => {
    const live = makeRecord();
    applyLive(live, turnStarted());
    applyLive(live, reasoning('thinking…'));
    applyLive(live, text('partial answer'));

    const replayed = makeRecord();
    applySync(
      replayed,
      syncSession({
        status: 'working',
        turnStartedAt: '2026-09-18T08:00:00.000000',
        uncommitted: message({ role: 'assistant', content: 'partial answer', reasoning: 'thinking…' }),
      }),
    );

    // Same transcript in the same order with the same text. The one difference is
    // deliberate and invisible to the reader: a restored block is not flagged
    // `streaming` (nothing is arriving *right now*); the next live delta adopts it
    // and flips the flag back on — see the adoption test below.
    expect(kinds(replayed.cells)).toEqual(kinds(live.cells));
    expect(replayed.cells.map((cell) => ('text' in cell ? cell.text : ''))).toEqual(
      live.cells.map((cell) => ('text' in cell ? cell.text : '')),
    );
    expect(replayed.status).toBe('working');
    expect(replayed.turn.active).toBe(true);
    // `turn_started_at` restores the real elapsed time rather than resetting it.
    expect(replayed.turn.startedAtMs).toBe(Date.parse('2026-09-18T08:00:00.000Z'));
  });

  it('restores unterminated tool arguments through the live streaming handler', () => {
    const live = makeRecord();
    applyLive(live, toolCallStream({ toolCallId: 'tc-9', toolName: 'Write', fragment: '{"path": "a.ts"' }));
    applyLive(live, toolCallStream({ toolCallId: 'tc-9', toolName: 'Write', fragment: ', "content": "hi' }));
    const liveCell = live.cells[0];

    const replayed = makeRecord();
    applySync(
      replayed,
      syncSession({
        status: 'working',
        uncommittedTools: [
          { tool_call_id: 'tc-9', tool_name: 'Write', args_fragment: '{"path": "a.ts", "content": "hi' },
        ],
      }),
    );

    expect(stableCells(replayed.cells)).toEqual(stableCells(live.cells));
    expect(replayed.cells).toHaveLength(1);
    expect(replayed.cells[0]).toMatchObject({ kind: 'tool_call', status: 'streaming' });
    expect((replayed.cells[0] as Extract<CellModel, { kind: 'tool_call' }>).display.subject).toBe('a.ts');
    expect(liveCell).toBeDefined();
  });

  it('keeps an event-anchored cell (a diff) in the live position', () => {
    // The message pass runs before the events pass, so an anchored diff would land
    // *behind* the separator the message pass inserted before it — `applySync` drops
    // those stale separators at the end of the assembly. Live, where the diff sits on
    // the tool card before the next round's text arrives, no separator is ever grown.
    const live = makeRecord();
    applyLive(live, toolCall({ toolCallId: 'tc-edit', toolName: 'Edit', toolArgs: { path: 'a.ts' } }));
    applyLive(live, diffContent({ toolCallId: 'tc-edit', path: 'a.ts', oldText: 'old\n', newText: 'new\n' }));
    applyLive(
      live,
      toolCallResult({ toolCallId: 'tc-edit', toolName: 'Edit', toolResult: 'ok', toolSuccess: true }),
    );
    applyLive(live, text('done editing'));
    applyLive(live, done());

    const replayed = makeRecord();
    applySync(
      replayed,
      syncSession({
        messages: [
          message({
            role: 'assistant',
            content: '',
            toolCalls: [{ id: 'tc-edit', name: 'Edit', arguments: { path: 'a.ts' } }],
          }),
          message({ role: 'tool', content: 'ok', toolCallId: 'tc-edit' }),
          message({ role: 'assistant', content: 'done editing' }),
        ],
        events: [diffContent({ toolCallId: 'tc-edit', path: 'a.ts', oldText: 'old\n', newText: 'new\n' })],
      }),
    );

    expect(kinds(live.cells)).toEqual(['tool_call', 'diff', 'assistant']);
    expect(stableCells(replayed.cells)).toEqual(stableCells(live.cells));
  });

  it('drops the effects of a replay (a restored error must not pop a toast)', () => {
    const record = makeRecord();
    applySync(
      record,
      syncSession({
        events: [errorEvent('boom')],
        messages: [],
      }),
    );

    // The error *did* land in the transcript and in `lastError`…
    expect(kinds(record.cells)).toEqual(['system']);
    expect(record.lastError).toBe('boom');
    // …but its attention effect never reached a caller: `applySync` returns void.
    expect(record.status).toBe('idle');
  });
});

describe('live reductions', () => {
  it('streams assistant text into one cell and closes it when a tool starts', () => {
    const record = makeRecord();
    applyLive(record, turnStarted());
    applyLive(record, text('Hello'));
    applyLive(record, text(' world'));

    expect(kinds(record.cells)).toEqual(['assistant']);
    expect(record.cells[0]).toMatchObject({ text: 'Hello world', streaming: true });

    applyLive(record, toolCall({ toolCallId: 'tc-1', toolName: 'Bash', toolArgs: { command: 'ls' } }));
    expect(record.cells[0]).toMatchObject({ streaming: false });
    expect(kinds(record.cells)).toEqual(['assistant', 'tool_call']);
  });

  it('adopts a replayed text block instead of growing a second cell', () => {
    const record = makeRecord();
    applySync(
      record,
      syncSession({
        status: 'working',
        uncommitted: message({ role: 'assistant', content: 'partial' }),
      }),
    );
    const replayedId = record.cells[0]?.id;

    applyLive(record, text(' continued'));

    expect(record.cells).toHaveLength(1);
    expect(record.cells[0]).toMatchObject({ id: replayedId, text: 'partial continued', streaming: true });
  });

  it('annotates a thinking block with its measured duration', () => {
    const record = makeRecord();
    applyLive(record, turnStarted());
    applyLive(record, reasoning('hmm'));
    clock = 2_500;
    applyLive(record, text('answer'));

    expect(record.cells[0]).toMatchObject({
      kind: 'thinking',
      text: 'hmm',
      streaming: false,
      durationMs: 1_500,
    });
  });

  it('budgets the streaming tool-argument preview without losing the buffer', () => {
    const record = makeRecord();
    const fragment = 'x'.repeat(600);
    applyLive(record, toolCallStream({ toolCallId: 'tc-1', toolName: 'Write', fragment }));

    const cell = record.cells[0];
    expect(cell).toMatchObject({ kind: 'tool_call', status: 'streaming' });
    expect((cell as Extract<CellModel, { kind: 'tool_call' }>).argsText).toHaveLength(600);

    // Same millisecond, same size class: the fragment is buffered, not rendered —
    // the cell (and therefore the bridge) stays untouched.
    applyLive(record, toolCallStream({ toolCallId: 'tc-1', toolName: 'Write', fragment: 'y' }));
    expect(record.cells[0]).toBe(cell);
    expect(record.toolArgsStreamFor('tc-1')?.text).toHaveLength(601);

    // The final fragment always renders, then the authoritative `tool_call` drops
    // the buffer and takes over.
    applyLive(
      record,
      toolCallStream({ toolCallId: 'tc-1', toolName: 'Write', fragment: '"a.ts"}', isFinal: true }),
    );
    expect((record.cells[0] as Extract<CellModel, { kind: 'tool_call' }>).status).toBe('pending');

    applyLive(record, toolCall({ toolCallId: 'tc-1', toolName: 'Write', toolArgs: { path: 'a.ts' } }));
    expect(record.toolArgsStreamFor('tc-1')).toBeUndefined();
    expect(record.cells).toHaveLength(1);
    expect(record.cells[0]).toMatchObject({
      argsText: '',
      args: { path: 'a.ts' },
      display: { title: 'Write', subject: 'a.ts' },
      status: 'pending',
    });
  });

  it('renders every fragment of an ordinary (small) call', () => {
    const record = makeRecord();
    applyLive(record, toolCallStream({ toolCallId: 'tc-1', toolName: 'Bash', fragment: '{"command": ' }));
    const first = record.cells[0];
    applyLive(record, toolCallStream({ toolCallId: 'tc-1', toolName: 'Bash', fragment: '"ls"}' }));

    expect(record.cells[0]).not.toBe(first);
    expect((record.cells[0] as Extract<CellModel, { kind: 'tool_call' }>).argsText).toBe('{"command": "ls"}');
    expect(record.cells[0]).toMatchObject({ display: { title: 'Bash', subject: 'ls' } });
  });

  it('finishes a tool call, flags failures and truncates huge results', () => {
    const record = makeRecord();
    applyLive(record, toolCall({ toolCallId: 'tc-1', toolName: 'Bash', toolArgs: { command: 'false' } }));
    applyLive(
      record,
      toolCallResult({
        toolCallId: 'tc-1',
        toolName: 'Bash',
        toolResult: 'o'.repeat(20_000),
        toolSuccess: false,
      }),
    );

    const cell = record.cells[0] as Extract<CellModel, { kind: 'tool_call' }>;
    expect(cell.status).toBe('failed');
    expect(cell.result).toMatchObject({ isError: true, truncated: true });
    expect(cell.result?.text).toContain('chars truncated');
    expect(truncateToolResult('short')).toBe('short');
  });

  it('anchors a TodoWrite todo cell directly after its tool call', () => {
    const record = makeRecord();
    const args = {
      todos: [
        { content: 'one', status: 'in_progress' },
        { content: 'two', status: 'weird' },
      ],
    };
    applyLive(record, toolCall({ toolCallId: 'tc-1', toolName: 'TodoWrite', toolArgs: args }));
    applyLive(
      record,
      toolCallResult({
        toolCallId: 'tc-1',
        toolName: 'TodoWrite',
        toolArgs: args,
        toolResult: 'ok',
        toolSuccess: true,
      }),
    );

    expect(kinds(record.cells)).toEqual(['tool_call', 'todo']);
    expect(record.cells[1]).toMatchObject({
      items: [
        { content: 'one', status: 'in_progress' },
        { content: 'two', status: 'pending' },
      ],
    });
  });

  it('builds an orphan tool cell for a result whose call was never announced', () => {
    const record = makeRecord();
    applyLive(
      record,
      toolCallResult({ toolCallId: 'tc-orphan', toolName: 'Read', toolResult: 'content', toolSuccess: true }),
    );

    expect(kinds(record.cells)).toEqual(['tool_call']);
    expect(record.cells[0]).toMatchObject({ toolCallId: 'tc-orphan', status: 'success', startedAt: null });
  });

  it('normalizes both ask shapes and answers them through the awaiting index', () => {
    const record = makeRecord();
    const effects = applyLive(
      record,
      askEvent({
        toolCallId: 'tc-ask',
        questions: [
          {
            id: 'q1',
            header: 'Choice',
            question: 'Which one?',
            multiSelect: false,
            options: [{ label: 'a', description: '' }],
            choices: [],
          },
        ],
      }),
    );

    expect(effects).toEqual<ReductionEffect[]>([{ kind: 'scrollToBottom' }]);
    expect(record.status).toBe('waiting-for-input');
    expect(record.awaitingAsks.get('tc-ask')).toBe(record.cells[0]?.id);

    // Re-delivered ask (a replay of the same pending question) keeps one cell.
    applyLive(
      record,
      askEvent({
        toolCallId: 'tc-ask',
        questions: [
          {
            id: 'q1',
            header: 'Choice',
            question: 'Which one?',
            multiSelect: false,
            options: [{ label: 'a', description: '' }],
            choices: [],
          },
        ],
      }),
    );
    expect(record.cells).toHaveLength(1);

    // A tool result proves the ask is over, even when another client answered it.
    applyLive(
      record,
      toolCallResult({
        toolCallId: 'tc-ask',
        toolName: 'AskUserQuestion',
        toolResult: '',
        toolSuccess: true,
      }),
    );
    expect(record.cells[0]).toMatchObject({ kind: 'ask', state: 'answered' });
    expect(record.status).toBe('idle');
  });

  it('marks the retired non-required ask unanswerable', () => {
    const record = makeRecord();
    applyLive(record, askEvent({ toolCallId: 'tc-bash', question: 'Run it?', choices: ['y', 'n'] }));

    expect(record.cells[0]).toMatchObject({
      kind: 'ask',
      state: 'cancelled',
      approval: false,
      questions: [
        {
          id: 'choice',
          required: true,
          options: [
            { label: 'y', description: '' },
            { label: 'n', description: '' },
          ],
        },
      ],
    });
    expect(record.hasAwaitingAsk()).toBe(false);
  });

  it('finishes the turn on done, flushing the metrics cell and promoting pending messages', () => {
    const record = makeRecord();
    applyLive(record, turnStarted());
    record.addPendingUser('req-1', 'queued');
    applyLive(record, userMessageAccepted('req-1'));
    applyLive(record, llmCallMetrics({ promptTokens: 10, completionTokens: 5 }));
    clock = 3_000;
    applyLive(record, done());

    expect(record.turn.active).toBe(false);
    expect(record.status).toBe('idle');
    expect(record.cells.filter((cell) => cell.kind === 'user')[0]).toMatchObject({ state: 'accepted' });
    expect(kinds(record.cells)).toContain('metrics');
    expect(record.totals).toEqual({ promptTokens: 10, completionTokens: 5, cachedTokens: 0 });
    // `done` is idempotent for the transcript: no second metrics cell.
    applyLive(record, done());
    expect(record.cells.filter((cell) => cell.kind === 'metrics')).toHaveLength(1);
  });

  it('discards pending messages and finishes the turn on an interrupt', () => {
    const record = makeRecord();
    applyLive(record, turnStarted());
    record.addPendingUser('req-1', 'never delivered');
    const effects = applyLive(record, interrupted());

    expect(effects).toEqual<ReductionEffect[]>([
      { kind: 'toast', level: 'info', message: 'Agent interrupted' },
    ]);
    expect(record.cells[0]).toMatchObject({ kind: 'user', state: 'discarded' });
    expect(record.turn.active).toBe(false);
  });

  it('surfaces an error as a system cell with the attention effect', () => {
    const record = makeRecord();
    applyLive(record, turnStarted());
    const effects = applyLive(record, errorEvent('boom'));

    expect(effects).toEqual<ReductionEffect[]>([{ kind: 'attention', level: 'error' }]);
    expect(record.cells[0]).toMatchObject({ kind: 'system', level: 'error', text: 'boom' });
    expect(record.lastError).toBe('boom');
    expect(record.turn.active).toBe(false);
  });

  it('treats a notice as informational: the turn keeps running', () => {
    const record = makeRecord();
    applyLive(record, turnStarted());
    applyLive(record, notice('LLM call failed, retrying'));

    expect(record.cells[0]).toMatchObject({ kind: 'system', level: 'warning' });
    expect(record.turn.active).toBe(true);
  });

  it('merges session_state_changed into the meta model and refreshes the title', () => {
    const record = makeRecord();
    applyLive(record, stateChanged({ model: 'gpt-5', thinking: true, yolo: true, title: 'Named' }));

    expect(record.meta).toMatchObject({ model: 'gpt-5', thinking: true, yolo: true });
    expect(record.title).toBe('Named');
    // Fields the event does not carry are left alone.
    expect(record.meta.provider).toBe('');
  });

  it('keeps the last known context window when the gateway reports none', () => {
    const record = makeRecord();
    applyLive(record, contextStats({ totalTokens: 100, windowTokens: 200_000 }));
    applyLive(record, contextStats({ totalTokens: 150, windowTokens: 0 }));

    expect(record.context).toEqual({ usedTokens: 150, windowTokens: 200_000, messageCount: 3 });
  });

  it('records the last turn result and raises the attention badge', () => {
    const record = makeRecord();
    const effects = applyLive(record, turnResult({ subtype: 'error_max_turns', isError: true }));

    expect(effects).toEqual<ReductionEffect[]>([{ kind: 'attention', level: 'error' }]);
    expect(record.turn.lastResult).toMatchObject({
      subtype: 'error_max_turns',
      isError: true,
      totalTokens: 120,
    });
  });

  it('replaces the whole model when a sync arrives on the live lane', () => {
    const record = makeRecord();
    applyLive(record, text('stale'));
    applyLive(record, syncSession({ messages: [message({ role: 'user', content: 'fresh' })], name: 'Sync' }));

    expect(kinds(record.cells)).toEqual(['user']);
    expect(record.title).toBe('Sync');
  });

  it('ignores an event it cannot represent', () => {
    const record = makeRecord();
    expect(applyLive(record, { type: 'from_the_future' } as never)).toEqual([]);
    expect(record.cells).toHaveLength(0);
  });
});

describe('helpers', () => {
  it('parses the gateway ISO timestamps, with and without a zone', () => {
    expect(parseIsoMs('2026-09-18T08:00:00.123456')).toBe(Date.parse('2026-09-18T08:00:00.123456Z'));
    expect(parseIsoMs('2026-09-18T08:00:00Z')).toBe(Date.parse('2026-09-18T08:00:00Z'));
    expect(parseIsoMs('2026-09-18T08:00:00+02:00')).toBe(Date.parse('2026-09-18T08:00:00+02:00'));
    expect(parseIsoMs(null)).toBeNull();
    expect(parseIsoMs('')).toBeNull();
    expect(parseIsoMs('not a date')).toBeNull();
  });
});
