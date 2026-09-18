import { afterEach, describe, expect, it } from 'vitest';

import { createHostHarness, flushMicrotasks, textPayload } from './support/harness';
import type { HostHarness } from './support/harness';
import { kinds } from './support/mirror';

/**
 * Multi-tab invariants: one socket, several sessions, and events that land in
 * exactly one tab. Plus the session-lifecycle operations that create tabs:
 * resume (activate an unopened session), fork, rewind, close.
 */

const teardown: HostHarness[] = [];
afterEach(() => {
  while (teardown.length > 0) {
    teardown.pop()?.dispose();
  }
});

async function bootWithTwoSessions(): Promise<{
  harness: HostHarness;
  first: string;
  second: string;
}> {
  const harness = createHostHarness();
  teardown.push(harness);
  await harness.boot();
  const first = harness.gateway.createdOrder[0] ?? '';
  await harness.intent({ type: 'newSession' });
  const second = harness.gateway.createdOrder[1] ?? '';
  return { harness, first, second };
}

describe('tab isolation', () => {
  it('delivers an event only to the session it belongs to', async () => {
    const { harness, first, second } = await bootWithTwoSessions();
    harness.wipe();

    harness.gateway.emit(textPayload('for the first tab', first));
    await flushMicrotasks();

    const patches = harness.ofType('patch');
    expect(patches).toHaveLength(1);
    expect(patches[0]?.sessionId).toBe(first);
    expect(kinds(harness.host.sessionManager.record(second)?.cells ?? [])).toEqual([]);
    expect(kinds(harness.host.sessionManager.record(first)?.cells ?? [])).toEqual(['assistant']);
  });

  it('drops events for sessions that are not open', async () => {
    const { harness, first } = await bootWithTwoSessions();
    harness.wipe();

    harness.gateway.emit(textPayload('nobody asked', 'sess-not-open'));
    await flushMicrotasks();

    expect(harness.ofType('patch')).toHaveLength(0);
    expect(kinds(harness.host.sessionManager.record(first)?.cells ?? [])).toEqual([]);
  });

  it('keeps patch cursors independent per session', async () => {
    const { harness, first, second } = await bootWithTwoSessions();
    harness.wipe();

    harness.gateway.emit(textPayload('a', first));
    harness.gateway.emit(textPayload('b', second));
    harness.gateway.emit(textPayload('c', first));
    harness.gateway.emit(textPayload('d', second));
    await flushMicrotasks();

    const view = harness.mirror;
    expect(view.errors).toEqual([]);
    expect(view.seq(first)).toBe(harness.host.sessionManager.record(first)?.seq);
    expect(view.seq(second)).toBe(harness.host.sessionManager.record(second)?.seq);
    expect(view.seq(first)).toBeGreaterThan(0);
    expect(view.seq(second)).toBeGreaterThan(0);
  });

  it('answers resync for one session with that session only', async () => {
    const { harness, first, second } = await bootWithTwoSessions();
    harness.wipe();

    harness.host.onResync(first);
    await flushMicrotasks();

    const hydrates = harness.ofType('hydrate');
    expect(hydrates).toHaveLength(1);
    expect(hydrates[0]?.session.sessionId).toBe(first);
    expect(harness.host.sessionManager.openSessionIds).toContain(second);
  });

  it('re-hydrates every session when the webview reloads', async () => {
    const { harness, first, second } = await bootWithTwoSessions();
    harness.gateway.emit(textPayload('first content', first));
    await flushMicrotasks();
    harness.wipe();

    await harness.ready();

    const hydrated = harness.ofType('hydrate').map((message) => message.session.sessionId);
    expect(hydrated).toContain(first);
    expect(hydrated).toContain(second);
    const firstHydrate = harness.hydrateFor(first);
    expect(kinds(firstHydrate?.session.cells ?? [])).toEqual(['assistant']);
  });
});

describe('activation', () => {
  it('activates an open tab, clears attention and re-sends the tab list', async () => {
    const { harness, first, second } = await bootWithTwoSessions();
    // A turn result while the second tab is active badges the first tab.
    harness.gateway.emit({
      type: 'turn_result',
      subtype: 'success',
      is_error: false,
      result: 'done',
      num_turns: 1,
      duration_ms: 10,
      usage: null,
      errors: [],
      session_id: first,
    });
    await flushMicrotasks();
    let tabs = harness.ofType('tabs');
    expect(tabs[tabs.length - 1]?.tabs.find((tab) => tab.sessionId === first)?.attention).toBe('result');

    harness.wipe();
    await harness.intent({ type: 'activateSession', sessionId: first });
    await flushMicrotasks();

    tabs = harness.ofType('tabs');
    const active = tabs[tabs.length - 1];
    expect(active?.activeSessionId).toBe(first);
    expect(active?.tabs.find((tab) => tab.sessionId === first)?.attention).toBe('none');
    // Activating an open tab never calls the gateway.
    expect(harness.gateway.calls('/api/session/resume')).toHaveLength(0);
    expect(harness.gateway.calls('/api/session/subscribe')).toHaveLength(2); // the two creations
    expect(second).not.toBe(first);
  });
});

describe('resume', () => {
  it('resumes an inactive session before subscribing and hydrates its history', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    const saved = harness.gateway.seedSession({
      sessionId: 'sess-saved',
      name: 'Saved session',
      messages: [
        { role: 'user', content: 'remember me', uuid: 'm1' },
        { role: 'assistant', content: 'of course', uuid: 'm2' },
      ],
    });
    await harness.boot();
    harness.wipe();

    await harness.intent({ type: 'activateSession', sessionId: saved.sessionId });
    await flushMicrotasks(20);

    const resumeIndex = harness.gateway.httpCalls.findIndex((call) => call.path === '/api/session/resume');
    const subscribeIndex = harness.gateway.httpCalls.findIndex(
      (call) => call.path === '/api/session/subscribe' && call.body?.['session_id'] === saved.sessionId,
    );
    expect(resumeIndex).toBeGreaterThanOrEqual(0);
    expect(subscribeIndex).toBeGreaterThan(resumeIndex);

    const hydrate = harness.hydrateFor(saved.sessionId);
    expect(hydrate?.session.title).toBe('Saved session');
    expect(kinds(hydrate?.session.cells ?? [])).toEqual(['user', 'assistant']);
    expect(harness.host.sessionManager.activeSessionId).toBe('sess-saved');
    expect(harness.host.sessionManager.openSessionIds).toContain('sess-saved');
  });

  it('reports a resume failure without touching the open tabs', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const first = harness.host.sessionManager.openSessionIds[0] ?? '';
    harness.gateway.httpFailures.set('/api/session/resume', { status: 404 });
    harness.wipe();

    await harness.intent({ type: 'activateSession', sessionId: 'sess-gone' });
    await flushMicrotasks(20);

    expect(harness.host.sessionManager.openSessionIds).toEqual([first]);
    const toasts = harness
      .ofType('ui')
      .filter((message) => message.action.kind === 'toast')
      .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
    expect(toasts.some((message) => message.startsWith('Resume failed'))).toBe(true);
  });
});

describe('fork', () => {
  it('forks into a new tab and leaves the source untouched', async () => {
    const { harness, first } = await bootWithTwoSessions();
    harness.gateway.emit(textPayload('source content', first));
    await flushMicrotasks();
    harness.wipe();

    await harness.intent({ type: 'runPromptCommand', sessionId: first, name: '/fork', argsText: 'current' });
    await flushMicrotasks(20);

    const forked = harness.gateway.createdOrder[harness.gateway.createdOrder.length - 1] ?? '';
    expect(forked).not.toBe(first);
    expect(harness.host.sessionManager.openSessionIds).toContain(forked);
    expect(harness.host.sessionManager.activeSessionId).toBe(forked);
    // The fork's own subscription happened (its own route, not the source's).
    expect(
      harness.gateway.calls('/api/session/subscribe').some((call) => call.body?.['session_id'] === forked),
    ).toBe(true);
    // The source still has its content and gets its own patches.
    expect(kinds(harness.host.sessionManager.record(first)?.cells ?? [])).toEqual(['assistant']);
  });

  it('opens the branch picker when /fork has no argument', async () => {
    const { harness, first } = await bootWithTwoSessions();

    await harness.intent({ type: 'runPromptCommand', sessionId: first, name: '/fork', argsText: '' });
    await flushMicrotasks();

    const panels = harness.ofType('panels');
    expect(panels[panels.length - 1]?.panels.branchPicker?.mode).toBe('fork');
    expect(panels[panels.length - 1]?.panels.branchPicker?.rows.at(-1)?.current).toBe(true);
  });
});

describe('rewind', () => {
  it('replaces the model from the sync the gateway pushes after a rewind', async () => {
    const { harness, first } = await bootWithTwoSessions();
    harness.gateway.emit(textPayload('before', first));
    await flushMicrotasks();
    harness.wipe();

    await harness.intent({
      type: 'runPromptCommand',
      sessionId: first,
      name: '/rewind',
      argsText: 'some-uuid',
    });
    await flushMicrotasks(20);

    expect(harness.gateway.calls('/api/session/rewind')).toHaveLength(1);
    const hydrates = harness.ofType('hydrate').filter((message) => message.session.sessionId === first);
    expect(hydrates.length).toBeGreaterThan(0);
    const last = hydrates[hydrates.length - 1];
    // The fake drops the last message and reports a draft; the model follows.
    expect(last?.session.draft).toBe('rewound draft');
    expect(harness.mirror.errors).toEqual([]);
    expect(harness.mirror.cells(first)).toEqual(harness.host.sessionManager.record(first)?.cells);
  });

  it('surfaces a rewind failure as a toast', async () => {
    const { harness, first } = await bootWithTwoSessions();
    harness.gateway.httpFailures.set('/api/session/rewind', { status: 500 });
    harness.wipe();

    await harness.intent({
      type: 'runPromptCommand',
      sessionId: first,
      name: '/rewind',
      argsText: 'bad-uuid',
    });
    await flushMicrotasks();

    const toasts = harness
      .ofType('ui')
      .filter((message) => message.action.kind === 'toast')
      .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
    expect(toasts.some((message) => message.startsWith('Rewind failed'))).toBe(true);
  });
});

describe('closing tabs', () => {
  it('unsubscribes, forgets the tab and stops delivering events', async () => {
    const { harness, first, second } = await bootWithTwoSessions();

    await harness.intent({ type: 'closeSession', sessionId: second });
    await flushMicrotasks();

    expect(harness.host.sessionManager.openSessionIds).toEqual([first]);
    const unsubscribe = harness.gateway.calls('/api/session/unsubscribe');
    expect(unsubscribe.some((call) => call.body?.['session_id'] === second)).toBe(true);

    const tabs = harness.ofType('tabs');
    expect(tabs[tabs.length - 1]?.tabs.map((tab) => tab.sessionId)).toEqual([first]);
    expect(tabs[tabs.length - 1]?.activeSessionId).toBe(first);

    harness.wipe();
    harness.gateway.emit(textPayload('after close', second));
    await flushMicrotasks();
    expect(harness.ofType('patch')).toHaveLength(0);
    expect(harness.gateway.dropped.some((event) => event['session_id'] === second)).toBe(true);
  });

  it('activates the remaining tab when the active one closes', async () => {
    const { harness, first, second } = await bootWithTwoSessions();
    expect(harness.host.sessionManager.activeSessionId).toBe(second);

    await harness.intent({ type: 'closeSession', sessionId: second });
    await flushMicrotasks();

    expect(harness.host.sessionManager.activeSessionId).toBe(first);
    const tabs = harness.ofType('tabs');
    expect(tabs[tabs.length - 1]?.activeSessionId).toBe(first);
  });

  it('closing the last tab leaves an empty tab bar', async () => {
    const harness = createHostHarness();
    teardown.push(harness);
    await harness.boot();
    const only = harness.host.sessionManager.openSessionIds[0] ?? '';

    await harness.intent({ type: 'closeSession', sessionId: only });
    await flushMicrotasks();

    const tabs = harness.ofType('tabs');
    expect(tabs[tabs.length - 1]?.tabs).toEqual([]);
    expect(tabs[tabs.length - 1]?.activeSessionId).toBeNull();
  });
});
