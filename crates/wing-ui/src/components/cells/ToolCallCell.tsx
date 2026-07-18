// src/components/cells/ToolCallCell.tsx — Tool call card with collapsible details.
//
// Shows tool icon + name + argument summary in header.
// Expands to show full JSON arguments.
// If a tool_call_result follows, merges it into the same card.

import { useState } from 'react'
import { ChevronDown, ChevronRight, Wrench, CheckCircle2, XCircle } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ToolCallChatItem, ToolResultChatItem } from '@/stores/sessionStore'

interface ToolCallCellProps extends CellProps<ToolCallChatItem> {
  /** Optional tool result (if next message is tool_call_result with same toolCallId) */
  result?: ToolResultChatItem | null
}

/** Extract a short summary from tool args for the collapsed header. */
function getArgsSummary(args: Record<string, unknown>): string {
  // Common patterns: file path, command, pattern
  if (typeof args.path === 'string') return args.path
  if (typeof args.command === 'string') {
    const cmd = args.command
    return cmd.length > 50 ? cmd.slice(0, 50) + '…' : cmd
  }
  if (typeof args.pattern === 'string') return `/${args.pattern}/`
  const keys = Object.keys(args)
  if (keys.length === 0) return ''
  return keys.slice(0, 2).join(', ')
}

export function ToolCallCell({ data, result }: ToolCallCellProps) {
  const [expanded, setExpanded] = useState(false)
  const summary = getArgsSummary(data.toolArgs)
  const hasResult = result != null

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-lg border border-border bg-bg-tool px-3 py-2.5 shadow-sm">
        {/* Header — always visible */}
        <button
          onClick={() => setExpanded(!expanded)}
          className="flex w-full items-center gap-2 text-left"
        >
          {expanded ? (
            <ChevronDown className="h-3.5 w-3.5 shrink-0 text-text-muted" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5 shrink-0 text-text-muted" />
          )}
          <Wrench className="h-3.5 w-3.5 shrink-0 text-accent" />
          <span className="text-sm font-medium text-text">{data.toolName}</span>
          {summary && <span className="truncate text-xs text-text-muted">{summary}</span>}
          {hasResult &&
            (result.success ? (
              <CheckCircle2 className="ml-auto h-3.5 w-3.5 shrink-0 text-success" />
            ) : (
              <XCircle className="ml-auto h-3.5 w-3.5 shrink-0 text-error" />
            ))}
        </button>

        {/* Expanded content */}
        {expanded && (
          <div className="mt-2 space-y-2">
            {/* Arguments */}
            <div>
              <div className="text-[10px] font-medium uppercase tracking-wider text-text-muted">
                Arguments
              </div>
              <pre className="mt-1 max-h-60 overflow-auto rounded bg-bg-code p-2 text-xs text-text-dim">
                {JSON.stringify(data.toolArgs, null, 2)}
              </pre>
            </div>

            {/* Result (if available) */}
            {hasResult && (
              <div>
                <div className="flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wider text-text-muted">
                  <span>Result</span>
                  {result!.success ? (
                    <CheckCircle2 className="h-3 w-3 text-success" />
                  ) : (
                    <XCircle className="h-3 w-3 text-error" />
                  )}
                </div>
                <pre className="mt-1 max-h-60 overflow-auto rounded bg-bg-code p-2 text-xs text-text-dim">
                  {result!.result}
                </pre>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  )
}
