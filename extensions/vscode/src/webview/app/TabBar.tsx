/**
 * The session tab bar.
 *
 * Browser-tab semantics, VS Code's editor tabs as the visual reference: a tablist
 * with one `role="tab"` per session (`multiEditorTabsControl.ts:186` `role: 'tablist'`,
 * `:893` `role: 'tab'`), the close action living *inside* the tab (`:459-467` — 16×16
 * icon button, `padding: 2px`), and a dot replacing that close icon when the tab is
 * not being pointed at (`:478-490`, the "dirty" indicator). We reuse that idiom for
 * `attention`: a background turn finished, so the user has to be told even while
 * looking at another tab.
 *
 * The status dot in front of the label is the shell's own addition (the TUI shows the
 * same state in its status bar); it is drawn with CSS because the webview cannot ship
 * the codicon font (see README's exception list).
 */

import type { ReactElement } from 'react';

import type { SessionAttention, TabModel } from '../../shared';
import { postToHost } from '../bridge/channel';
import styles from '../styles/app.module.css';
import { statusLabel, tabLabel } from './selectors';

export interface TabBarProps {
  readonly tabs: readonly TabModel[];
  readonly activeSessionId: string | null;
  /** Open the sessions panel (the history entry point). */
  readonly onOpenHistory: () => void;
}

export function TabBar({ tabs, activeSessionId, onOpenHistory }: TabBarProps): ReactElement {
  return (
    <div className={styles.tabBar} data-testid="tab-bar">
      {/* Scrollable strip: the `+` and history buttons stay put on the right. */}
      <div className={styles.tabs} role="tablist" aria-label="Sessions" data-testid="tab-list">
        {tabs.length === 0 ? (
          <span className={styles.tabsEmpty}>No sessions</span>
        ) : (
          tabs.map((tab) => <Tab key={tab.sessionId} tab={tab} active={tab.sessionId === activeSessionId} />)
        )}
      </div>
      <button
        type="button"
        className={styles.iconButton}
        data-testid="new-session-button"
        aria-label="New session"
        title="New session"
        onClick={() => postToHost({ type: 'newSession' })}
      >
        +
      </button>
      <button
        type="button"
        className={styles.iconButton}
        data-testid="history-button"
        aria-label="Session history"
        title="Session history"
        onClick={onOpenHistory}
      >
        {/* No codicon font: a clock is drawn from two bars. */}
        <span className={styles.historyGlyph} aria-hidden="true" />
      </button>
    </div>
  );
}

interface TabProps {
  readonly tab: TabModel;
  readonly active: boolean;
}

function Tab({ tab, active }: TabProps): ReactElement {
  const label = tabLabel(tab);
  const attention = attentionLabel(tab.attention);

  return (
    <div
      className={styles.tab}
      role="tab"
      aria-selected={active}
      aria-label={`${label} — ${statusLabel(tab.status)}${attention === null ? '' : ` — ${attention}`}`}
      data-testid="tab"
      data-session-id={tab.sessionId}
      data-status={tab.status}
      data-attention={tab.attention}
      data-active={active ? 'true' : 'false'}
      tabIndex={active ? 0 : -1}
      onClick={() => postToHost({ type: 'activateSession', sessionId: tab.sessionId })}
      onKeyDown={(event) => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          postToHost({ type: 'activateSession', sessionId: tab.sessionId });
        }
      }}
    >
      <span className={styles.tabStatusDot} data-status={tab.status} aria-hidden="true" />
      <span className={styles.tabLabel}>{label}</span>
      <button
        type="button"
        className={styles.tabClose}
        aria-label={`Close ${label}`}
        title="Close session"
        data-testid="close-tab"
        onClick={(event) => {
          event.stopPropagation();
          postToHost({ type: 'closeSession', sessionId: tab.sessionId });
        }}
      >
        <span className={styles.closeGlyph} aria-hidden="true">
          ×
        </span>
        {tab.attention === 'none' ? null : (
          <span className={styles.attentionGlyph} data-attention={tab.attention} aria-hidden="true" />
        )}
      </button>
    </div>
  );
}

/** Wording for the background-turn badge; `null` when there is nothing to say. */
export function attentionLabel(attention: SessionAttention): string | null {
  switch (attention) {
    case 'result':
      return 'Finished in the background';
    case 'error':
      return 'Failed in the background';
    case 'none':
      return null;
    default:
      return null;
  }
}
