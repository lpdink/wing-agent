// src/hooks/useGatewayClient.ts — GatewayClient with reactive URL.
//
// Recreates the GatewayClient when connectionStore.gatewayUrl changes.
// Automatically syncs clientId from connectionStore.
// Client construction is lazy (render-safe); no side effects in render phase.

import { useRef } from 'react'
import { GatewayClient } from '@wing-agent/sdk'
import { useConnectionStore } from '@/stores/connectionStore'

export function useGatewayClient(): { client: GatewayClient } {
  const clientRef = useRef<GatewayClient | null>(null)
  const urlRef = useRef<string | null>(null)
  const gatewayUrl = useConnectionStore((s) => s.gatewayUrl)
  const clientId = useConnectionStore((s) => s.clientId)

  // Recreate client when URL changes (GatewayClient construction is side-effect-free)
  if (!clientRef.current || urlRef.current !== gatewayUrl) {
    clientRef.current = new GatewayClient({ baseUrl: gatewayUrl })
    urlRef.current = gatewayUrl
  }

  // Sync clientId from store (set after WS connects)
  if (clientRef.current.clientId !== clientId) {
    clientRef.current.clientId = clientId
  }

  return { client: clientRef.current }
}
