import { describe, expect, it } from 'vitest';

import { SESSION_TITLE_MAX_LENGTH, SessionRecord } from '../src';
import type { CellModel, CellPatch, UserCellModel } from '../src';

/**
 * `SessionRecord` is the package's mutable state and the source of every `CellPatch`
 * a view applies. Two invariants are pinned here and nowhere else at this level:
 *
 * 1. **cells only change through the mutation helpers**, and every helper journals
 *    exactly one patch that reproduces its effect;
 * 2. the lookup maps the reducer needs (tool call → cell, request → pending cell,
 *    ask → cell) stay consistent with the array across every path — including
 *    `remove` and `clear`.
 *
 * The host's integration suite covers the same object through the product path
 * (`extensions/vscode/tests/host/session.*.test.ts`); these tests pin the contract
 * the record itself promises, with no host, no socket and no bridge in between.
 */

let clock = 1_000;

function makeRecord(options: { readonly workspace?: string | null } = {}): SessionRecord {
  clock = 1_000;
  return new SessionRecord({
    sessionId: 's-1',
    now: () => clock,
    workspace: options.workspace ?? null,
    createdAt: '2026-09-18T08:00:00.000',
  });
}

function assistant(record: SessionRecord, text: string): CellModel {
  const cell: CellModel = {
    kind: 'assistant',
    id: record.newCellId(),
    createdAt: record.now(),
    text,
    streaming: true,
  };
  record.pushCell(cell);
  return cell;
}

describe('cell mutations and the journal', () => {
  it('appends a cell and journals one append op', () => {
    const record = makeRecord();
    const cell = assistant(record, 'hello');

    expect(record.cells).toHaveLength(1);
    expect(record.cellById(cell.id)).toBe(cell);
    expect(record.takeJournal()).toEqual<CellPatch[]>([{ op: 'append', cell }]);
    // Taking the journal drains it: a second take sees nothing.
    expect(record.takeJournal()).toEqual([]);
  });

  it('inserts after an anchor, and throws for an unknown one', () => {
    const record = makeRecord();
    const first = assistant(record, 'first');
    record.takeJournal();
    const second: CellModel = {
      kind: 'separator',
      id: record.newCellId(),
      createdAt: record.now(),
      label: '',
    };

    expect(record.insertAfter(first.id, second)).toEqual<CellPatch>({
      op: 'insert_after',
      afterCellId: first.id,
      cell: second,
    });
    expect(record.cells.map((cell) => cell.id)).toEqual([first.id, second.id]);
    expect(() => record.insertAfter('nope', second)).toThrow(/unknown anchor/);
  });

  it('replaces a cell by id and refuses an unknown one', () => {
    const record = makeRecord();
    const cell = assistant(record, 'hi');
    record.takeJournal();

    const updated: CellModel = { ...cell, text: 'hi!' } as CellModel;
    expect(record.update(updated)).toEqual<CellPatch>({ op: 'update', cell: updated });
    expect(record.cellById(cell.id)).toBe(updated);
    expect(() => record.update({ ...updated, id: 'ghost' })).toThrow(/unknown cell/);
  });

  it('appends streamed text to every text-bearing kind and rejects the others', () => {
    const record = makeRecord();
    const assistantCell = assistant(record, 'a');
    const thinking: CellModel = {
      kind: 'thinking',
      id: record.newCellId(),
      createdAt: record.now(),
      text: 't',
      streaming: true,
      durationMs: null,
    };
    record.pushCell(thinking);
    const system: CellModel = {
      kind: 'system',
      id: record.newCellId(),
      createdAt: record.now(),
      level: 'info',
      text: 's',
    };
    record.pushCell(system);
    const user = record.addPendingUser('req-1', 'u').cell;
    const tool: CellModel = {
      kind: 'tool_call',
      id: record.newCellId(),
      createdAt: record.now(),
      toolCallId: 'tc-1',
      name: 'Bash',
      status: 'streaming',
      display: { title: 'Bash', subject: '' },
      argsText: '',
      args: null,
      result: null,
      startedAt: null,
      finishedAt: null,
    };
    record.pushCell(tool);
    record.takeJournal();

    for (const cell of [assistantCell, thinking, system, user]) {
      expect(record.appendText(cell.id, 'X')).toEqual<CellPatch>({
        op: 'append_text',
        cellId: cell.id,
        text: 'X',
      });
    }
    expect(record.cellById(assistantCell.id)).toMatchObject({ text: 'aX' });
    expect(record.cellById(user.id)).toMatchObject({ text: 'uX', state: 'pending' });
    // An empty delta is not a change: no patch, no journal entry.
    expect(record.appendText(assistantCell.id, '')).toBeNull();
    expect(() => record.appendText(tool.id, 'X')).toThrow(/cannot receive text/);
    expect(() => record.appendText('ghost', 'X')).toThrow(/unknown cell/);
  });

  it('removes a cell, reindexes the tail and forgets its lookups', () => {
    const record = makeRecord();
    const tool: CellModel = {
      kind: 'tool_call',
      id: record.newCellId(),
      createdAt: record.now(),
      toolCallId: 'tc-1',
      name: 'Bash',
      status: 'pending',
      display: { title: 'Bash', subject: 'ls' },
      argsText: '',
      args: null,
      result: null,
      startedAt: null,
      finishedAt: null,
    };
    record.pushCell(tool);
    const after = assistant(record, 'after');
    record.toolCells.set('tc-1', tool.id);
    record.takeJournal();

    expect(record.remove(tool.id)).toEqual<CellPatch>({ op: 'remove', cellId: tool.id });
    expect(record.cellById(tool.id)).toBeUndefined();
    expect(record.toolCells.has('tc-1')).toBe(false);
    // The tail is re-indexed: the surviving cell is still addressable.
    expect(record.cellById(after.id)).toBe(after);
    expect(record.cells).toHaveLength(1);
    expect(() => record.remove('ghost')).toThrow(/unknown cell/);
  });

  it('replaces the whole transcript and rebuilds the tool index', () => {
    const record = makeRecord();
    assistant(record, 'old');
    record.takeJournal();
    const cells: CellModel[] = [
      {
        kind: 'user',
        id: 'u1',
        createdAt: record.now(),
        text: 'replaced',
        state: 'accepted',
      },
      {
        kind: 'tool_call',
        id: 't1',
        createdAt: record.now(),
        toolCallId: 'tc-9',
        name: 'Read',
        status: 'pending',
        display: { title: 'Read', subject: 'a.ts' },
        argsText: '',
        args: null,
        result: null,
        startedAt: null,
        finishedAt: null,
      },
    ];

    expect(record.replaceAll(cells)).toEqual<CellPatch>({ op: 'replace_all', cells });
    expect(record.cells).toEqual(cells);
    expect(record.toolCells.get('tc-9')).toBe('t1');
  });
});

describe('pending user messages', () => {
  it('registers, promotes and discards them through their request ids', () => {
    const record = makeRecord();
    const { cell } = record.addPendingUser('req-1', 'queued');
    expect(cell.state).toBe('pending');
    expect(record.pendingRequests.get('req-1')).toBe(cell.id);

    const promoted = record.promotePending('req-1');
    expect(promoted).toMatchObject({ op: 'update' });
    expect(record.cellById(cell.id)).toMatchObject({ state: 'accepted' });
    expect(record.pendingRequests.size).toBe(0);
    // Promoting twice is a no-op, not an error.
    expect(record.promotePending('req-1')).toBeNull();
  });

  it('keeps a pending message out of "the last committed cell"', () => {
    const record = makeRecord();
    assistant(record, 'answer');
    const { cell } = record.addPendingUser('req-1', 'queued');

    expect(record.lastCommittedCell()?.kind).toBe('assistant');
    record.promotePending('req-1');
    expect(record.lastCommittedCell()?.id).toBe(cell.id);
  });

  it('promotes or discards every pending message in one call', () => {
    const record = makeRecord();
    record.addPendingUser('req-1', 'one');
    record.addPendingUser('req-2', 'two');

    const promoted = record.promoteAllPending();
    expect(promoted).toHaveLength(2);
    expect(record.cells.every((c) => (c as UserCellModel).state === 'accepted')).toBe(true);

    record.addPendingUser('req-3', 'three');
    const discarded = record.discardAllPending();
    expect(discarded).toHaveLength(1);
    expect(record.cells.at(-1)).toMatchObject({ state: 'discarded' });
    expect(record.pendingRequests.size).toBe(0);
  });

  it('drops a pending message entirely when it never left the client', () => {
    const record = makeRecord();
    const { cell } = record.addPendingUser('req-1', 'never sent');

    expect(record.removePending('req-1')).toEqual<CellPatch>({ op: 'remove', cellId: cell.id });
    expect(record.cells).toHaveLength(0);
    expect(record.removePending('req-1')).toBeNull();
  });

  it('queues a pending message behind the transcript, not after it', () => {
    const record = makeRecord();
    assistant(record, 'streaming answer');
    record.addPendingUser('req-1', 'queued');

    // The host's own "last cell" rule: pending messages are visible but never the
    // anchor a streamed delta attaches to.
    expect(record.cells.map((cell) => cell.kind)).toEqual(['assistant', 'user']);
  });
});

describe('asks and status', () => {
  it('tracks awaiting asks and cancels them at turn end', () => {
    const record = makeRecord();
    const ask: CellModel = {
      kind: 'ask',
      id: record.newCellId(),
      createdAt: record.now(),
      requestId: 'tc-1',
      sessionId: record.sessionId,
      questions: [],
      state: 'awaiting',
      answers: [],
      approval: true,
    };
    record.pushCell(ask);
    record.registerAsk('tc-1', ask.id);

    expect(record.hasAwaitingAsk()).toBe(true);
    expect(record.refreshStatus()).toBe(true);
    expect(record.status).toBe('waiting-for-input');

    const patches = record.cancelAwaitingAsks();
    expect(patches).toHaveLength(1);
    expect(record.cellById(ask.id)).toMatchObject({ state: 'cancelled' });
    expect(record.hasAwaitingAsk()).toBe(false);
    expect(record.resolveAsk('tc-1')).toBeNull();
  });

  it('derives the status from the ask / turn state, and flags only the transitions', () => {
    const record = makeRecord();
    expect(record.status).toBe('idle');

    record.turn = { active: true, startedAtMs: record.now(), lastResult: null };
    expect(record.refreshStatus()).toBe(true);
    expect(record.status).toBe('working');
    // Same input twice: no change, so the host does not re-post `state`/`tabs`.
    expect(record.refreshStatus()).toBe(false);

    record.turn = { active: false, startedAtMs: 0, lastResult: null };
    record.refreshStatus();
    expect(record.status).toBe('idle');
  });
});

describe('title, draft and errors', () => {
  it('derives the title from the explicit name, the first user message, then the workspace', () => {
    const record = makeRecord({ workspace: '/tmp/work/my-project' });
    expect(record.title).toBe('my-project');

    const { cell } = record.addPendingUser('req-1', 'x'.repeat(SESSION_TITLE_MAX_LENGTH + 20));
    record.promotePending('req-1');
    record.refreshTitle();
    expect(record.title).toBe('x'.repeat(SESSION_TITLE_MAX_LENGTH));
    expect(record.cellById(cell.id)).toBeDefined();

    record.explicitTitle = 'Explicit';
    record.refreshTitle();
    expect(record.title).toBe('Explicit');
  });

  it('falls back to the placeholder with nothing to derive from', () => {
    const record = makeRecord();
    expect(record.title).toBe('New session');
  });

  it('burns a draft token only when a draft is installed (one-shot restore)', () => {
    const record = makeRecord();
    expect(record.draftSeq).toBe(0);

    record.setDraft('restored');
    expect(record.draftSeq).toBe(1);
    // Consuming clears the local copy without burning a token: the *same* text can
    // be restored twice (two failed sends), while a re-sent `state` cannot clobber
    // typing.
    record.consumeDraft();
    expect(record.draft).toBeNull();
    expect(record.draftSeq).toBe(1);

    record.setDraft('restored');
    expect(record.draftSeq).toBe(2);
    record.setDraft(null);
    expect(record.draftSeq).toBe(2);
  });

  it('truncates the last error and reports it once', () => {
    const record = makeRecord();
    record.setLastError('y'.repeat(1_000));
    expect(record.lastError).toHaveLength(400);
    expect(record.lastError?.endsWith('...')).toBe(true);

    record.dirtyState = false;
    record.clearLastError();
    expect(record.lastError).toBeNull();
    expect(record.dirtyState).toBe(true);
    // Clearing twice is not a change (no extra `state` post).
    record.dirtyState = false;
    record.clearLastError();
    expect(record.dirtyState).toBe(false);
  });
});

describe('snapshots and turn metrics', () => {
  it('hands out the state snapshot without cells and the view snapshot with a copy', () => {
    const record = makeRecord();
    assistant(record, 'hi');

    const state = record.stateModel();
    expect(state.sessionId).toBe('s-1');
    expect('cells' in state).toBe(false);

    const view = record.viewModel();
    expect(view.cells).toEqual(record.cells);
    expect(view.cells).not.toBe(record.cells);
  });

  it('emits the turn metrics cell exactly once, with the turn duration', () => {
    const record = makeRecord();
    record.turn = { active: true, startedAtMs: 500, lastResult: null };
    clock = 1_750;
    record.recordTurnUsage(
      { promptTokens: 10, completionTokens: 5, cachedTokens: 2, tokensPerSecond: 3, ttftMs: 40 },
      'test-model',
    );

    const first = record.takeMetricsCell();
    expect(first).toEqual({
      usage: {
        promptTokens: 10,
        completionTokens: 5,
        cachedTokens: 2,
        tokensPerSecond: 3,
        ttftMs: 40,
      },
      durationMs: 1_250,
      model: 'test-model',
    });
    // One metrics cell per turn, even if `finishTurn` runs twice.
    expect(record.takeMetricsCell()).toBeNull();
    record.resetTurnUsage();
    expect(record.takeMetricsCell()).toBeNull();
  });

  it('clears the reduction anchors but keeps the draft token monotone', () => {
    const record = makeRecord();
    const cell = assistant(record, 'streaming');
    record.lastAssistantCellId = cell.id;
    record.thinkingStartedAtMs = 5;
    record.setToolArgsStream('tc-1', {
      text: '{',
      renderedLength: 1,
      renderedAtMs: 1,
      fragmentsSinceRender: 0,
    });
    record.toolCells.set('tc-1', cell.id);
    record.pendingRequests.set('req-1', cell.id);
    record.setDraft('text');

    record.clear();

    expect(record.cells).toHaveLength(0);
    expect(record.cellById(cell.id)).toBeUndefined();
    expect(record.lastAssistantCellId).toBeNull();
    expect(record.thinkingStartedAtMs).toBeNull();
    expect(record.toolArgsStreamFor('tc-1')).toBeUndefined();
    expect(record.toolCells.size).toBe(0);
    expect(record.pendingRequests.size).toBe(0);
    // A replay that re-installs a draft must still look newer than the one a view
    // adopted before the replay.
    expect(record.draftSeq).toBe(1);
  });
});
