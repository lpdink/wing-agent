/**
 * The session list, as rows.
 *
 * Pure derivation from two inputs: the gateway's `GET /api/session/list` response
 * and — for the one session that is open — the live `SessionRecord`. Nothing here
 * talks to the network; the polling that produces the list lives in
 * `src/connection/runtime.ts` (design.md D6: the gateway only pushes events for
 * the sessions a client subscribed to, so the list is polled and the row you are
 * looking at is overlaid with live data).
 *
 * The wire vocabulary and the model vocabulary differ in one place: the gateway
 * says `waiting` and the reduction says `waiting-for-input` (an open question /
 * approval). Both become `waiting` here so the UI has a single set of states.
 */

import type { SessionInfo } from '@wing-agent/client';
import type { SessionAttention, SessionStatus } from '@wing-agent/session';

export type SessionRowStatus = 'inactive' | 'idle' | 'working' | 'waiting';

export interface SessionRow {
  readonly id: string;
  /** Gateway name, or `(untitled)` — the same fallback the VSCode list uses. */
  readonly title: string;
  readonly status: SessionRowStatus;
  readonly workspace: string | null;
  readonly lastInteractionMs: number | null;
  /** `2 min ago` / `2026-09-14` / `—` (never an empty cell). */
  readonly updatedLabel: string;
  /** This is the session the main pane shows. */
  readonly current: boolean;
  /** Attention badge of an *inactive* session that finished a turn (`none` most of the time). */
  readonly attention: SessionAttention;
}

/** Live state of the open session, which wins over the polled list. */
export interface SessionRowOverlay {
  readonly id: string;
  readonly title: string;
  readonly status: SessionStatus;
  readonly attention: SessionAttention;
}

/** `waiting-for-input` (model) → `waiting` (row); the rest are shared names. */
export function rowStatusFromModel(status: SessionStatus): SessionRowStatus {
  return status === 'waiting-for-input' ? 'waiting' : status;
}

/** Wire status → row status. */
export function rowStatusFromWire(status: SessionInfo['status']): SessionRowStatus {
  return status;
}

/** ISO-8601 → epoch ms; `null` when missing or unparseable. */
export function parseIsoMs(value: string | null | undefined): number | null {
  if (value === null || value === undefined || value === '') {
    return null;
  }
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : null;
}

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/**
 * A short "when" label. Deterministic (driven by `nowMs`, never by `Date.now()`),
 * which is what makes the list testable and the screenshots stable.
 */
export function relativeTimeLabel(fromMs: number | null, nowMs: number): string {
  if (fromMs === null) {
    return '—';
  }
  const delta = nowMs - fromMs;
  if (delta < 45_000) {
    return 'just now';
  }
  if (delta < 90_000) {
    return '1 min ago';
  }
  if (delta < HOUR) {
    return `${Math.round(delta / MINUTE)} min ago`;
  }
  if (delta < 90 * MINUTE) {
    return '1 h ago';
  }
  if (delta < DAY) {
    return `${Math.round(delta / HOUR)} h ago`;
  }
  if (delta < 7 * DAY) {
    return `${Math.round(delta / DAY)} d ago`;
  }
  return new Date(fromMs).toISOString().slice(0, 10);
}

/**
 * Build the rows: newest interaction first, the open session overlaid with live
 * data and tagged `current`.
 *
 * Sessions without a timestamp sort last (a fresh, never-used session), ties keep
 * the gateway's order.
 */
export function buildSessionRows(
  sessions: readonly SessionInfo[],
  nowMs: number,
  overlay: SessionRowOverlay | null,
): SessionRow[] {
  const rows = sessions.map((info, index) => {
    const lastInteractionMs = parseIsoMs(info.last_interaction);
    const isCurrent = overlay !== null && overlay.id === info.id;
    const title =
      isCurrent && overlay.title.trim() !== ''
        ? overlay.title
        : info.name !== null && info.name !== ''
          ? info.name
          : '(untitled)';
    return {
      row: {
        id: info.id,
        title,
        status: isCurrent ? rowStatusFromModel(overlay.status) : rowStatusFromWire(info.status),
        workspace: info.workspace,
        lastInteractionMs,
        updatedLabel: relativeTimeLabel(lastInteractionMs, nowMs),
        current: isCurrent,
        attention: isCurrent && overlay !== null ? overlay.attention : 'none',
      } satisfies SessionRow,
      index,
    };
  });
  rows.sort((left, right) => {
    const a = left.row.lastInteractionMs;
    const b = right.row.lastInteractionMs;
    if (a === b) {
      return left.index - right.index;
    }
    if (a === null) {
      return 1;
    }
    if (b === null) {
      return -1;
    }
    return b - a;
  });
  return rows.map((entry) => entry.row);
}
