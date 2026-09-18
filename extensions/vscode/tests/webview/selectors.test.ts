import { describe, expect, it } from 'vitest';

import type { CellModel } from '../../src/shared';
import { makeFixtureSession, makePendingUserCell } from '../../src/testing/fixtures';
import {
  baseName,
  contextUsage,
  formatDuration,
  formatTokens,
  selectLastMetrics,
  selectQueuedMessages,
  statusLabel,
  tabLabel,
  ttftMs,
} from '../../src/webview/app/selectors';

/**
 * The shell's display projections (step 05).
 *
 * They must be *pure lookups over host data* — these tests pin the exact wording and
 * the two thresholds, which is what keeps the status area honest.
 */

describe('selectQueuedMessages', () => {
  it('returns the pending user cells in transcript order', () => {
    const cells: readonly CellModel[] = [
      { kind: 'user', id: 'u1', createdAt: 0, text: 'accepted', state: 'accepted' },
      makePendingUserCell('u2', 'first queued'),
      { kind: 'user', id: 'u3', createdAt: 0, text: 'dropped', state: 'discarded' },
      makePendingUserCell('u4', 'second queued'),
    ];

    expect(selectQueuedMessages(cells).map((cell) => cell.id)).toEqual(['u2', 'u4']);
  });

  it('is empty when nothing is queued', () => {
    expect(selectQueuedMessages(makeFixtureSession().cells)).toEqual([]);
  });
});

describe('selectLastMetrics / ttftMs', () => {
  it('picks the newest metrics cell', () => {
    expect(selectLastMetrics(makeFixtureSession().cells)?.id).toBe('metrics-1');
    expect(ttftMs(makeFixtureSession())).toBe(288);
  });

  it('is null when no turn has finished', () => {
    expect(selectLastMetrics([{ kind: 'separator', id: 's', createdAt: 0, label: '' }])).toBeNull();
    expect(ttftMs(makeFixtureSession({ cells: [] }))).toBeNull();
  });
});

describe('contextUsage', () => {
  it('maps the ratio onto the ring and its thresholds (75% / 90%)', () => {
    expect(contextUsage({ usedTokens: 1000, windowTokens: 100_000, messageCount: 1 })).toEqual({
      percent: 1,
      level: 'normal',
      known: true,
    });
    expect(contextUsage({ usedTokens: 75_000, windowTokens: 100_000, messageCount: 1 }).level).toBe(
      'warning',
    );
    expect(contextUsage({ usedTokens: 90_000, windowTokens: 100_000, messageCount: 1 }).level).toBe('error');
  });

  it('clamps and treats a zero window as unknown', () => {
    expect(contextUsage({ usedTokens: 5, windowTokens: 0, messageCount: 1 })).toEqual({
      percent: 0,
      level: 'normal',
      known: false,
    });
    expect(contextUsage({ usedTokens: 300_000, windowTokens: 100_000, messageCount: 1 }).percent).toBe(100);
  });
});

describe('formatting', () => {
  it('formats token counts compactly', () => {
    expect(formatTokens(0)).toBe('0');
    expect(formatTokens(999)).toBe('999');
    expect(formatTokens(2048)).toBe('2.0k');
    expect(formatTokens(200_000)).toBe('200.0k');
    expect(formatTokens(1_500_000)).toBe('1.5M');
    expect(formatTokens(Number.NaN)).toBe('0');
  });

  it('formats durations like the thinking cell', () => {
    expect(formatDuration(288)).toBe('288ms');
    expect(formatDuration(5120)).toBe('5.1s');
  });

  it('takes the last path segment', () => {
    expect(baseName('/a/b/c')).toBe('c');
    expect(baseName('/a/b/')).toBe('b');
    expect(baseName('')).toBe('');
  });

  it('words the session status once, for every surface', () => {
    expect(statusLabel('idle')).toBe('Idle');
    expect(statusLabel('working')).toBe('Working');
    expect(statusLabel('waiting-for-input')).toBe('Waiting for input');
  });

  it('falls back to the id when the host has no title', () => {
    expect(tabLabel({ sessionId: 'session-a', title: '', status: 'idle', attention: 'none' })).toBe(
      'session-a',
    );
  });
});
