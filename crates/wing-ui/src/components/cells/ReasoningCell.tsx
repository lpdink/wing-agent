// src/components/cells/ReasoningCell.tsx — Thinking/reasoning block.
//
// Collapsible block with accent left border. Dimmed text to distinguish from main response.
// Shows streaming indicator while reasoning events are being received.

import { useState } from 'react'
import { ChevronDown, ChevronRight, Brain } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ReasoningChatItem } from '@/stores/sessionStore'

export function ReasoningCell({ data }: CellProps<ReasoningChatItem>) {
  const [expanded, setExpanded] = useState(false)

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-lg border-l-[3px] border-reasoning-border bg-bg-elevated/50 px-4 py-3">
        <button
          onClick={() => setExpanded(!expanded)}
          className="flex w-full items-center gap-2 text-left"
        >
          {expanded ? (
            <ChevronDown className="h-3.5 w-3.5 text-text-muted" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5 text-text-muted" />
          )}
          <Brain className="h-3.5 w-3.5 text-reasoning-border" />
          <span className="text-xs font-medium text-text-muted">
            Reasoning
            {data.streaming && <span className="ml-1 animate-pulse">●</span>}
          </span>
        </button>
        {expanded && (
          <div className="mt-2 whitespace-pre-wrap text-sm italic leading-relaxed text-text-dim">
            {data.content}
          </div>
        )}
      </div>
    </div>
  )
}
