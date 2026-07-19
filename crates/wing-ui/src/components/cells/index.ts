// src/components/cells/index.ts — Register all Cell components with the registry.

import { registry } from '@/core/cell-registry'
import { UserMessageCell } from './UserMessage'
import { AssistantMessageCell } from './AssistantMessage'
import { ToolCallCell } from './ToolCallCell'
import { ToolGroupCell } from './ToolGroupCell'
import { ReasoningCell } from './ReasoningCell'
import { DiffCell } from './DiffCell'
import { AskCell } from './AskCell'
import { FallbackCell } from './FallbackCell'
import { ErrorCell, ToolResultCell } from './TurnCells'

// Register all cells
registry.register('user', UserMessageCell)
registry.register('assistant', AssistantMessageCell)
registry.register('tool_call', ToolCallCell)
registry.register('tool_call_result', ToolResultCell)
registry.register('tool_group', ToolGroupCell)
registry.register('reasoning', ReasoningCell)
registry.register('diff', DiffCell)
registry.register('ask', AskCell)
registry.register('error', ErrorCell)

// Register fallback for unknown types
registry.registerFallback(FallbackCell)

// Re-export for direct usage
export {
  UserMessageCell,
  AssistantMessageCell,
  ToolCallCell,
  ToolGroupCell,
  ReasoningCell,
  DiffCell,
  AskCell,
  FallbackCell,
  ErrorCell,
  ToolResultCell,
}
