import { useState, useEffect } from 'react'
import { Sidebar } from './Sidebar'
import { MainHeader } from './MainHeader'
import { ChatArea } from './ChatArea'
import { InputArea } from './InputArea'
import { StatusBar } from './StatusBar'
import { mockSessions } from '@/lib/mock-data'

/**
 * AppShell — top-level layout composing sidebar + main area.
 *
 * Manages:
 * - Sidebar open/close state
 * - Responsive auto-collapse at < 768px
 * - Theme initialization from localStorage
 */
export function AppShell() {
  const [sidebarOpen, setSidebarOpen] = useState(true)

  // Initialize theme from localStorage (defaults to light)
  useEffect(() => {
    const stored = localStorage.getItem('wing-theme')
    if (stored === 'dark') {
      document.documentElement.dataset.theme = 'dark'
    }
  }, [])

  // Auto-collapse sidebar on narrow viewports
  useEffect(() => {
    const mq = window.matchMedia('(max-width: 767px)')
    const handler = (e: MediaQueryListEvent | MediaQueryList) => {
      if (e.matches) setSidebarOpen(false)
    }
    handler(mq)
    mq.addEventListener('change', handler)
    return () => mq.removeEventListener('change', handler)
  }, [])

  const activeSession = mockSessions[0]

  return (
    <div className="flex h-screen overflow-hidden bg-bg">
      <Sidebar open={sidebarOpen} />

      <div className="flex min-w-0 flex-1 flex-col">
        <MainHeader
          sessionTitle={activeSession?.name ?? 'New Session'}
          sidebarOpen={sidebarOpen}
          onToggleSidebar={() => setSidebarOpen((v) => !v)}
        />
        <ChatArea />
        <InputArea />
        <StatusBar />
      </div>
    </div>
  )
}
