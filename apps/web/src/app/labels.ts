/**
 * Small text helpers shared by the shell components.
 *
 * Kept out of the components so they are testable as plain functions and so the
 * vocabulary ("waiting for input", "reconnecting in 4s") has exactly one home.
 */

import type { ConnectionView } from '../connection/runtime';
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

/** Connection text for the top bar; the countdown makes a retry visible. */
export function connectionLabel(view: ConnectionView): string {
  switch (view.phase) {
    case 'connected':
      return 'connected';
    case 'connecting':
      return view.attempt === 0 ? 'connecting…' : `connecting… (attempt ${view.attempt + 1})`;
    case 'reconnecting':
      return view.reconnectInMs === null
        ? 'reconnecting…'
        : `reconnecting in ${Math.max(1, Math.ceil(view.reconnectInMs / 1_000))}s`;
    case 'offline':
      return view.unauthorized ? 'unauthorized' : 'offline';
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
