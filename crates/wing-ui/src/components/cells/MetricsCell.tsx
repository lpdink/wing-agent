// src/components/cells/MetricsCell.tsx — LLM call metrics indicator.
//
// Compact row showing: model / tokens / cached / speed.

import { Gauge } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { MetricsChatItem } from '@/stores/sessionStore'

function formatTokens(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`
  return String(n)
}

function formatSpeed(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k tok/s`
  return `${n.toFixed(0)} tok/s`
}

export function MetricsCell({ data }: CellProps<MetricsChatItem>) {
  return (
    <div className="flex justify-center">
      <div className="flex items-center gap-3 rounded-full bg-bg-elevated px-3 py-1 text-[11px] text-text-muted">
        <Gauge className="h-3 w-3" />
        <span className="font-medium">{data.model}</span>
        <span className="text-border">|</span>
        <span>
          {formatTokens(data.promptTokens)} → {formatTokens(data.completionTokens)}
        </span>
        {data.cachedTokens > 0 && (
          <>
            <span className="text-border">|</span>
            <span className="text-success">{formatTokens(data.cachedTokens)} cached</span>
          </>
        )}
        {data.tokensPerSec > 0 && (
          <>
            <span className="text-border">|</span>
            <span>{formatSpeed(data.tokensPerSec)}</span>
          </>
        )}
      </div>
    </div>
  )
}
