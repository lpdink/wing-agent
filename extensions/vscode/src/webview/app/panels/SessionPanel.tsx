/**
 * The session picker (`/ss`, and the tab bar's history button).
 *
 * Host-opened: the panel is on screen exactly while `panels.sessionPicker` is
 * non-null (`interfaces.md`), so data and visibility always arrive together. The
 * rows are the gateway's session list — including sessions that are *not* open as
 * tabs, which is what makes this the way back to an earlier conversation.
 *
 * Selecting a row is the TUI's `/ss <id>`: one `runPromptCommand` and the host
 * decides whether that means "activate an open tab" or "resume from disk". The
 * webview never touches the tab list itself.
 *
 * Highlight starts on the row the host marked `current` (that is where the user is),
 * falling back to the first row.
 */

import { useEffect, useRef } from 'react';
import type { ReactElement } from 'react';

import type { SessionCandidateModel, SessionListStatus, SessionPickerModel } from '../../../shared';
import { postToHost } from '../../bridge/channel';
import styles from '../../styles/panels.module.css';
import { statusLabel } from '../selectors';
import { PanelEmpty, PanelShell } from './PanelShell';
import { optionId, useListNav } from './listNav';

export interface SessionPanelProps {
  /** Session the command is issued from (the active one). */
  readonly sessionId: string;
  readonly picker: SessionPickerModel;
  readonly onClose: () => void;
}

export function SessionPanel({ sessionId, picker, onClose }: SessionPanelProps): ReactElement {
  const rows = picker.rows;
  const listRef = useRef<HTMLDivElement>(null);

  const select = (index: number): void => {
    const row = rows[index];
    if (row === undefined) {
      return;
    }
    // `/ss <id>` — the host resumes (or activates) it, exactly like the TUI.
    postToHost({ type: 'runPromptCommand', sessionId, name: '/ss', argsText: row.sessionId });
    onClose();
  };

  const nav = useListNav(
    rows.map(() => ({ selectable: true })),
    'session-panel',
    {
      // The user's own session is the anchor; a picker without a `current` row (a
      // session that is not open yet) starts at the top.
      initialIndex: currentRowIndex(rows),
      onSelect: select,
      onEscape: onClose,
    },
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
                <span className={styles.optionLabel}>{rowTitle(row)}</span>
                <span className={styles.optionMeta} title={row.workspace ?? undefined}>
                  {row.workspace ?? 'No workspace'}
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

/** The `current` row, or the first row when the host marked none (`-1` → first). */
function currentRowIndex(rows: readonly SessionCandidateModel[]): number {
  const index = rows.findIndex((row) => row.current);
  return index >= 0 ? index : -1;
}

/** A row without a title still has to be clickable: fall back to the id. */
function rowTitle(row: SessionCandidateModel): string {
  return row.title === '' ? row.sessionId : row.title;
}

/**
 * The gateway has one more status than the UI (`inactive` = on disk, not loaded);
 * the dot keys off the raw value so the stylesheet can colour it.
 */
function sessionStatusLabel(status: SessionListStatus): string {
  return status === 'inactive' ? 'Not loaded' : statusLabel(status);
}
