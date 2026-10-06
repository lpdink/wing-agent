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
      'thinking', // uncommitted reasoning — behind the diff, so no separator
      'tool_call', // uncommitted_tools: tc-stream (streaming args)
    ]);
    const streamed = cells[cells.length - 1];
    expect(streamed?.kind === 'tool_call' && streamed.argsText).toBe('{"command":"pn');
    expect(streamed?.kind === 'tool_call' && streamed.status).toBe('streaming');
    // Mid-turn replay restores the elapsed-time anchor.
    expect(hydrate?.session.turn.active).toBe(true);
    expect(hydrate?.session.status).toBe('working');
  });

  it('restores a working turn from the snapshot status alone (empty projections)', async () => {
    // The regression: the turn is in flight but nothing is finalized yet — the
    // first LLM call of a round is still in flight, or we are at a round
    // boundary. Both projections are empty; `status` is the authority, so the
    // turn (and its elapsed anchor) must still come up. Inferring idle from an
    // empty projection left the tab spinning-less while live events arrived.
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = {
      messages: [{ role: 'user', content: 'go', uuid: 'm1' }],
      status: 'working',
      turnStartedAt: '2026-09-18T08:00:00.000',
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    const hydrate = harness.hydrateFor(sessionId);

    expect(hydrate?.session.turn.active).toBe(true);
    expect(hydrate?.session.status).toBe('working');
    // Naive ISO timestamps are read as UTC (the backend sends UTC).
    expect(hydrate?.session.turn.startedAtMs).toBe(Date.parse('2026-09-18T08:00:00.000Z'));
  });

  it('restores a waiting snapshot as a running turn (blocked on an ask)', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = { status: 'waiting', turnStartedAt: '2026-09-18T08:00:00.000' };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    expect(harness.hydrateFor(sessionId)?.session.turn.active).toBe(true);
  });

  it('lets an idle status beat a stale projection (status is authoritative)', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    harness.gateway.seedOnCreate = {
      status: 'idle',
      uncommitted: { role: 'assistant', content: 'partial', uuid: 'm9' },
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    const hydrate = harness.hydrateFor(sessionId);
    // The content still replays — only the *turn state* follows the status.
    expect(hydrate?.session.cells.some((cell) => cell.kind === 'assistant')).toBe(true);
    expect(hydrate?.session.turn.active).toBe(false);
  });

  it('clears a stale running turn when the snapshot says idle', async () => {
    // The mirror image: the turn ran, the tab showed it running, then a
    // reconnect replays a snapshot taken after the turn ended (its `done` was
    // missed while disconnected). The idle snapshot has to clear the running
    // state instead of leaving the tab spinning forever.
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    const clientId = harness.gateway.sockets[0]?.clientId ?? '';

    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    await flushMicrotasks();
    await harness.ready();
    expect(harness.hydrateFor(sessionId)?.session.turn.active).toBe(true);

    harness.gateway.pushSync(clientId, sessionId);
    await flushMicrotasks();
    await harness.ready();
    expect(harness.hydrateFor(sessionId)?.session.turn.active).toBe(false);
  });

  it('drops a sync_session that does not state the status (nothing half-applied)', async () => {
    // The snapshot must state the turn state; a payload without it is a
    // protocol error and must not replace the view (a half-applied replay is
    // worse than a dropped one — the client cannot know what the old one said).
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    await harness.ready();
    const before = harness.hydrateFor(sessionId)?.session.cells.length ?? 0;

    harness.gateway.emit({
      type: 'sync_session',
      session_id: sessionId,
      messages: [{ role: 'user', content: 'never applied', uuid: 'mx' }],
      created_at: '2026-09-18T08:00:00.000Z',
      request_id: 'broken',
    });
    await flushMicrotasks();
    await harness.ready();

    const hydrate = harness.hydrateFor(sessionId);
    expect(hydrate?.session.cells.length).toBe(before);
    expect(hydrate?.session.cells.some((cell) => cell.kind === 'user' && cell.text === 'never applied')).toBe(
      false,
    );
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
    // 旧网关形态：不带 dropped_request_ids → 回落"全部丢弃"。
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

  it('discards only the reported requests and keeps messages that arrived mid-interrupt', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    await harness.intent({ type: 'sendMessage', sessionId, text: 'dropped by interrupt' });
    let record = harness.host.sessionManager.record(sessionId);
    const droppedId = [...(record?.pendingRequests.keys() ?? [])][0] ?? '';

    await harness.intent({ type: 'sendMessage', sessionId, text: 'sent while interrupting' });
    record = harness.host.sessionManager.record(sessionId);
    const liveId = [...(record?.pendingRequests.keys() ?? [])].find((id) => id !== droppedId) ?? '';

    harness.gateway.emit({
      type: 'interrupted',
      session_id: sessionId,
      dropped_request_ids: [droppedId],
    });
    await flushMicrotasks();

    // 按文本断言（此时 transcript 为空，pending 的相对顺序是实现的内部形态）。
    const stateOf = (text: string): string | undefined => {
      const cell = harness.host.sessionManager
        .record(sessionId)
        ?.cells.find((candidate) => candidate.kind === 'user' && candidate.text === text);
      return cell !== undefined && cell.kind === 'user' ? cell.state : undefined;
    };
    expect(stateOf('dropped by interrupt')).toBe('discarded');
    expect(stateOf('sent while interrupting')).toBe('pending');

    // 幸存的 pending 随后被模型消费 → 正常提升，仍能对上号。
    harness.gateway.emit({
      type: 'user_message_accepted',
      content: 'sent while interrupting',
      origin_request_id: liveId,
      session_id: sessionId,
    });
    await flushMicrotasks();

    expect(stateOf('dropped by interrupt')).toBe('discarded');
    expect(stateOf('sent while interrupting')).toBe('accepted');
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

  it('replays the assistant text above the tool calls it announces (stream order)', async () => {
    // The live lane renders in stream order: reasoning → text → tool calls.
    // Replay used to push the calls first (the order its own code happened to
    // list them in), putting every "now doing X" sentence below its own tool
    // cards, with the ReAct separator in between.
    const live = createHostHarness();
    teardown.push(live);
    await live.boot();
    const liveId = live.gateway.createdOrder[0] ?? '';
    live.gateway.emit({ type: 'turn_started', session_id: liveId });
    live.gateway.emit({ type: 'reasoning', content: 'let me look', session_id: liveId });
    live.gateway.emit(textPayload('Reading the file now.', liveId));
    live.gateway.emit({
      type: 'tool_call',
      tool_name: 'Read',
      tool_args: { path: 'main.rs' },
      tool_call_id: 'tc-read',
      session_id: liveId,
    });
    live.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'Read',
      tool_args: { path: 'main.rs' },
      tool_call_id: 'tc-read',
      tool_result: 'fn main() {}',
      tool_success: true,
      model: 'test-model',
      session_id: liveId,
    });
    await flushMicrotasks();

    const replayed = createHostHarness();
    teardown.push(replayed);
    replayed.gateway.seedOnCreate = {
      messages: [
        {
          role: 'assistant',
          content: 'Reading the file now.',
          reasoning_content: 'let me look',
          tool_calls: [{ id: 'tc-read', name: 'Read', arguments: { path: 'main.rs' } }],
          uuid: 'm1',
        },
        { role: 'tool', tool_call_id: 'tc-read', content: 'fn main() {}', uuid: 'm2' },
      ],
    };
    await replayed.boot();
    await flushMicrotasks();

    const liveCells = live.host.sessionManager.record(liveId)?.cells ?? [];
    const replayId = replayed.gateway.createdOrder[0] ?? '';
    const replayCells = replayed.host.sessionManager.record(replayId)?.cells ?? [];

    expect(kinds(liveCells)).toEqual(['thinking', 'assistant', 'tool_call']);
    expect(kinds(replayCells)).toEqual(kinds(liveCells));
    expect(stableCells(replayCells)).toEqual(stableCells(liveCells));
  });

  it('keeps replay == live when a tool card carries an anchored diff', async () => {
    // Resume anchors the diff in the events pass — after the message pass
    // already grew the turn separator. Live, where the diff sits on the tool
    // card before the next round's text arrives, never grows it.
    const live = createHostHarness();
    teardown.push(live);
    await live.boot();
    const liveId = live.gateway.createdOrder[0] ?? '';
    live.gateway.emit({
      type: 'tool_call',
      tool_name: 'Edit',
      tool_args: { path: 'a.ts' },
      tool_call_id: 'tc-edit',
      session_id: liveId,
    });
    live.gateway.emit({
      type: 'diff_content',
      path: 'a.ts',
      old_text: 'old',
      new_text: 'new',
      old_start_line: 1,
      new_start_line: 1,
      tool_call_id: 'tc-edit',
      session_id: liveId,
    });
    live.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'Edit',
      tool_args: { path: 'a.ts' },
      tool_call_id: 'tc-edit',
      tool_result: 'ok',
      tool_success: true,
      model: 'test-model',
      session_id: liveId,
    });
    live.gateway.emit({ type: 'reasoning', content: 'turn two', session_id: liveId });
    live.gateway.emit(textPayload('Now verify.', liveId));
    live.gateway.emit({ type: 'done', session_id: liveId });
    await flushMicrotasks();

    const replayed = createHostHarness();
    teardown.push(replayed);
    replayed.gateway.seedOnCreate = {
      messages: [
        {
          role: 'assistant',
          content: '',
          tool_calls: [{ id: 'tc-edit', name: 'Edit', arguments: { path: 'a.ts' } }],
          uuid: 'm1',
        },
        { role: 'tool', tool_call_id: 'tc-edit', content: 'ok', uuid: 'm2' },
        { role: 'assistant', content: 'Now verify.', reasoning_content: 'turn two', uuid: 'm3' },
      ],
      events: [
        {
          type: 'diff_content',
          path: 'a.ts',
          old_text: 'old',
          new_text: 'new',
          tool_call_id: 'tc-edit',
        },
      ],
    };
    await replayed.boot();
    await flushMicrotasks();

    const liveCells = live.host.sessionManager.record(liveId)?.cells ?? [];
    const replayId = replayed.gateway.createdOrder[0] ?? '';
    const replayCells = replayed.host.sessionManager.record(replayId)?.cells ?? [];

    expect(kinds(liveCells)).toEqual(['tool_call', 'diff', 'thinking', 'assistant']);
    expect(kinds(replayCells)).toEqual(kinds(liveCells));
    expect(stableCells(replayCells)).toEqual(stableCells(liveCells));
  });

  it('replays the ReAct separator between turns, not inside the announcing message', async () => {
    // Two turns, each announcing its tool calls in text. The only separator
    // is the turn boundary — no second one between a sentence and its calls.
    const replayed = createHostHarness();
    teardown.push(replayed);
    replayed.gateway.seedOnCreate = {
      messages: [
        {
          role: 'assistant',
          content: 'Listing the directory.',
          reasoning_content: 'turn one',
          tool_calls: [{ id: 'tc-ls', name: 'Bash', arguments: { command: 'ls' } }],
          uuid: 'm1',
        },
        { role: 'tool', tool_call_id: 'tc-ls', content: 'a.txt', uuid: 'm2' },
        {
          role: 'assistant',
          content: 'Now reading a.txt.',
          reasoning_content: 'turn two',
          tool_calls: [{ id: 'tc-read', name: 'Read', arguments: { path: 'a.txt' } }],
          uuid: 'm3',
        },
      ],
    };
    await replayed.boot();
    await flushMicrotasks();

    const id = replayed.gateway.createdOrder[0] ?? '';
    expect(kinds(replayed.host.sessionManager.record(id)?.cells ?? [])).toEqual([
      'thinking',
      'assistant',
      'tool_call',
      'separator', // turn boundary — before the next turn's thinking
      'thinking',
      'assistant',
      'tool_call',
    ]);
  });

  it('keeps the truncated flag of a capped tool result on both lanes', async () => {
    // A 20k result is over this layer's 16k cap but *under* the backend's
    // 100k storage cap, so both lanes see the same text and both must report
    // `truncated: true` — replay used to hardcode `false`, and the webview's
    // "Output truncated" note then vanished after a resume. (Above the
    // backend cap the lanes differ — a known limitation, see the next test.)
    const long = 'x'.repeat(20_000);

    const live = createHostHarness();
    teardown.push(live);
    await live.boot();
    const liveId = live.gateway.createdOrder[0] ?? '';
    live.gateway.emit({
      type: 'tool_call',
      tool_name: 'Bash',
      tool_args: { command: 'cat big.log' },
      tool_call_id: 'tc-long',
      session_id: liveId,
    });
    live.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'Bash',
      tool_args: { command: 'cat big.log' },
      tool_call_id: 'tc-long',
      tool_result: long,
      tool_success: true,
      model: 'test-model',
      session_id: liveId,
    });
    await flushMicrotasks();

    const replayed = createHostHarness();
    teardown.push(replayed);
    replayed.gateway.seedOnCreate = {
      messages: [
        {
          role: 'assistant',
          content: '',
          tool_calls: [{ id: 'tc-long', name: 'Bash', arguments: { command: 'cat big.log' } }],
          uuid: 'm1',
        },
        { role: 'tool', tool_call_id: 'tc-long', content: long, uuid: 'm2' },
      ],
    };
    await replayed.boot();
    await flushMicrotasks();

    const resultOf = (harness: HostHarness) => {
      const id = harness.gateway.createdOrder[0] ?? '';
      const cell = harness.host.sessionManager.record(id)?.cells[0];
      return cell?.kind === 'tool_call' ? cell.result : null;
    };

    const liveResult = resultOf(live);
    expect(liveResult?.truncated).toBe(true);
    expect(resultOf(replayed)).toEqual(liveResult);
  });

  it('replays a backend-capped tool result verbatim (the full text is not in history)', async () => {
    // The backend caps results over `tool_result_truncate.max_length` (100k
    // by default) *before* storing them: the live event carries the full
    // text, the stored message only head + marker + tail. Replay cannot
    // recover the rest, so it renders the stored text as-is and `truncated`
    // stays false — this layer capped nothing, and the backend's own marker
    // inside the text is the notice. Pinned so the flag is not "fixed" by
    // sniffing that marker: doing it honestly would need the projection to
    // carry the fact.
    const head = 'h'.repeat(200);
    const tail = 't'.repeat(200);
    const stored = `${head}\n... [truncated, original length: 120000 chars, full result saved to /tmp/wing_truncated_x.txt]\n${tail}`;

    const replayed = createHostHarness();
    teardown.push(replayed);
    replayed.gateway.seedOnCreate = {
      messages: [
        {
          role: 'assistant',
          content: '',
          tool_calls: [{ id: 'tc-big', name: 'Bash', arguments: { command: 'cat huge.log' } }],
          uuid: 'm1',
        },
        { role: 'tool', tool_call_id: 'tc-big', content: stored, uuid: 'm2' },
      ],
    };
    await replayed.boot();
    await flushMicrotasks();

    const id = replayed.gateway.createdOrder[0] ?? '';
    const cell = replayed.host.sessionManager.record(id)?.cells[0];
    const result = cell?.kind === 'tool_call' ? cell.result : null;
    expect(result?.text).toBe(stored);
    expect(result?.truncated).toBe(false);
  });

  it('replays a failed tool result as success (the projection carries no flag)', async () => {
    // Pinned known limitation: `serialize_message` emits no failure flag, so
    // replay cannot tell a failed result from a successful one and the live
    // lane's error styling is lost on resume (the TUI replay has the same
    // gap). Documented rather than guessed from the payload text — an honest
    // fix means the projection carries the fact.
    const replayed = createHostHarness();
    teardown.push(replayed);
    replayed.gateway.seedOnCreate = {
      messages: [
        {
          role: 'assistant',
          content: '',
          tool_calls: [{ id: 'tc-fail', name: 'Bash', arguments: { command: 'false' } }],
          uuid: 'm1',
        },
        // What the backend stores for a failed call (tool_executor.py).
        {
          role: 'tool',
          tool_call_id: 'tc-fail',
          content: "Error executing tool 'Bash': exit code 1",
          uuid: 'm2',
        },
      ],
    };
    await replayed.boot();
    await flushMicrotasks();

    const id = replayed.gateway.createdOrder[0] ?? '';
    const cell = replayed.host.sessionManager.record(id)?.cells[0];
    const result = cell?.kind === 'tool_call' ? cell.result : null;
    expect(result?.isError).toBe(false);
    expect(cell?.kind === 'tool_call' ? cell.status : null).toBe('success');
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
