/**
 * A file mutation: the shared `DiffBlock` card.
 *
 * The card takes Wing's `DiffCellModel` directly — the host already computed and
 * windowed the rows (that is the borrow card's whole adaptation: no `structuredPatch`
 * in the renderer). `Open diff` is the *cell's* action, not the card's, so it stays
 * here: it asks the host to open the editor's diff view, and the web bridge answers
 * with a notice (a browser has no native diff — step 08's difference ①).
 */

import type { ReactElement } from 'react';

import type { DiffCellModel, SessionId } from '@wing-agent/session';
import { DiffBlock, postToHost } from '@wing-agent/ui';

import { DIFF_LABELS } from './labels';

export function DiffRow({
  cell,
  sessionId,
}: {
  readonly cell: DiffCellModel;
  readonly sessionId: SessionId;
}): ReactElement {
  return (
    <article className="cell" data-cell-id={cell.id} data-cell-kind="diff">
      <DiffBlock cell={cell} labels={DIFF_LABELS} />
      <div className="cell__diff-actions">
        <button
          type="button"
          className="cell__diff-open"
          onClick={() => {
            postToHost({ type: 'openDiff', sessionId, cellId: cell.id });
          }}
        >
          Open diff
        </button>
      </div>
    </article>
  );
}
