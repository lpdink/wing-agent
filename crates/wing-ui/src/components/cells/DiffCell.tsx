// src/components/cells/DiffCell.tsx — GitHub-style diff view.
//
// Shows file path header + line-by-line diff with green/red highlighting.

import { FileText } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { DiffChatItem } from '@/stores/sessionStore'

/** Parse unified diff format into structured lines. */
function parseDiff(oldText: string | null, newText: string): DiffLine[] {
  const lines: DiffLine[] = []
  const newLines = newText.split('\n')
  const oldLines = oldText ? oldText.split('\n') : []

  // Simple approach: if no old text, everything is added
  if (!oldText) {
    for (const line of newLines) {
      lines.push({ type: 'add', content: line })
    }
    return lines
  }

  // Use a simple LCS-based diff for small files, fallback to showing new text
  if (oldLines.length + newLines.length > 2000) {
    // For large diffs, just show the new text as context
    for (const line of newLines) {
      lines.push({ type: 'ctx', content: line })
    }
    return lines
  }

  // Simple line-by-line diff
  const oldSet = new Set(oldLines)
  const newSet = new Set(newLines)

  // Show deletions first
  for (const line of oldLines) {
    if (!newSet.has(line)) {
      lines.push({ type: 'del', content: line })
    }
  }

  // Show new/unchanged lines
  for (const line of newLines) {
    if (!oldSet.has(line)) {
      lines.push({ type: 'add', content: line })
    } else {
      lines.push({ type: 'ctx', content: line })
    }
  }

  return lines
}

interface DiffLine {
  type: 'add' | 'del' | 'ctx'
  content: string
}

function diffLineClass(type: DiffLine['type']): string {
  switch (type) {
    case 'add':
      return 'bg-diff-add-bg text-diff-add-text'
    case 'del':
      return 'bg-diff-del-bg text-diff-del-text'
    case 'ctx':
      return 'text-text-dim'
  }
}

function diffLinePrefix(type: DiffLine['type']): string {
  switch (type) {
    case 'add':
      return '+'
    case 'del':
      return '-'
    case 'ctx':
      return ' '
  }
}

export function DiffCell({ data }: CellProps<DiffChatItem>) {
  const lines = parseDiff(data.oldText, data.newText)

  return (
    <div className="flex justify-start">
      <div className="max-w-[90%] overflow-hidden rounded-lg border border-border">
        {/* File path header */}
        <div className="flex items-center gap-2 border-b border-border bg-bg-elevated px-3 py-1.5">
          <FileText className="h-3.5 w-3.5 text-text-muted" />
          <span className="font-mono text-xs text-text-dim">{data.path}</span>
        </div>

        {/* Diff body */}
        <div className="overflow-x-auto bg-bg-code">
          {lines.map((line, i) => (
            <div key={i} className={`flex font-mono text-xs leading-5 ${diffLineClass(line.type)}`}>
              <span className="w-6 shrink-0 select-none px-1 text-right text-text-muted">
                {i + 1}
              </span>
              <span className="w-4 shrink-0 select-none text-text-muted">
                {diffLinePrefix(line.type)}
              </span>
              <span className="whitespace-pre px-1">{line.content}</span>
            </div>
          ))}
        </div>
      </div>
    </div>
  )
}
