/**
 * The shell: responsive frame, session list, main pane, banner and overlays.
 *
 * Desktop is a two-column grid (sidebar + main); below the breakpoint the sidebar
 * becomes a drawer over the main pane and the top bar grows a "Sessions" toggle
 * (step 11 polishes the touch behaviour; the structure is the one shipped here).
 * Local state is only "what is open" (drawer, settings dialog) — every piece of
 * *data* comes from the runtime snapshot.
 */

import { useState, type ReactElement } from 'react';

import type { RuntimeSnapshot } from '../connection/runtime';
import type { PageLocation } from '../settings/urls';

import type { ShellActions } from './App';
import { ConnectionBanner } from './ConnectionBanner';
import { NoticeStack } from './NoticeStack';
import { SessionList } from './SessionList';
import { SessionPane } from './SessionPane';
import { SettingsDialog } from './SettingsDialog';
import { connectionLabel } from './labels';

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

  const connection = snapshot.connection;
  const record = snapshot.record;
  const openSettings = (): void => {
    setDrawerOpen(false);
    setSettingsOpen(true);
  };
  const selectSession = (sessionId: string): void => {
    setDrawerOpen(false);
    actions.activate(sessionId);
  };

  return (
    <div className="shell" data-phase={connection.phase}>
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
          <span className={`dot dot--${connection.phase}`} aria-hidden="true" />
          <span className="topbar__status-text">{connectionLabel(connection)}</span>
          <span className="topbar__address" title={connection.address}>
            {connection.address}
          </span>
          {connection.phase === 'offline' ? (
            <button type="button" className="button" onClick={actions.reconnect}>
              Reconnect
            </button>
          ) : null}
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
        </main>
      </div>

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

      <NoticeStack notices={snapshot.notices} onDismiss={actions.dismissNotice} />
    </div>
  );
}
