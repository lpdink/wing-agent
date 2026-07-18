import { Zap, Wifi, WifiOff, RefreshCw } from 'lucide-react'
import { useConnectionStore } from '@/stores/connectionStore'
import { useSessionStore } from '@/stores/sessionStore'

/**
 * StatusBar — bottom bar showing connection status, model info,
 * token usage, and turn count.
 */
export function StatusBar() {
  const status = useConnectionStore((s) => s.status)
  const sessionInfo = useSessionStore((s) => s.sessionInfo)

  const statusConfig = {
    connected: { icon: Wifi, color: 'bg-success', text: 'Connected' },
    connecting: { icon: RefreshCw, color: 'bg-warning', text: 'Connecting…' },
    reconnecting: { icon: RefreshCw, color: 'bg-warning', text: 'Reconnecting…' },
    disconnected: { icon: WifiOff, color: 'bg-error', text: 'Disconnected' },
  }

  const { icon: StatusIcon, color, text } = statusConfig[status]

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

      {/* Center — model + token progress */}
      <div className="flex items-center gap-3 text-text-muted">
        {sessionInfo && (
          <>
            <span>{sessionInfo.model}</span>
            <span className="flex items-center gap-1.5">
              <Zap className="h-3 w-3" />
              <span>
                {sessionInfo.total_tokens.toLocaleString()} /{' '}
                {(sessionInfo.context_window_tokens / 1000).toFixed(0)}k
              </span>
            </span>
          </>
        )}
      </div>

      {/* Right — session name */}
      <div className="max-w-48 truncate text-text-muted">{sessionInfo?.session_name ?? ''}</div>
    </div>
  )
}
