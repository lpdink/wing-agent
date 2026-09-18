/**
 * Reconnect pacing — the pure part of the reconnect state machine.
 *
 * Mirrors `crates/wing/src/app/transport.rs::backoff` exactly:
 * `min(base · 2^attempt, max)` → 1s, 2s, 4s, 8s, 16s, 30s, 30s, …
 *
 * The exponent is pinned at 5 (like `attempt.min(5)` in Rust) so the sequence
 * never overflows and the cap is reached on the sixth attempt.
 */

export interface ReconnectOptions {
  /** First retry delay (attempt 0). Default 1000 ms. */
  readonly baseDelayMs: number;
  /** Upper bound for every retry delay. Default 30 000 ms. */
  readonly maxDelayMs: number;
}

export const DEFAULT_RECONNECT_OPTIONS: ReconnectOptions = {
  baseDelayMs: 1_000,
  maxDelayMs: 30_000,
};

/** Exponent cap: `2^5 · 1s = 32s`, already past the 30s ceiling. */
const MAX_EXPONENT = 5;

/** Retry delay for a (0-based) reconnect attempt. */
export function reconnectDelayMs(
  attempt: number,
  options: ReconnectOptions = DEFAULT_RECONNECT_OPTIONS,
): number {
  const exponent = Math.min(Math.max(Math.trunc(attempt), 0), MAX_EXPONENT);
  return Math.min(options.baseDelayMs * 2 ** exponent, options.maxDelayMs);
}
