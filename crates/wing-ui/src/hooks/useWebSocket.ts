// src/hooks/useWebSocket.ts — WebSocket lifecycle with reactive URL.
//
// Creates WebSocketClient, connects on mount, syncs status/clientId
// to connectionStore. Recreates the client when gatewayUrl changes.

import { useEffect, useRef } from 'react'
import { WebSocketClient } from '@wing-agent/sdk'
import type { ConnectionStatus } from '@wing-agent/sdk'
import { useConnectionStore } from '@/stores/connectionStore'
import { toWsUrl } from '@/lib/gateway-config'

export function useWebSocket(): { wsClient: WebSocketClient } {
  const wsRef = useRef<WebSocketClient | null>(null)
  const urlRef = useRef<string | null>(null)
  const gatewayUrl = useConnectionStore((s) => s.gatewayUrl)
  const setStatus = useConnectionStore((s) => s.setStatus)
  const setClientId = useConnectionStore((s) => s.setClientId)

  const wsUrl = toWsUrl(gatewayUrl)

  // Recreate client when URL changes
  if (!wsRef.current || urlRef.current !== wsUrl) {
    // Disconnect previous client if exists
    if (wsRef.current) {
      wsRef.current.disconnect()
    }
    wsRef.current = new WebSocketClient({ url: wsUrl })
    urlRef.current = wsUrl
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
  }, [gatewayUrl, setStatus, setClientId])

  return { wsClient: wsRef.current }
}
