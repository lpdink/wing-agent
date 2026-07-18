// src/components/cells/DiffCell.tsx — GitHub-style diff view using LCS-based line diff.
//
// Uses the `diff` package (diffLines) for proper Myers/LCS diffing.
// Shows file path header + line-by-line diff with green/red highlighting.

import { diffLines, type Change } from 'diff'
import { FileText } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { DiffChatItem } from '@/stores/sessionStore'

function changeToLineClass(change: Change): string {
  if (change.added) return 'bg-diff-add-bg text-diff-add-text'
  if (change.removed) return 'bg-diff-del-bg text-diff-del-text'
  return 'text-text-dim'
}

function changeToPrefix(change: Change): string {
  if (change.added) return '+'
  if (change.removed) return '-'
  return ' '
}

export function DiffCell({ data }: CellProps<DiffChatItem>) {
  const oldText = data.oldText ?? ''
  const changes = diffLines(oldText, data.newText)

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
          {changes.map((change, ci) => {
            const lines = change.value.replace(/\n$/, '').split('\n')
            const lineClass = changeToLineClass(change)
            const prefix = changeToPrefix(change)

            return lines.map((line, li) => (
              <div key={`${ci}-${li}`} className={`flex font-mono text-xs leading-5 ${lineClass}`}>
                <span className="w-4 shrink-0 select-none text-text-muted/50">{prefix}</span>
                <span className="whitespace-pre px-1">{line}</span>
              </div>
            ))
          })}
        </div>
      </div>
    </div>
  )
}
