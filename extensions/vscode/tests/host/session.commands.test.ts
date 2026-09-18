import { afterEach, describe, expect, it } from 'vitest';

import { createHostHarness, flushMicrotasks, textPayload } from './support/harness';
import type { HostHarness } from './support/harness';

/**
 * Local (frontend-only) commands, TUI parity.
 *
 * The whole point of this file: a command the frontend owns must **never** leave
 * as a chat message. `/context` and `/skills` shipped as pass-through text once
 * (the user typed them and the model was asked to "execute" them); these tests
 * pin the local lane by asserting on both sides — the transcript cell the user
 * sees **and** the silence on the wire.
 *
 * Reference: `crates/wing/src/app/commands.rs` (`COMMANDS`, the TUI's table).
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
  await flushMicrotasks(30);
  return { harness, sessionId: harness.gateway.createdOrder[0] ?? '' };
}

function toasts(harness: HostHarness): string[] {
  return harness
    .ofType('ui')
    .filter((message) => message.action.kind === 'toast')
    .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
}

function lastCell(harness: HostHarness, sessionId: string) {
  const cells = harness.mirror.cells(sessionId);
  return cells[cells.length - 1];
}

describe('/context and /skills', () => {
  it('/context renders the context window + system prompt as a system cell', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.session(sessionId).runtime.messageCount = 7;
    harness.gateway.session(sessionId).runtime.totalTokens = 12_345;
    harness.gateway.session(sessionId).runtime.contextWindowTokens = 200_000;
    harness.gateway.session(sessionId).runtime.systemPrompt = 'You are wing.';
    const framesBefore = harness.clientFrames().length;
    harness.wipe();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/context', argsText: '' });
    await flushMicrotasks(30);

    const cell = lastCell(harness, sessionId);
    expect(cell?.kind).toBe('system');
    expect(cell !== undefined && cell.kind === 'system' ? cell.text : '').toBe(
      ['Messages: 7', 'Tokens: 12345 / 200000', '', '--- System Prompt ---', 'You are wing.'].join('\n'),
    );
    // Nothing left the client: not a chat message, not a WS frame.
    expect(harness.clientFrames().length).toBe(framesBefore);
    expect(harness.ofType('patch')).toHaveLength(1);
  });

  it('/skills answers from the session info and says so when empty', async () => {
    const { harness, sessionId } = await boot();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/skills', argsText: '' });
    await flushMicrotasks(30);
    expect(lastCell(harness, sessionId)).toMatchObject({ kind: 'system', text: 'No skills loaded.' });

    harness.gateway.session(sessionId).runtime.skillsInfo = 'skills: alpha, beta';
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/skills', argsText: '' });
    await flushMicrotasks(30);
    expect(lastCell(harness, sessionId)).toMatchObject({ kind: 'system', text: 'skills: alpha, beta' });
  });

  it('a failure is reported instead of being sent as a message', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.httpFailures.set('/api/session/info', { status: 500, body: '{"error":"boom"}' });
    const framesBefore = harness.clientFrames().length;

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/context', argsText: '' });
    await flushMicrotasks(30);

    expect(toasts(harness).some((message) => message.startsWith('Context info failed'))).toBe(true);
    expect(harness.clientFrames().length).toBe(framesBefore);
  });

  it('typing /context in the composer stays local too', async () => {
    const { harness, sessionId } = await boot();
    const framesBefore = harness.clientFrames().length;

    await harness.intent({ type: 'sendMessage', sessionId, text: '/context' });
    await flushMicrotasks(30);

    expect(harness.clientFrames().length).toBe(framesBefore);
    expect(lastCell(harness, sessionId)?.kind).toBe('system');
  });

  it('/clear answers locally instead of asking the model to "clear"', async () => {
    const { harness, sessionId } = await boot();
    const framesBefore = harness.clientFrames().length;

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/clear', argsText: '' });
    await flushMicrotasks(30);

    expect(harness.clientFrames().length).toBe(framesBefore);
    expect(toasts(harness)).toContain(
      'Clearing the chat view is not supported in the VS Code extension yet — use /new for a fresh session.',
    );
  });

  it('a gateway prompt command still goes to the model', async () => {
    const { harness, sessionId } = await boot();
    const framesBefore = harness.clientFrames().length;

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/init', argsText: '' });
    await flushMicrotasks();

    expect(harness.clientFrames().length).toBe(framesBefore + 1);
    expect(harness.clientFrames()[framesBefore]).toMatchObject({ content: '/init', session_id: sessionId });
  });
});

describe('the rest of the local table', () => {
  it('/reload summarizes the result as a toast', async () => {
    const { harness, sessionId } = await boot();
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/reload', argsText: '' });
    await flushMicrotasks(30);
    expect(toasts(harness).some((message) => message.startsWith('✅ Reload:'))).toBe(true);
  });

  it('/copy takes the last assistant message, or the N-th (1-based)', async () => {
    const { harness, sessionId } = await boot();
    harness.gateway.emit(textPayload('first answer', sessionId));
    // A tool call separates the two assistant messages: consecutive `text`
    // events continue the same cell (TUI parity).
    harness.gateway.emit({
      type: 'tool_call',
      tool_name: 'Bash',
      tool_args: { command: 'echo hi' },
      tool_call_id: 'tc-copy',
      session_id: sessionId,
    });
    harness.gateway.emit({
      type: 'tool_call_result',
      tool_name: 'Bash',
      tool_args: { command: 'echo hi' },
      tool_call_id: 'tc-copy',
      tool_result: 'hi',
      tool_success: true,
      model: 'test-model',
      session_id: sessionId,
    });
    harness.gateway.emit(textPayload('second answer', sessionId));
    await flushMicrotasks();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/copy', argsText: '' });
    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/copy', argsText: '1' });
    expect(harness.editor.copies).toEqual(['second answer', 'first answer']);

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/copy', argsText: '9' });
    expect(toasts(harness)).toContain('No assistant message to copy');
  });

  it('/title shows the current title and sets a new one', async () => {
    const { harness, sessionId } = await boot();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/title', argsText: '' });
    expect(toasts(harness)).toContain('title: (not set)');

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/title', argsText: 'Release notes' });
    await flushMicrotasks(30);
    const update = harness.gateway.calls('/api/session/update').at(-1);
    expect(update?.body).toMatchObject({ session_id: sessionId, title: 'Release notes' });
    expect(harness.host.sessionManager.record(sessionId)?.title).toBe('Release notes');
  });

  it('/workdir shows the workspace and sets a new one', async () => {
    const { harness, sessionId } = await boot();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/workdir', argsText: '' });
    expect(toasts(harness)).toContain('workdir: /workspace');

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/workdir', argsText: '/tmp/other' });
    await flushMicrotasks(30);
    expect(harness.gateway.calls('/api/session/update').at(-1)?.body).toMatchObject({
      session_id: sessionId,
      workspace: '/tmp/other',
    });
    expect(harness.host.sessionManager.record(sessionId)?.meta.workspace).toBe('/tmp/other');
  });

  it('/agents lists templates and switches by name', async () => {
    const { harness, sessionId } = await boot();

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/agents', argsText: '' });
    expect(toasts(harness)).toContain('Agents: default (default), explorer');

    await harness.intent({ type: 'runPromptCommand', sessionId, name: '/agents', argsText: 'explorer' });
    await flushMicrotasks(30);
    expect(harness.gateway.calls('/api/session/update').at(-1)?.body).toMatchObject({
      session_id: sessionId,
      agent: 'explorer',
    });
  });
});

describe('runtime state on resume', () => {
  it('a resumed session shows the gateway yolo state (checkpoint② #3)', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    harness.gateway.seedSession({
      sessionId: 'persisted-1',
      name: 'Earlier work',
      runtime: { yolo: true, thinking: true, reasoningEffort: 'high', model: 'claude-opus-4-6' },
      messages: [{ role: 'user', content: 'hello from before', uuid: 'u1' }],
    });

    await harness.intent({ type: 'activateSession', sessionId: 'persisted-1' });
    await flushMicrotasks(30);

    const record = harness.host.sessionManager.record('persisted-1');
    expect(record?.meta.yolo).toBe(true);
    expect(record?.meta.thinking).toBe(true);
    expect(record?.meta.reasoningEffort).toBe('high');
    expect(record?.meta.model).toBe('claude-opus-4-6');
    // And the webview was told (this is what the status area renders).
    const states = harness.ofType('state').filter((message) => message.state.sessionId === 'persisted-1');
    expect(states.at(-1)?.state.meta.yolo).toBe(true);
  });

  it('the runtime refresh never overwrites a model pick that raced it', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    harness.gateway.seedSession({
      sessionId: 'persisted-2',
      runtime: { model: 'stale-model', yolo: true },
    });

    // Hold the `/api/session/info` response so the model switch lands first.
    const gate = harness.gateway.holdNext('/api/session/info');
    await harness.intent({ type: 'activateSession', sessionId: 'persisted-2' });
    await harness.intent({
      type: 'setModel',
      sessionId: 'persisted-2',
      provider: 'openai',
      model: 'gpt-5.2',
    });
    gate.release();
    await flushMicrotasks(30);

    const record = harness.host.sessionManager.record('persisted-2');
    expect(record?.meta.model).toBe('gpt-5.2');
    expect(record?.meta.provider).toBe('openai');
    // The refresh still delivered the fields the pick could not know about.
    expect(record?.meta.yolo).toBe(true);
    expect(harness.mirror.errors).toEqual([]);
  });
});
