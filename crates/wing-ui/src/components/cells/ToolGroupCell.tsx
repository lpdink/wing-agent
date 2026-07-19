// src/components/cells/ToolGroupCell.tsx — Compressed readonly tool call group.
//
// Renders consecutive read-only tool calls (Read, Grep, Glob, etc.)
// as a single-line summary with expandable details.

import { useState } from 'react'
import { ChevronRight, FileSearch } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ToolGroupChatItem } from '@/stores/sessionStore'

export function ToolGroupCell({ data }: CellProps<ToolGroupChatItem>) {
  const [expanded, setExpanded] = useState(false)

  return (
    <div className="my-1">
      {/* Summary line */}
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs text-text-muted transition-colors hover:bg-bg-elevated"
      >
        <ChevronRight
          className={`h-3 w-3 shrink-0 transition-transform ${expanded ? 'rotate-90' : ''}`}
        />
        <FileSearch className="h-3.5 w-3.5 shrink-0 text-text-dim" />
        <span className="truncate">{data.summary}</span>
        <span className="ml-auto shrink-0 text-text-dim">{data.tools.length}</span>
      </button>

      {/* Expanded details */}
      {expanded && (
        <div className="ml-7 mt-1 space-y-1 border-l border-border pl-3">
          {data.tools.map((tool, i) => (
            <div key={i} className="text-xs text-text-muted">
              <span className="font-medium text-text-dim">{tool.name}</span>
              {tool.args && 'path' in tool.args && (
                <span className="ml-1.5 text-text-dim">{String(tool.args.path)}</span>
              )}
              {tool.args && 'pattern' in tool.args && (
                <span className="ml-1.5 text-text-dim">{String(tool.args.pattern)}</span>
              )}
              {tool.result !== undefined && (
                <span className={`ml-1.5 ${tool.success ? 'text-success' : 'text-error'}`}>
                  {tool.success ? '✓' : '✗'}
                </span>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
