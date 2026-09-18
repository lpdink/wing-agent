/**
 * Cell dispatch.
 *
 * `CellView` is memoized on `(cell, sessionId)`: the host replaces only the cell
 * objects it patched (see `applyCellPatches`), so a streamed token re-renders one
 * cell and leaves the rest of the transcript alone. This is the top half of the
 * "stable prefix" rule; `MarkdownBlock` is the bottom half.
 */

import { memo } from 'react';
import type { ReactElement } from 'react';

import type { CellModel, SessionId } from '../../shared';
import { unhandledVariant } from '../../shared';
import {
  AssistantCell,
  DiffCell,
  MetricsCell,
  SeparatorCell,
  SystemCell,
  ThinkingCell,
  TodoCell,
  ToolCallCell,
  UserCell,
} from './Cells';
import { AskCell } from './AskCell';

export const CellView = memo(function CellView({
  cell,
  sessionId,
}: {
  readonly cell: CellModel;
  readonly sessionId: SessionId;
}): ReactElement {
  switch (cell.kind) {
    case 'user':
      return <UserCell cell={cell} />;
    case 'assistant':
      return <AssistantCell cell={cell} />;
    case 'thinking':
      return <ThinkingCell cell={cell} />;
    case 'system':
      return <SystemCell cell={cell} />;
    case 'tool_call':
      return <ToolCallCell cell={cell} />;
    case 'diff':
      return <DiffCell cell={cell} sessionId={sessionId} />;
    case 'todo':
      return <TodoCell cell={cell} />;
    case 'ask':
      return <AskCell cell={cell} sessionId={sessionId} />;
    case 'metrics':
      return <MetricsCell cell={cell} />;
    case 'separator':
      return <SeparatorCell cell={cell} />;
    default:
      // Compile-time exhaustiveness (`cell` is `never` here). At runtime a newer
      // host must not crash an older renderer, so an unknown kind is dropped (with
      // a warning) instead of thrown.
      unhandledVariant(cell, 'CellView');
      return <></>;
  }
});
