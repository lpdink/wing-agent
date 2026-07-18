import { Zap } from 'lucide-react'

/**
 * StatusBar — bottom bar showing connection status, model info,
 * token usage, and turn count.
 *
 * All values are mocked in Phase 3; real data arrives in Phase 4.
 */
export function StatusBar() {
  return (
    <div className="flex h-7 shrink-0 items-center justify-between border-t border-border bg-bg-surface px-3 text-xs">
      {/* Left — connection status */}
      <div className="flex items-center gap-1.5 text-text-dim">
        <span className="h-1.5 w-1.5 rounded-full bg-success" />
        <span>Connected</span>
      </div>

      {/* Center — model + token progress */}
      <div className="flex items-center gap-3 text-text-muted">
        <span>gpt-4o</span>
        <span className="flex items-center gap-1.5">
          <Zap className="h-3 w-3" />
          <span>12,847 / 128k</span>
        </span>
      </div>

      {/* Right — turn count */}
      <div className="text-text-muted">Turn 3</div>
    </div>
  )
}
