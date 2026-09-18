import type { CellModel, CellPatch, ResyncReason } from '../../shared';
import { assertNever } from '../../shared';

/**
 * Cell patch application — the webview's whole "reducer".
 *
 * Pure, DOM-free and dependency-free so it can be unit tested at full speed and
 * reused by the preview harness. It never guesses: any op it cannot apply
 * faithfully returns a {@link ResyncReason}, which the bridge controller turns
 * into a `resync` request (the host answers with a full `hydrate`).
 *
 * Invariants (see `src/shared/bridge.ts`):
 * - `update` / `append_text` / `remove` address existing cells;
 * - `append` never re-uses an existing id;
 * - `insert_after` addresses an existing cell.
 */

export type ApplyPatchesResult =
  | { readonly ok: true; readonly cells: readonly CellModel[] }
  | { readonly ok: false; readonly reason: ResyncReason };

/** Apply an ordered batch of patches to a transcript. */
export function applyCellPatches(
  cells: readonly CellModel[],
  patches: readonly CellPatch[],
): ApplyPatchesResult {
  let current: CellModel[] = [...cells];
  let index = indexCells(current);

  for (const patch of patches) {
    const applied = applyOne(current, index, patch);
    if (!applied.ok) {
      return applied;
    }
    current = applied.cells;
    index = applied.index;
  }

  return { ok: true, cells: current };
}

interface Applied {
  readonly ok: true;
  readonly cells: CellModel[];
  readonly index: Map<string, number>;
}

function indexCells(cells: readonly CellModel[]): Map<string, number> {
  const index = new Map<string, number>();
  for (const [position, cell] of cells.entries()) {
    index.set(cell.id, position);
  }
  return index;
}

function applyOne(
  cells: CellModel[],
  index: Map<string, number>,
  patch: CellPatch,
): Applied | { ok: false; reason: ResyncReason } {
  switch (patch.op) {
    case 'append': {
      if (index.has(patch.cell.id)) {
        return { ok: false, reason: 'duplicate-cell' };
      }
      const position = cells.length;
      cells.push(patch.cell);
      index.set(patch.cell.id, position);
      return { ok: true, cells, index };
    }
    case 'insert_after': {
      const after = index.get(patch.afterCellId);
      if (after === undefined) {
        return { ok: false, reason: 'unknown-cell' };
      }
      if (index.has(patch.cell.id)) {
        return { ok: false, reason: 'duplicate-cell' };
      }
      cells.splice(after + 1, 0, patch.cell);
      return { ok: true, cells, index: indexCells(cells) };
    }
    case 'update': {
      const position = index.get(patch.cell.id);
      if (position === undefined) {
        return { ok: false, reason: 'unknown-cell' };
      }
      cells[position] = patch.cell;
      return { ok: true, cells, index };
    }
    case 'append_text': {
      const position = index.get(patch.cellId);
      if (position === undefined) {
        return { ok: false, reason: 'unknown-cell' };
      }
      const cell = cells[position];
      const updated = cell === undefined ? undefined : appendText(cell, patch.text);
      if (updated === undefined) {
        return { ok: false, reason: 'unsupported-op' };
      }
      cells[position] = updated;
      return { ok: true, cells, index };
    }
    case 'remove': {
      const position = index.get(patch.cellId);
      if (position === undefined) {
        return { ok: false, reason: 'unknown-cell' };
      }
      cells.splice(position, 1);
      return { ok: true, cells, index: indexCells(cells) };
    }
    case 'replace_all': {
      cells.splice(0, cells.length, ...patch.cells);
      return { ok: true, cells, index: indexCells(cells) };
    }
    default:
      return assertNever(patch, 'applyCellPatches');
  }
}

/**
 * Append streamed text to a text-bearing cell, or `undefined` when the cell kind
 * cannot receive text (the caller then asks for a resync instead of guessing).
 */
function appendText(cell: CellModel, text: string): CellModel | undefined {
  switch (cell.kind) {
    case 'user':
      return { ...cell, text: cell.text + text };
    case 'assistant':
      return { ...cell, text: cell.text + text };
    case 'thinking':
      return { ...cell, text: cell.text + text };
    case 'system':
      return { ...cell, text: cell.text + text };
    case 'tool_call':
    case 'diff':
    case 'todo':
    case 'ask':
    case 'metrics':
    case 'separator':
      return undefined;
    default:
      return assertNever(cell, 'appendText');
  }
}

/** True when `seq` continues the stream at `lastSeq`. */
export function isExpectedSeq(lastSeq: number, seq: number): boolean {
  return seq === lastSeq + 1;
}
