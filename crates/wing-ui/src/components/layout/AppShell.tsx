import { useEffect } from 'react'
import { Sidebar } from './Sidebar'
import { MainHeader } from './MainHeader'
import { ChatArea } from './ChatArea'
import { InputArea } from './InputArea'
import { StatusBar } from './StatusBar'
import { CommandPalette } from '@/components/ui/CommandPalette'
import { ConnectionBanner } from '@/components/ui/ConnectionBanner'
import { SettingsPanel } from '@/components/ui/SettingsPanel'
import { useUiStore } from '@/stores/uiStore'
import { useSession } from '@/hooks/useSession'
import { useSessionStore } from '@/stores/sessionStore'
import { useKeyboardShortcuts } from '@/hooks/useKeyboardShortcuts'

/**
 * AppShell — top-level layout composing sidebar + main area.
 *
 * Manages:
 * - Sidebar open/close state (from uiStore)
 * - Responsive auto-collapse at < 768px
 * - Initial session list loading
 * - Global keyboard shortcuts
 * - Command palette, settings panel, connection banner
 */
export function AppShell() {
  const sidebarOpen = useUiStore((s) => s.sidebarOpen)
  const setSidebarOpen = useUiStore((s) => s.setSidebarOpen)
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const { loadSessions } = useSession()

  // Global keyboard shortcuts
  useKeyboardShortcuts()

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
        <ConnectionBanner />
        <ChatArea />
        <InputArea />
        <StatusBar />
      </div>

      {/* Overlays */}
      <CommandPalette />
      <SettingsPanel />
    </div>
  )
}
