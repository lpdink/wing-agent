// src/components/ui/ConnectionBanner.tsx — Disconnection/reconnection banner.
//
// Shows a prominent banner when the Gateway connection is lost.
// Driven by connectionStore.status — hidden when connected.

import { WifiOff, RefreshCw } from 'lucide-react'
import { useConnectionStore } from '@/stores/connectionStore'

/**
 * ConnectionBanner — displayed above ChatArea when disconnected/reconnecting.
 * Automatically hides when connection is restored.
 */
export function ConnectionBanner() {
  const status = useConnectionStore((s) => s.status)

  if (status === 'connected' || status === 'connecting') return null

  const isReconnecting = status === 'reconnecting'

  return (
    <div className="flex shrink-0 items-center justify-center gap-2 border-b border-warning/20 bg-warning/10 px-4 py-2">
      {isReconnecting ? (
        <RefreshCw className="h-3.5 w-3.5 animate-spin text-warning" />
      ) : (
        <WifiOff className="h-3.5 w-3.5 text-error" />
      )}
      <span className="text-xs font-medium text-text-dim">
        {isReconnecting ? 'Reconnecting to Gateway…' : 'Disconnected from Gateway'}
      </span>
    </div>
  )
}
