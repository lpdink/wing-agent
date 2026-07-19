// src/components/cells/ReasoningCell.tsx — Thinking/reasoning as a process line.
//
// Three states:
// - streaming: auto-expanded, shimmer animation on text
// - collapsed: process line trigger ("Thinking" + duration)
// - expanded: indented dimmed text (user toggled)
//
// Auto-collapses 500ms after streaming ends (unless user manually expanded).

import { useState, useEffect, useRef } from 'react'
import { Brain } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ReasoningChatItem } from '@/stores/sessionStore'
import { ProcessLine } from './ProcessLine'

const AUTO_COLLAPSE_DELAY = 500

export function ReasoningCell({ data }: CellProps<ReasoningChatItem>) {
  const isStreaming = data.streaming === true

  // Track whether user has manually interacted with expand/collapse
  const [userExpanded, setUserExpanded] = useState(false)
  const [autoCollapsed, setAutoCollapsed] = useState(false)
  const userInteractedRef = useRef(false)
  const wasStreamingRef = useRef(isStreaming)

  // Auto-collapse after streaming ends (500ms delay)
  useEffect(() => {
    const wasStreaming = wasStreamingRef.current
    wasStreamingRef.current = isStreaming

    if (wasStreaming && !isStreaming && !userInteractedRef.current) {
      const timer = setTimeout(() => setAutoCollapsed(true), AUTO_COLLAPSE_DELAY)
      return () => clearTimeout(timer)
    }
  }, [isStreaming])

  const handleToggle = () => {
    userInteractedRef.current = true
    // During streaming the content is force-expanded; toggling just marks
    // "user wants it open" so auto-collapse is suppressed after streaming ends.
    if (isStreaming) {
      setUserExpanded(true)
      return
    }
    setUserExpanded((prev) => !prev)
    setAutoCollapsed(false)
  }

  // Determine expanded state
  const expanded = isStreaming || (userExpanded && !autoCollapsed)

  return (
    <ProcessLine icon={Brain} label="Thinking" expanded={expanded} onToggle={handleToggle}>
      <div
        className={`whitespace-pre-wrap text-[13px] leading-relaxed text-text-dim ${
          isStreaming ? 'animate-shimmer' : ''
        }`}
      >
        {data.content}
      </div>
    </ProcessLine>
  )
}
