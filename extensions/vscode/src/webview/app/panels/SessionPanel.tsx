/**
 * `/ss` (session) panel — also the tab bar's history entry point.
 *
 * Two data sources, in this order:
 *
 * 1. `panels.sessionCatalog` — the gateway's session list (`GET /api/session/list`),
 *    which can include sessions that are *not* open as tabs (so this is where
 *    "resume a session" lives);
 * 2. the open tabs plus the hydrated sessions — always available, because the host
 *    streams `tabs` on every change. Used as a fallback while the catalog is `null`
 *    (host has not fetched it yet), never as a second source of truth: the fallback
 *    only projects host data that is already in the webview.
 *
 * Selecting a row: open tab → `activateSession`; not open → `/ss <id>` through
 * `runPromptCommand`, which is exactly what the TUI's `/ss <id>` does (the host
 * resolves the command against the gateway).
 */

import { useEffect, useRef } from 'react';
import type { ReactElement } from 'react';

import type { SessionCandidateModel, SessionListStatus, SessionViewModel, TabModel } from '../../../shared';
import { postToHost } from '../../bridge/channel';
import styles from '../../styles/panels.module.css';
import { statusLabel } from '../selectors';
import { PanelEmpty, PanelShell } from './PanelShell';
import { optionId, useListNav } from './listNav';

export interface SessionPanelProps {
  readonly sessionId: string;
  readonly catalog: readonly SessionCandidateModel[] | null;
  readonly tabs: readonly TabModel[];
  readonly sessions: Readonly<Record<string, SessionViewModel>>;
  readonly onClose: () => void;
}

/** Rows shown when the host has not fetched the gateway's session list yet. */
export function fallbackRows(
  tabs: readonly TabModel[],
  sessions: Readonly<Record<string, SessionViewModel>>,
  activeSessionId: string,
): readonly SessionCandidateModel[] {
  return tabs.map((tab) => ({
    sessionId: tab.sessionId,
    title: tab.title === '' ? tab.sessionId : tab.title,
    workspace: sessions[tab.sessionId]?.meta.workspace ?? '',
    status: tab.status === 'waiting-for-input' ? 'waiting-for-input' : tab.status,
    current: tab.sessionId === activeSessionId,
  }));
}

export function SessionPanel({
  sessionId,
  catalog,
  tabs,
  sessions,
  onClose,
}: SessionPanelProps): ReactElement {
  const rows = catalog ?? fallbackRows(tabs, sessions, sessionId);
  const listRef = useRef<HTMLDivElement>(null);
  const openTabIds = new Set(tabs.map((tab) => tab.sessionId));

  const select = (index: number): void => {
    const row = rows[index];
    if (row === undefined) {
      return;
    }
    if (openTabIds.has(row.sessionId)) {
      postToHost({ type: 'activateSession', sessionId: row.sessionId });
    } else {
      // Not open as a tab: let the host resume it (TUI `/ss <id>` semantics).
      postToHost({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: row.sessionId });
    }
    onClose();
  };

  const nav = useListNav(
    rows.map(() => ({ selectable: true })),
    'session-panel',
    { onSelect: select, onEscape: onClose },
  );

  useEffect(() => {
    listRef.current?.focus();
  }, []);

  return (
    <PanelShell
      title="Sessions"
      testId="session-panel"
      onClose={onClose}
      hint="↑↓ navigate · Enter open · Esc close"
    >
      {rows.length === 0 ? (
        <PanelEmpty text="No sessions yet." />
      ) : (
        <div
          className={styles.list}
          role="listbox"
          aria-label="Sessions"
          tabIndex={-1}
          ref={listRef}
          data-testid="session-panel-list"
          data-source={catalog === null ? 'tabs' : 'catalog'}
          onKeyDown={nav.onKeyDown}
          {...nav.listProps}
        >
          {rows.map((row, index) => (
            <div
              key={row.sessionId}
              className={styles.option}
              role="option"
              id={optionId('session-panel', index)}
              aria-selected={index === nav.index}
              aria-current={row.current ? 'true' : undefined}
              data-highlighted={index === nav.index ? 'true' : 'false'}
              data-current={row.current ? 'true' : 'false'}
              data-testid="session-row"
              onClick={() => nav.activate(index)}
            >
              <span className={styles.statusDot} data-status={row.status} aria-hidden="true" />
              <span className={styles.optionColumn}>
                <span className={styles.optionLabel}>{row.title}</span>
                <span className={styles.optionMeta} title={row.workspace}>
                  {row.workspace === '' ? 'No workspace' : row.workspace}
                </span>
              </span>
              <span className={styles.optionMeta}>{sessionStatusLabel(row.status)}</span>
            </div>
          ))}
        </div>
      )}
    </PanelShell>
  );
}

/**
 * The gateway has one more status than the UI (`inactive` = on disk, not loaded);
 * the dot keys off the raw value so the stylesheet can colour it.
 */
function sessionStatusLabel(status: SessionListStatus): string {
  return status === 'inactive' ? 'Not loaded' : statusLabel(status);
}
