/**
 * The thinking cell, drawn with the shared `ReasoningRow`.
 *
 * Behaviour is the old `ThinkingCell`'s, line for line: the block is open while the
 * model is thinking, collapses to one line when the stream ends, and a user toggle
 * wins over that default forever. The "forever" is the package's collapse-override
 * store — the row is handed the *same key* the old cell used (`${cell.id}:thinking`),
 * so a session switch (which unmounts the rows) still remembers what the reader
 * chose before the swap.
 *
 * What the card adds on top: the think icon, a shimmer while running, and the
 * settled preview line (the latest complete paragraph's first line).
 */

import type { ReactElement } from 'react';

import type { ThinkingCellModel } from '@wing-agent/session';
import { ReasoningRow } from '@wing-agent/ui';

export function ThinkingRow({ cell }: { readonly cell: ThinkingCellModel }): ReactElement {
  return (
    <article
      className="cell"
      data-cell-id={cell.id}
      data-cell-kind="thinking"
      data-streaming={cell.streaming ? 'true' : 'false'}
    >
      <ReasoningRow
        text={cell.text}
        streaming={cell.streaming}
        durationMs={cell.durationMs}
        // The override store's key, unchanged from the swapped-out cell: the
        // reader's expand/collapse choice survives this step and a session switch.
        collapseKey={`${cell.id}:thinking`}
      />
    </article>
  );
}
