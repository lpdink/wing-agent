import { beforeEach, describe, expect, it } from 'vitest';

import type { SessionViewModel } from '../../src/shared';
import { EMPTY_PANELS } from '../../src/shared';
import { createAppStore, createInitialState, selectActiveSession } from '../../src/webview/state/store';
import type { AppStoreApi } from '../../src/webview/state/store';
import { makeEmptySession, makeFixtureSession } from '../../src/testing/fixtures';

/**
 * Store semantics: the mirror must follow the host exactly, and must refuse
 * (with a reason) anything it cannot follow. All of it runs without a DOM.
 */

let store: AppStoreApi;
const session: SessionViewModel = makeFixtureSession();

beforeEach(() => {
  store = createAppStore();
});

describe('initial state', () => {
  it('starts empty and connecting', () => {
    const state = createInitialState();
    expect(state.sessions).toEqual({});
    expect(state.tabs).toEqual([]);
    expect(state.activeSessionId).toBeNull();
    expect(state.toasts).toEqual([]);
    expect(state.bridge.connection).toBe('connecting');
    expect(selectActiveSession(state)).toBeNull();
  });
});

describe('hydrate', () => {
  it('stores the snapshot and focuses it when no tab was active', () => {
    store.getState().hydrate(session);

    const state = store.getState();
    expect(state.sessions[session.sessionId]).toEqual(session);
    expect(state.activeSessionId).toBe(session.sessionId);
    expect(selectActiveSession(state)?.title).toBe(session.title);
  });

  it('does not steal focus from an already active tab', () => {
    store.getState().hydrate(session);
    store.getState().hydrate(makeEmptySession('session-b'));

    expect(store.getState().activeSessionId).toBe(session.sessionId);
    expect(Object.keys(store.getState().sessions)).toEqual(['session-a', 'session-b']);
  });
});

describe('applyPatch', () => {
  beforeEach(() => {
    store.getState().hydrate(session);
  });

  it('applies an ordered batch and advances the sequence', () => {
    const outcome = store
      .getState()
      .applyPatch(session.sessionId, 1, [
        { op: 'append', cell: { kind: 'system', id: 's-new', createdAt: 1, level: 'info', text: 'hi' } },
      ]);

    expect(outcome).toEqual({ ok: true });
    const updated = store.getState().sessions[session.sessionId];
    expect(updated?.seq).toBe(1);
    expect(updated?.cells.at(-1)?.id).toBe('s-new');
  });

  it('reports a sequence gap instead of applying out of order', () => {
    const outcome = store
      .getState()
      .applyPatch(session.sessionId, 5, [
        { op: 'append', cell: { kind: 'system', id: 's-new', createdAt: 1, level: 'info', text: 'hi' } },
      ]);

    expect(outcome).toEqual({ ok: false, reason: 'seq-gap' });
    expect(store.getState().sessions[session.sessionId]?.seq).toBe(session.seq);
  });

  it('reports protocol when the session was never hydrated', () => {
    expect(store.getState().applyPatch('unknown-session', 1, [])).toEqual({ ok: false, reason: 'protocol' });
  });

  it('propagates the patch-level failure reason', () => {
    const outcome = store.getState().applyPatch(session.sessionId, 1, [{ op: 'remove', cellId: 'ghost' }]);
    expect(outcome).toEqual({ ok: false, reason: 'unknown-cell' });
  });
});

describe('applyState', () => {
  beforeEach(() => {
    store.getState().hydrate(session);
  });

  it('replaces status/meta/turn while keeping the transcript and the sequence', () => {
    const outcome = store.getState().applyState({
      ...session,
      status: 'working',
      title: 'Renamed',
      turn: { active: true, startedAtMs: 1234, lastResult: null },
      seq: 999,
    });

    expect(outcome).toEqual({ ok: true });
    const updated = store.getState().sessions[session.sessionId];
    expect(updated?.status).toBe('working');
    expect(updated?.title).toBe('Renamed');
    expect(updated?.turn.startedAtMs).toBe(1234);
    expect(updated?.cells).toEqual(session.cells);
    // `state` never moves the patch cursor: only `patch`/`hydrate` do.
    expect(updated?.seq).toBe(session.seq);
  });

  it('refuses state for an unknown session', () => {
    expect(store.getState().applyState({ ...session, sessionId: 'ghost' })).toEqual({
      ok: false,
      reason: 'protocol',
    });
  });
});

describe('applyPanels', () => {
  beforeEach(() => {
    store.getState().hydrate(session);
  });

  it('replaces the overlay payload', () => {
    const outcome = store.getState().applyPanels(session.sessionId, {
      modelPicker: {
        sessionId: session.sessionId,
        rows: [{ provider: 'p', model: 'm', selected: true }],
        activeIndex: 0,
      },
      globalNotice: { level: 'warning', text: 'gateway offline' },
    });

    expect(outcome).toEqual({ ok: true });
    const panels = store.getState().sessions[session.sessionId]?.panels;
    expect(panels?.modelPicker?.rows[0]?.model).toBe('m');
    expect(panels?.globalNotice?.level).toBe('warning');
  });

  it('refuses panels for an unknown session', () => {
    expect(store.getState().applyPanels('ghost', EMPTY_PANELS)).toEqual({ ok: false, reason: 'protocol' });
  });
});

describe('applyTabs', () => {
  it('replaces the tab bar verbatim (the host owns the order)', () => {
    store.getState().applyTabs(
      [
        { sessionId: 'b', title: 'B', status: 'working', attention: 'none' },
        { sessionId: 'a', title: 'A', status: 'idle', attention: 'result' },
      ],
      'a',
    );

    expect(store.getState().tabs.map((tab) => tab.sessionId)).toEqual(['b', 'a']);
    expect(store.getState().activeSessionId).toBe('a');
  });
});

describe('applyUi', () => {
  it('queues toasts with stable ids', () => {
    store.getState().applyUi({ kind: 'toast', level: 'error', message: 'boom' });
    store.getState().applyUi({ kind: 'toast', level: 'info', message: 'fyi' });

    const toasts = store.getState().toasts;
    expect(toasts).toHaveLength(2);
    expect(toasts[0]).toMatchObject({ level: 'error', message: 'boom' });
    expect(new Set(toasts.map((toast) => toast.id)).size).toBe(2);
  });

  it('dismisses one toast by id', () => {
    store.getState().applyUi({ kind: 'toast', level: 'info', message: 'one' });
    store.getState().applyUi({ kind: 'toast', level: 'info', message: 'two' });
    const [first] = store.getState().toasts;

    store.getState().dismissToast(first?.id ?? '');

    expect(store.getState().toasts.map((toast) => toast.message)).toEqual(['two']);
  });

  it('treats interaction-only actions as model no-ops', () => {
    const before = store.getState();
    store.getState().applyUi({ kind: 'focusComposer' });
    store.getState().applyUi({ kind: 'scrollToBottom' });
    store.getState().applyUi({ kind: 'closeOverlays' });
    expect(store.getState().toasts).toEqual(before.toasts);
  });
});

describe('bridge status', () => {
  it('records readiness and the last round-trip', () => {
    store.getState().markReady(4);
    store.getState().notePong('ping-1', 12);

    expect(store.getState().bridge).toEqual({
      connection: 'ready',
      protocolVersion: 4,
      lastPong: { id: 'ping-1', rttMs: 12 },
    });
  });
});
