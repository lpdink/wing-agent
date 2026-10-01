/**
 * React hook: subscribe to `visualViewport.resize` and return the keyboard offset.
 *
 * On mobile, when the soft keyboard opens, `window.visualViewport.height` shrinks.
 * This hook tracks that and updates the composer's `bottom` style so it stays
 * visible above the keyboard.
 *
 * Lives here (in `src/app/`) rather than in `src/lib/` because it imports React
 * — `src/lib/` is the framework-free zone.
 */

import { useCallback, useEffect, useRef, useState } from 'react';

import { computeKeyboardOffset } from '../lib/viewport';

/**
 * React hook: subscribe to `visualViewport.resize` and return the current
 * keyboard offset (height of the keyboard in px, or 0).
 *
 * Also updates the composer element's `style.bottom` so CSS `position: fixed`
 * keeps it above the keyboard.
 *
 * @param composerRef - ref to the composer element (for setting bottom style)
 * @param transcriptRef - ref to the transcript element (for scroll-to-bottom)
 * @returns the keyboard offset in px (0 when no keyboard)
 */
export function useVisualViewportOffset(
  composerRef: { readonly current: HTMLDivElement | null },
  transcriptRef: { readonly current: HTMLDivElement | null },
): number {
  const [offset, setOffset] = useState(0);
  const layoutHeight = useRef(window.innerHeight);

  const handleResize = useCallback(() => {
    const vv = window.visualViewport;
    if (vv == null) {
      setOffset(0);
      return;
    }
    const newOffset = computeKeyboardOffset(vv, { innerHeight: layoutHeight.current });
    setOffset(newOffset);

    // Update the composer's bottom style for keyboard-aware positioning
    if (composerRef.current !== null) {
      if (newOffset > 0) {
        composerRef.current.style.bottom = `${newOffset}px`;
      } else {
        composerRef.current.style.bottom = '';
      }
    }

    // When the keyboard opens, scroll the transcript to the bottom
    if (newOffset > 0 && transcriptRef.current !== null) {
      transcriptRef.current.scrollTop = transcriptRef.current.scrollHeight;
    }
  }, [composerRef, transcriptRef]);

  useEffect(() => {
    const vv = window.visualViewport;
    // Guard: jsdom / old browsers don't have visualViewport
    if (!vv || typeof vv.addEventListener !== 'function') {
      return;
    }

    vv.addEventListener('resize', handleResize);

    const onOrientationChange = (): void => {
      setTimeout(() => {
        layoutHeight.current = window.innerHeight;
      }, 100);
    };
    window.addEventListener('orientationchange', onOrientationChange);

    return () => {
      vv.removeEventListener('resize', handleResize);
      window.removeEventListener('orientationchange', onOrientationChange);
    };
  }, [handleResize]);

  return offset;
}
