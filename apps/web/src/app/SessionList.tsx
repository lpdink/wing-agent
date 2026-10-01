/**
 * The session list: what exists, what it is doing, and how to open one.
 *
 * Rows come from the runtime snapshot (polled `/api/session/list`, with the open
 * session overlaid by live events — design.md D6); this component only renders and
 * reports clicks. `New session` and `Refresh` are the two actions the shell owns
 * here; `+` is the only way to get a session on a fresh gateway, so it is never
 * hidden behind an overflow menu.
 */

import type { ReactElement } from 'react';

import type { SessionRow } from '../sessions/rows';

import { statusLabel } from './labels';

export interface SessionListProps {
  readonly rows: readonly SessionRow[];
  readonly listError: string | null;
  readonly onSelect: (sessionId: string) => void;
  readonly onNew: () => void;
  readonly onRefresh: () => void;
}

export function SessionList({ rows, listError, onSelect, onNew, onRefresh }: SessionListProps): ReactElement {
  return (
    <div className="sessions">
      <div className="sessions__head">
        <h2 className="sessions__title">Sessions</h2>
        <button type="button" className="button button--primary" onClick={onNew}>
          New session
        </button>
      </div>

      {rows.length === 0 ? (
        <p className="sessions__empty">
          {listError === null ? 'No sessions yet.' : 'The session list is unavailable.'}
        </p>
      ) : (
        <ul className="sessions__list">
          {rows.map((row) => (
            <li key={row.id}>
              <button
                type="button"
                className={row.current ? 'session session--current' : 'session'}
                aria-current={row.current ? 'true' : undefined}
                onClick={() => {
                  onSelect(row.id);
                }}
              >
                <span className="session__top">
                  <span className="session__title" title={row.title}>
                    {row.title}
                  </span>
                  {row.attention !== 'none' ? (
                    <span className={`badge badge--${row.attention}`}>
                      {row.attention === 'error' ? 'failed' : 'finished'}
                    </span>
                  ) : null}
                </span>
                <span className="session__meta">
                  <span className={`status status--${row.status}`}>{statusLabel(row.status)}</span>
                  <span className="session__time">{row.updatedLabel}</span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="sessions__foot">
        <button type="button" className="button button--ghost" onClick={onRefresh}>
          Refresh
        </button>
      </div>
    </div>
  );
}
