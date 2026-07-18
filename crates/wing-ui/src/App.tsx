import { AppShell } from '@/components/layout/AppShell'
import { useWebSocket } from '@/hooks/useWebSocket'
import { useSessionEvents } from '@/hooks/useSessionEvents'

/**
 * GatewayProvider — initializes WebSocket connection and event handling.
 * Must wrap the entire app to provide real-time event streaming.
 */
function GatewayProvider({ children }: { children: React.ReactNode }) {
  const { wsClient } = useWebSocket()
  useSessionEvents(wsClient)
  return <>{children}</>
}

export function App() {
  return (
    <GatewayProvider>
      <AppShell />
    </GatewayProvider>
  )
}
