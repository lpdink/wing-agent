/**
 * Small text helpers shared by the shell components.
 *
 * Kept out of the components so they are testable as plain functions and so the
 * vocabulary ("waiting for input", "Reconnecting in 4s") has exactly one home.
 * The connection *vocabulary* now lives with the indicator (`ConnectionStatus.tsx`)
 * — the top-bar dot and status word it replaced are gone (step 08b).
 */

import type { SessionRowStatus } from '../sessions/rows';

/** Status text for the top bar / session rows. */
export function statusLabel(status: SessionRowStatus): string {
  switch (status) {
    case 'inactive':
      return 'inactive';
    case 'idle':
      return 'idle';
    case 'working':
      return 'working';
    case 'waiting':
      return 'waiting for input';
  }
}

/** Token counts with thousands separators (`12,480`); `—` for nothing known. */
export function formatTokens(tokens: number): string {
  if (tokens <= 0) {
    return '—';
  }
  return tokens.toLocaleString('en-US');
}

/** Percent of the context window in use, or `null` when the window is unknown. */
export function contextPercent(used: number, windowTokens: number): number | null {
  if (windowTokens <= 0) {
    return null;
  }
  return Math.min(100, Math.round((used / windowTokens) * 100));
}
