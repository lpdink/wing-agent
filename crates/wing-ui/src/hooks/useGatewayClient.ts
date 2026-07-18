// src/hooks/useGatewayClient.ts — GatewayClient singleton.
//
// Creates and caches a GatewayClient instance.
// Automatically syncs clientId from connectionStore.

import { useRef } from 'react'
import { GatewayClient } from '@wing-agent/sdk'
import { useConnectionStore } from '@/stores/connectionStore'
import { getGatewayHttpBase } from '@/lib/gateway-config'

export function useGatewayClient(): { client: GatewayClient } {
  const clientRef = useRef<GatewayClient | null>(null)
  const clientId = useConnectionStore((s) => s.clientId)

  if (!clientRef.current) {
    clientRef.current = new GatewayClient({ baseUrl: getGatewayHttpBase() })
  }

  // Sync clientId from store (set after WS connects)
  if (clientRef.current.clientId !== clientId) {
    clientRef.current.clientId = clientId
  }

  return { client: clientRef.current }
}
