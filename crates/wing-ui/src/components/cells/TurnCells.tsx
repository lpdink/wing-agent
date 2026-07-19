// src/components/cells/TurnCells.tsx — Error and standalone ToolResult cells.
//
// TurnStarted and Done cells removed — turn boundaries are conveyed
// naturally by user messages. ChatArea SKIP_TYPES still filters these
// event types from the render list.

import { AlertCircle, Wrench } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ErrorChatItem, ToolResultChatItem } from '@/stores/sessionStore'

export function ErrorCell({ data }: CellProps<ErrorChatItem>) {
  return (
    <div className="flex justify-center">
      <div className="flex items-center gap-2 rounded-lg bg-error/10 px-3 py-2 text-sm text-error">
        <AlertCircle className="h-4 w-4" />
        <span>{data.message}</span>
      </div>
    </div>
  )
}

/** Standalone tool result (when not merged into a ToolCallCell). */
export function ToolResultCell({ data }: CellProps<ToolResultChatItem>) {
  return (
    <div className="flex h-6 items-center gap-1.5 px-1 text-[13px] text-text-muted">
      <Wrench className="h-3.5 w-3.5 shrink-0 text-text-dim" />
      <span className="text-text-dim">
        {data.toolName} result {data.success ? '✓' : '✗'}
      </span>
    </div>
  )
}
