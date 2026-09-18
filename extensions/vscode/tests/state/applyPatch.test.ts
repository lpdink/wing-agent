import { describe, expect, it } from 'vitest';

import type { CellModel, CellPatch } from '../../src/shared';
import { applyCellPatches, isExpectedSeq } from '../../src/webview/state/applyPatch';
import { makeAllCells, makeFixtureSession } from '../../src/testing/fixtures';

/**
 * Patch application is the webview's only state transition — every guarantee
 * about "the renderer never shows something the host did not say" rests on it.
 * These tests pin the success paths *and* every failure mode that must trigger a
 * resync instead of a best-effort guess.
 */

function ids(cells: readonly { id: string }[]): string[] {
  return cells.map((cell) => cell.id);
}

describe('applyCellPatches', () => {
  it('appends and keeps the order of the incoming batch', () => {
    const cells = makeAllCells();
    const result = applyCellPatches(cells, [
      { op: 'append', cell: { kind: 'separator', id: 'new-1', createdAt: 1, label: 'after' } },
      { op: 'append', cell: { kind: 'separator', id: 'new-2', createdAt: 2, label: 'after too' } },
    ]);

    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(ids(result.cells).slice(-2)).toEqual(['new-1', 'new-2']);
      expect(result.cells).toHaveLength(cells.length + 2);
    }
  });

  it('does not mutate the input transcript', () => {
    const cells = makeAllCells();
    const before = ids(cells);

    applyCellPatches(cells, [{ op: 'remove', cellId: before[0] ?? '' }]);

    expect(ids(cells)).toEqual(before);
  });

  it('inserts directly after the anchor (out-of-order tool results)', () => {
    const cells = makeAllCells();
    const anchor = ids(cells)[1] ?? '';

    const result = applyCellPatches(cells, [
      {
        op: 'insert_after',
        afterCellId: anchor,
        cell: { kind: 'separator', id: 'inserted', createdAt: 3, label: '' },
      },
    ]);

    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(ids(result.cells).slice(0, 3)).toEqual([ids(cells)[0], anchor, 'inserted']);
    }
  });

  it('replaces a cell in place on update', () => {
    const cells = makeAllCells();
    const target = cells.find((cell) => cell.kind === 'assistant');
    expect(target).toBeDefined();

    const result = applyCellPatches(cells, [
      {
        op: 'update',
        cell: { kind: 'assistant', id: target?.id ?? '', createdAt: 0, text: 'final', streaming: false },
      },
    ]);

    expect(result.ok).toBe(true);
    if (result.ok) {
      const updated = result.cells.find((cell) => cell.id === target?.id);
      expect(updated?.kind === 'assistant' && updated.text).toBe('final');
      expect(result.cells).toHaveLength(cells.length);
    }
  });

  it('appends streamed text to text-bearing cells only', () => {
    const cells = makeAllCells();
    const assistant = cells.find((cell) => cell.kind === 'assistant');
    const separator = cells.find((cell) => cell.kind === 'separator');
    expect(assistant).toBeDefined();
    expect(separator).toBeDefined();

    const streamed = applyCellPatches(cells, [
      { op: 'append_text', cellId: assistant?.id ?? '', text: ' more' },
    ]);
    expect(streamed.ok).toBe(true);
    if (streamed.ok) {
      const updated = streamed.cells.find((cell) => cell.id === assistant?.id);
      expect(updated?.kind === 'assistant' && updated.text.endsWith(' more')).toBe(true);
    }

    const rejected = applyCellPatches(cells, [
      { op: 'append_text', cellId: separator?.id ?? '', text: 'nope' },
    ]);
    expect(rejected).toEqual({ ok: false, reason: 'unsupported-op' });
  });

  it('removes cells and re-indexes the rest', () => {
    const cells = makeAllCells();
    const [first, second] = ids(cells);

    const result = applyCellPatches(cells, [
      { op: 'remove', cellId: first ?? '' },
      { op: 'append_text', cellId: second ?? '', text: '' },
    ]);

    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.cells).toHaveLength(cells.length - 1);
      expect(ids(result.cells)[0]).toBe(second);
    }
  });

  it('replaces the whole transcript (compaction / rewind)', () => {
    const cells = makeAllCells();
    const replacement = [{ kind: 'separator', id: 'only', createdAt: 9, label: 'compacted' }] as const;

    const result = applyCellPatches(cells, [{ op: 'replace_all', cells: replacement }]);

    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(ids(result.cells)).toEqual(['only']);
    }
  });

  it('reports unknown-cell when an op targets a cell it never saw', () => {
    const cells = makeAllCells();
    const ops: CellPatch[] = [
      { op: 'append_text', cellId: 'ghost', text: 'x' },
      { op: 'update', cell: { kind: 'separator', id: 'ghost', createdAt: 0, label: '' } },
      { op: 'remove', cellId: 'ghost' },
      {
        op: 'insert_after',
        afterCellId: 'ghost',
        cell: { kind: 'separator', id: 'x', createdAt: 0, label: '' },
      },
    ];

    for (const op of ops) {
      expect(applyCellPatches(cells, [op])).toEqual({ ok: false, reason: 'unknown-cell' });
    }
  });

  it('reports duplicate-cell when append reuses an id', () => {
    const cells = makeAllCells();
    const existing = cells[0];
    expect(existing).toBeDefined();

    const result = applyCellPatches(cells, [{ op: 'append', cell: { ...existing!, createdAt: 99 } }]);

    expect(result).toEqual({ ok: false, reason: 'duplicate-cell' });
  });

  it('stops at the first failing op and reports it', () => {
    const cells = makeAllCells();

    const result = applyCellPatches(cells, [
      { op: 'append', cell: { kind: 'separator', id: 'ok-1', createdAt: 0, label: '' } },
      { op: 'remove', cellId: 'missing' },
      { op: 'append', cell: { kind: 'separator', id: 'never', createdAt: 0, label: '' } },
    ]);

    expect(result).toEqual({ ok: false, reason: 'unknown-cell' });
  });

  it('is a no-op for an empty batch', () => {
    const cells = makeAllCells();
    const result = applyCellPatches(cells, []);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.cells).toEqual(cells);
    }
  });

  it('handles a long append_text stream without losing order', () => {
    let cells: readonly CellModel[] = makeFixtureSession().cells;
    const target = cells.find((cell) => cell.kind === 'assistant');
    expect(target).toBeDefined();

    for (let i = 0; i < 500; i += 1) {
      const result = applyCellPatches(cells, [
        { op: 'append_text', cellId: target?.id ?? '', text: `${i} ` },
      ]);
      expect(result.ok).toBe(true);
      if (result.ok) {
        cells = result.cells;
      }
    }

    const updated = cells.find((cell) => cell.id === target?.id);
    expect(updated?.kind === 'assistant' && updated.text.includes('0 1 2 3 4 5 6 7 8 9 10 11 ')).toBe(true);
    expect(updated?.kind === 'assistant' && updated.text.endsWith('498 499 ')).toBe(true);
  });
});

describe('isExpectedSeq', () => {
  it('accepts exactly the next sequence number', () => {
    expect(isExpectedSeq(0, 1)).toBe(true);
    expect(isExpectedSeq(7, 8)).toBe(true);
    expect(isExpectedSeq(7, 9)).toBe(false);
    expect(isExpectedSeq(7, 7)).toBe(false);
    expect(isExpectedSeq(7, 3)).toBe(false);
  });
});
