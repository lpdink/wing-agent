// src/components/cells/ToolCallCell.tsx — Tool call as a process line.
//
// Non-mutation tools: ProcessLine trigger (icon + name + summary + status).
// Mutation tools (Write/Edit): compact one-liner (DiffCell is the real content).
// TodoWrite: special checklist rendering (see TodoChecklist below).

import { useState } from 'react'
import { Wrench, FilePen, CheckCircle2, XCircle } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ToolCallChatItem, ToolResultChatItem } from '@/stores/sessionStore'
import { getToolCategory } from '@/stores/sessionStore'
import { ProcessLine } from './ProcessLine'
import { TodoChecklist } from './TodoChecklist'

interface ToolCallCellProps extends CellProps<ToolCallChatItem> {
  /** Optional tool result (if next message is tool_call_result with same toolCallId) */
  result?: ToolResultChatItem | null
}

/** Extract a short summary from tool args for the collapsed header. */
function getArgsSummary(args: Record<string, unknown>): string {
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
  const isMutation = getToolCategory(data.toolName) === 'mutation'
  const isTodoWrite = data.toolName === 'TodoWrite' || data.toolName === 'todo_write'

  // Mutation tools: compact one-liner (DiffCell is the real content)
  if (isMutation) {
    return (
      <div className="flex h-6 items-center gap-1.5 px-1 text-[13px] text-text-muted">
        <FilePen className="h-3.5 w-3.5 shrink-0 text-accent" />
        <span className="shrink-0 font-medium">{data.toolName}</span>
        {summary && <span className="min-w-0 truncate text-text-dim">{summary}</span>}
        {hasResult &&
          (result.success ? (
            <CheckCircle2 className="ml-auto h-3 w-3 shrink-0 text-success" />
          ) : (
            <XCircle className="ml-auto h-3 w-3 shrink-0 text-error" />
          ))}
      </div>
    )
  }

  // TodoWrite: special checklist rendering
  if (isTodoWrite) {
    return <TodoChecklist data={data} />
  }

  // Non-mutation tools: process line
  const status = hasResult ? (result.success ? 'success' : 'error') : null

  return (
    <ProcessLine
      icon={Wrench}
      label={data.toolName}
      subject={summary || undefined}
      status={status}
      expanded={expanded}
      onToggle={() => setExpanded(!expanded)}
    >
      {/* Arguments */}
      <div>
        <div className="text-[10px] font-medium uppercase tracking-wider text-text-dim">
          Arguments
        </div>
        <pre className="mt-1 max-h-60 overflow-auto rounded bg-bg-code p-2 text-xs text-text-dim">
          {JSON.stringify(data.toolArgs, null, 2)}
        </pre>
      </div>

      {/* Result (if available) */}
      {hasResult && (
        <div className="mt-2">
          <div className="flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wider text-text-dim">
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
    </ProcessLine>
  )
}
