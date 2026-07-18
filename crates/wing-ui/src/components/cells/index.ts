// src/components/cells/index.ts — Register all Cell components with the registry.

import { registry } from '@/core/cell-registry'
import { UserMessageCell } from './UserMessage'
import { AssistantMessageCell } from './AssistantMessage'
import { ToolCallCell } from './ToolCallCell'
import { ReasoningCell } from './ReasoningCell'
import { DiffCell } from './DiffCell'
import { AskCell } from './AskCell'
import { MetricsCell } from './MetricsCell'
import { FallbackCell } from './FallbackCell'
import { TurnStartedCell, DoneCell, ErrorCell, ToolResultCell } from './TurnCells'

// Register all cells
registry.register('user', UserMessageCell)
registry.register('assistant', AssistantMessageCell)
registry.register('tool_call', ToolCallCell)
registry.register('tool_call_result', ToolResultCell)
registry.register('reasoning', ReasoningCell)
registry.register('diff', DiffCell)
registry.register('ask', AskCell)
registry.register('metrics', MetricsCell)
registry.register('turn_started', TurnStartedCell)
registry.register('done', DoneCell)
registry.register('error', ErrorCell)

// Register fallback for unknown types
registry.registerFallback(FallbackCell)

// Re-export for direct usage
export {
  UserMessageCell,
  AssistantMessageCell,
  ToolCallCell,
  ReasoningCell,
  DiffCell,
  AskCell,
  MetricsCell,
  FallbackCell,
  TurnStartedCell,
  DoneCell,
  ErrorCell,
  ToolResultCell,
}
