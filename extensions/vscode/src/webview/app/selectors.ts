/**
 * Display projections over host data.
 *
 * Everything in here is a **pure function of what the host already sent** — no
 * business rule is re-derived, no state is invented. That is the line the shell
 * must not cross (01 D7): the host decides *what* a queue is, the webview decides
 * *where it is drawn*.
 *
 * Used by the status area (context ring, token totals, TTFT) and the composer
 * (queue summary).
 */

import type {
  CellModel,
  ContextUsageModel,
  MetricsCellModel,
  SessionStatus,
  SessionViewModel,
  TabModel,
  UserCellModel,
} from '../../shared';

/** User messages the host has not accepted yet (still queued). */
export function selectQueuedMessages(cells: readonly CellModel[]): readonly UserCellModel[] {
  return cells.filter((cell): cell is UserCellModel => cell.kind === 'user' && cell.state === 'pending');
}

/**
 * The newest `metrics` cell in the transcript — the source of the TTFT readout.
 *
 * The host emits one per finished turn; the status area shows the latest one. This
 * is a lookup, not a computation: every number printed comes from the host.
 */
export function selectLastMetrics(cells: readonly CellModel[]): MetricsCellModel | null {
  for (let index = cells.length - 1; index >= 0; index -= 1) {
    const cell = cells[index];
    if (cell !== undefined && cell.kind === 'metrics') {
      return cell;
    }
  }
  return null;
}

/** How alarming the context usage is — thresholds from VS Code's context widget. */
export type ContextLevel = 'normal' | 'warning' | 'error';

export interface ContextUsage {
  /** 0–100, clamped (the widget clamps too). */
  readonly percent: number;
  readonly level: ContextLevel;
  /** True when the gateway reported a window size (0 means "unknown"). */
  readonly known: boolean;
}

/**
 * Context window usage.
 *
 * Thresholds: `>= 90` error, `>= 75` warning — `chatContextUsageWidget.ts:468-473`
 * (`this.domNode.classList.add('error'|'warning')`). A window size of `0` is
 * treated as unknown (the gateway may not report one) and renders as 0%.
 */
export function contextUsage(context: ContextUsageModel): ContextUsage {
  const known = context.windowTokens > 0;
  const ratio = known ? context.usedTokens / context.windowTokens : 0;
  const percent = Math.max(0, Math.min(100, ratio * 100));
  return {
    percent,
    level: percent >= 90 ? 'error' : percent >= 75 ? 'warning' : 'normal',
    known,
  };
}

/**
 * Compact token count: `512`, `2.0k`, `1.5M`.
 *
 * Presentation only (the exact value is available through the element's `title`).
 */
export function formatTokens(count: number): string {
  if (!Number.isFinite(count) || count < 0) {
    return '0';
  }
  if (count < 1000) {
    return String(Math.round(count));
  }
  if (count < 1_000_000) {
    return `${(count / 1000).toFixed(1)}k`;
  }
  return `${(count / 1_000_000).toFixed(1)}M`;
}

/** `1536ms` → `1.5s`, `820ms` → `820ms` (same rounding the thinking cell uses). */
export function formatDuration(ms: number): string {
  if (ms < 1000) {
    return `${Math.round(ms)}ms`;
  }
  return `${(ms / 1000).toFixed(1)}s`;
}

/** Last path segment (`/a/b/c` → `c`); `''` when there is nothing to show. */
export function baseName(path: string): string {
  const trimmed = path.replace(/[/\\]+$/, '');
  if (trimmed === '') {
    return '';
  }
  const parts = trimmed.split(/[/\\]/);
  return parts[parts.length - 1] ?? trimmed;
}

/**
 * Human wording for a session status.
 *
 * `waiting-for-input` reads as "Waiting for input" everywhere (tab dot tooltip,
 * status area, a11y labels) so the same state is never described two ways.
 */
export function statusLabel(status: SessionStatus): string {
  switch (status) {
    case 'idle':
      return 'Idle';
    case 'working':
      return 'Working';
    case 'waiting-for-input':
      return 'Waiting for input';
    default:
      return status;
  }
}

/** TTFT in ms of the newest metrics cell; `null` when there is none. */
export function ttftMs(session: SessionViewModel): number | null {
  return selectLastMetrics(session.cells)?.usage.ttftMs ?? null;
}

/** Names shown in the tab bar are already host-derived; this is the fallback. */
export function tabLabel(tab: TabModel): string {
  return tab.title === '' ? tab.sessionId : tab.title;
}
