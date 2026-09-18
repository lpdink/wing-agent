import { afterEach, describe, expect, it } from 'vitest';

import type { CellModel } from '../../src/shared';
import { createHostHarness, flushMicrotasks, textPayload } from './support/harness';
import type { HostHarness } from './support/harness';
import { kinds } from './support/mirror';

/**
 * Replay == live: the `sync_session` replay and the event stream must produce
 * the same model through the same code path, and the patch stream the webview
 * receives must reconstruct exactly the host's transcript.
 *
 * Every test here also feeds the captured messages through `WebviewMirror`
 * (the shipped `applyCellPatches`), so "no duplicates, no losses, no misses"
 * is checked against the real consumer, not against a re-implementation.
 */

const teardown: HostHarness[] = [];
afterEach(() => {
  while (teardown.length > 0) {
    teardown.pop()?.dispose();
  }
});

/**
 * Compare two transcripts on everything that is *content*.
 *
 * Cell ids and every wall-clock field are dropped: they are per-path by nature
 * (a replayed tool call has no measured `startedAt`, a replayed thinking block
 * has no measured duration, and the history carries no failure flag for tool
 * results either). Anything else differing is a replay ≠ live bug.
 */
function stableCells(cells: readonly CellModel[]): unknown[] {
  return cells.map((cell, index) => {
    const copy: Record<string, unknown> = { ...cell, id: `#${index}` };
    if ('createdAt' in copy) {
      copy['createdAt'] = 0;
    }
    if ('startedAt' in copy) {
      copy['startedAt'] = null;
      copy['finishedAt'] = null;
    }
    if ('durationMs' in copy) {
      copy['durationMs'] = null;
    }
    return copy;
  });
}

/** One conversation, twice: once as a live event stream, once as a replay. */
async function runLiveConversation(): Promise<HostHarness> {
  const harness = createHostHarness();
  teardown.push(harness);
  await harness.boot();
  const sessionId = harness.gateway.createdOrder[0] ?? '';
  harness.gateway.emit({
    type: 'tool_call',
    tool_name: 'Bash',
    tool_args: { command: 'ls -la' },
    tool_call_id: 'tc-1',
    session_id: sessionId,
  });
  harness.gateway.emit({
    type: 'tool_call_result',
    tool_name: 'Bash',
    tool_args: { command: 'ls -la' },
    tool_call_id: 'tc-1',
    tool_result: 'file.txt',
    tool_success: true,
    model: 'test-model',
    session_id: sessionId,
  });
  harness.gateway.emit(textPayload('after the tool', sessionId));
  harness.gateway.emit({ type: 'done', session_id: sessionId });
  await flushMicrotasks();
  return harness;
}

async function runReplayedConversation(): Promise<HostHarness> {
  const harness = createHostHarness();
  teardown.push(harness);
  harness.gateway.seedOnCreate = {
    messages: [
      {
        role: 'assistant',
        content: '',
        tool_calls: [{ id: 'tc-1', name: 'Bash', arguments: { command: 'ls -la' } }],
        uuid: 'm1',
      },
      { role: 'tool', tool_call_id: 'tc-1', content: 'file.txt', uuid: 'm2' },
      { role: 'assistant', content: 'after the tool', uuid: 'm3' },
    ],
  };
  await harness.boot();
  await flushMicrotasks();
  return harness;
}

describe('replay assembly', () => {
  it('rebuilds messages → uncommitted → tools → events and hydrates the webview', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = {
      name: 'Replay order',
      messages: [
        { role: 'user', content: 'first', uuid: 'm1' },
        {
          role: 'assistant',
          content: '',
          reasoning_content: 'hmm',
          tool_calls: [{ id: 'tc-edit', name: 'Edit', arguments: { path: 'a.ts' } }],
          uuid: 'm2',
        },
        { role: 'tool', tool_call_id: 'tc-edit', content: 'ok', uuid: 'm3' },
      ],
      uncommitted: {
        role: 'assistant',
        content: '',
        reasoning_content: 'still thinking',
        uuid: 'm4',
      },
      uncommittedTools: [{ tool_call_id: 'tc-stream', tool_name: 'Bash', args_fragment: '{"command":"pn' }],
      events: [
        {
          type: 'diff_content',
          path: 'a.ts',
          old_text: 'old line',
          new_text: 'new line',
          old_start_line: 4,
          new_start_line: 4,
          tool_call_id: 'tc-edit',
        },
      ],
      turnStartedAt: '2026-09-18T08:00:00.000',
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    const hydrate = harness.hydrateFor(sessionId);
    expect(hydrate).toBeDefined();
    const cells = hydrate?.session.cells ?? [];

    expect(kinds(cells)).toEqual([
      'user', // messages: first
      'thinking', // m2 reasoning
      'tool_call', // m2 tool_calls
      'diff', // events: anchored after tc-edit
      'separator', // ReAct rule: the uncommitted thinking block follows a tool call
      'thinking', // uncommitted reasoning
      'tool_call', // uncommitted_tools: tc-stream (streaming args)
    ]);
    const streamed = cells[6];
    expect(streamed?.kind === 'tool_call' && streamed.argsText).toBe('{"command":"pn');
    expect(streamed?.kind === 'tool_call' && streamed.status).toBe('streaming');
    // Mid-turn replay restores the elapsed-time anchor.
    expect(hydrate?.session.turn.active).toBe(true);
    expect(hydrate?.session.status).toBe('working');
  });

  it('continues a replayed streaming tool call with live fragments (same path)', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = {
      uncommittedTools: [{ tool_call_id: 'tc-stream', tool_name: 'Bash', args_fragment: '{"command":"pn' }],
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.emit({
      type: 'tool_call_stream',
      tool_call_id: 'tc-stream',
      tool_name: 'Bash',
      args_fragment: 'pm test"}',
      is_final: true,
      session_id: sessionId,
    });
    await flushMicrotasks();

    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    const cells = view.cells(sessionId);
    expect(kinds(cells)).toEqual(['tool_call']);
    const tool = cells[0];
    expect(tool?.kind === 'tool_call' && tool.argsText).toBe('{"command":"pnpm test"}');
    expect(tool?.kind === 'tool_call' && tool.status).toBe('pending');
    // The partial parse drives the one-line subject while the args stream.
    expect(tool?.kind === 'tool_call' && tool.display.subject).toBe('pnpm test');
    // The webview's mirror has exactly what the host has.
    expect(view.cells(sessionId)).toEqual(harness.host.sessionManager.record(sessionId)?.cells);
  });

  it('anchors live diffs to their tool call and keeps sibling order', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'Edit',
      tool_args: { path: 'f.txt' },
      tool_call_id: 'tc-e',
      session_id: sessionId,
    });
    for (const index of [0, 1, 2]) {
      harness.gateway.emit({
        type: 'diff_content',
        path: 'f.txt',
        old_text: `old ${index}`,
        new_text: `new ${index}`,
        old_start_line: 10 + index,
        new_start_line: 10 + index,
        tool_call_id: 'tc-e',
        session_id: sessionId,
      });
    }
    // A diff whose tool call was never seen falls back to append.
    harness.gateway.emit({
      type: 'diff_content',
      path: 'orphan.txt',
      old_text: null,
      new_text: 'content',
      tool_call_id: 'tc-missing',
      session_id: sessionId,
    });
    await flushMicrotasks();

    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    expect(kinds(view.cells(sessionId))).toEqual(['tool_call', 'diff', 'diff', 'diff', 'diff']);
    const record = harness.host.sessionManager.record(sessionId);
    const diffs = (record?.cells ?? []).filter((cell) => cell.kind === 'diff');
    // core normalises a missing `old_start_line` to 1 ("no window information").
    expect(diffs.map((cell) => (cell.kind === 'diff' ? cell.oldStartLine : 0))).toEqual([10, 11, 12, 1]);
    // New-file payload: every row is an addition, and the hunk header is git's.
    const orphan = diffs[3];
    if (orphan?.kind === 'diff') {
      expect(orphan.lines[0]?.text).toBe('@@ -0,0 +1,1 @@');
      expect(orphan.lines[1]?.kind).toBe('add');
      expect(orphan.added).toBe(1);
    }
  });

  it('renders a windowed diff with absolute line numbers', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'Edit',
      tool_args: { path: 'main.rs' },
      tool_call_id: 'tc-w',
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'diff_content',
      path: 'main.rs',
      old_text: 'line 7\nline 8\nold 9\nline 10',
      new_text: 'line 7\nline 8\nnew 9\nline 10',
      old_start_line: 7,
      new_start_line: 7,
      tool_call_id: 'tc-w',
      session_id: sessionId,
    });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    const diff = (record?.cells ?? []).find((cell) => cell.kind === 'diff');
    if (diff?.kind !== 'diff') {
      throw new Error('expected a diff cell');
    }
    expect(diff.lines[0]?.text).toBe('@@ -7,4 +7,4 @@');
    expect(diff.lines.map((line) => [line.kind, line.oldLine, line.newLine])).toEqual([
      ['hunk', null, null],
      ['context', 7, 7],
      ['context', 8, 8],
      ['del', 9, null],
      ['add', null, 9],
      ['context', 10, 10],
    ]);
    expect(diff.added).toBe(1);
    expect(diff.removed).toBe(1);
    expect(diff.truncated).toBe(false);
  });

  it('inserts a todo cell under a successful TodoWrite and keeps out-of-order results paired', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'TodoWrite',
      tool_args: { todos: [{ content: 'first task', status: 'in_progress' }] },
      tool_call_id: 'tc-todo',
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'Bash',
      tool_args: { command: 'ls' },
      tool_call_id: 'tc-bash',
      session_id: sessionId,
    });
    // Bash finishes first (concurrent execution) — pairing is by id.
    harness.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'Bash',
      tool_args: { command: 'ls' },
      tool_call_id: 'tc-bash',
      tool_result: 'file.txt',
      tool_success: true,
      model: 'm',
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'TodoWrite',
      tool_args: { todos: [{ content: 'first task', status: 'in_progress' }] },
      tool_call_id: 'tc-todo',
      tool_result: 'Todo updated.',
      tool_success: true,
      model: 'm',
      session_id: sessionId,
    });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    const cells = record?.cells ?? [];
    expect(kinds(cells)).toEqual(['tool_call', 'todo', 'tool_call']);
    const todo = cells[1];
    expect(todo?.kind === 'todo' && todo.items).toEqual([{ content: 'first task', status: 'in_progress' }]);
    const bash = cells[2];
    expect(bash?.kind === 'tool_call' && bash.status).toBe('success');
    expect(bash?.kind === 'tool_call' && bash.result?.text).toBe('file.txt');

    // A failed TodoWrite leaves the tool call without a todo cell.
    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'TodoWrite',
      tool_args: { todos: [] },
      tool_call_id: 'tc-bad',
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'TodoWrite',
      tool_args: { todos: [] },
      tool_call_id: 'tc-bad',
      tool_result: 'boom',
      tool_success: false,
      model: 'm',
      session_id: sessionId,
    });
    await flushMicrotasks();
    expect(kinds(record?.cells ?? [])).toEqual(['tool_call', 'todo', 'tool_call', 'tool_call']);
  });

  it('inserts a ReAct separator before text that follows a tool call', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'Bash',
      tool_args: { command: 'ls' },
      tool_call_id: 'tc-1',
      session_id: sessionId,
    });
    harness.gateway.emit(textPayload('after tool', sessionId));
    harness.gateway.emit({ ...textPayload(' more', sessionId) });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(kinds(record?.cells ?? [])).toEqual(['tool_call', 'separator', 'assistant']);
    const assistant = record?.cells[2];
    expect(assistant?.kind === 'assistant' && assistant.text).toBe('after tool more');
  });

  it('finalizes streaming flags and thinking duration when the turn ends', async () => {
    const now = () => 1_000;
    const harness = createHostHarness({ now });
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    harness.gateway.emit({ type: 'reasoning', content: 'pondering', session_id: sessionId });
    harness.gateway.emit(textPayload('answer', sessionId));
    await flushMicrotasks();

    let record = harness.host.sessionManager.record(sessionId);
    expect(kinds(record?.cells ?? [])).toEqual(['thinking', 'assistant']);

    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();

    record = harness.host.sessionManager.record(sessionId);
    const thinking = record?.cells[0];
    const assistant = record?.cells[1];
    expect(thinking?.kind === 'thinking' && thinking.streaming).toBe(false);
    expect(thinking?.kind === 'thinking' && thinking.durationMs).toBe(0);
    expect(assistant?.kind === 'assistant' && assistant.streaming).toBe(false);
    expect(record?.status).toBe('idle');
  });

  it('emits one metrics cell per finished turn and accumulates totals', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    harness.gateway.emit({
      type: 'llm_call_metrics',
      model: 'test-model',
      prompt_tokens: 100,
      completion_tokens: 20,
      cached_tokens: 80,
      first_chunk_rt_ms: 120,
      tokens_per_sec: 30,
      stop_reason: 'end_turn',
      session_id: sessionId,
    });
    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(kinds(record?.cells ?? [])).toEqual(['metrics']);
    const metrics = record?.cells[0];
    expect(metrics?.kind === 'metrics' && metrics.usage.promptTokens).toBe(100);
    expect(metrics?.kind === 'metrics' && metrics.model).toBe('test-model');
    expect(record?.totals).toEqual({ promptTokens: 100, completionTokens: 20, cachedTokens: 80 });

    // A second turn adds one more cell — never a duplicate for the first turn.
    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    harness.gateway.emit({
      type: 'llm_call_metrics',
      model: 'test-model',
      prompt_tokens: 5,
      completion_tokens: 1,
      cached_tokens: 0,
      first_chunk_rt_ms: 10,
      tokens_per_sec: 2,
      stop_reason: null,
      session_id: sessionId,
    });
    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();
    expect(kinds(record?.cells ?? [])).toEqual(['metrics', 'metrics']);
    expect(record?.totals.promptTokens).toBe(105);
  });

  it('degrades unknown and malformed events without breaking the stream', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    harness.gateway.lastSocket.emit({ type: 'from_the_future', payload: 1, session_id: sessionId });
    harness.gateway.lastSocket.emit({ type: 'text', session_id: sessionId }); // missing `content`
    harness.gateway.emit(textPayload('survives', sessionId));
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(kinds(record?.cells ?? [])).toEqual(['assistant']);
    expect(record?.cells[0]?.kind === 'assistant' && record.cells[0].text).toBe('survives');
  });
});

describe('pending user messages', () => {
  it('promotes on acceptance, keeps the cell in place, and updates the title', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    await harness.intent({ type: 'sendMessage', sessionId, text: 'Summarize the repository.' });
    const frame = harness.clientFrames().find((candidate) => candidate['session_id'] === sessionId);
    const rawRequestId = frame?.['request_id'];
    const requestId = typeof rawRequestId === 'string' ? rawRequestId : '';

    let record = harness.host.sessionManager.record(sessionId);
    expect(record?.cells).toHaveLength(1);
    expect(record?.cells[0]?.kind === 'user' && record.cells[0].state).toBe('pending');
    // The title updates the moment the message is sent (not on tab switch).
    expect(record?.title).toBe('Summarize the repository.');
    const tabTitles = harness.ofType('tabs').map((message) => message.tabs[0]?.title);
    expect(tabTitles).toContain('Summarize the repository.');

    harness.gateway.emit({
      type: 'user_message_accepted',
      content: 'Summarize the repository.',
      origin_request_id: requestId,
      session_id: sessionId,
    });
    await flushMicrotasks();

    record = harness.host.sessionManager.record(sessionId);
    expect(record?.cells).toHaveLength(1);
    expect(record?.cells[0]?.kind === 'user' && record.cells[0].state).toBe('accepted');
    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    expect(view.cells(sessionId)).toEqual(record?.cells);
  });

  it('keeps pending messages at the tail while other cells arrive', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.gateway.seedOnCreate = {};
    harness.wipe();

    await harness.intent({ type: 'sendMessage', sessionId, text: 'queued behind the turn' });
    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    harness.gateway.emit(textPayload('streaming answer', sessionId));
    await flushMicrotasks();

    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    // The committed assistant cell is inserted *before* the queued message: the
    // pending bubble stays at the bottom (TUI renders pending below content),
    // which is also where the backend will place the message once committed.
    expect(kinds(view.cells(sessionId))).toEqual(['assistant', 'user']);
    const record = harness.host.sessionManager.record(sessionId);
    expect(record?.cells).toHaveLength(2);
    expect(record?.cells[1]?.kind === 'user' && record.cells[1].state).toBe('pending');
  });

  it('discards pending messages on interrupt and accepts them on done', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    await harness.intent({ type: 'sendMessage', sessionId, text: 'message one' });
    harness.gateway.emit({ type: 'interrupted', session_id: sessionId });
    await flushMicrotasks();

    let record = harness.host.sessionManager.record(sessionId);
    expect(record?.cells[0]?.kind === 'user' && record.cells[0].state).toBe('discarded');
    const toasts = harness
      .ofType('ui')
      .filter((message) => message.action.kind === 'toast')
      .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
    expect(toasts).toContain('Agent interrupted');

    // A second message accepted through `done` (the turn-end safety net).
    harness.gateway.emit({ type: 'user_message_accepted', content: 'x', origin_request_id: 'other' });
    await harness.intent({ type: 'sendMessage', sessionId, text: 'message two' });
    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();

    record = harness.host.sessionManager.record(sessionId);
    const states = (record?.cells ?? []).map((cell) => (cell.kind === 'user' ? cell.state : cell.kind));
    expect(states).toEqual(['discarded', 'accepted']);
  });

  it('drops the pending cell when the frame cannot be sent', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';

    // Kill the socket without telling the client, then send: the frame write
    // throws, and the optimistic cell must not linger.
    const socket = harness.gateway.lastSocket;
    socket.readyState = 3;
    harness.wipe();
    await harness.intent({ type: 'sendMessage', sessionId, text: 'doomed' });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(record?.cells.filter((cell) => cell.kind === 'user')).toHaveLength(0);
    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    expect(view.cells(sessionId)).toHaveLength(0);
  });
});

describe('patch stream integrity', () => {
  it('keeps seq strictly +1 across a long replay + live mix', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = {
      messages: [{ role: 'user', content: 'seed', uuid: 'm1' }],
      events: [
        {
          type: 'diff_content',
          path: 'a.ts',
          old_text: 'a',
          new_text: 'b',
          tool_call_id: 'tc-none',
        },
      ],
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    for (let index = 0; index < 20; index += 1) {
      harness.gateway.emit(textPayload(`chunk ${index}`, sessionId));
      harness.gateway.emit({
        type: 'tool_call_stream',
        tool_call_id: 'tc-live',
        tool_name: 'Write',
        args_fragment: `{"path":"f${index}.ts","content":"${'x'.repeat(index)}`,
        is_final: false,
        session_id: sessionId,
      });
    }
    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();

    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    const record = harness.host.sessionManager.record(sessionId);
    expect(view.cells(sessionId)).toEqual(record?.cells);
    expect(view.seq(sessionId)).toBe(record?.seq);
  });
});

describe('sync replacement', () => {
  it('replaces the transcript when a sync arrives on the live lane (rewind)', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.gateway.emit(textPayload('before rewind', sessionId));
    await flushMicrotasks();
    expect(harness.host.sessionManager.record(sessionId)?.cells).toHaveLength(1);

    harness.wipe();
    harness.gateway.session(sessionId).messages = [{ role: 'user', content: 'kept message', uuid: 'm9' }];
    harness.gateway.session(sessionId).draft = 'kept message';
    harness.gateway.pushSyncToSubscribers(sessionId);
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(kinds(record?.cells ?? [])).toEqual(['user']);
    expect(record?.cells[0]?.kind === 'user' && record.cells[0].text).toBe('kept message');
    // The draft rides along and is consumed exactly once.
    const hydrates = harness.ofType('hydrate');
    expect(hydrates[hydrates.length - 1]?.session.draft).toBe('kept message');
    expect(record?.draft).toBeNull();

    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    expect(view.cells(sessionId)).toEqual(record?.cells);
  });
});

describe('replay == live', () => {
  it('produces the same cells for the same conversation on both lanes', async () => {
    const live = await runLiveConversation();
    const replayed = await runReplayedConversation();
    const liveId = live.gateway.createdOrder[0] ?? '';
    const replayId = replayed.gateway.createdOrder[0] ?? '';

    const liveCells = live.host.sessionManager.record(liveId)?.cells ?? [];
    const replayCells = replayed.host.sessionManager.record(replayId)?.cells ?? [];

    // The ReAct separator is part of the transcript (the TUI inserts it in the
    // shared `push`, so both lanes must agree on it).
    expect(kinds(liveCells)).toEqual(['tool_call', 'separator', 'assistant']);
    expect(kinds(replayCells)).toEqual(kinds(liveCells));
    expect(stableCells(replayCells)).toEqual(stableCells(liveCells));
  });

  it('produces the same cells when a replayed tool call gains live fragments', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = {
      messages: [
        {
          role: 'assistant',
          content: '',
          tool_calls: [{ id: 'tc-1', name: 'Bash', arguments: { command: 'ls -la' } }],
          uuid: 'm1',
        },
        { role: 'tool', tool_call_id: 'tc-1', content: 'file.txt', uuid: 'm2' },
      ],
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    const replayed = stableCells(harness.host.sessionManager.record(sessionId)?.cells ?? []);

    // The continuation arriving as a live event (same order, same content).
    harness.gateway.emit(textPayload('after the tool', sessionId));
    await flushMicrotasks();

    const afterEvents = stableCells(harness.host.sessionManager.record(sessionId)?.cells ?? []);
    expect(afterEvents.slice(0, replayed.length)).toEqual(replayed);
    expect(kinds(harness.host.sessionManager.record(sessionId)?.cells ?? [])).toEqual([
      'tool_call',
      'separator',
      'assistant',
    ]);
  });
});

describe('turn accounting', () => {
  it('does not re-emit the previous turn usage for a turn without LLM calls', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';

    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    harness.gateway.emit({
      type: 'llm_call_metrics',
      model: 'test-model',
      prompt_tokens: 100,
      completion_tokens: 20,
      cached_tokens: 80,
      first_chunk_rt_ms: 120,
      tokens_per_sec: 30,
      stop_reason: 'end_turn',
      session_id: sessionId,
    });
    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();
    // A second turn that never reached the model (interrupt / LLM failure /
    // rejected prompt command) must not publish the first turn's numbers.
    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    harness.gateway.emit({ type: 'interrupted', session_id: sessionId });
    await flushMicrotasks();

    const metrics = (harness.host.sessionManager.record(sessionId)?.cells ?? []).filter(
      (cell) => cell.kind === 'metrics',
    );
    expect(metrics).toHaveLength(1);
    expect(metrics[0]?.kind === 'metrics' && metrics[0].usage).toMatchObject({
      promptTokens: 100,
      completionTokens: 20,
      cachedTokens: 80,
    });

    const totals = harness.host.sessionManager.record(sessionId)?.totals;
    expect(totals).toMatchObject({
      promptTokens: 100,
      completionTokens: 20,
      cachedTokens: 80,
    });
  });
});
