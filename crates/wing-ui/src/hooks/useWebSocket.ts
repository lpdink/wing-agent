// src/hooks/useWebSocket.ts — WebSocket lifecycle management.
//
// Creates WebSocketClient, connects on mount, syncs status/clientId
// to connectionStore, and exposes the client for event subscription.

import { useEffect, useRef } from 'react'
import { WebSocketClient } from '@wing-agent/sdk'
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

    // Sync connection status to store
    const handleStatusChange = (
      status: 'disconnected' | 'connecting' | 'connected' | 'reconnecting',
    ) => {
      setStatus(status)
    }

    ws.onStatusChange(handleStatusChange)
    ws.connect()

    return () => {
      ws.offStatusChange(handleStatusChange)
      ws.disconnect()
    }
  }, [setStatus])

  // Poll for clientId changes (ConnectResponse arrives async after 'connected')
  useEffect(() => {
    const ws = wsRef.current!
    const checkClientId = () => {
      if (ws.clientId && ws.clientId !== useConnectionStore.getState().clientId) {
        setClientId(ws.clientId)
      }
    }
    // Check periodically for clientId (WS doesn't expose a direct callback for this)
    const interval = setInterval(checkClientId, 100)
    return () => clearInterval(interval)
  }, [setClientId])

  return { wsClient: wsRef.current }
}
