// src/components/cells/ReasoningCell.tsx — Thinking/reasoning block.
//
// Three states:
// - streaming: real-time content display (auto-expanded)
// - preview: first 3 lines + fade mask + expand button (default after streaming)
// - expanded: full content (user toggled)
//
// Accent left border, dimmed italic text to distinguish from main response.

import { useState } from 'react'
import { Brain, ChevronDown, ChevronUp } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ReasoningChatItem } from '@/stores/sessionStore'

const PREVIEW_LINES = 3

export function ReasoningCell({ data }: CellProps<ReasoningChatItem>) {
  const [userExpanded, setUserExpanded] = useState(false)

  const isStreaming = data.streaming === true
  const lines = data.content.split('\n')
  const isShort = lines.length <= PREVIEW_LINES

  // Streaming: always show full content
  if (isStreaming) {
    return (
      <div className="flex justify-start">
        <div className="max-w-[85%] rounded-lg border-l-[3px] border-reasoning-border bg-bg-elevated/50 px-4 py-3">
          <div className="flex items-center gap-2">
            <Brain className="h-3.5 w-3.5 text-reasoning-border" />
            <span className="text-xs font-medium text-text-muted">
              Reasoning <span className="animate-pulse">●</span>
            </span>
          </div>
          <div className="mt-2 whitespace-pre-wrap text-sm italic leading-relaxed text-text-dim">
            {data.content}
          </div>
        </div>
      </div>
    )
  }

  // Preview or expanded (post-streaming)
  const showFull = userExpanded || isShort

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-lg border-l-[3px] border-reasoning-border bg-bg-elevated/50 px-4 py-3">
        <div className="flex items-center gap-2">
          <Brain className="h-3.5 w-3.5 text-reasoning-border" />
          <span className="text-xs font-medium text-text-muted">Reasoning</span>
        </div>

        <div className="relative mt-2">
          <div
            className={`whitespace-pre-wrap text-sm italic leading-relaxed text-text-dim ${
              showFull ? '' : 'overflow-hidden'
            }`}
            style={!showFull ? { maxHeight: `${PREVIEW_LINES * 1.625}em` } : undefined}
          >
            {data.content}
          </div>

          {/* Fade mask for preview state */}
          {!showFull && (
            <div className="pointer-events-none absolute inset-x-0 bottom-0 h-6 bg-gradient-to-t from-bg-elevated/80 to-transparent" />
          )}
        </div>

        {/* Expand/collapse button (only for long content) */}
        {!isShort && (
          <button
            onClick={() => setUserExpanded(!userExpanded)}
            className="mt-1.5 flex items-center gap-1 text-xs text-text-muted transition-colors hover:text-text"
          >
            {userExpanded ? (
              <>
                <ChevronUp className="h-3 w-3" />
                <span>收起</span>
              </>
            ) : (
              <>
                <ChevronDown className="h-3 w-3" />
                <span>展开全文</span>
              </>
            )}
          </button>
        )}
      </div>
    </div>
  )
}
