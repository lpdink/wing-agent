// src/components/cells/ProcessLine.tsx — Shared visual primitive for process metadata.
//
// Reasoning and tool calls share this "process line" rendering:
// - Collapsed: single muted 13px trigger line (icon + label + subject + duration + status)
// - Expanded: indented content area with 1px border-left guide
//
// Design principle: "Process is Metadata" — no border, no background, no shadow.

import { type ReactNode, type ComponentType } from 'react'
import { ChevronRight } from 'lucide-react'

export interface ProcessLineProps {
  /** Icon component (14px rendered) */
  icon: ComponentType<{ className?: string }>
  /** Primary label text (e.g., "Thinking", tool name) */
  label: string
  /** Secondary subject text (e.g., args summary), truncated */
  subject?: string
  /** Duration text (e.g., "3.2s") */
  duration?: string
  /** Status: success/failure indicator */
  status?: 'success' | 'error' | null
  /** Whether the content is expanded */
  expanded: boolean
  /** Toggle expand/collapse */
  onToggle: () => void
  /** Expanded content (rendered in indented container) */
  children?: ReactNode
}

export function ProcessLine({
  icon: Icon,
  label,
  subject,
  duration,
  status,
  expanded,
  onToggle,
  children,
}: ProcessLineProps) {
  return (
    <div>
      {/* Trigger line — 24px height, 13px muted text */}
      <button
        onClick={onToggle}
        className="group/pl flex h-6 w-full items-center gap-1.5 rounded-sm px-1 text-left transition-colors hover:bg-bg-elevated/50"
      >
        {/* Chevron — visible on hover or when expanded */}
        <ChevronRight
          className={`h-3 w-3 shrink-0 text-text-dim transition-transform ${
            expanded ? 'rotate-90' : 'opacity-30 group-hover/pl:opacity-100'
          }`}
        />
        <Icon className="h-3.5 w-3.5 shrink-0 text-text-dim" />
        <span className="shrink-0 text-[13px] leading-6 text-text-muted">{label}</span>
        {subject && (
          <span className="min-w-0 truncate text-[13px] leading-6 text-text-dim">{subject}</span>
        )}
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          {duration && <span className="text-[11px] leading-6 text-text-dim">{duration}</span>}
          {status === 'success' && <span className="text-[11px] text-success">✓</span>}
          {status === 'error' && <span className="text-[11px] text-error">✗</span>}
        </span>
      </button>

      {/* Expanded content — indented with border-left guide */}
      {expanded && children && (
        <div className="ml-[22px] border-l border-border py-1 pl-3">{children}</div>
      )}
    </div>
  )
}
