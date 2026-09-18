/**
 * The transcript scroller.
 *
 * Scrolling rules (all sourced, none invented):
 *
 * - "At the bottom" is `scrollTop + clientHeight >= scrollHeight - 2` — the same
 *   2px tolerance VS Code's chat list uses
 *   (`chatListWidget.ts:342-344`, `get isScrolledToBottom`).
 * - New content scrolls into view only while the user is at the bottom, so a
 *   streamed answer never yanks the view away from text the user is reading.
 * - Scrolling up reveals a "scroll to bottom" button shaped like VS Code's
 *   (`CHAT:3723-3735`: absolute, bottom 7px, right 12px, 27×27, fully round).
 */

import { useCallback, useLayoutEffect, useRef, useState } from 'react';
import type { ReactElement, RefObject } from 'react';

import type { SessionViewModel } from '../../shared';
import { CellView } from '../chat/CellView';
import styles from '../styles/chat.module.css';

/** VS Code: `chatListWidget.ts:342-344` — `scrollHeight - 2`. */
const AT_BOTTOM_TOLERANCE_PX = 2;

export function TranscriptView({ session }: { readonly session: SessionViewModel | null }): ReactElement {
  const scroller = useRef<HTMLDivElement>(null);
  const { pinned, handleScroll, scrollToBottom } = useStickToBottom(scroller, session);

  return (
    <div className={styles.transcriptWrapper}>
      <div className={styles.transcript} data-testid="transcript" ref={scroller} onScroll={handleScroll}>
        {session === null ? (
          <p className={styles.emptyState} data-testid="empty-state">
            The extension host has not hydrated a session yet.
          </p>
        ) : (
          session.cells.map((cell) => <CellView key={cell.id} cell={cell} sessionId={session.sessionId} />)
        )}
      </div>
      {pinned || session === null ? null : (
        <button
          type="button"
          className={styles.scrollDown}
          data-testid="scroll-to-bottom"
          aria-label="Scroll to bottom"
          title="Scroll to bottom"
          onClick={scrollToBottom}
        >
          ↓
        </button>
      )}
    </div>
  );
}

interface StickyScroll {
  /** True while the view follows the newest content. */
  readonly pinned: boolean;
  readonly handleScroll: () => void;
  readonly scrollToBottom: () => void;
}

/**
 * Keep the transcript pinned to its bottom.
 *
 * `signal` is the session object: the host replaces it on every patch, which
 * makes it an exact "content changed" signal without a second source of truth.
 */
function useStickToBottom(scroller: RefObject<HTMLDivElement | null>, signal: unknown): StickyScroll {
  const [pinned, setPinned] = useState(true);

  const handleScroll = useCallback(() => {
    const element = scroller.current;
    if (element === null) {
      return;
    }
    setPinned(element.scrollHeight - element.scrollTop - element.clientHeight <= AT_BOTTOM_TOLERANCE_PX);
  }, [scroller]);

  useLayoutEffect(() => {
    const element = scroller.current;
    if (element === null || !pinned) {
      return;
    }
    element.scrollTop = element.scrollHeight;
  }, [scroller, pinned, signal]);

  const scrollToBottom = useCallback(() => {
    const element = scroller.current;
    if (element !== null) {
      element.scrollTop = element.scrollHeight;
    }
    setPinned(true);
  }, [scroller]);

  return { pinned, handleScroll, scrollToBottom };
}
