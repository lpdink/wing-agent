import { useEffect } from 'react'
import { Sidebar } from './Sidebar'
import { MainHeader } from './MainHeader'
import { ChatArea } from './ChatArea'
import { InputArea } from './InputArea'
import { StatusBar } from './StatusBar'
import { useUiStore } from '@/stores/uiStore'
import { useSession } from '@/hooks/useSession'
import { useSessionStore } from '@/stores/sessionStore'

/**
 * AppShell — top-level layout composing sidebar + main area.
 *
 * Manages:
 * - Sidebar open/close state (from uiStore)
 * - Responsive auto-collapse at < 768px
 * - Initial session list loading
 */
export function AppShell() {
  const sidebarOpen = useUiStore((s) => s.sidebarOpen)
  const setSidebarOpen = useUiStore((s) => s.setSidebarOpen)
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const { loadSessions } = useSession()

  // Auto-collapse sidebar on narrow viewports
  useEffect(() => {
    const mq = window.matchMedia('(max-width: 767px)')
    const handler = (e: MediaQueryListEvent | MediaQueryList) => {
      if (e.matches) setSidebarOpen(false)
    }
    handler(mq)
    mq.addEventListener('change', handler)
    return () => mq.removeEventListener('change', handler)
  }, [setSidebarOpen])

  // Load sessions on mount
  useEffect(() => {
    loadSessions()
  }, [loadSessions])

  return (
    <div className="flex h-screen overflow-hidden bg-bg">
      <Sidebar open={sidebarOpen} />

      <div className="flex min-w-0 flex-1 flex-col">
        {activeSessionId && (
          <MainHeader
            sidebarOpen={sidebarOpen}
            onToggleSidebar={() => useUiStore.getState().toggleSidebar()}
          />
        )}
        <ChatArea />
        <InputArea />
        <StatusBar />
      </div>
    </div>
  )
}
