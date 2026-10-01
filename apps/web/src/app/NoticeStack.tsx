/**
 * Transient messages from the runtime (failures, reduction toasts).
 *
 * They exist because the shell cannot otherwise report a failure that has no
 * place to live: "could not create a session", "that session no longer exists",
 * a replayed error event. The runtime auto-dismisses them (`noticeTtlMs`); the
 * stack only renders and reports the dismiss click.
 */

import type { ReactElement } from 'react';

import type { Notice } from '../connection/runtime';

export interface NoticeStackProps {
  readonly notices: readonly Notice[];
  readonly onDismiss: (id: Notice['id']) => void;
}

export function NoticeStack({ notices, onDismiss }: NoticeStackProps): ReactElement | null {
  if (notices.length === 0) {
    return null;
  }
  return (
    <div className="notices" role="status" aria-live="polite">
      {notices.map((notice) => (
        <div key={notice.id} className={`notice notice--${notice.level}`}>
          <span className="notice__text">{notice.text}</span>
          <button
            type="button"
            className="notice__close"
            aria-label="Dismiss"
            onClick={() => {
              onDismiss(notice.id);
            }}
          >
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}
