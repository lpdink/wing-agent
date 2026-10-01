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
 * - Following means "always at the newest content", so it survives changes that do
 *   **not** come with a host patch: the user expanding a collapsed cell, streaming
 *   markdown re-laying out, a web font finishing, async highlighting. The content
 *   wrapper is observed for size changes; `overflow-anchor: none` (see the CSS)
 *   keeps the browser's scroll anchoring from fighting the explicit position.
 */

import { Fragment, useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { ReactElement, ReactNode, RefObject } from 'react';

import type { CellModel, SessionId, SessionViewModel } from '@wing-agent/session';
import { CellView } from '../chat/CellView';
import styles from '../styles/chat.module.css';

/** VS Code: `chatListWidget.ts:342-344` — `scrollHeight - 2`. */
const AT_BOTTOM_TOLERANCE_PX = 2;

export interface TranscriptViewProps {
  readonly session: SessionViewModel | null;
  /**
   * Cell renderer override, for shells that draw the rows themselves.
   *
   * The web shell (`apps/web`) replaces a handful of the renderer's cards with the
   * shared code/tool/ask components (its step 08b) and needs to keep *this*
   * scroller's rules while doing so — the alternative would be a second copy of the
   * stick-to-bottom contract below. The signature is {@link CellView}'s, so an
   * override is a drop-in for one cell; with no `renderCell` the rows come from
   * `CellView`, i.e. exactly what the VS Code webview renders.
   */
  readonly renderCell?: ((cell: CellModel, sessionId: SessionId) => ReactNode) | undefined;
}

export function TranscriptView({ session, renderCell }: TranscriptViewProps): ReactElement {
  const scroller = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const { pinned, handleScroll, scrollToBottom } = useStickToBottom(scroller, content, session);

  return (
    <div className={styles.transcriptWrapper}>
      <div className={styles.transcript} data-testid="transcript" ref={scroller} onScroll={handleScroll}>
        {session === null ? (
          <p className={styles.emptyState} data-testid="empty-state">
            The extension host has not hydrated a session yet.
          </p>
        ) : (
          <div className={styles.transcriptContent} data-testid="transcript-content" ref={content}>
            {session.cells.map((cell) =>
              renderCell === undefined ? (
                <CellView key={cell.id} cell={cell} sessionId={session.sessionId} />
              ) : (
                // The Fragment keeps the cell's identity stable across renders, so
                // React updates a streamed row in place instead of remounting it.
                <Fragment key={cell.id}>{renderCell(cell, session.sessionId)}</Fragment>
              ),
            )}
          </div>
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
 *
 * The `ResizeObserver` is the second, content-shaped signal: cell expansion and
 * async layout change the transcript without any patch, and those are exactly the
 * cases where "follow" used to get lost. It is optional (`typeof … === 'undefined'`)
 * so the renderer keeps working in DOMs without it (jsdom, older Electron).
 */
function useStickToBottom(
  scroller: RefObject<HTMLDivElement | null>,
  content: RefObject<HTMLDivElement | null>,
  signal: unknown,
): StickyScroll {
  const [pinned, setPinned] = useState(true);
  // Read by the observer and the layout effect; the state drives the button.
  const pinnedRef = useRef(true);

  const stick = useCallback(() => {
    const element = scroller.current;
    if (element !== null && pinnedRef.current) {
      element.scrollTop = element.scrollHeight;
    }
  }, [scroller]);

  const setPinnedBoth = useCallback((next: boolean) => {
    pinnedRef.current = next;
    setPinned(next);
  }, []);

  const handleScroll = useCallback(() => {
    const element = scroller.current;
    if (element === null) {
      return;
    }
    setPinnedBoth(element.scrollHeight - element.scrollTop - element.clientHeight <= AT_BOTTOM_TOLERANCE_PX);
  }, [scroller, setPinnedBoth]);

  // Content changed: follow when pinned (this is also the only path in a DOM
  // without ResizeObserver, e.g. jsdom tests).
  useLayoutEffect(() => {
    stick();
  }, [stick, signal, pinned]);

  // Height changed without a content change: expanding a collapsed cell, a
  // streamed markdown block settling, font/highlight work finishing.
  useEffect(() => {
    const element = content.current;
    if (element === null || typeof ResizeObserver === 'undefined') {
      return;
    }
    const observer = new ResizeObserver(() => {
      stick();
    });
    observer.observe(element);
    return () => {
      observer.disconnect();
    };
  }, [content, stick]);

  const scrollToBottom = useCallback(() => {
    const element = scroller.current;
    if (element !== null) {
      element.scrollTop = element.scrollHeight;
    }
    setPinnedBoth(true);
  }, [scroller, setPinnedBoth]);

  return { pinned, handleScroll, scrollToBottom };
}
