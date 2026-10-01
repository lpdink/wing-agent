/**
 * The shell: responsive frame, session list, main pane, banner and overlays.
 *
 * Desktop is a two-column grid (sidebar + main); below the breakpoint the sidebar
 * becomes a drawer over the main pane and the top bar grows a "Sessions" toggle
 * (step 11 polishes the touch behaviour; the structure is the one shipped here).
 * Local state is only "what is open" (drawer, settings dialog) — every piece of
 * *data* comes from the runtime snapshot.
 */

import { useCallback, useEffect, useRef, useState, type ReactElement } from 'react';

import type { RuntimeSnapshot } from '../connection/runtime';
import type { PageLocation } from '../settings/urls';

import type { ShellActions } from './App';
import { BranchPanel } from './BranchPanel';
import { Composer } from './Composer';
import { ConnectionBanner } from './ConnectionBanner';
import { ConnectionStatus } from './ConnectionStatus';
import { InstallPrompt } from './InstallPrompt';
import { ModelPanel } from './ModelPanel';
import { NoticeStack } from './NoticeStack';
import { SessionList } from './SessionList';
import { SessionPane } from './SessionPane';
import { SettingsDialog } from './SettingsDialog';

import { useSwipeToClose } from './useSwipeToClose';

export interface ShellProps {
  readonly snapshot: RuntimeSnapshot;
  readonly actions: ShellActions;
  readonly location: PageLocation;
  /** `false` when the browser refused persistent storage (shown in the dialog). */
  readonly settingsPersistent?: boolean;
}

export function Shell({ snapshot, actions, location, settingsPersistent = true }: ShellProps): ReactElement {
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const shellRef = useRef<HTMLDivElement>(null);

  const connection = snapshot.connection;
  const record = snapshot.record;
  const openSettings = useCallback((): void => {
    setDrawerOpen(false);
    setSettingsOpen(true);
  }, []);
  const selectSession = useCallback(
    (sessionId: string): void => {
      setDrawerOpen(false);
      actions.activate(sessionId);
    },
    [actions],
  );

  // The model picker is open when `panels.modelPicker` is not null.
  const modelPicker = record?.panels.modelPicker ?? null;
  const branchPicker = record?.panels.branchPicker ?? null;

  // Global Escape: close any open overlay.
  const handleOverlayClose = useCallback((): void => {
    if (settingsOpen) {
      setSettingsOpen(false);
      return;
    }
    if (modelPicker !== null || branchPicker !== null) {
      actions.closeOverlays();
      return;
    }
  }, [settingsOpen, modelPicker, branchPicker, actions]);

  // Global Escape key handler.
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        handleOverlayClose();
      }
    };
    const el = shellRef.current;
    if (el !== null) {
      el.addEventListener('keydown', handleKeyDown);
      return () => {
        el.removeEventListener('keydown', handleKeyDown);
      };
    }
  }, [handleOverlayClose]);

  // Close overlays on overlay background click.
  const onOverlayClick = useCallback(
    (event: React.MouseEvent): void => {
      if (event.target === event.currentTarget) {
        handleOverlayClose();
      }
    },
    [handleOverlayClose],
  );

  // Swipe-to-close for overlay panels on mobile.
  const isMobile = globalThis.matchMedia?.('(max-width: 899px)').matches ?? false;
  const swipeHandlers = useSwipeToClose(handleOverlayClose, isMobile);

  return (
    <div ref={shellRef} className="shell" data-phase={connection.phase}>
      <header className="topbar">
        <button
          type="button"
          className="topbar__drawer"
          aria-expanded={drawerOpen}
          onClick={() => {
            setDrawerOpen((open) => !open);
          }}
        >
          Sessions
        </button>
        <div className="topbar__identity">
          <span className="topbar__brand">Wing</span>
          <span className="topbar__subject" title={record?.title ?? ''}>
            {record === null ? 'No session open' : record.title}
          </span>
        </div>
        <div className="topbar__status">
          <ConnectionStatus view={connection} onReconnect={actions.reconnect} />
          <span className="topbar__address" title={connection.address}>
            {connection.address}
          </span>
          <button type="button" className="button" onClick={openSettings}>
            Settings
          </button>
        </div>
      </header>

      <div className="shell__body">
        <aside className="sidebar" data-open={drawerOpen ? 'true' : 'false'} aria-label="Sessions">
          <SessionList
            rows={snapshot.sessions}
            listError={snapshot.listError}
            onSelect={selectSession}
            onNew={actions.newSession}
            onRefresh={actions.refreshSessions}
          />
        </aside>
        {drawerOpen ? (
          <button
            type="button"
            className="scrim"
            aria-label="Close the session list"
            onClick={() => {
              setDrawerOpen(false);
            }}
          />
        ) : null}

        <main className="main">
          <ConnectionBanner
            view={connection}
            listError={snapshot.listError}
            onOpenSettings={openSettings}
            onReconnect={actions.reconnect}
          />
          <SessionPane snapshot={snapshot} onNewSession={actions.newSession} />

          {record !== null ? <Composer record={record} actions={actions} /> : null}
        </main>
      </div>

      {/* ── Overlays ──────────────────────────────────────────────── */}
      {settingsOpen ? (
        <SettingsDialog
          settings={snapshot.settings}
          location={location}
          view={connection}
          persistent={settingsPersistent}
          onSave={(next) => {
            actions.applySettings(next);
            setSettingsOpen(false);
          }}
          onClose={() => {
            setSettingsOpen(false);
          }}
        />
      ) : null}

      {modelPicker !== null && record !== null ? (
        <div className="overlay" onClick={onOverlayClick} {...swipeHandlers}>
          <ModelPanel
            modelPicker={modelPicker}
            thinking={record.meta.thinking}
            reasoningEffort={record.meta.reasoningEffort}
            actions={actions}
            onClose={actions.closeOverlays}
          />
        </div>
      ) : null}

      {branchPicker !== null && record !== null ? (
        <div className="overlay" onClick={onOverlayClick} {...swipeHandlers}>
          <BranchPanel
            branchPicker={branchPicker}
            sessionId={record.sessionId}
            actions={actions}
            onClose={actions.closeOverlays}
          />
        </div>
      ) : null}

      <NoticeStack notices={snapshot.notices} onDismiss={actions.dismissNotice} />
      <InstallPrompt />
    </div>
  );
}
