import { describe, expect, it } from 'vitest';

import type { SessionInfo } from '@wing-agent/client';

import {
  buildSessionRows,
  parseIsoMs,
  relativeTimeLabel,
  rowStatusFromModel,
  rowStatusFromWire,
  type SessionRowOverlay,
} from '../src/sessions/rows';

function info(overrides: Partial<SessionInfo> & { id: string }): SessionInfo {
  return {
    name: null,
    created_at: '2026-09-30T08:00:00Z',
    template_name: 'default',
    workspace: '/tmp/ws',
    last_interaction: '2026-10-01T12:00:00Z',
    status: 'idle',
    ...overrides,
  };
}

const NOW = Date.parse('2026-10-01T12:10:00Z');

describe('parseIsoMs', () => {
  it('parses an ISO string and rejects the rest', () => {
    expect(parseIsoMs('2026-10-01T12:00:00Z')).toBe(Date.parse('2026-10-01T12:00:00Z'));
    for (const value of [null, undefined, '', 'yesterday', '2026-13-45T99:99:99Z']) {
      expect(parseIsoMs(value)).toBeNull();
    }
  });
});

describe('relativeTimeLabel', () => {
  it('renders the same buckets the session list uses', () => {
    const at = (iso: string): string => relativeTimeLabel(Date.parse(iso), NOW);
    expect(at('2026-10-01T12:09:30Z')).toBe('just now');
    expect(at('2026-10-01T12:08:40Z')).toBe('1 min ago');
    expect(at('2026-10-01T11:40:00Z')).toBe('30 min ago');
    expect(at('2026-10-01T11:00:00Z')).toBe('1 h ago');
    expect(at('2026-10-01T06:10:00Z')).toBe('6 h ago');
    expect(at('2026-09-29T12:10:00Z')).toBe('2 d ago');
    expect(at('2026-09-14T12:10:00Z')).toBe('2026-09-14');
  });

  it('never renders an empty cell', () => {
    expect(relativeTimeLabel(null, NOW)).toBe('—');
  });

  it('treats a future timestamp as "just now" (clock skew between hosts)', () => {
    expect(relativeTimeLabel(NOW + 60_000, NOW)).toBe('just now');
  });
});

describe('status vocabulary', () => {
  it('maps the model status to the row status', () => {
    expect(rowStatusFromModel('waiting-for-input')).toBe('waiting');
    expect(rowStatusFromModel('working')).toBe('working');
    expect(rowStatusFromModel('idle')).toBe('idle');
  });

  it('passes the wire status through', () => {
    expect(rowStatusFromWire('inactive')).toBe('inactive');
    expect(rowStatusFromWire('waiting')).toBe('waiting');
  });
});

describe('buildSessionRows', () => {
  it('sorts by last interaction, newest first', () => {
    const rows = buildSessionRows(
      [
        info({ id: 'b', last_interaction: '2026-10-01T10:00:00Z' }),
        info({ id: 'a', last_interaction: '2026-10-01T11:00:00Z' }),
        info({ id: 'c', last_interaction: '2026-10-01T09:00:00Z' }),
      ],
      NOW,
      null,
    );
    expect(rows.map((row) => row.id)).toEqual(['a', 'b', 'c']);
  });

  it('keeps sessions without a timestamp last, in gateway order', () => {
    const rows = buildSessionRows(
      [
        info({ id: 'never-2', last_interaction: null }),
        info({ id: 'seen', last_interaction: '2026-10-01T11:00:00Z' }),
        info({ id: 'never-1', last_interaction: null }),
      ],
      NOW,
      null,
    );
    expect(rows.map((row) => row.id)).toEqual(['seen', 'never-2', 'never-1']);
  });

  it('falls back to `(untitled)` and keeps a real name', () => {
    const rows = buildSessionRows(
      [info({ id: 'a', name: '' }), info({ id: 'b', name: 'Fix the parser' })],
      NOW,
      null,
    );
    expect(rows.find((row) => row.id === 'a')?.title).toBe('(untitled)');
    expect(rows.find((row) => row.id === 'b')?.title).toBe('Fix the parser');
  });

  it('overlays the open session with live data and marks it current', () => {
    const overlay: SessionRowOverlay = {
      id: 'b',
      title: 'renamed by the model',
      status: 'waiting-for-input',
      attention: 'result',
    };
    const rows = buildSessionRows(
      [info({ id: 'a', status: 'idle' }), info({ id: 'b', name: 'old name', status: 'working' })],
      NOW,
      overlay,
    );
    const current = rows.find((row) => row.id === 'b');
    expect(current).toMatchObject({
      current: true,
      title: 'renamed by the model',
      status: 'waiting',
      attention: 'result',
    });
    expect(rows.find((row) => row.id === 'a')).toMatchObject({ current: false, attention: 'none' });
  });

  it('keeps the polled status of a session that is not open', () => {
    const rows = buildSessionRows([info({ id: 'a', status: 'working' })], NOW, {
      id: 'other',
      title: 'other',
      status: 'idle',
      attention: 'none',
    });
    expect(rows[0]).toMatchObject({ id: 'a', status: 'working', current: false });
  });

  it('hands the relative label to the row', () => {
    const rows = buildSessionRows([info({ id: 'a', last_interaction: '2026-10-01T11:40:00Z' })], NOW, null);
    expect(rows[0]?.updatedLabel).toBe('30 min ago');
  });
});
