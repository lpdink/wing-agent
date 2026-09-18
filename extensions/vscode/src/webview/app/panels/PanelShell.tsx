/**
 * The shell's floating panels.
 *
 * Chrome modelled on VS Code's action widget — the container behind the model
 * picker (`platform/actionWidget/browser/actionWidget.css:6-18`: `border: 1px solid
 * editorHoverWidget-border`, `border-radius: cornerRadius-large`,
 * `background: menu-background`, `color: menu-foreground`, `padding: 4px`) — with
 * the click-catching backdrop from the same file (`:20-38`, transparent, closes on
 * click outside, no dimming).
 *
 * The panel is a modal dialog: it traps Escape, moves focus to its list, and hands
 * focus back to the composer when it closes (the caller re-focuses).
 */

import type { ReactElement, ReactNode } from 'react';

import styles from '../../styles/panels.module.css';

export interface PanelShellProps {
  /** Visible title, also used as the dialog's accessible name. */
  readonly title: string;
  readonly testId: string;
  readonly onClose: () => void;
  readonly children: ReactNode;
  /** Dimmed hint row at the bottom (keyboard help). */
  readonly hint?: string;
}

export function PanelShell({ title, testId, onClose, children, hint }: PanelShellProps): ReactElement {
  return (
    <div className={styles.layer}>
      {/* Click outside closes, like VS Code's `.context-view-block` (actionWidget.css:20-38). */}
      <div className={styles.backdrop} onMouseDown={onClose} data-testid={`${testId}-backdrop`} />
      <div className={styles.panel} role="dialog" aria-modal="true" aria-label={title} data-testid={testId}>
        <div className={styles.header}>
          <span className={styles.title}>{title}</span>
          <button
            type="button"
            className={styles.close}
            onClick={onClose}
            aria-label={`Close ${title}`}
            data-testid={`${testId}-close`}
          >
            ×
          </button>
        </div>
        {children}
        {hint === undefined ? null : <div className={styles.hint}>{hint}</div>}
      </div>
    </div>
  );
}

/** Shared empty state inside a panel (catalog missing, or genuinely empty). */
export function PanelEmpty({ text }: { readonly text: string }): ReactElement {
  return (
    <p className={styles.empty} data-testid="panel-empty">
      {text}
    </p>
  );
}
