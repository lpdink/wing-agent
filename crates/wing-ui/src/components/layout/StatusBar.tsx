import { Zap, Wifi, WifiOff, RefreshCw } from 'lucide-react'
import { useConnectionStore } from '@/stores/connectionStore'
import { useSessionStore } from '@/stores/sessionStore'

/**
 * StatusBar — bottom bar showing connection status, model info,
 * token usage (real-time from llm_call_metrics), and session config.
 */
export function StatusBar() {
  const status = useConnectionStore((s) => s.status)
  const metrics = useSessionStore((s) => s.metrics)
  const sessionConfig = useSessionStore((s) => s.sessionConfig)

  const statusConfig = {
    connected: { icon: Wifi, color: 'bg-success', text: 'Connected' },
    connecting: { icon: RefreshCw, color: 'bg-warning', text: 'Connecting…' },
    reconnecting: { icon: RefreshCw, color: 'bg-warning', text: 'Reconnecting…' },
    disconnected: { icon: WifiOff, color: 'bg-error', text: 'Disconnected' },
  }

  const { icon: StatusIcon, color, text } = statusConfig[status]

  // Model: prefer metrics (real-time), fallback to sessionConfig
  const model = metrics?.model ?? sessionConfig.model

  return (
    <div className="flex h-7 shrink-0 items-center justify-between border-t border-border bg-bg-surface px-3 text-xs">
      {/* Left — connection status */}
      <div className="flex items-center gap-1.5 text-text-dim">
        <StatusIcon
          className={`h-3.5 w-3.5 ${status === 'connected' ? 'text-success' : status === 'disconnected' ? 'text-error' : 'text-warning'}`}
        />
        <span className={`h-1.5 w-1.5 rounded-full ${color}`} />
        <span>{text}</span>
      </div>

      {/* Center — model + token metrics */}
      <div className="flex items-center gap-3 text-text-muted">
        {model && <span>{model}</span>}
        {metrics && (
          <span className="flex items-center gap-1.5">
            <Zap className="h-3 w-3" />
            <span>
              {(metrics.promptTokens / 1000).toFixed(1)}k →{' '}
              {(metrics.completionTokens / 1000).toFixed(1)}k
              {metrics.cachedTokens > 0 && ` | ${metrics.cachedTokens.toLocaleString()} cached`}
              {` | ${metrics.tokensPerSec.toFixed(0)} tok/s`}
            </span>
          </span>
        )}
        {sessionConfig.thinking && (
          <span className="rounded bg-bg-elevated px-1 py-0.5 text-[10px]">thinking</span>
        )}
        {sessionConfig.yolo && (
          <span className="rounded bg-bg-elevated px-1 py-0.5 text-[10px]">yolo</span>
        )}
      </div>

      {/* Right — reasoning effort */}
      <div className="max-w-48 truncate text-text-muted">{sessionConfig.reasoningEffort ?? ''}</div>
    </div>
  )
}
