/**
 * The webview root: the shell around step 04's transcript.
 *
 * Layout (top to bottom): tab bar · transcript (or welcome / waiting state) ·
 * banners · toasts · status row · composer. Panels float above the shell, anchored to
 * the composer — but every one of them is **host-opened**: the panels render exactly
 * while `panels.{modelPicker,sessionPicker,branchPicker}` is non-null (`interfaces.md`),
 * and the webview keeps no local open/close state for them.
 *
 * What lives *here* and nowhere else is the app's local interaction state — the drafts
 * (one per session, so switching tabs does not lose them) and the focus token handed to
 * the composer. None of it is part of the bridge contract: the host cannot see a draft,
 * and it must not have to (see `05_webview_shell/design.md` D1 for the authority table).
 *
 * The host's `ui` actions are honoured through `uiSignals` (counters, not booleans, so
 * two consecutive actions are two events). The one effect that matters beyond focus is
 * "the last overlay just closed" → focus the composer: whether the host closed it
 * (`modelPicker` back to `null`) or the user did (`closeOverlays`), the next keystroke
 * must land in the input.
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import type { ReactElement } from 'react';

import { postToHost } from '../bridge/channel';
import { useAppStore } from '../state/appStore';
import { selectActiveSession } from '../state/store';
import { TranscriptView } from './TranscriptView';
import { TabBar } from './TabBar';
import { StatusArea } from './StatusArea';
import { Composer } from './Composer';
import { Welcome } from './Welcome';
import { BranchPanel } from './panels/BranchPanel';
import { ModelPanel } from './panels/ModelPanel';
import { SessionPanel } from './panels/SessionPanel';
import appStyles from '../styles/app.module.css';

export function App(): ReactElement {
  const session = useAppStore(selectActiveSession);
  const tabs = useAppStore((state) => state.tabs);
  const activeSessionId = useAppStore((state) => state.activeSessionId);
  const toasts = useAppStore((state) => state.toasts);
  const closeOverlays = useAppStore((state) => state.uiSignals.closeOverlays);
  const focusComposer = useAppStore((state) => state.uiSignals.focusComposer);

  const [drafts, setDrafts] = useState<Readonly<Record<string, string>>>({});
  /** Bumped to ask the composer for focus (tab switch, host action, overlay close). */
  const [focusNonce, setFocusNonce] = useState(0);

  // The host asked for the overlays to go away (Escape semantics are shared with the
  // extension); the counter changes identity on every request.
  useEffect(() => {
    if (closeOverlays > 0) {
      setFocusNonce((nonce) => nonce + 1);
    }
  }, [closeOverlays]);

  useEffect(() => {
    setFocusNonce((nonce) => nonce + 1);
  }, [activeSessionId, focusComposer]);

  const modelPicker = session?.panels.modelPicker ?? null;
  const sessionPicker = session?.panels.sessionPicker ?? null;
  const branchPicker = session?.panels.branchPicker ?? null;
  const overlayOpen = modelPicker !== null || sessionPicker !== null || branchPicker !== null;

  /** Close every host-owned overlay; the host clears the pickers it owns. */
  const closeOverlaysNow = useCallback((): void => {
    postToHost({ type: 'closeOverlays' });
    setFocusNonce((nonce) => nonce + 1);
  }, []);

  const sessionId = session?.sessionId ?? null;
  const draft = sessionId === null ? '' : (drafts[sessionId] ?? '');

  /**
   * The host hands a draft back after resume / rewind / fork (`state.draft`, and
   * `hydrate` for a fork). It is a **one-shot restore**: the host clears its copy
   * right after shipping it, so a later `state` carries `null` — which must never
   * clear what the user is typing. Adoption is therefore "once per session and
   * value": the adopted value is remembered, a repeat is ignored, and `null` is
   * never adopted.
   */
  const adoptedDrafts = useRef<Record<string, string>>({});
  const restoredDraft = session?.draft ?? null;
  useEffect(() => {
    if (sessionId === null || restoredDraft === null) {
      return;
    }
    if (adoptedDrafts.current[sessionId] === restoredDraft) {
      return;
    }
    adoptedDrafts.current[sessionId] = restoredDraft;
    setDrafts((previous) =>
      previous[sessionId] === restoredDraft ? previous : { ...previous, [sessionId]: restoredDraft },
    );
  }, [sessionId, restoredDraft]);

  // Drafts belong to open tabs: dropping a closed tab's draft (and its adoption
  // record) keeps both maps from growing for the lifetime of the view.
  useEffect(() => {
    const open = new Set(tabs.map((tab) => tab.sessionId));
    for (const id of Object.keys(adoptedDrafts.current)) {
      if (!open.has(id)) {
        delete adoptedDrafts.current[id];
      }
    }
    setDrafts((previous) => {
      const kept = Object.entries(previous).filter(([id]) => open.has(id));
      return kept.length === Object.keys(previous).length ? previous : Object.fromEntries(kept);
    });
  }, [tabs]);

  const setDraft = useCallback(
    (text: string): void => {
      if (sessionId === null) {
        return;
      }
      setDrafts((previous) => ({ ...previous, [sessionId]: text }));
    },
    [sessionId],
  );

  const notice = session?.panels.globalNotice ?? null;

  return (
    <div className={appStyles.root}>
      <TabBar tabs={tabs} activeSessionId={activeSessionId} />

      {session === null ? (
        <div className={appStyles.waiting} data-testid="empty-state">
          Waiting for the extension host…
        </div>
      ) : session.cells.length === 0 && session.status !== 'working' ? (
        <div className={appStyles.welcomeHost}>
          <Welcome onSuggest={setDraft} />
        </div>
      ) : (
        <TranscriptView session={session} />
      )}

      {/* Host-driven banners, rendered above the composer so they never hide the
       * transcript: the app-level notice (`panels.globalNotice` — gateway down,
       * reconnecting, …) and the session's last error, which is the only place a
       * failed turn is visible outside its own cells. Both are host state; the shell
       * never invents or clears them. */}
      {notice === null ? null : (
        <div className={appStyles.notice} data-testid="global-notice" data-level={notice.level} role="alert">
          {notice.text}
        </div>
      )}
      {session?.lastError == null ? null : (
        <div className={appStyles.notice} data-testid="session-error" data-level="error" role="alert">
          {session.lastError}
        </div>
      )}

      {toasts.length === 0 ? null : <Toasts />}

      <StatusArea session={session} />

      <Composer
        session={session}
        draft={draft}
        onDraftChange={setDraft}
        focusToken={`${sessionId ?? 'none'}:${focusComposer}:${focusNonce}`}
        overlayOpen={overlayOpen}
      />

      {/* Every overlay below is rendered because the *host* says so. Closing is also a
       * host decision: the webview only asks (`closeOverlays`). */}
      {modelPicker === null || session === null ? null : (
        <ModelPanel
          sessionId={session.sessionId}
          picker={modelPicker}
          meta={session.meta}
          onClose={closeOverlaysNow}
        />
      )}

      {sessionPicker === null || session === null ? null : (
        <SessionPanel sessionId={session.sessionId} picker={sessionPicker} onClose={closeOverlaysNow} />
      )}

      {branchPicker === null || session === null ? null : (
        <BranchPanel sessionId={session.sessionId} picker={branchPicker} onClose={closeOverlaysNow} />
      )}
    </div>
  );
}

function Toasts(): ReactElement {
  const toasts = useAppStore((state) => state.toasts);
  return (
    <div className={appStyles.toasts} data-testid="toasts" role="log">
      {toasts.map((toast) => (
        <div key={toast.id} className={appStyles.toast} data-toast-level={toast.level}>
          {toast.message}
        </div>
      ))}
    </div>
  );
}
