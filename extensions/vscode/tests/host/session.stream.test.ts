import { afterEach, describe, expect, it } from 'vitest';

import type { ToolCallCellModel } from '../../src/shared';
import { createHostHarness, flushMicrotasks } from './support/harness';
import type { HostHarness } from './support/harness';

/**
 * Streaming tool arguments — the O(n²) fix (review #109 [P1-2]).
 *
 * A provider emits one `tool_call_stream` fragment per SSE chunk; the bridge
 * used to receive the *whole* cell for every fragment (measured: ~100 MB of
 * `postMessage` traffic for a 200 KB `Write`). These tests pin the two
 * invariants that make the fix safe:
 *
 * 1. whatever the host skips, it skips in its own model too — the webview's
 *    mirror (`WebviewMirror`, shipped `applyCellPatches`) never diverges;
 * 2. the authoritative, complete arguments still reach the webview with the
 *    final `tool_call`, so the expanded card is never stuck with the preview.
 */

const teardown: HostHarness[] = [];

afterEach(() => {
  for (const harness of teardown.splice(0)) {
    harness.dispose();
  }
});

/** The one tool-call cell of a session (fails loudly when it is missing). */
function toolCell(harness: HostHarness, sessionId: string): ToolCallCellModel {
  const cell = harness.host.sessionManager.record(sessionId)?.cells.find((c) => c.kind === 'tool_call');
  if (cell === undefined || cell.kind !== 'tool_call') {
    throw new Error('no tool_call cell');
  }
  return cell;
}

/** Bytes the host actually pushed across the bridge (what the reviewer measured). */
function postedBytes(harness: HostHarness): number {
  return harness.posted.reduce((total, message) => total + JSON.stringify(message).length, 0);
}

function streamFragments(
  harness: HostHarness,
  sessionId: string,
  argsText: string,
  fragmentCount: number,
  options: { readonly toolName?: string; readonly toolCallId?: string; readonly isFinal?: boolean } = {},
): void {
  const toolName = options.toolName ?? 'Write';
  const toolCallId = options.toolCallId ?? 'tc-stream';
  const finalFragment = options.isFinal ?? true;
  const size = Math.ceil(argsText.length / fragmentCount);
  for (let index = 0; index < argsText.length; index += size) {
    const fragment = argsText.slice(index, index + size);
    harness.gateway.emit({
      type: 'tool_call_stream',
      tool_call_id: toolCallId,
      tool_name: toolName,
      args_fragment: fragment,
      is_final: finalFragment && index + size >= argsText.length,
      session_id: sessionId,
    });
  }
}

describe('streaming tool arguments over the bridge', () => {
  it('bounds the traffic for a 200 KB Write instead of re-sending the whole cell', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    // 200 KB of arguments in ~1000 fragments — the review's worst case.
    const content = 'x'.repeat(200_000);
    const argsText = JSON.stringify({ path: 'big.txt', content });
    const startedAt = Date.now();
    streamFragments(harness, sessionId, argsText, 1000);
    await flushMicrotasks();
    const elapsedMs = Date.now() - startedAt;

    // The model and the webview's mirror agree (nothing was skipped on one side
    // only — that is the invariant the throttle could have broken).
    const record = harness.host.sessionManager.record(sessionId);
    expect(harness.mirror.errors).toEqual([]);
    expect(harness.mirror.cells(sessionId)).toEqual(record?.cells);

    // Budget: one message per rendered update, not per fragment, and each one
    // carries only the capped preview (never the full 200 KB).
    const patchMessages = harness.ofType('patch');
    expect(patchMessages.length).toBeLessThan(200);
    expect(patchMessages.length).toBeGreaterThan(1);
    for (const message of harness.posted) {
      expect(JSON.stringify(message).length).toBeLessThan(64 * 1024);
    }
    // Before the fix this was ~100 MB for the same stream.
    expect(postedBytes(harness)).toBeLessThan(2 * 1024 * 1024);
    expect(elapsedMs).toBeLessThan(2_000);

    // The preview that crossed is capped and says so.
    const preview = toolCell(harness, sessionId).argsText;
    expect(preview.length).toBeLessThan(9_000);
    expect(preview.startsWith('{"path":"big.txt","content":"xxx')).toBe(true);
    expect(preview).toContain('more characters');
    expect(toolCell(harness, sessionId).status).toBe('pending');
  });

  it('hands the complete, parsed arguments over with the final tool_call', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    const args = { path: 'big.txt', content: 'y'.repeat(50_000) };
    streamFragments(harness, sessionId, JSON.stringify(args), 400);
    await flushMicrotasks();
    // While streaming, the parsed view is unknown (partial JSON) — the cell
    // carries the capped preview and no `args`.
    expect(toolCell(harness, sessionId).args).toBeNull();

    harness.gateway.emit({
      type: 'tool_call',
      tool_call_id: 'tc-stream',
      tool_name: 'Write',
      tool_args: args,
      session_id: sessionId,
    });
    await flushMicrotasks();

    const cell = toolCell(harness, sessionId);
    // Authoritative: the full object, byte-exact, and the preview is dropped.
    expect(cell.args).toEqual(args);
    expect(cell.argsText).toBe('');
    expect(cell.display).toEqual({ title: 'Write', subject: 'big.txt' });
    expect(harness.mirror.cells(sessionId)).toEqual(harness.host.sessionManager.record(sessionId)?.cells);
    expect(harness.mirror.errors).toEqual([]);
  });

  it('renders every fragment of an ordinary (small) call — no visible change', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    const argsText = JSON.stringify({ command: 'pnpm run test' });
    for (let index = 0; index < argsText.length; index += 2) {
      harness.gateway.emit({
        type: 'tool_call_stream',
        tool_call_id: 'tc-small',
        tool_name: 'Bash',
        args_fragment: argsText.slice(index, index + 2),
        is_final: index + 2 >= argsText.length,
        session_id: sessionId,
      });
    }
    await flushMicrotasks();

    const cell = toolCell(harness, sessionId);
    expect(cell.argsText).toBe(argsText);
    expect(cell.display).toEqual({ title: 'Bash', subject: 'pnpm run test' });
    // One patch per fragment: the throttle never engages below 512 characters.
    expect(harness.ofType('patch').length).toBe(Math.ceil(argsText.length / 2));
  });

  it('leaves a capped, streamable cell behind when a turn is interrupted mid-args', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    // No `is_final`, no `tool_call` — the interrupt path (the backend drops the
    // half-written block from the context, the UI keeps what the user saw).
    streamFragments(harness, sessionId, JSON.stringify({ content: 'z'.repeat(20_000) }), 200, {
      isFinal: false,
    });
    await flushMicrotasks();
    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();

    const cell = toolCell(harness, sessionId);
    expect(cell.status).toBe('streaming');
    expect(cell.args).toBeNull();
    expect(cell.argsText.length).toBeLessThan(9_000);
    expect(harness.mirror.cells(sessionId)).toEqual(harness.host.sessionManager.record(sessionId)?.cells);
  });

  it('renders a replayed in-flight call (uncommitted_tools) immediately, capped', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    // 300 KB of arguments were in flight when the session was replayed: the
    // snapshot must not carry them (the cap applies to the replay lane too).
    const huge = JSON.stringify({ path: 'resumed.txt', content: 'r'.repeat(300_000) });
    harness.gateway.seedOnCreate = {
      uncommittedTools: [{ tool_call_id: 'tc-replay', tool_name: 'Write', args_fragment: huge }],
    };
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    // The hydrate carries the (capped) preview: the same 300 KB went over the
    // bridge in full before the fix.
    const hydrateBytes = JSON.stringify(harness.hydrateFor(sessionId)).length;
    harness.wipe();

    // A resumed session must show the live card at once (hydrate carries it);
    // the buffer then continues with the live fragments on the same path.
    harness.gateway.emit({
      type: 'tool_call_stream',
      tool_call_id: 'tc-replay',
      tool_name: 'Write',
      args_fragment: '"}',
      is_final: true,
      session_id: sessionId,
    });
    await flushMicrotasks();

    const cell = toolCell(harness, sessionId);
    expect(cell.status).toBe('pending');
    expect(cell.display.subject).toBe('resumed.txt');
    expect(cell.argsText.length).toBeLessThan(9_000);
    expect(cell.argsText).toContain('more characters');
    // The hydrate the webview received stays small (it was ~300 KB before).
    expect(hydrateBytes).toBeLessThan(64 * 1024);
  });

  it('keeps two concurrently streaming calls apart (one budget each)', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();

    const argsA = JSON.stringify({ command: 'pnpm test', extra: 'A'.repeat(5_000) });
    const argsB = JSON.stringify({ command: 'git status', extra: 'B'.repeat(5_000) });
    for (let index = 0; index < 40; index += 1) {
      for (const [id, text, name] of [
        ['tc-a', argsA, 'Bash'],
        ['tc-b', argsB, 'Bash'],
      ] as const) {
        harness.gateway.emit({
          type: 'tool_call_stream',
          tool_call_id: id,
          tool_name: name,
          args_fragment: text.slice(index * 128, (index + 1) * 128),
          is_final: false,
          session_id: sessionId,
        });
      }
    }
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    const cells = (record?.cells ?? []).filter(
      (cell): cell is ToolCallCellModel => cell.kind === 'tool_call',
    );
    expect(cells).toHaveLength(2);
    // Each call has its own buffer: the subjects never cross over, and neither
    // cell carries the other's text.
    expect(cells.map((cell) => cell.display.subject)).toStrictEqual(['pnpm test', 'git status']);
    expect(cells[0]?.argsText.startsWith('{"command":"pnpm test"')).toBe(true);
    expect(cells[1]?.argsText.startsWith('{"command":"git status"')).toBe(true);
    expect(cells[0]?.argsText).not.toContain('B'.repeat(64));
    expect(harness.mirror.cells(sessionId)).toEqual(record?.cells);
    expect(harness.mirror.errors).toEqual([]);
  });

  it('keeps streaming across a mid-stream resync (hydrate) without losing the buffer', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();
    const args = JSON.stringify({ path: 'big.txt', content: 'c'.repeat(20_000) });

    const emitRange = (from: number, to: number): void => {
      const size = 500;
      for (let index = from; index < to; index += size) {
        harness.gateway.emit({
          type: 'tool_call_stream',
          tool_call_id: 'tc-resync',
          tool_name: 'Write',
          args_fragment: args.slice(index, index + size),
          is_final: index + size >= to && to === args.length,
          session_id: sessionId,
        });
      }
    };
    emitRange(0, 10_000);
    await flushMicrotasks();
    // The webview asked for a snapshot mid-stream (`resync`): the host answers
    // with `hydrate` and the *host-side* buffer keeps accumulating.
    harness.wipe();
    harness.host.onResync(sessionId);
    emitRange(10_000, args.length);
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(harness.mirror.errors).toEqual([]);
    expect(harness.mirror.cells(sessionId)).toEqual(record?.cells);
    expect(harness.mirror.seq(sessionId)).toBe(record?.seq);
    // The card survived the snapshot and finished with the right subject.
    expect(toolCell(harness, sessionId).display.subject).toBe('big.txt');
  });

  it('keeps the authoritative args when a stray fragment arrives after the final tool_call', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    harness.wipe();
    const args = { command: 'pnpm test' };

    harness.gateway.emit({
      type: 'tool_call_stream',
      tool_call_id: 'tc-late',
      tool_name: 'Bash',
      args_fragment: '{"command"',
      is_final: false,
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'tool_call',
      tool_call_id: 'tc-late',
      tool_name: 'Bash',
      tool_args: args,
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'tool_call_stream',
      tool_call_id: 'tc-late',
      tool_name: 'Bash',
      args_fragment: '{"partial"',
      is_final: false,
      session_id: sessionId,
    });
    await flushMicrotasks();

    // `args` is what every renderer reads (`Cells.tsx::argsText`), so a late
    // fragment can never change what the card shows.
    const cell = toolCell(harness, sessionId);
    expect(cell.args).toEqual(args);
    expect(harness.mirror.cells(sessionId)).toEqual(harness.host.sessionManager.record(sessionId)?.cells);
  });
});
