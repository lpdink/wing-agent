/**
 * The webview root: the shell around step 04's transcript.
 *
 * Layout (top to bottom): tab bar · transcript (or welcome / waiting state) · toasts ·
 * status row · composer. Panels float above the shell, anchored to the composer, which
 * is where the input that opens them lives.
 *
 * What lives *here* and nowhere else is the app's local interaction state — the drafts
 * (one per session, so switching tabs does not lose them), which overlay is open, and
 * the focus token handed to the composer. None of it is part of the bridge contract:
 * the host cannot see a draft, and it must not have to (see `05_webview_shell/design.md`
 * D1 for the full authority table).
 *
 * The host's `ui` actions are honoured through `uiSignals` (counters, not booleans, so
 * two consecutive actions are two events).
 */

import { useCallback, useEffect, useState } from 'react';
import type { ReactElement } from 'react';

import { postToHost } from '../bridge/channel';
import { useAppStore } from '../state/appStore';
import { selectActiveSession } from '../state/store';
import { TranscriptView } from './TranscriptView';
import { TabBar } from './TabBar';
import { StatusArea } from './StatusArea';
import { Composer } from './Composer';
import type { ComposerOverlay } from './Composer';
import { Welcome } from './Welcome';
import { BranchPanel } from './panels/BranchPanel';
import { ModelPanel } from './panels/ModelPanel';
import { SessionPanel } from './panels/SessionPanel';
import appStyles from '../styles/app.module.css';

export function App(): ReactElement {
  const session = useAppStore(selectActiveSession);
  const tabs = useAppStore((state) => state.tabs);
  const sessions = useAppStore((state) => state.sessions);
  const activeSessionId = useAppStore((state) => state.activeSessionId);
  const toasts = useAppStore((state) => state.toasts);
  const closeOverlays = useAppStore((state) => state.uiSignals.closeOverlays);
  const focusComposer = useAppStore((state) => state.uiSignals.focusComposer);

  const [drafts, setDrafts] = useState<Readonly<Record<string, string>>>({});
  const [overlay, setOverlay] = useState<ComposerOverlay | null>(null);
  /** Bumped to ask the composer for focus (tab switch, host action, overlay close). */
  const [focusNonce, setFocusNonce] = useState(0);

  // The host may ask for the overlays to go away (Escape semantics are shared with
  // the extension); the counter changes identity on every request.
  useEffect(() => {
    if (closeOverlays > 0) {
      setOverlay(null);
      setFocusNonce((nonce) => nonce + 1);
    }
  }, [closeOverlays]);

  useEffect(() => {
    setFocusNonce((nonce) => nonce + 1);
  }, [activeSessionId, focusComposer]);

  // Drafts belong to open tabs: dropping a closed tab's draft keeps the map from
  // growing for the lifetime of the view.
  useEffect(() => {
    setDrafts((previous) => {
      const open = new Set(tabs.map((tab) => tab.sessionId));
      const kept = Object.entries(previous).filter(([id]) => open.has(id));
      return kept.length === Object.keys(previous).length ? previous : Object.fromEntries(kept);
    });
  }, [tabs]);

  const closeOverlay = useCallback((): void => {
    setOverlay(null);
    setFocusNonce((nonce) => nonce + 1);
  }, []);

  const sessionId = session?.sessionId ?? null;
  const draft = sessionId === null ? '' : (drafts[sessionId] ?? '');
  const setDraft = useCallback(
    (text: string): void => {
      if (sessionId === null) {
        return;
      }
      setDrafts((previous) => ({ ...previous, [sessionId]: text }));
    },
    [sessionId],
  );

  const modelPicker = session?.panels.modelPicker ?? null;
  const notice = session?.panels.globalNotice ?? null;

  return (
    <div className={appStyles.root}>
      <TabBar
        tabs={tabs}
        activeSessionId={activeSessionId}
        onOpenHistory={() => setOverlay({ kind: 'sessions' })}
      />

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
        onOpenOverlay={setOverlay}
        focusToken={`${sessionId ?? 'none'}:${focusComposer}:${focusNonce}`}
      />

      {modelPicker === null || session === null ? null : (
        <ModelPanel
          sessionId={session.sessionId}
          picker={modelPicker}
          meta={session.meta}
          onClose={() => {
            // A webview-local close cannot clear a host-owned overlay: ask the host.
            closeOverlay();
            postToHost({ type: 'closeOverlays' });
          }}
        />
      )}

      {overlay?.kind === 'sessions' && session !== null ? (
        <SessionPanel
          sessionId={session.sessionId}
          catalog={session.panels.sessionCatalog?.sessions ?? null}
          tabs={tabs}
          sessions={sessions}
          onClose={closeOverlay}
        />
      ) : null}

      {overlay?.kind === 'branches' && session !== null ? (
        <BranchPanel
          sessionId={session.sessionId}
          mode={overlay.mode}
          catalog={
            session.panels.branchCatalog?.sessionId === session.sessionId
              ? session.panels.branchCatalog
              : null
          }
          onClose={closeOverlay}
        />
      ) : null}
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
