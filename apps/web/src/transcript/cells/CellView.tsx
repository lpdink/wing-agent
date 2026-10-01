/**
 * Cell dispatch — the web transcript's own row renderer.
 *
 * Step 08 rendered every cell through the package's `CellView` switch. Step 08b
 * swaps the rows whose cards the port replaced (markdown fences, the process rows,
 * the diff card, the ask trio, reasoning) for the shared wing-app components, and
 * this is the switch that does it. The kinds with no replacement are still the
 * package's own components — the same ones the VS Code webview renders — so the
 * swap is opt-in per kind rather than a fork of the transcript.
 *
 * `memo` on `(cell, sessionId, workspace)`: the record replaces only the cell
 * objects it patched, and `sessionId` / `workspace` are strings, so a streamed
 * token re-renders one row. (The session *object* is rebuilt on every version bump,
 * which is why it is not a prop.)
 */

import { memo, type ReactElement } from 'react';

import type { AssistantCellModel, CellModel, SessionId } from '@wing-agent/session';
import { MetricsCell, SeparatorCell, SystemCell, TodoCell, UserCell, unhandledVariant } from '@wing-agent/ui';

import { AskRow } from './AskRow';
import { DiffRow } from './DiffRow';
import { WebMarkdownStream } from './markdown';
import { ThinkingRow } from './ThinkingRow';
import { ToolCallRow } from './ToolCallRow';

export interface WebCellViewProps {
  readonly cell: CellModel;
  readonly sessionId: SessionId;
  /** The open session's working directory (`''` when unknown) — the Bash card's prompt. */
  readonly workspace: string;
}

export const WebCellView = memo(function WebCellView({
  cell,
  sessionId,
  workspace,
}: WebCellViewProps): ReactElement {
  switch (cell.kind) {
    case 'user':
      return <UserCell cell={cell} />;
    case 'assistant':
      return <AssistantRow cell={cell} />;
    case 'thinking':
      return <ThinkingRow cell={cell} />;
    case 'system':
      return <SystemCell cell={cell} />;
    case 'tool_call':
      return <ToolCallRow cell={cell} workspace={workspace} />;
    case 'diff':
      return <DiffRow cell={cell} sessionId={sessionId} />;
    case 'todo':
      return <TodoCell cell={cell} />;
    case 'ask':
      return <AskRow cell={cell} />;
    case 'metrics':
      return <MetricsCell cell={cell} />;
    case 'separator':
      return <SeparatorCell cell={cell} />;
    default:
      // Compile-time exhaustiveness (`cell` is `never` here). At runtime a newer
      // host must not crash an older renderer, so an unknown kind is dropped (with
      // a warning) instead of thrown — the package's own rule.
      unhandledVariant(cell, 'web CellView');
      return <></>;
  }
});

/** The assistant answer: the shared markdown pipeline, fences drawn by the new card. */
function AssistantRow({ cell }: { readonly cell: AssistantCellModel }): ReactElement {
  return (
    <article
      className="cell"
      data-cell-id={cell.id}
      data-cell-kind="assistant"
      data-streaming={cell.streaming ? 'true' : 'false'}
    >
      <WebMarkdownStream text={cell.text} streaming={cell.streaming} />
    </article>
  );
}
