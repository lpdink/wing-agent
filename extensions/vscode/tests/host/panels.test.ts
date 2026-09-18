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
    expect(panels?.commandCatalog?.commands.map((command) => command.name)).toEqual(['/init', '/review']);
    expect(lastPanels(harness)?.commandCatalog?.commands[1]?.aliases).toEqual(['/rv']);
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
    // Applying a model closes the picker.
    expect(lastPanels(harness)?.modelPicker).toBeNull();

    // The gateway's `session_state_changed` carries no provider, so the host
    // applies the user's choice optimistically (TUI `runner.rs` does the same).
    const record = harness.host.sessionManager.record(sessionId);
    expect(record?.meta.model).toBe('gpt-5.2');
    expect(record?.meta.provider).toBe('openai');
    const state = harness
      .ofType('state')
      .filter((message) => message.state.sessionId === sessionId)
      .at(-1);
    expect(state?.state.meta.provider).toBe('openai');

    // Re-opening the picker highlights the new provider's row.
    await harness.intent({ type: 'openModelPicker', sessionId });
    await flushMicrotasks();
    const reopened = lastPanels(harness)?.modelPicker;
    expect(reopened?.rows.filter((row) => row.selected)).toEqual([
      { provider: 'openai', model: 'gpt-5.2', selected: true },
    ]);
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

describe('session picker', () => {
  it('lists sessions through /ss with the current one flagged', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.seedSession({ sessionId: 'sess-older', name: 'Older session' });
    harness.gateway.seedSession({
      sessionId: 'sess-yet-older',
      messages: [{ role: 'user', content: 'first message' }],
    });
    harness.wipe();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: '' });
    await flushMicrotasks();

    // Bare `/ss`: the host fetches the list and opens the picker (the webview
    // never fabricates rows).
    expect(harness.gateway.calls('/api/session/list')).toHaveLength(1);
    const panel = lastPanels(harness)?.sessionPicker;
    expect(panel?.rows.map((row) => row.sessionId)).toEqual([sessionId, 'sess-older', 'sess-yet-older']);
    expect(panel?.rows.map((row) => row.current)).toEqual([true, false, false]);
    expect(panel?.rows.map((row) => row.status)).toEqual(['idle', 'idle', 'idle']);
    expect(panel?.rows.map((row) => row.workspace)).toEqual(['/workspace', '/workspace', '/workspace']);
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
    expect(lastPanels(harness)?.branchPicker?.mode).toBe('rewind');
    expect(harness.gateway.calls('/api/session/branches')).toHaveLength(1);

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

    const panel = lastPanels(harness)?.branchPicker;
    expect(panel?.mode).toBe('rewind');
    expect(panel?.rows.map((row) => row.uuid)).toEqual(['u1', 'current']);
    expect(panel?.rows.map((row) => row.current)).toEqual([false, true]);
    expect(panel?.rows[0]?.content).toBe('first user message');
    // The row model has exactly the contract's fields (no role / preview).
    expect(Object.keys(panel?.rows[0] ?? {}).sort()).toEqual(['content', 'current', 'uuid']);
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

  it('closes the three overlays on closeOverlays but keeps the notice', async () => {
    const { harness, sessionId } = await boot();
    harness.host.sessionManager.setGlobalNotice({
      level: 'warning',
      text: 'Gateway connection lost',
    });
    await harness.intent({ type: 'openModelPicker', sessionId });
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: '' });
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/rewind', argsText: '' });
    await flushMicrotasks();
    expect(lastPanels(harness)?.modelPicker).not.toBeNull();
    expect(lastPanels(harness)?.sessionPicker).not.toBeNull();
    expect(lastPanels(harness)?.branchPicker).not.toBeNull();

    await harness.intent({ type: 'closeOverlays' });
    await flushMicrotasks();

    const closed = harness.host.sessionManager.record(sessionId)?.panels;
    expect(closed?.modelPicker).toBeNull();
    expect(closed?.sessionPicker).toBeNull();
    expect(closed?.branchPicker).toBeNull();
    // The banner is not an overlay: it expires with the connection, not on Esc.
    expect(lastPanels(harness)?.globalNotice).toEqual({
      level: 'warning',
      text: 'Gateway connection lost',
    });
    // The catalog is data, not an overlay, and survives too.
    expect(lastPanels(harness)?.commandCatalog?.commands.length).toBeGreaterThan(0);
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

describe('pickers close after a choice', () => {
  it('clears the session picker after resuming the chosen row', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.seedSession({ sessionId: 'sess-older', name: 'Older session' });
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: '' });
    await flushMicrotasks();
    expect(lastPanels(harness)?.sessionPicker?.rows.length).toBeGreaterThan(1);

    await harness.intent({
      type: 'runPromptCommand',
      sessionId,
      name: '/ss',
      argsText: 'sess-older',
    });
    await flushMicrotasks(20);

    // Assert on the *source* session's panels: the resumed session pushes its
    // own (empty) panels, and that would mask a stale picker.
    const source = harness.host.sessionManager.record(sessionId);
    expect(source?.panels.sessionPicker).toBeNull();
    expect(
      harness
        .ofType('panels')
        .some((message) => message.sessionId === sessionId && message.panels.sessionPicker === null),
    ).toBe(true);
    expect(harness.host.sessionManager.openSessionIds).toContain('sess-older');
  });

  it('closes the picker even when the follow-up subscribe fails', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.seedSession({ sessionId: 'sess-older', name: 'Older session' });
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: '' });
    await flushMicrotasks();
    // The chosen session exists (resume works) but its replay cannot attach.
    harness.gateway.httpFailures.set('/api/session/subscribe', { status: 503 });

    await harness.intent({
      type: 'runPromptCommand',
      sessionId,
      name: '/ss',
      argsText: 'sess-older',
    });
    await flushMicrotasks(30);

    expect(harness.host.sessionManager.record(sessionId)?.panels.sessionPicker).toBeNull();
    // The tab is open and its subscribe is being retried, not silently dropped.
    expect(harness.host.sessionManager.openSessionIds).toContain('sess-older');
  });

  it('clears the branch picker after a rewind and after a fork', async () => {
    const { harness, sessionId } = await boot();
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/rewind', argsText: '' });
    await flushMicrotasks();
    expect(lastPanels(harness)?.branchPicker?.mode).toBe('rewind');

    await harness.intent({
      type: 'runPromptCommand',
      sessionId,
      name: '/rewind',
      argsText: 'u1',
    });
    await flushMicrotasks(20);
    expect(harness.host.sessionManager.record(sessionId)?.panels.branchPicker).toBeNull();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/fork', argsText: '' });
    await flushMicrotasks();
    expect(lastPanels(harness)?.branchPicker?.mode).toBe('fork');

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/fork', argsText: 'u1' });
    await flushMicrotasks(20);
    // The *source* session's picker closes; the forked tab has none.
    expect(harness.host.sessionManager.record(sessionId)?.panels.branchPicker).toBeNull();
    expect(harness.gateway.createdOrder.length).toBe(2);
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
    expect(lastPanels(harness)?.sessionPicker).not.toBeNull();

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
