import { afterEach, describe, expect, it } from 'vitest';

import { createHostHarness, flushMicrotasks, textPayload } from './support/harness';
import type { HostHarness } from './support/harness';

/**
 * Panel data, control-plane mutations and editor actions — everything the
 * webview asks for that is not "reduce an event".
 *
 * Panels are data (`panels` messages), never rendering: the host fetches,
 * normalizes and ships rows; the webview only draws them.
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

function lastPanels(harness: HostHarness) {
  const panels = harness.ofType('panels');
  return panels[panels.length - 1]?.panels;
}

function toasts(harness: HostHarness): string[] {
  return harness
    .ofType('ui')
    .filter((message) => message.action.kind === 'toast')
    .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
}

describe('command catalog', () => {
  it('fetches the prompt commands once per connection and puts them in every panel', async () => {
    const { harness, sessionId } = await boot();

    expect(harness.gateway.calls('/api/commands')).toHaveLength(1);
    const panels = harness.host.sessionManager.record(sessionId)?.panels;
    expect(panels?.commands?.map((command) => command.name)).toEqual(['/init', '/review']);
    expect(lastPanels(harness)?.commands?.[1]?.aliases).toEqual(['/rv']);
    expect(harness.ofType('panels').some((message) => message.sessionId === sessionId)).toBe(true);
  });

  it('sends a gateway prompt command as plain message text', async () => {
    const { harness, sessionId } = await boot();
    harness.wipe();

    await harness.intent({
      type: 'runPromptCommand',
      sessionId,
      name: '/review',
      argsText: 'src/host',
    });

    const frames = harness.clientFrames().filter((frame) => frame['session_id'] === sessionId);
    expect(frames).toHaveLength(1);
    expect(frames[0]?.['content']).toBe('/review src/host');
  });
});

describe('model picker', () => {
  it('lists models with the current selection marked and applies a choice', async () => {
    const { harness, sessionId } = await boot();
    // The agent snapshot travels with the replay (that is where model+provider
    // come from; `session_state_changed` has no provider field).
    harness.gateway.session(sessionId).agent = {
      model_name: 'claude-sonnet-4-6',
      provider_name: 'anthropic',
      workspace: '/workspace',
      tools: [],
      skills: [],
      rules: [],
      system_prompt: null,
    };
    harness.gateway.pushSyncToSubscribers(sessionId);
    await flushMicrotasks();

    await harness.intent({ type: 'openModelPicker', sessionId });
    await flushMicrotasks();

    const picker = lastPanels(harness)?.modelPicker;
    expect(picker?.rows).toHaveLength(3);
    expect(picker?.rows.find((row) => row.selected)).toEqual({
      provider: 'anthropic',
      model: 'claude-sonnet-4-6',
      selected: true,
    });

    harness.wipe();
    await harness.intent({
      type: 'setModel',
      sessionId,
      provider: 'openai',
      model: 'gpt-5.2',
    });
    await flushMicrotasks();

    const update = harness.gateway.calls('/api/session/update');
    expect(update).toHaveLength(1);
    expect(update[0]?.body).toMatchObject({ session_id: sessionId, model: 'gpt-5.2', provider: 'openai' });
    // Applying a model closes the picker; the meta itself comes from the event.
    expect(lastPanels(harness)?.modelPicker).toBeNull();
  });

  it('surfaces a model-list failure as a toast and leaves the panel closed', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.httpFailures.set('/api/models', { status: 500 });
    harness.wipe();

    await harness.intent({ type: 'openModelPicker', sessionId });
    await flushMicrotasks();

    expect(lastPanels(harness)?.modelPicker ?? null).toBeNull();
    expect(toasts(harness).some((message) => message.startsWith('Could not list models'))).toBe(true);
  });
});

describe('session history panel', () => {
  it('lists sessions through /ss with the active one highlighted', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.seedSession({ sessionId: 'sess-older', name: 'Older session' });
    harness.gateway.seedSession({
      sessionId: 'sess-yet-older',
      messages: [{ role: 'user', content: 'first message' }],
    });
    harness.wipe();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: '' });
    await flushMicrotasks();

    const panel = lastPanels(harness)?.sessions;
    expect(panel?.rows.map((row) => row.sessionId)).toEqual([sessionId, 'sess-older', 'sess-yet-older']);
    expect(panel?.activeIndex).toBe(0);
    expect(panel?.rows[0]?.title).toBe('(untitled)');
    expect(panel?.rows[1]?.title).toBe('Older session');
    // The gateway's `first_user_message` rule fills the untitled row.
    expect(panel?.rows[2]?.title).toBe('first message');
  });

  it('resumes the chosen session from `/ss <id>`', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.seedSession({ sessionId: 'sess-older', name: 'Older session' });
    harness.wipe();

    await harness.intent({
      type: 'runPromptCommand',
      sessionId,
      name: '/session',
      argsText: 'sess-older',
    });
    await flushMicrotasks(20);

    expect(harness.host.sessionManager.openSessionIds).toContain('sess-older');
    expect(harness.hydrateFor('sess-older')).toBeDefined();
  });
});

describe('branch picker refresh', () => {
  it('refreshes an open picker when the gateway re-emits branch targets', async () => {
    const { harness, sessionId } = await boot();
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/rewind', argsText: '' });
    await flushMicrotasks();
    expect(lastPanels(harness)?.branches?.mode).toBe('rewind');

    harness.wipe();
    harness.gateway.emit({
      type: 'branch_targets',
      session_id: sessionId,
      targets: [
        { uuid: 'u1', content: 'first user message', role: 'user' },
        { uuid: 'current', content: '(current)', role: 'user' },
      ],
    });
    await flushMicrotasks();

    const panel = lastPanels(harness)?.branches;
    expect(panel?.rows.map((row) => row.uuid)).toEqual(['u1', 'current']);
    expect(panel?.rows.map((row) => row.current)).toEqual([false, true]);
    expect(panel?.rows[0]?.preview).toBe('first user message');
  });
});

describe('control-plane mutations', () => {
  it('routes thinking / effort / yolo through /api/session/update', async () => {
    const { harness, sessionId } = await boot();

    await harness.intent({ type: 'setThinking', sessionId, enabled: false });
    await harness.intent({ type: 'setEffort', sessionId, effort: 'low' });
    await harness.intent({ type: 'setYolo', sessionId, enabled: true });
    await flushMicrotasks();

    const bodies = harness.gateway.calls('/api/session/update').map((call) => call.body);
    expect(bodies).toEqual([
      { session_id: sessionId, thinking: false },
      { session_id: sessionId, reasoning_effort: 'low' },
      { session_id: sessionId, yolo: true },
    ]);
  });

  it('surfaces an update failure as a toast', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.httpFailures.set('/api/session/update', {
      status: 400,
      body: JSON.stringify({ error: 'bad model' }),
    });
    harness.wipe();

    await harness.intent({ type: 'setYolo', sessionId, enabled: true });
    await flushMicrotasks();

    expect(toasts(harness).some((message) => message.startsWith('Update failed'))).toBe(true);
  });

  it('reports compact start and result, and its failure', async () => {
    const { harness, sessionId } = await boot();
    harness.wipe();

    await harness.intent({ type: 'compact', sessionId });
    await flushMicrotasks();
    expect(toasts(harness)).toContain('Compacting context…');
    expect(toasts(harness)).toContain('Context compacted: 12345 → 3210 tokens');

    harness.gateway.httpFailures.set('/api/session/compact', { status: 504 });
    harness.wipe();
    await harness.intent({ type: 'compact', sessionId });
    await flushMicrotasks();
    expect(toasts(harness).some((message) => message.startsWith('Compact failed'))).toBe(true);
  });

  it('requests an interrupt over HTTP', async () => {
    const { harness, sessionId } = await boot();
    harness.wipe();

    await harness.intent({ type: 'interrupt', sessionId });
    await flushMicrotasks();

    expect(harness.gateway.calls('/api/session/interrupt')).toHaveLength(1);
    // The fake gateway answers with the `interrupted` event, which the reducer
    // turns into a toast and an idle status.
    expect(toasts(harness)).toContain('Agent interrupted');
    expect(harness.host.sessionManager.record(sessionId)?.status).toBe('idle');
  });

  it('closes every overlay on closeOverlays without touching the draft', async () => {
    const { harness, sessionId } = await boot();
    await harness.intent({ type: 'openModelPicker', sessionId });
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: '' });
    await flushMicrotasks();
    expect(lastPanels(harness)?.modelPicker).not.toBeNull();
    expect(lastPanels(harness)?.sessions).not.toBeNull();

    await harness.intent({ type: 'closeOverlays' });
    await flushMicrotasks();

    expect(lastPanels(harness)?.modelPicker).toBeNull();
    expect(lastPanels(harness)?.sessions).toBeNull();
    expect(lastPanels(harness)?.branches).toBeNull();
  });
});

describe('editor actions', () => {
  it('forwards links, files, clipboard and diff cells to the editor seam', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'Edit',
      tool_args: { path: 'src/a.ts' },
      tool_call_id: 'tc-diff',
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'diff_content',
      path: 'src/a.ts',
      old_text: 'old',
      new_text: 'new',
      old_start_line: 3,
      new_start_line: 3,
      tool_call_id: 'tc-diff',
      session_id: sessionId,
    });
    await flushMicrotasks();

    const diff = harness.host.sessionManager.record(sessionId)?.cells.find((cell) => cell.kind === 'diff');
    if (diff?.kind !== 'diff') {
      throw new Error('expected a diff cell');
    }

    await harness.intent({ type: 'openLink', href: 'https://example.com/doc' });
    await harness.intent({ type: 'openFile', path: '/work/src/a.ts', line: 42 });
    await harness.intent({ type: 'copyText', text: 'copied' });
    await harness.intent({ type: 'openDiff', sessionId, cellId: diff.id });
    await flushMicrotasks();

    expect(harness.editor.links).toEqual(['https://example.com/doc']);
    expect(harness.editor.files).toEqual([{ path: '/work/src/a.ts', line: 42 }]);
    expect(harness.editor.copies).toEqual(['copied']);
    expect(harness.editor.diffs).toEqual([{ path: 'src/a.ts', oldText: 'old', newText: 'new' }]);
  });

  it('warns when a diff cell has disappeared', async () => {
    const { harness } = await boot();
    harness.wipe();

    await harness.intent({ type: 'openDiff', sessionId: 'sess-x', cellId: 'cell-gone' });
    await flushMicrotasks();

    expect(harness.editor.diffs).toEqual([]);
    expect(toasts(harness).some((message) => message.includes('no longer in the transcript'))).toBe(true);
  });
});

describe('local commands typed into the composer', () => {
  it('handles /new, /ss and /model without sending a message', async () => {
    const { harness, sessionId } = await boot();
    harness.wipe();

    await harness.intent({ type: 'sendMessage', sessionId, text: '/model' });
    await harness.intent({ type: 'sendMessage', sessionId, text: '/ss' });
    await flushMicrotasks();

    // No user frames left the client for a local command.
    expect(harness.clientFrames().filter((frame) => frame['session_id'] === sessionId)).toHaveLength(0);
    expect(lastPanels(harness)?.modelPicker).not.toBeNull();
    expect(lastPanels(harness)?.sessions).not.toBeNull();

    await harness.intent({ type: 'sendMessage', sessionId, text: '/new' });
    await flushMicrotasks(20);
    expect(harness.gateway.createdOrder.length).toBe(2);
  });

  it('sends an ordinary message verbatim', async () => {
    const { harness, sessionId } = await boot();
    harness.wipe();

    await harness.intent({ type: 'sendMessage', sessionId, text: 'hello /world' });

    const frames = harness.clientFrames().filter((frame) => frame['session_id'] === sessionId);
    expect(frames).toHaveLength(1);
    expect(frames[0]?.['content']).toBe('hello /world');
  });
});

describe('attention and status derivation', () => {
  it('tracks working/waiting/idle and badges background results', async () => {
    const { harness, sessionId } = await boot();
    const statuses = (): string[] =>
      harness.ofType('tabs').map((message) => {
        const tab = message.tabs.find((candidate) => candidate.sessionId === sessionId);
        return `${tab?.status ?? 'gone'}:${tab?.attention ?? 'none'}`;
      });

    harness.gateway.emit({ type: 'turn_started', session_id: sessionId });
    await flushMicrotasks();
    expect(statuses()).toContain('working:none');

    harness.gateway.emit({
      type: 'turn_result',
      subtype: 'success',
      is_error: false,
      result: 'ok',
      num_turns: 1,
      duration_ms: 5,
      usage: { input_tokens: 3, output_tokens: 4 },
      errors: [],
      session_id: sessionId,
    });
    harness.gateway.emit({ type: 'done', session_id: sessionId });
    await flushMicrotasks();

    const record = harness.host.sessionManager.record(sessionId);
    expect(record?.status).toBe('idle');
    expect(record?.turn.lastResult?.totalTokens).toBe(7);
    expect(record?.turn.lastResult?.resultText).toBe('ok');
    // The session is active in its own tab: no badge.
    expect(record?.attention).toBe('none');
    void textPayload;
  });
});
