import { afterEach, describe, expect, it } from 'vitest';

import { createHostHarness, flushMicrotasks, textPayload } from './support/harness';
import type { HostHarness } from './support/harness';

/**
 * Event-lane branches that the first round validated only with throwaway probes
 * (review N7): notices, context stats, delivered acks, hard errors and meta
 * merges. All of them are behaviour the webview renders, so they get pins here.
 */

const teardown: HostHarness[] = [];
afterEach(() => {
  while (teardown.length > 0) {
    teardown.pop()?.dispose();
  }
});

async function boot(): Promise<{ harness: HostHarness; sessionId: string }> {
  const harness = createHostHarness();
  teardown.push(harness);
  await harness.boot();
  return { harness, sessionId: harness.gateway.createdOrder[0] ?? '' };
}

function systemTexts(harness: HostHarness, sessionId: string): string[] {
  return (harness.host.sessionManager.record(sessionId)?.cells ?? [])
    .filter((cell) => cell.kind === 'system')
    .map((cell) => (cell.kind === 'system' ? cell.text : ''));
}

describe('notice events', () => {
  it('renders a notice cell with its retry hint and does not end the turn', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    await flushMicrotasks();

    harness.gateway.emit({
      type: 'notice',
      message: 'Rate limited by the provider',
      level: 'warning',
      attempt: 2,
      max_attempts: 5,
      retry_in_s: 4,
      session_id: sessionId,
    });
    await flushMicrotasks();

    expect(systemTexts(harness, sessionId)).toEqual([
      'Rate limited by the provider (attempt 2/5, retrying in 4s)',
    ]);
    // A notice is not a turn boundary: the agent is still working.
    expect(harness.host.sessionManager.record(sessionId)?.status).toBe('working');
  });
});

describe('context stats', () => {
  it('updates the context window and keeps it when the payload reports zero', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.emit({
      type: 'context_stats',
      message_count: 4,
      total_tokens: 1_000,
      context_window_tokens: 200_000,
      system_prompt_parts: [],
      session_id: sessionId,
    });
    await flushMicrotasks();
    expect(harness.host.sessionManager.record(sessionId)?.context).toEqual({
      usedTokens: 1_000,
      messageCount: 4,
      windowTokens: 200_000,
    });

    // `window: 0` means "not reported" — the last known window must survive
    // (TUI parity: the gauge never drops to zero mid-session).
    harness.gateway.emit({
      type: 'context_stats',
      message_count: 6,
      total_tokens: 1_500,
      context_window_tokens: 0,
      system_prompt_parts: [],
      session_id: sessionId,
    });
    await flushMicrotasks();
    expect(harness.host.sessionManager.record(sessionId)?.context).toEqual({
      usedTokens: 1_500,
      messageCount: 6,
      windowTokens: 200_000,
    });
  });
});

describe('delivered acks', () => {
  it('is a pure no-op (no cells, no state churn)', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    await flushMicrotasks();
    harness.wipe();

    harness.gateway.emit({
      type: 'delivered',
      delivered_messages: ['m-1'],
      session_id: sessionId,
    });
    await flushMicrotasks();

    expect(harness.ofType('patch')).toHaveLength(0);
    expect(harness.ofType('state')).toHaveLength(0);
    expect(harness.ofType('panels')).toHaveLength(0);
  });
});

describe('hard errors', () => {
  it('ends the turn, records the error and badges a background tab', async () => {
    const { harness, sessionId } = await boot();
    const second = harness.gateway.seedSession({ sessionId: 'sess-second', name: 'Second' });
    await harness.intent({ type: 'activateSession', sessionId: second.sessionId });
    await flushMicrotasks(20);

    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    harness.gateway.emit({
      type: 'error',
      message: 'provider exploded while streaming the answer',
      status_code: 500,
      error_code: 'provider_error',
      detail: null,
      session_id: sessionId,
    });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(record?.status).toBe('idle');
    expect(record?.lastError).toBe('provider exploded while streaming the answer');
    expect(systemTexts(harness, sessionId)).toEqual(['provider exploded while streaming the answer']);
    // The failing session is not the active one → the tab asks for attention.
    expect(record?.attention).toBe('error');
    const tab = harness
      .ofType('tabs')
      .at(-1)
      ?.tabs.find((candidate) => candidate.sessionId === sessionId);
    expect(tab?.attention).toBe('error');
  });
});

describe('meta merges', () => {
  it('merges session_state_changed fields and refreshes the tab title', async () => {
    const { harness, sessionId } = await boot();
    harness.wipe();

    harness.gateway.emit({
      type: 'session_state_changed',
      model: 'gpt-5.2',
      thinking: true,
      reasoning_effort: 'high',
      yolo: true,
      title: 'Renamed by /rename',
      agent: 'reviewer',
      session_id: sessionId,
    });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(record?.meta).toMatchObject({
      model: 'gpt-5.2',
      thinking: true,
      reasoningEffort: 'high',
      yolo: true,
      agent: 'reviewer',
    });
    expect(record?.title).toBe('Renamed by /rename');
    // The title change reaches the tab bar (the webview reads it from there).
    expect(
      harness
        .ofType('tabs')
        .at(-1)
        ?.tabs.find((candidate) => candidate.sessionId === sessionId)?.title,
    ).toBe('Renamed by /rename');
    // Partial updates keep the fields the event left out (`null` = unchanged).
    harness.gateway.emit({
      type: 'session_state_changed',
      model: null,
      thinking: null,
      reasoning_effort: null,
      yolo: false,
      title: null,
      agent: null,
      session_id: sessionId,
    });
    await flushMicrotasks();
    expect(harness.host.sessionManager.record(sessionId)?.meta).toMatchObject({
      model: 'gpt-5.2',
      thinking: true,
      reasoningEffort: 'high',
      yolo: false,
      agent: 'reviewer',
    });
  });

  it('keeps the first user message as the title and prefers an explicit one', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.emit(textPayload('unused', sessionId));
    await flushMicrotasks();
    // A live assistant message must not become the title — the title stays at
    // the workspace fallback until the first user message arrives.
    expect(harness.host.sessionManager.record(sessionId)?.title).toBe('workspace');

    // …but the first user message sent through the host does.
    await harness.intent({ type: 'sendMessage', sessionId, text: 'x'.repeat(140) });
    await flushMicrotasks();
    const title = harness.host.sessionManager.record(sessionId)?.title ?? '';
    // Backend rule: `content[:100]` — exactly, no ellipsis, same as `/ss` rows.
    expect(Array.from(title).length).toBe(100);
    expect(title).toBe('x'.repeat(100));
  });
});

describe('large payloads', () => {
  it('slices text larger than the patch chunk limit, first delta included', async () => {
    const { harness, sessionId } = await boot();
    harness.wipe();

    const big = 'y'.repeat(70_000);
    harness.gateway.emit(textPayload(big, sessionId));
    await flushMicrotasks();

    const ops = harness.ofType('patch').flatMap((message) => message.patches);
    const append = ops.filter((op) => op.op === 'append' || op.op === 'insert_after');
    const appends = ops.filter((op) => op.op === 'append_text');
    expect(append).toHaveLength(1);
    expect(appends).toHaveLength(1);
    const cellText = (append[0] as { cell: { text: string } }).cell.text;
    expect(cellText.length).toBe(64 * 1024);
    expect(appends[0]?.op === 'append_text' && appends[0].text.length).toBe(70_000 - 64 * 1024);
    // The webview rebuilds the full text from the two ops.
    expect(harness.mirror.errors).toEqual([]);
    expect(harness.mirror.cells(sessionId)).toEqual(harness.host.sessionManager.record(sessionId)?.cells);
  });
});
