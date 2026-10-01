/**
 * The transcript: the open session's cells, rendered by `@wing-agent/ui`.
 *
 * This is deliberately the thinnest layer that can exist — the container, the rows,
 * the scroll behaviour and every markdown/cell renderer come from the shared package
 * (`TranscriptView` + `CellView`, the same components the VS Code webview mounts), so
 * "the web shows what the editor shows" is a property of *one* implementation rather
 * than of two that have to be kept in step. What is left here is the projection the
 * package cannot do for us:
 *
 * - **version → view model.** Step 07's `recordVersion` bumps on every reduced event
 *   batch; the cells themselves are immutable objects inside a mutable record, so the
 *   memo key is `(record, version)` — and `CellView`'s own `memo(cell)` then keeps a
 *   streamed token down to one re-rendered cell.
 * - **the empty state**, in this app's words (the package's own text talks about an
 *   extension host).
 *
 * Scroll rules (stick to the bottom, pause while the user reads, the scroll-down
 * affordance) are the package's, unchanged — see its `app/TranscriptView.tsx` for the
 * sourced rules; this component only guarantees the *signal* it needs: a new session
 * object whenever the content changed.
 */

import { useMemo, type ReactElement } from 'react';

import type { SessionRecord } from '@wing-agent/session';
import { TranscriptView } from '@wing-agent/ui';

export interface TranscriptProps {
  /** The open session's model (step 07's `snapshot.record`). */
  readonly record: SessionRecord;
  /** Step 07's `snapshot.recordVersion` — the "cells may have changed" key. */
  readonly version: number;
}

export function Transcript({ record, version }: TranscriptProps): ReactElement {
  const session = useMemo(
    // `version` is the point of this memo: `record` is a stable object that is
    // mutated in place, so its identity alone says nothing about the cells — the
    // version is the "they may have changed" signal React keys the recomputation on.
    () => record.viewModel(),
    [record, version],
  );

  if (session.cells.length === 0) {
    return (
      <p className="pane__empty" data-testid="transcript-empty">
        This session has no messages yet.
      </p>
    );
  }

  return <TranscriptView session={session} />;
}
