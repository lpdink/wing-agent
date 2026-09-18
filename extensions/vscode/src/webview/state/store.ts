import { createStore } from 'zustand/vanilla';
import type { StoreApi } from 'zustand/vanilla';

import type {
  CellPatch,
  PanelsModel,
  ResyncReason,
  SessionId,
  SessionStateModel,
  SessionViewModel,
  TabModel,
  UiActionModel,
} from '../../shared';
import { BRIDGE_PROTOCOL_VERSION, unhandledVariant } from '../../shared';

import { applyCellPatches, isExpectedSeq } from './applyPatch';

/**
 * The webview's mirror of the host's model.
 *
 * Vanilla zustand store (no React import) so the state machine can be tested in
 * the node project at full speed; `useAppStore.ts` adds the React hook, and the
 * preview harness reuses the very same store.
 *
 * Everything here is *derived*: the host is the sole authority. Actions never
 * invent content — when a message cannot be applied faithfully they report a
 * reason, and `bridge/controller.ts` turns that into a `resync`.
 */

export interface ToastModel {
  readonly id: string;
  readonly level: 'info' | 'warning' | 'error';
  readonly message: string;
}

/**
 * How long a toast stays on screen before it dismisses itself (ms).
 *
 * Review #109 [P2-4]: `dismissToast` used to have no production caller, so every
 * "Compacting context…" / "Copied to clipboard" / "Not sent…" piled up in the
 * `role="log"` region for the lifetime of the window (`retainContextWhenHidden`
 * keeps the webview alive) — unreadable after a few dozen, and a screen reader
 * re-announces the whole region on every push. Errors get longer *and* a manual
 * close button.
 */
export const TOAST_TIMEOUT_MS: Readonly<Record<ToastModel['level'], number>> = {
  info: 4_000,
  warning: 8_000,
  error: 20_000,
};

/**
 * Newest N toasts that are kept.
 *
 * The same leak as above, with a hard bound so a burst (a retry ladder, a batch
 * of tool errors) can never grow the DOM without limit.
 */
export const MAX_TOASTS = 5;

export interface BridgeStatusModel {
  readonly connection: 'connecting' | 'ready';
  readonly protocolVersion: number;
  readonly lastPong: { readonly id: string; readonly rttMs: number } | null;
}

/**
 * Counters for the interaction-only `ui` actions.
 *
 * These actions are *events*, not state: the host asks the webview to focus the
 * composer / close its overlays / pin the transcript. Nothing about the session
 * model changes, so the counter (not a boolean) is the right shape — two
 * consecutive `focusComposer` actions must both be observable, and a component
 * reacts with `useEffect` on the number.
 *
 * Step 05 owns the effects. `scrollToBottom` is carried for completeness; the
 * transcript already pins itself (step 04), so nothing consumes it today.
 */
export interface UiSignalsModel {
  readonly closeOverlays: number;
  readonly focusComposer: number;
  readonly scrollToBottom: number;
}

export type ApplyOutcome = { readonly ok: true } | { readonly ok: false; readonly reason: ResyncReason };

export interface AppState {
  readonly sessions: Readonly<Record<SessionId, SessionViewModel>>;
  readonly tabs: readonly TabModel[];
  readonly activeSessionId: SessionId | null;
  readonly toasts: readonly ToastModel[];
  readonly bridge: BridgeStatusModel;
  readonly uiSignals: UiSignalsModel;
}

export interface AppActions {
  /** Replace a session with a full snapshot (after `ready`, a `resync`, or a tab (re)open). */
  hydrate(session: SessionViewModel): void;
  /** Apply one ordered patch batch. Reports a reason when the mirror cannot follow. */
  applyPatch(sessionId: SessionId, seq: number, patches: readonly CellPatch[]): ApplyOutcome;
  /**
   * Replace everything about a session except its cells.
   *
   * Deliberately does **not** adopt `state.seq`: the patch cursor is owned by
   * `hydrate` / `applyPatch`, so a stray `state` message cannot disable gap
   * detection.
   */
  applyState(state: SessionStateModel): ApplyOutcome;
  /** Replace a session's overlay data. */
  applyPanels(sessionId: SessionId, panels: PanelsModel): ApplyOutcome;
  /** Replace the tab bar (host-authoritative). */
  applyTabs(tabs: readonly TabModel[], activeSessionId: SessionId | null): void;
  /** Apply a one-shot UI action. */
  applyUi(action: UiActionModel): void;
  dismissToast(id: string): void;
  /** Mark the channel as live (first message from the host). */
  markReady(protocolVersion: number): void;
  /** Record a channel round-trip measurement. */
  notePong(id: string, rttMs: number): void;
}

export type AppStore = AppState & AppActions;

export type AppStoreApi = StoreApi<AppStore>;

/** A fresh, empty state (also used to reset between tests). */
export function createInitialState(): AppState {
  return {
    sessions: {},
    tabs: [],
    activeSessionId: null,
    toasts: [],
    bridge: { connection: 'connecting', protocolVersion: BRIDGE_PROTOCOL_VERSION, lastPong: null },
    uiSignals: { closeOverlays: 0, focusComposer: 0, scrollToBottom: 0 },
  };
}

/** Per-toast sequence so ids stay stable and testable (no wall clock involved). */
let toastSeq = 0;

/** Build an isolated store — one per app instance, one per test. */
export function createAppStore(): AppStoreApi {
  return createStore<AppStore>()((set, get) => ({
    ...createInitialState(),

    hydrate: (session) => {
      set((state) => ({
        sessions: { ...state.sessions, [session.sessionId]: session },
        activeSessionId: state.activeSessionId ?? session.sessionId,
      }));
    },

    applyPatch: (sessionId, seq, patches) => {
      const session = get().sessions[sessionId];
      if (session === undefined) {
        return { ok: false, reason: 'protocol' };
      }
      if (!isExpectedSeq(session.seq, seq)) {
        return { ok: false, reason: 'seq-gap' };
      }
      const result = applyCellPatches(session.cells, patches);
      if (!result.ok) {
        return result;
      }
      set((state) => ({
        sessions: { ...state.sessions, [sessionId]: { ...session, cells: result.cells, seq } },
      }));
      return { ok: true };
    },

    applyState: (next) => {
      const session = get().sessions[next.sessionId];
      if (session === undefined) {
        return { ok: false, reason: 'protocol' };
      }
      // The incoming `seq` is ignored on purpose: the patch cursor only moves on
      // `hydrate` (snapshot boundary) and `patch` (ordered ops). Adopting a
      // sequence number from a `state` message would silently disable gap
      // detection.
      set((state) => ({
        sessions: {
          ...state.sessions,
          [next.sessionId]: { ...next, cells: session.cells, seq: session.seq },
        },
      }));
      return { ok: true };
    },

    applyPanels: (sessionId, panels) => {
      const session = get().sessions[sessionId];
      if (session === undefined) {
        return { ok: false, reason: 'protocol' };
      }
      set((state) => ({ sessions: { ...state.sessions, [sessionId]: { ...session, panels } } }));
      return { ok: true };
    },

    applyTabs: (tabs, activeSessionId) => {
      set((state) => {
        // A closed tab's snapshot is dead weight (review #109 [P2-4]): the
        // webview lives for the whole window, so its transcript — cells, tool
        // results, diff rows — would otherwise be retained forever. The host's
        // tab list is the authority for what is open, and it always announces a
        // session in the same breath as its snapshot (`adopt` posts hydrate then
        // tabs; `onReady` posts tabs then hydrates each), so neither order can
        // drop a live one.
        const open = new Set(tabs.map((tab) => tab.sessionId));
        const kept: Record<SessionId, SessionViewModel> = {};
        let dropped = false;
        for (const [sessionId, session] of Object.entries(state.sessions)) {
          if (open.has(sessionId)) {
            kept[sessionId] = session;
          } else {
            dropped = true;
          }
        }
        return dropped ? { tabs, activeSessionId, sessions: kept } : { tabs, activeSessionId };
      });
    },

    applyUi: (action) => {
      switch (action.kind) {
        case 'toast': {
          toastSeq += 1;
          const toast: ToastModel = { id: `toast-${toastSeq}`, level: action.level, message: action.message };
          // Newest N win: the region is a live log, not a ledger (see MAX_TOASTS).
          set((state) => ({ toasts: [...state.toasts, toast].slice(-MAX_TOASTS) }));
          return;
        }
        case 'focusComposer':
        case 'scrollToBottom':
        case 'closeOverlays': {
          // Interaction-only actions: step 05 owns the effects (composer focus,
          // scroll pinning, overlay teardown). The model does not change — that is
          // exactly why they are not part of `SessionStateModel` — but each action
          // is recorded as a counter so a mounted component can react to it.
          const key = action.kind;
          set((state) => ({ uiSignals: { ...state.uiSignals, [key]: state.uiSignals[key] + 1 } }));
          return;
        }
        default:
          // Same gate as the bridge controller: exhaustive at compile time, a
          // warning (never a crash) at runtime.
          unhandledVariant(action, 'applyUi');
          return;
      }
    },

    dismissToast: (id) => {
      set((state) => ({ toasts: state.toasts.filter((toast) => toast.id !== id) }));
    },

    markReady: (protocolVersion) => {
      set((state) => ({ bridge: { ...state.bridge, connection: 'ready', protocolVersion } }));
    },

    notePong: (id, rttMs) => {
      set((state) => ({ bridge: { ...state.bridge, lastPong: { id, rttMs } } }));
    },
  }));
}

/** The active session's view, or `null` when nothing is open. */
export function selectActiveSession(state: AppState): SessionViewModel | null {
  const id = state.activeSessionId;
  return id === null ? null : (state.sessions[id] ?? null);
}
