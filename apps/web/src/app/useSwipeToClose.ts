/**
 * React hook: swipe-to-close gesture for overlay panels on mobile.
 *
 * Spread `onTouchStart` and `onTouchEnd` onto a panel element to detect a
 * downward swipe that closes it.
 */

import { useRef, type TouchEvent } from 'react';

import { isDownwardSwipe } from '../lib/gesture';

/**
 * React hook: spread `onTouchStart` and `onTouchEnd` onto an element to detect
 * swipe-to-close.
 *
 * @param onSwipeDown - callback fired when a downward swipe is detected.
 * @param enabled - `false` to disable gesture detection (desktop / scrolled).
 */
export function useSwipeToClose(
  onSwipeDown: () => void,
  enabled: boolean,
): {
  readonly onTouchStart: (event: TouchEvent<HTMLElement>) => void;
  readonly onTouchEnd: (event: TouchEvent<HTMLElement>) => void;
} {
  const startRef = useRef<{ x: number; y: number; time: number } | null>(null);

  return {
    onTouchStart: (event: TouchEvent<HTMLElement>) => {
      if (!enabled) {
        return;
      }
      const touch = event.touches[0];
      if (touch === undefined) {
        return;
      }
      startRef.current = { x: touch.clientX, y: touch.clientY, time: Date.now() };
    },

    onTouchEnd: (event: TouchEvent<HTMLElement>) => {
      if (!enabled || startRef.current === null) {
        startRef.current = null;
        return;
      }
      const touch = event.changedTouches[0];
      if (touch === undefined) {
        startRef.current = null;
        return;
      }
      const end = { x: touch.clientX, y: touch.clientY, time: Date.now() };
      if (isDownwardSwipe(startRef.current, end)) {
        onSwipeDown();
      }
      startRef.current = null;
    },
  };
}
