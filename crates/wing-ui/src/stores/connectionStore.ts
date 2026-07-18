// src/stores/connectionStore.ts — WebSocket connection state.

import { create } from 'zustand'
import type { ConnectionStatus } from '@wing-agent/sdk'
import { getGatewayHttpBase } from '@/lib/gateway-config'

interface ConnectionState {
  status: ConnectionStatus
  clientId: string | null
  gatewayUrl: string
}

interface ConnectionActions {
  setStatus: (status: ConnectionStatus) => void
  setClientId: (clientId: string | null) => void
  setGatewayUrl: (url: string) => void
}

export type ConnectionStore = ConnectionState & ConnectionActions

export const useConnectionStore = create<ConnectionStore>((set) => ({
  // State
  status: 'disconnected',
  clientId: null,
  gatewayUrl: getGatewayHttpBase(),

  // Actions
  setStatus: (status) => set({ status }),
  setClientId: (clientId) => set({ clientId }),
  setGatewayUrl: (url) => set({ gatewayUrl: url }),
}))
