// src/components/layout/Sidebar.tsx — Session navigation with time grouping.
//
// Features: new session, search (name + workspace), time-grouped list,
// workspace display, right-click rename, theme toggle.

import { useState, useEffect, useMemo, useRef } from 'react'
import clsx from 'clsx'
import { Plus, Search, MessageSquare, Sun, Moon, Settings, Pencil } from 'lucide-react'
import { useSessionStore } from '@/stores/sessionStore'
import { useUiStore } from '@/stores/uiStore'
import { useSession } from '@/hooks/useSession'
import { ContextMenu } from '@/components/ui/ContextMenu'
import { THEME_STORAGE_KEY } from '@/lib/constants'
import type { SessionInfo } from '@wing-agent/sdk'

interface SidebarProps {
  open: boolean
}

// ── Time grouping helpers ──────────────────────────────────────

type TimeGroup = 'Today' | 'Yesterday' | 'Last 7 days' | 'Last 30 days' | 'Older'

function getTimeGroup(iso: string | null): TimeGroup {
  if (!iso) return 'Older'
  const d = new Date(iso)
  const now = new Date()
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate())
  const startOfYesterday = new Date(startOfToday.getTime() - 86_400_000)
  const sevenDaysAgo = new Date(startOfToday.getTime() - 7 * 86_400_000)
  const thirtyDaysAgo = new Date(startOfToday.getTime() - 30 * 86_400_000)

  if (d >= startOfToday) return 'Today'
  if (d >= startOfYesterday) return 'Yesterday'
  if (d >= sevenDaysAgo) return 'Last 7 days'
  if (d >= thirtyDaysAgo) return 'Last 30 days'
  return 'Older'
}

const GROUP_ORDER: TimeGroup[] = ['Today', 'Yesterday', 'Last 7 days', 'Last 30 days', 'Older']

function groupSessions(sessions: SessionInfo[]): Map<TimeGroup, SessionInfo[]> {
  const groups = new Map<TimeGroup, SessionInfo[]>()
  for (const s of sessions) {
    const group = getTimeGroup(s.last_interaction)
    const list = groups.get(group) ?? []
    list.push(s)
    groups.set(group, list)
  }
  return groups
}

/** Shorten workspace path for display: ~/foo/bar → ~/bar */
function shortWorkspace(ws: string | null): string {
  if (!ws) return ''
  const home = ws.match(/^\/Users\/[^/]+|^\/home\/[^/]+/)
  if (home) {
    const rest = ws.slice(home[0].length)
    const parts = rest.split('/').filter(Boolean)
    if (parts.length <= 1) return `~/${parts[0] ?? ''}`
    return `~/…/${parts[parts.length - 1]}`
  }
  const parts = ws.split('/').filter(Boolean)
  return parts[parts.length - 1] ?? ws
}

// ── Component ──────────────────────────────────────────────────

export function Sidebar({ open }: SidebarProps) {
  const [search, setSearch] = useState('')
  const [isDark, setIsDark] = useState(false)
  const [renamingId, setRenamingId] = useState<string | null>(null)
  const [renameValue, setRenameValue] = useState('')
  const renameInputRef = useRef<HTMLInputElement>(null)

  const sessions = useSessionStore((s) => s.sessions)
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const { selectSession, createSession, updateSession } = useSession()

  useEffect(() => {
    setIsDark(document.documentElement.dataset.theme === 'dark')
  }, [])

  useEffect(() => {
    if (renamingId && renameInputRef.current) {
      renameInputRef.current.focus()
      renameInputRef.current.select()
    }
  }, [renamingId])

  const toggleTheme = () => {
    const next = !isDark
    setIsDark(next)
    document.documentElement.dataset.theme = next ? 'dark' : 'light'
    localStorage.setItem(THEME_STORAGE_KEY, next ? 'dark' : 'light')
  }

  // Filter by name + workspace
  const filteredSessions = useMemo(() => {
    if (!search.trim()) return sessions
    const q = search.toLowerCase()
    return sessions.filter(
      (s) =>
        (s.name ?? '').toLowerCase().includes(q) || (s.workspace ?? '').toLowerCase().includes(q),
    )
  }, [sessions, search])

  const grouped = useMemo(() => groupSessions(filteredSessions), [filteredSessions])

  const startRename = (session: SessionInfo) => {
    setRenamingId(session.id)
    setRenameValue(session.name ?? '')
  }

  const commitRename = () => {
    if (renamingId) {
      const trimmed = renameValue.trim()
      if (trimmed) {
        updateSession({ title: trimmed }, renamingId)
      }
    }
    setRenamingId(null)
  }

  const handleRenameKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') commitRename()
    else if (e.key === 'Escape') setRenamingId(null)
  }

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

  const renderSessionItem = (session: SessionInfo) => {
    const isRenaming = renamingId === session.id

    const itemContent = (
      <button
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
          {isRenaming ? (
            <input
              ref={renameInputRef}
              value={renameValue}
              onChange={(e) => setRenameValue(e.target.value)}
              onBlur={commitRename}
              onKeyDown={handleRenameKeyDown}
              onClick={(e) => e.stopPropagation()}
              className="w-full rounded border border-border-focus bg-bg-input px-1.5 py-0.5 text-sm text-text outline-none"
            />
          ) : (
            <div className="truncate text-sm">{session.name ?? 'Untitled'}</div>
          )}
          <div className="mt-0.5 flex items-center gap-1.5 text-[11px] text-text-muted">
            <span>{formatTime(session.last_interaction)}</span>
            {session.workspace && (
              <>
                <span className="text-text-muted/50">·</span>
                <span className="truncate">{shortWorkspace(session.workspace)}</span>
              </>
            )}
          </div>
        </div>
      </button>
    )

    if (isRenaming) return <div key={session.id}>{itemContent}</div>

    return (
      <ContextMenu
        key={session.id}
        menu={
          <button
            onClick={() => startRename(session)}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs text-text-dim transition-colors hover:bg-bg-hover hover:text-text"
          >
            <Pencil className="h-3.5 w-3.5" />
            <span>Rename</span>
          </button>
        }
      >
        {itemContent}
      </ContextMenu>
    )
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
            placeholder="Search sessions…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="w-full bg-transparent text-sm text-text placeholder-text-muted outline-none"
          />
        </div>
      </div>

      {/* ─── Session list (grouped) ─────────────────────── */}
      <div className="min-h-0 flex-1 overflow-y-auto px-2 py-1">
        {filteredSessions.length === 0 ? (
          <div className="px-3 py-8 text-center text-sm text-text-muted">
            {sessions.length === 0 ? 'No sessions yet' : 'No matches'}
          </div>
        ) : (
          GROUP_ORDER.map((group) => {
            const items = grouped.get(group)
            if (!items || items.length === 0) return null
            return (
              <div key={group} className="mb-2">
                <div className="px-2.5 pb-1 pt-2 text-[11px] font-medium uppercase tracking-wider text-text-muted">
                  {group}
                </div>
                {items.map(renderSessionItem)}
              </div>
            )
          })
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
        <button
          onClick={() => useUiStore.getState().toggleSettings()}
          className="rounded-md p-1.5 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text"
        >
          <Settings className="h-4 w-4" />
        </button>
      </div>
    </div>
  )
}
