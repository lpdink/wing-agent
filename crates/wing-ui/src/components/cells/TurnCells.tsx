// src/components/cells/TurnCells.tsx — TurnStarted, Done, Error, ToolResult cells.
//
// Simple indicator cells for turn lifecycle events and standalone tool results.

import { AlertCircle, Wrench } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type {
  TurnStartedChatItem,
  DoneChatItem,
  ErrorChatItem,
  ToolResultChatItem,
} from '@/stores/sessionStore'

export function TurnStartedCell(_props: CellProps<TurnStartedChatItem>) {
  return (
    <div className="flex justify-center">
      <span className="rounded-full bg-bg-elevated px-3 py-1 text-xs text-text-muted">
        Turn started
      </span>
    </div>
  )
}

export function DoneCell(_props: CellProps<DoneChatItem>) {
  return (
    <div className="flex justify-center">
      <span className="rounded-full bg-bg-elevated px-3 py-1 text-xs text-text-muted">
        Turn complete
      </span>
    </div>
  )
}

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
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-lg border border-border bg-bg-elevated px-3 py-2">
        <div className="flex items-center gap-2 text-sm">
          <Wrench className="h-3.5 w-3.5 text-text-muted" />
          <span className="text-text-dim">
            {data.toolName} result {data.success ? '✓' : '✗'}
          </span>
        </div>
      </div>
    </div>
  )
}
