// src/components/cells/ToolGroupCell.tsx — Compressed readonly tool call group.
//
// Renders consecutive read-only tool calls (Read, Grep, Glob, etc.)
// as a process line summary with expandable details.
// Visual specs aligned with ProcessLine: 13px, muted, 24px height.

import { useState } from 'react'
import { FileSearch } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ToolGroupChatItem } from '@/stores/sessionStore'
import { ProcessLine } from './ProcessLine'

export function ToolGroupCell({ data }: CellProps<ToolGroupChatItem>) {
  const [expanded, setExpanded] = useState(false)

  return (
    <ProcessLine
      icon={FileSearch}
      label={data.summary}
      subject={String(data.tools.length)}
      expanded={expanded}
      onToggle={() => setExpanded(!expanded)}
    >
      <div className="space-y-0.5">
        {data.tools.map((tool, i) => (
          <div key={i} className="flex items-center gap-1.5 text-[13px] leading-5">
            <span className="shrink-0 font-medium text-text-dim">{tool.name}</span>
            {tool.args && 'path' in tool.args && (
              <span className="min-w-0 truncate text-text-dim">{String(tool.args.path)}</span>
            )}
            {tool.args && 'pattern' in tool.args && (
              <span className="min-w-0 truncate text-text-dim">{String(tool.args.pattern)}</span>
            )}
            {tool.result !== undefined && (
              <span
                className={`ml-auto shrink-0 text-[11px] ${tool.success ? 'text-success' : 'text-error'}`}
              >
                {tool.success ? '✓' : '✗'}
              </span>
            )}
          </div>
        ))}
      </div>
    </ProcessLine>
  )
}
