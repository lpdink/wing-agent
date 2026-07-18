import { useState, useEffect } from 'react'
import clsx from 'clsx'
import { Plus, Search, MessageSquare, Sun, Moon, Settings } from 'lucide-react'
import { useSessionStore } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'

interface SidebarProps {
  open: boolean
}

/**
 * Sidebar — new session button, search, session list,
 * and bottom toolbar with theme toggle.
 */
export function Sidebar({ open }: SidebarProps) {
  const [search, setSearch] = useState('')
  const [isDark, setIsDark] = useState(false)

  const sessions = useSessionStore((s) => s.sessions)
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const { selectSession, createSession } = useSession()

  // Sync isDark state with DOM
  useEffect(() => {
    setIsDark(document.documentElement.dataset.theme === 'dark')
  }, [])

  const toggleTheme = () => {
    const next = !isDark
    setIsDark(next)
    document.documentElement.dataset.theme = next ? 'dark' : 'light'
    localStorage.setItem('wing-theme', next ? 'dark' : 'light')
  }

  const filteredSessions = sessions.filter((s) =>
    (s.name ?? '').toLowerCase().includes(search.toLowerCase()),
  )

  const formatTime = (iso: string | null): string => {
    if (!iso) return ''
    const d = new Date(iso)
    const now = new Date()
    const diffDays = Math.floor((now.getTime() - d.getTime()) / 86_400_000)
    if (diffDays === 0) {
      return d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })
    }
    if (diffDays === 1) return 'Yesterday'
    if (diffDays < 7) return `${diffDays}d ago`
    return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
  }

  return (
    <div
      className={clsx(
        'flex h-full shrink-0 flex-col border-r border-border bg-bg-surface transition-[width] duration-300 ease-in-out overflow-hidden',
        open ? 'w-[280px]' : 'w-0 border-r-0',
      )}
    >
      {/* ─── New session button ──────────────────────────── */}
      <div className="shrink-0 p-3">
        <button
          onClick={() => createSession()}
          className="flex w-full items-center gap-2 rounded-lg border border-border px-3 py-2 text-sm text-text-dim transition-colors hover:border-border-hover hover:bg-bg-elevated hover:text-text"
        >
          <Plus className="h-4 w-4" />
          <span>New Session</span>
        </button>
      </div>

      {/* ─── Search ─────────────────────────────────────── */}
      <div className="shrink-0 px-3 pb-2">
        <div className="flex items-center gap-2 rounded-lg bg-bg-input px-2.5 py-1.5 transition-colors focus-within:ring-1 focus-within:ring-border-focus">
          <Search className="h-3.5 w-3.5 shrink-0 text-text-muted" />
          <input
            type="text"
            placeholder="Search…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="w-full bg-transparent text-sm text-text placeholder-text-muted outline-none"
          />
        </div>
      </div>

      {/* ─── Session list ───────────────────────────────── */}
      <div className="min-h-0 flex-1 overflow-y-auto px-2 py-1">
        {filteredSessions.length === 0 ? (
          <div className="px-3 py-8 text-center text-sm text-text-muted">
            {sessions.length === 0 ? 'No sessions yet' : 'No matches'}
          </div>
        ) : (
          filteredSessions.map((session) => (
            <button
              key={session.id}
              onClick={() => selectSession(session.id)}
              className={clsx(
                'flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left transition-colors',
                activeSessionId === session.id
                  ? 'bg-bg-elevated text-text'
                  : 'text-text-dim hover:bg-bg-hover hover:text-text',
              )}
            >
              <MessageSquare className="h-4 w-4 shrink-0 text-text-muted" />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm">{session.name ?? 'Untitled'}</div>
                <div className="mt-0.5 truncate text-[11px] text-text-muted">
                  {formatTime(session.last_interaction)}
                </div>
              </div>
            </button>
          ))
        )}
      </div>

      {/* ─── Bottom toolbar ─────────────────────────────── */}
      <div className="flex shrink-0 items-center justify-between border-t border-border px-3 py-2">
        <button
          onClick={toggleTheme}
          className="flex items-center gap-2 rounded-md px-2 py-1.5 text-sm text-text-dim transition-colors hover:bg-bg-elevated hover:text-text"
        >
          {isDark ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
          <span>{isDark ? 'Light' : 'Dark'}</span>
        </button>
        <button className="rounded-md p-1.5 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text">
          <Settings className="h-4 w-4" />
        </button>
      </div>
    </div>
  )
}
