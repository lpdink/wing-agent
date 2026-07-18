// src/hooks/useWebSocket.ts — WebSocket lifecycle management.
//
// Creates WebSocketClient, connects on mount, syncs status/clientId
// to connectionStore, and exposes the client for event subscription.

import { useEffect, useRef } from 'react'
import { WebSocketClient } from '@wing-agent/sdk'
import type { ConnectionStatus } from '@wing-agent/sdk'
import { useConnectionStore } from '@/stores/connectionStore'
import { getGatewayWsUrl } from '@/lib/gateway-config'

export function useWebSocket(): { wsClient: WebSocketClient } {
  const wsRef = useRef<WebSocketClient | null>(null)
  const setStatus = useConnectionStore((s) => s.setStatus)
  const setClientId = useConnectionStore((s) => s.setClientId)

  if (!wsRef.current) {
    wsRef.current = new WebSocketClient({ url: getGatewayWsUrl() })
  }

  useEffect(() => {
    const ws = wsRef.current!

    // Sync connection status to store + extract clientId on connect
    const handleStatusChange = (status: ConnectionStatus) => {
      setStatus(status)
      if (status === 'connected' && ws.clientId) {
        setClientId(ws.clientId)
      } else if (status === 'disconnected') {
        setClientId(null)
      }
    }

    ws.onStatusChange(handleStatusChange)
    ws.connect()

    return () => {
      ws.offStatusChange(handleStatusChange)
      ws.disconnect()
    }
  }, [setStatus, setClientId])

  return { wsClient: wsRef.current }
}
