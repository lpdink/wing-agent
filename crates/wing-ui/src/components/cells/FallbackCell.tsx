// src/components/cells/FallbackCell.tsx — Fallback renderer for unknown ChatItem types.
//
// Should never appear in normal use. Displays type name + raw JSON for debugging.

import { AlertTriangle } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { ChatItem } from '@/stores/sessionStore'

export function FallbackCell({ data }: CellProps<ChatItem>) {
  return (
    <div className="flex justify-start">
      <div className="rounded-lg border border-border bg-bg-elevated px-3 py-2">
        <div className="flex items-center gap-1.5 text-xs text-text-muted">
          <AlertTriangle className="h-3 w-3" />
          <span>Unknown type: {data.type}</span>
        </div>
        <pre className="mt-1 overflow-x-auto text-[10px] text-text-dim">
          {JSON.stringify(data, null, 2)}
        </pre>
      </div>
    </div>
  )
}
