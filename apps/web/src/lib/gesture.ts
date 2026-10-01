/**
 * Pure touch gesture detection helpers.
 *
 * Framework-free: no React imports. The React hook (`useSwipeToClose`) lives
 * in `src/app/useSwipeToClose.ts`.
 */

/**
 * Configuration for swipe detection.
 */
export interface SwipeConfig {
  /** Minimum vertical distance before the gesture is treated as a swipe (px). */
  readonly threshold?: number;
  /** Maximum horizontal distance the finger can move before we cancel (px). */
  readonly maxLateralDrift?: number;
  /** Time limit for the gesture (ms). */
  readonly maxDurationMs?: number;
}

export const DEFAULT_SWIPE_CONFIG: Required<SwipeConfig> = {
  threshold: 80,
  maxLateralDrift: 40,
  maxDurationMs: 500,
};

/**
 * Pure: detect whether a touch sequence is a downward swipe.
 *
 * @returns `true` when the gesture meets the swipe criteria.
 */
export function isDownwardSwipe(
  start: { readonly x: number; readonly y: number; readonly time: number },
  end: { readonly x: number; readonly y: number; readonly time: number },
  config: Required<SwipeConfig> = DEFAULT_SWIPE_CONFIG,
): boolean {
  const dx = end.x - start.x;
  const dy = end.y - start.y;
  const dt = end.time - start.time;

  // Must move downward (dy positive).
  if (dy <= 0) {
    return false;
  }

  // Must exceed the vertical threshold.
  if (dy < config.threshold) {
    return false;
  }

  // Must not drift too far horizontally.
  if (Math.abs(dx) > config.maxLateralDrift) {
    return false;
  }

  // Must complete within the time limit.
  if (dt > config.maxDurationMs) {
    return false;
  }

  return true;
}
